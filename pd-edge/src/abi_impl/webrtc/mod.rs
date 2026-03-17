use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex, Once},
    time::Duration,
};

use ::webrtc::{
    api::{
        APIBuilder, interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
        setting_engine::SettingEngine,
    },
    data_channel::{
        RTCDataChannel, data_channel_init::RTCDataChannelInit,
        data_channel_message::DataChannelMessage,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};
use axum::body::Bytes;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use edge_abi::symbols::webrtc;
use pd_edge_host_function::pd_edge_host_function;
use vm::{CallOutcome, Value, Vm, VmError};

use super::{SharedProxyVmContext, http};

const DOWNSTREAM_CONNECTION_HANDLE: i64 = 0;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_DATA_CHANNEL_LABEL: &str = "pd-edge";

fn ensure_rustls_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WebRtcPhase {
    #[default]
    Inactive,
    Configured,
    RemoteDescriptionSet,
    OfferCreated,
    AnswerCreated,
    Connecting,
    Open,
    Closed,
    Failed,
}

impl WebRtcPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Configured => "configured",
            Self::RemoteDescriptionSet => "remote-description-set",
            Self::OfferCreated => "offer-created",
            Self::AnswerCreated => "answer-created",
            Self::Connecting => "connecting",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
enum WebRtcMessage {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Default)]
struct WebRtcRuntimeState {
    data_channel: Option<Arc<RTCDataChannel>>,
    open: bool,
    closed: bool,
    failure_message: Option<String>,
}

type SharedWebRtcRuntime = Arc<Mutex<WebRtcRuntimeState>>;
type SharedWebRtcInbox = Arc<Mutex<VecDeque<WebRtcMessage>>>;

#[derive(Clone)]
struct WebRtcIoState {
    peer: Arc<RTCPeerConnection>,
    runtime: SharedWebRtcRuntime,
    inbox: SharedWebRtcInbox,
    inbox_notify: Arc<tokio::sync::Notify>,
    open_notify: Arc<tokio::sync::Notify>,
    close_notify: Arc<tokio::sync::Notify>,
}

impl fmt::Debug for WebRtcIoState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let runtime = self.runtime.lock().expect("webrtc runtime lock poisoned");
        f.debug_struct("WebRtcIoState")
            .field("open", &runtime.open)
            .field("closed", &runtime.closed)
            .field("has_data_channel", &runtime.data_channel.is_some())
            .field("failure_message", &runtime.failure_message)
            .finish()
    }
}

impl WebRtcIoState {
    fn new(peer: Arc<RTCPeerConnection>) -> Self {
        Self {
            peer,
            runtime: Arc::new(Mutex::new(WebRtcRuntimeState::default())),
            inbox: Arc::new(Mutex::new(VecDeque::new())),
            inbox_notify: Arc::new(tokio::sync::Notify::new()),
            open_notify: Arc::new(tokio::sync::Notify::new()),
            close_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn current_data_channel(&self) -> Option<Arc<RTCDataChannel>> {
        self.runtime
            .lock()
            .expect("webrtc runtime lock poisoned")
            .data_channel
            .clone()
    }

    fn is_open(&self) -> bool {
        self.runtime
            .lock()
            .expect("webrtc runtime lock poisoned")
            .open
    }

    fn is_closed(&self) -> bool {
        self.runtime
            .lock()
            .expect("webrtc runtime lock poisoned")
            .closed
    }

    fn failure_message(&self) -> Option<String> {
        self.runtime
            .lock()
            .expect("webrtc runtime lock poisoned")
            .failure_message
            .clone()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WebRtcConnectionState {
    phase: WebRtcPhase,
    present: bool,
    data_channel_label: String,
    ice_server_urls: Vec<String>,
    local_description_json: Option<String>,
    remote_description_json: Option<String>,
    failure_message: Option<String>,
    io: Option<Arc<WebRtcIoState>>,
}

impl WebRtcConnectionState {
    fn configure(&mut self) {
        self.present = true;
        self.failure_message = None;
        if self.data_channel_label.is_empty() {
            self.data_channel_label = DEFAULT_DATA_CHANNEL_LABEL.to_string();
        }
        if matches!(
            self.phase,
            WebRtcPhase::Inactive | WebRtcPhase::Closed | WebRtcPhase::Failed
        ) {
            self.phase = WebRtcPhase::Configured;
        }
    }

    fn peer_created(&self) -> bool {
        self.io.is_some()
    }

    fn set_ice_server_urls(&mut self, ice_server_urls: Vec<String>) {
        self.configure();
        self.ice_server_urls = ice_server_urls;
    }

    fn set_data_channel_label(&mut self, label: String) {
        self.configure();
        self.data_channel_label = label;
    }

    fn set_remote_description_json(&mut self, description_json: String) {
        self.configure();
        self.remote_description_json = Some(description_json);
        self.phase = WebRtcPhase::RemoteDescriptionSet;
    }

    fn set_local_description_json(&mut self, description_json: String, phase: WebRtcPhase) {
        self.configure();
        self.local_description_json = Some(description_json);
        self.phase = phase;
    }

    fn note_connecting(&mut self) {
        self.configure();
        if !self.is_open() {
            self.phase = WebRtcPhase::Connecting;
        }
    }

    fn attach_io(&mut self, io: Arc<WebRtcIoState>) {
        self.configure();
        self.io = Some(io);
    }

    pub(crate) fn refresh_async_state(&mut self) {
        let Some(io) = self.io.as_ref() else {
            return;
        };
        if let Some(message) = io.failure_message() {
            self.phase = WebRtcPhase::Failed;
            self.present = true;
            self.failure_message = Some(message);
            return;
        }
        if io.is_open() {
            self.phase = WebRtcPhase::Open;
            self.failure_message = None;
            return;
        }
        if io.is_closed() {
            self.phase = WebRtcPhase::Closed;
        }
    }

    fn is_open(&self) -> bool {
        self.io.as_ref().is_some_and(|io| io.is_open())
    }

    fn io(&self) -> Option<Arc<WebRtcIoState>> {
        self.io.clone()
    }

    pub(crate) fn is_present(&self) -> bool {
        self.present
    }

    pub(crate) fn phase(&self) -> WebRtcPhase {
        self.phase
    }

    fn ice_server_urls(&self) -> &[String] {
        &self.ice_server_urls
    }

    fn data_channel_label(&self) -> &str {
        if self.data_channel_label.is_empty() {
            DEFAULT_DATA_CHANNEL_LABEL
        } else {
            &self.data_channel_label
        }
    }

    fn eof(&mut self) -> bool {
        self.refresh_async_state();
        matches!(self.phase, WebRtcPhase::Closed | WebRtcPhase::Failed)
    }

    fn mark_closed(&mut self) {
        self.phase = WebRtcPhase::Closed;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebRtcHandle {
    Downstream,
    DefaultUpstream,
    Dynamic(i64),
}

fn decode_connection(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<WebRtcHandle, VmError> {
    if connection == DOWNSTREAM_CONNECTION_HANDLE {
        return Ok(WebRtcHandle::Downstream);
    }
    if connection == http::default_upstream_webrtc_connection_handle() {
        return Ok(WebRtcHandle::DefaultUpstream);
    }
    if http::webrtc_connection_exists(context, connection) {
        return Ok(WebRtcHandle::Dynamic(connection));
    }
    Err(VmError::HostError(format!(
        "invalid webrtc connection handle {connection}; reserved handles are 0 (downstream), 1 (default upstream), and allocated handles start at 2",
    )))
}

fn webrtc_connection_operation_on_downstream() -> VmError {
    VmError::HostError(
        "downstream webrtc connections are unavailable in the current one-shot HTTP runtime"
            .to_string(),
    )
}

fn parse_csv_urls(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn connection_state(
    context: &SharedProxyVmContext,
    connection: WebRtcHandle,
) -> WebRtcConnectionState {
    let guard = context.lock_webrtc();
    match connection {
        WebRtcHandle::Downstream => WebRtcConnectionState::default(),
        WebRtcHandle::DefaultUpstream => guard.default_upstream_webrtc.clone(),
        WebRtcHandle::Dynamic(handle) => guard
            .webrtc_connections
            .get(&handle)
            .expect("webrtc connection should exist while handle is in use")
            .clone(),
    }
}

fn with_connection_state_mut<T>(
    context: &SharedProxyVmContext,
    connection: i64,
    mutate: impl FnOnce(&mut WebRtcConnectionState) -> Result<T, VmError>,
) -> Result<T, VmError> {
    let handle = decode_connection(context, connection)?;
    let mut guard = context.lock_webrtc();
    match handle {
        WebRtcHandle::Downstream => Err(webrtc_connection_operation_on_downstream()),
        WebRtcHandle::DefaultUpstream => mutate(&mut guard.default_upstream_webrtc),
        WebRtcHandle::Dynamic(handle) => mutate(
            guard
                .webrtc_connections
                .get_mut(&handle)
                .expect("webrtc connection should exist while handle is in use"),
        ),
    }
}

async fn create_peer_connection(ice_server_urls: &[String]) -> Result<Arc<WebRtcIoState>, VmError> {
    ensure_rustls_provider();
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|err| VmError::HostError(format!("failed to register webrtc codecs: {err}")))?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine).map_err(|err| {
        VmError::HostError(format!("failed to register webrtc interceptors: {err}"))
    })?;
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_include_loopback_candidate(true);
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();
    let ice_servers = if ice_server_urls.is_empty() {
        Vec::new()
    } else {
        vec![RTCIceServer {
            urls: ice_server_urls.to_vec(),
            ..Default::default()
        }]
    };
    let peer = Arc::new(
        api.new_peer_connection(RTCConfiguration {
            ice_servers,
            ..Default::default()
        })
        .await
        .map_err(|err| {
            VmError::HostError(format!("failed to create webrtc peer connection: {err}"))
        })?,
    );
    let io = Arc::new(WebRtcIoState::new(peer.clone()));
    attach_peer_callbacks(&peer, &io).await;
    Ok(io)
}

async fn attach_peer_callbacks(peer: &Arc<RTCPeerConnection>, io: &Arc<WebRtcIoState>) {
    let runtime = io.runtime.clone();
    let close_notify = io.close_notify.clone();
    peer.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let runtime = runtime.clone();
        let close_notify = close_notify.clone();
        Box::pin(async move {
            let mut runtime = runtime.lock().expect("webrtc runtime lock poisoned");
            match state {
                RTCPeerConnectionState::Connected => {}
                RTCPeerConnectionState::Failed => {
                    runtime.failure_message = Some("webrtc peer connection failed".to_string());
                    runtime.closed = true;
                }
                RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed => {
                    runtime.closed = true;
                }
                _ => {}
            }
            drop(runtime);
            close_notify.notify_waiters();
        })
    }));

    let io_for_data_channel = io.clone();
    peer.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
        let io_for_data_channel = io_for_data_channel.clone();
        Box::pin(async move {
            attach_data_channel_handlers(data_channel, io_for_data_channel).await;
        })
    }));
}

async fn attach_data_channel_handlers(data_channel: Arc<RTCDataChannel>, io: Arc<WebRtcIoState>) {
    {
        let mut runtime = io.runtime.lock().expect("webrtc runtime lock poisoned");
        runtime.data_channel = Some(data_channel.clone());
        runtime.closed = false;
        runtime.failure_message = None;
    }

    let runtime = io.runtime.clone();
    let open_notify = io.open_notify.clone();
    let channel_for_open = data_channel.clone();
    data_channel.on_open(Box::new(move || {
        let runtime = runtime.clone();
        let open_notify = open_notify.clone();
        let channel_for_open = channel_for_open.clone();
        Box::pin(async move {
            let mut runtime = runtime.lock().expect("webrtc runtime lock poisoned");
            runtime.data_channel = Some(channel_for_open);
            runtime.open = true;
            runtime.closed = false;
            runtime.failure_message = None;
            drop(runtime);
            open_notify.notify_waiters();
        })
    }));

    let inbox = io.inbox.clone();
    let inbox_notify = io.inbox_notify.clone();
    data_channel.on_message(Box::new(move |message: DataChannelMessage| {
        let inbox = inbox.clone();
        let inbox_notify = inbox_notify.clone();
        Box::pin(async move {
            let next = if message.is_string {
                WebRtcMessage::Text(String::from_utf8_lossy(&message.data).into_owned())
            } else {
                WebRtcMessage::Binary(message.data.to_vec())
            };
            inbox
                .lock()
                .expect("webrtc inbox lock poisoned")
                .push_back(next);
            inbox_notify.notify_waiters();
        })
    }));

    let runtime = io.runtime.clone();
    let close_notify = io.close_notify.clone();
    data_channel.on_close(Box::new(move || {
        let runtime = runtime.clone();
        let close_notify = close_notify.clone();
        Box::pin(async move {
            let mut runtime = runtime.lock().expect("webrtc runtime lock poisoned");
            runtime.open = false;
            runtime.closed = true;
            drop(runtime);
            close_notify.notify_waiters();
        })
    }));
}

async fn ensure_peer_connection(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Arc<WebRtcIoState>, VmError> {
    let handle = decode_connection(context, connection)?;
    if let Some(io) = connection_state(context, handle).io() {
        return Ok(io);
    }
    let state = connection_state(context, handle);
    let io = create_peer_connection(state.ice_server_urls()).await?;
    with_connection_state_mut(context, connection, |state| {
        state.attach_io(io.clone());
        Ok(())
    })?;
    Ok(io)
}

async fn ensure_local_data_channel(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Arc<RTCDataChannel>, VmError> {
    let io = ensure_peer_connection(context, connection).await?;
    if let Some(channel) = io.current_data_channel() {
        return Ok(channel);
    }
    let label = connection_state(context, decode_connection(context, connection)?)
        .data_channel_label()
        .to_string();
    let channel = io
        .peer
        .create_data_channel(&label, Some(RTCDataChannelInit::default()))
        .await
        .map_err(|err| {
            VmError::HostError(format!("failed to create webrtc data channel: {err}"))
        })?;
    attach_data_channel_handlers(channel.clone(), io).await;
    Ok(channel)
}

async fn wait_until_open(io: &Arc<WebRtcIoState>) -> Result<bool, VmError> {
    if io.is_open() {
        return Ok(true);
    }
    if let Some(message) = io.failure_message() {
        return Err(VmError::HostError(message));
    }
    let _ = tokio::time::timeout(CONNECT_TIMEOUT, io.open_notify.notified()).await;
    if let Some(message) = io.failure_message() {
        return Err(VmError::HostError(message));
    }
    Ok(io.is_open())
}

async fn ensure_connection_open(
    context: &SharedProxyVmContext,
    connection: i64,
) -> Result<Arc<WebRtcIoState>, VmError> {
    let io = ensure_peer_connection(context, connection).await?;
    with_connection_state_mut(context, connection, |state| {
        state.note_connecting();
        Ok(())
    })?;
    if !wait_until_open(&io).await? {
        return Err(VmError::HostError(
            "webrtc connection did not reach the open state before timeout".to_string(),
        ));
    }
    with_connection_state_mut(context, connection, |state| {
        state.refresh_async_state();
        Ok(())
    })?;
    Ok(io)
}

fn parse_description_json(description_json: &str) -> Result<RTCSessionDescription, VmError> {
    serde_json::from_str(description_json).map_err(|err| {
        VmError::HostError(format!(
            "webrtc session description must be valid JSON: {err}",
        ))
    })
}

fn serialize_description_json(description: &RTCSessionDescription) -> Result<String, VmError> {
    serde_json::to_string(description).map_err(|err| {
        VmError::HostError(format!(
            "failed to serialize webrtc session description: {err}",
        ))
    })
}

async fn pop_next_message(io: &Arc<WebRtcIoState>) -> Result<Option<WebRtcMessage>, VmError> {
    loop {
        if let Some(message) = io
            .inbox
            .lock()
            .expect("webrtc inbox lock poisoned")
            .pop_front()
        {
            return Ok(Some(message));
        }
        if let Some(message) = io.failure_message() {
            return Err(VmError::HostError(message));
        }
        if io.is_closed() {
            return Ok(None);
        }
        io.inbox_notify.notified().await;
    }
}

/// Allocates a WebRTC connection handle.
#[pd_edge_host_function(name = webrtc::connection::NEW.name, scope = webrtc)]
async fn connection_new(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    let handle = http::allocate_webrtc_connection_handle(&context)?;
    Ok(CallOutcome::Return(vec![Value::Int(handle)]))
}

/// Returns the WebRTC connection handle for the current downstream flow.
#[pd_edge_host_function(name = webrtc::connection::DOWNSTREAM.name, scope = webrtc)]
async fn connection_downstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        DOWNSTREAM_CONNECTION_HANDLE,
    )]))
}

/// Returns the default upstream handle for the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::DEFAULT_UPSTREAM.name, scope = webrtc)]
async fn connection_default_upstream(
    _vm: &mut Vm,
    _context: SharedProxyVmContext,
) -> Result<CallOutcome, VmError> {
    Ok(CallOutcome::Return(vec![Value::Int(
        http::default_upstream_webrtc_connection_handle(),
    )]))
}

/// Returns whether the WebRTC connection handle is present.
#[pd_edge_host_function(name = webrtc::connection::IS_PRESENT.name, scope = webrtc)]
async fn connection_is_present(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let present = match decode_connection(&context, connection)? {
        WebRtcHandle::Downstream => false,
        handle => connection_state(&context, handle).is_present(),
    };
    Ok(CallOutcome::Return(vec![Value::Bool(present)]))
}

/// Sets the ICE server list for the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::SET_ICE_SERVERS.name, scope = webrtc)]
async fn connection_set_ice_servers(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    urls: String,
) -> Result<CallOutcome, VmError> {
    let urls = parse_csv_urls(&urls);
    with_connection_state_mut(&context, connection, |state| {
        if state.peer_created() {
            return Err(VmError::HostError(
                "webrtc ice server configuration is read-only after the peer connection is created"
                    .to_string(),
            ));
        }
        state.set_ice_server_urls(urls);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the data channel label for the WebRTC connection.
#[pd_edge_host_function(
    name = webrtc::connection::SET_DATA_CHANNEL_LABEL.name,
    scope = webrtc
)]
async fn connection_set_data_channel_label(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    label: String,
) -> Result<CallOutcome, VmError> {
    if label.trim().is_empty() {
        return Err(VmError::HostError(
            "webrtc data channel label must not be empty".to_string(),
        ));
    }
    with_connection_state_mut(&context, connection, |state| {
        if state.peer_created() {
            return Err(VmError::HostError(
                "webrtc data channel label is read-only after the peer connection is created"
                    .to_string(),
            ));
        }
        state.set_data_channel_label(label);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Sets the remote session description for the WebRTC connection.
#[pd_edge_host_function(
    name = webrtc::connection::SET_REMOTE_DESCRIPTION.name,
    scope = webrtc
)]
async fn connection_set_remote_description(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    description_json: String,
) -> Result<CallOutcome, VmError> {
    let description = parse_description_json(&description_json)?;
    let io = ensure_peer_connection(&context, connection).await?;
    io.peer
        .set_remote_description(description)
        .await
        .map_err(|err| {
            VmError::HostError(format!("failed to set webrtc remote description: {err}"))
        })?;
    with_connection_state_mut(&context, connection, |state| {
        state.set_remote_description_json(description_json);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

/// Creates an SDP offer for the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::CREATE_OFFER.name, scope = webrtc)]
async fn connection_create_offer(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let io = ensure_peer_connection(&context, connection).await?;
    let _ = ensure_local_data_channel(&context, connection).await?;
    let offer = io
        .peer
        .create_offer(None)
        .await
        .map_err(|err| VmError::HostError(format!("failed to create webrtc offer: {err}")))?;
    let mut gather_complete = io.peer.gathering_complete_promise().await;
    io.peer
        .set_local_description(offer)
        .await
        .map_err(|err| VmError::HostError(format!("failed to set webrtc local offer: {err}")))?;
    let _ = gather_complete.recv().await;
    let local = io
        .peer
        .local_description()
        .await
        .ok_or_else(|| VmError::HostError("webrtc local offer is unavailable".to_string()))?;
    let json = serialize_description_json(&local)?;
    with_connection_state_mut(&context, connection, |state| {
        state.set_local_description_json(json.clone(), WebRtcPhase::OfferCreated);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![Value::string(json)]))
}

/// Creates an SDP answer for the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::CREATE_ANSWER.name, scope = webrtc)]
async fn connection_create_answer(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let io = ensure_peer_connection(&context, connection).await?;
    let answer = io
        .peer
        .create_answer(None)
        .await
        .map_err(|err| VmError::HostError(format!("failed to create webrtc answer: {err}")))?;
    let mut gather_complete = io.peer.gathering_complete_promise().await;
    io.peer
        .set_local_description(answer)
        .await
        .map_err(|err| VmError::HostError(format!("failed to set webrtc local answer: {err}")))?;
    let _ = gather_complete.recv().await;
    let local = io
        .peer
        .local_description()
        .await
        .ok_or_else(|| VmError::HostError("webrtc local answer is unavailable".to_string()))?;
    let json = serialize_description_json(&local)?;
    with_connection_state_mut(&context, connection, |state| {
        state.set_local_description_json(json.clone(), WebRtcPhase::AnswerCreated);
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![Value::string(json)]))
}

/// Attempts to connect the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::CONNECT.name, scope = webrtc)]
async fn connection_connect(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let io = ensure_peer_connection(&context, connection).await?;
    with_connection_state_mut(&context, connection, |state| {
        state.note_connecting();
        Ok(())
    })?;
    let open = wait_until_open(&io).await?;
    with_connection_state_mut(&context, connection, |state| {
        state.refresh_async_state();
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![Value::Bool(open)]))
}

/// Returns the current phase for the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::GET_PHASE.name, scope = webrtc)]
async fn connection_get_phase(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let phase = match decode_connection(&context, connection)? {
        WebRtcHandle::Downstream => WebRtcPhase::Inactive,
        handle => {
            let mut state = connection_state(&context, handle);
            state.refresh_async_state();
            state.phase()
        }
    };
    Ok(CallOutcome::Return(vec![Value::string(phase.as_str())]))
}

/// Sends a text message over the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::SEND_TEXT.name, scope = webrtc)]
async fn connection_send_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    text: String,
) -> Result<CallOutcome, VmError> {
    let io = ensure_connection_open(&context, connection).await?;
    let data_channel = io.current_data_channel().ok_or_else(|| {
        VmError::HostError(
            "webrtc data channel is unavailable before the connection opens".to_string(),
        )
    })?;
    let sent = data_channel
        .send_text(text)
        .await
        .map_err(|err| VmError::HostError(format!("failed to send webrtc text message: {err}")))?;
    Ok(CallOutcome::Return(vec![Value::Int(sent as i64)]))
}

/// Reads a text message from the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::READ_TEXT.name, scope = webrtc)]
async fn connection_read_text(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let io = ensure_connection_open(&context, connection).await?;
    let message = pop_next_message(&io).await?;
    let text = match message {
        Some(WebRtcMessage::Text(text)) => text,
        Some(WebRtcMessage::Binary(_)) => {
            return Err(VmError::HostError(
                "next webrtc message is binary; call webrtc::connection::read_binary_base64"
                    .to_string(),
            ));
        }
        None => String::new(),
    };
    Ok(CallOutcome::Return(vec![Value::string(text)]))
}

/// Sends a base64-encoded binary message over the WebRTC connection.
#[pd_edge_host_function(
    name = webrtc::connection::SEND_BINARY_BASE64.name,
    scope = webrtc
)]
async fn connection_send_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
    payload: String,
) -> Result<CallOutcome, VmError> {
    let bytes = STANDARD.decode(payload).map_err(|err| {
        VmError::HostError(format!(
            "webrtc binary payload must be base64 encoded: {err}",
        ))
    })?;
    let io = ensure_connection_open(&context, connection).await?;
    let data_channel = io.current_data_channel().ok_or_else(|| {
        VmError::HostError(
            "webrtc data channel is unavailable before the connection opens".to_string(),
        )
    })?;
    let sent = data_channel
        .send(&Bytes::from(bytes))
        .await
        .map_err(|err| {
            VmError::HostError(format!("failed to send webrtc binary message: {err}"))
        })?;
    Ok(CallOutcome::Return(vec![Value::Int(sent as i64)]))
}

/// Reads a base64-encoded binary message from the WebRTC connection.
#[pd_edge_host_function(
    name = webrtc::connection::READ_BINARY_BASE64.name,
    scope = webrtc
)]
async fn connection_read_binary_base64(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let io = ensure_connection_open(&context, connection).await?;
    let message = pop_next_message(&io).await?;
    let payload = match message {
        Some(WebRtcMessage::Binary(bytes)) => STANDARD.encode(bytes),
        Some(WebRtcMessage::Text(_)) => {
            return Err(VmError::HostError(
                "next webrtc message is text; call webrtc::connection::read_text".to_string(),
            ));
        }
        None => String::new(),
    };
    Ok(CallOutcome::Return(vec![Value::string(payload)]))
}

/// Returns whether the WebRTC connection has reached EOF.
#[pd_edge_host_function(name = webrtc::connection::EOF.name, scope = webrtc)]
async fn connection_eof(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let eof = match decode_connection(&context, connection)? {
        WebRtcHandle::Downstream => false,
        handle => {
            let mut state = connection_state(&context, handle);
            state.eof()
        }
    };
    Ok(CallOutcome::Return(vec![Value::Bool(eof)]))
}

/// Closes the WebRTC connection.
#[pd_edge_host_function(name = webrtc::connection::CLOSE.name, scope = webrtc)]
async fn connection_close(
    _vm: &mut Vm,
    context: SharedProxyVmContext,
    connection: i64,
) -> Result<CallOutcome, VmError> {
    let io = ensure_peer_connection(&context, connection).await?;
    io.peer
        .close()
        .await
        .map_err(|err| VmError::HostError(format!("failed to close webrtc connection: {err}")))?;
    with_connection_state_mut(&context, connection, |state| {
        state.mark_closed();
        Ok(())
    })?;
    Ok(CallOutcome::Return(vec![]))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::HeaderMap;

    use super::*;
    use crate::abi_impl::{ProxyVmContext, RateLimiterStore};

    fn test_context() -> SharedProxyVmContext {
        Arc::new(ProxyVmContext::from_request_headers(
            HeaderMap::new(),
            Arc::new(RateLimiterStore::new()),
        ))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn webrtc_connections_can_exchange_text_messages() {
        let left = test_context();
        let right = test_context();

        let left_handle = http::allocate_webrtc_connection_handle(&left).expect("left handle");
        let right_handle = http::allocate_webrtc_connection_handle(&right).expect("right handle");

        let offer = create_offer_for_test(&left, left_handle)
            .await
            .expect("left offer should build");
        set_remote_description_for_test(&right, right_handle, offer)
            .await
            .expect("right should accept offer");
        let answer = create_answer_for_test(&right, right_handle)
            .await
            .expect("right answer should build");
        set_remote_description_for_test(&left, left_handle, answer)
            .await
            .expect("left should accept answer");

        assert!(
            connect_for_test(&left, left_handle)
                .await
                .expect("left should connect")
        );
        assert!(
            connect_for_test(&right, right_handle)
                .await
                .expect("right should connect")
        );

        send_text_for_test(&left, left_handle, "ping")
            .await
            .expect("left send should succeed");
        let echoed = read_text_for_test(&right, right_handle)
            .await
            .expect("right read should succeed");
        assert_eq!(echoed, "ping");
    }

    async fn create_offer_for_test(
        context: &SharedProxyVmContext,
        connection: i64,
    ) -> Result<String, VmError> {
        let io = ensure_peer_connection(context, connection).await?;
        let _ = ensure_local_data_channel(context, connection).await?;
        let offer =
            io.peer.create_offer(None).await.map_err(|err| {
                VmError::HostError(format!("failed to create webrtc offer: {err}"))
            })?;
        let mut gather_complete = io.peer.gathering_complete_promise().await;
        io.peer.set_local_description(offer).await.map_err(|err| {
            VmError::HostError(format!("failed to set webrtc local offer: {err}"))
        })?;
        let _ = gather_complete.recv().await;
        let local =
            io.peer.local_description().await.ok_or_else(|| {
                VmError::HostError("webrtc local offer is unavailable".to_string())
            })?;
        let json = serialize_description_json(&local)?;
        with_connection_state_mut(context, connection, |state| {
            state.set_local_description_json(json.clone(), WebRtcPhase::OfferCreated);
            Ok(())
        })?;
        Ok(json)
    }

    async fn set_remote_description_for_test(
        context: &SharedProxyVmContext,
        connection: i64,
        description_json: String,
    ) -> Result<(), VmError> {
        let description = parse_description_json(&description_json)?;
        let io = ensure_peer_connection(context, connection).await?;
        io.peer
            .set_remote_description(description)
            .await
            .map_err(|err| {
                VmError::HostError(format!("failed to set webrtc remote description: {err}"))
            })?;
        with_connection_state_mut(context, connection, |state| {
            state.set_remote_description_json(description_json);
            Ok(())
        })?;
        Ok(())
    }

    async fn create_answer_for_test(
        context: &SharedProxyVmContext,
        connection: i64,
    ) -> Result<String, VmError> {
        let io = ensure_peer_connection(context, connection).await?;
        let mut gather_complete = io.peer.gathering_complete_promise().await;
        let answer =
            io.peer.create_answer(None).await.map_err(|err| {
                VmError::HostError(format!("failed to create webrtc answer: {err}"))
            })?;
        io.peer.set_local_description(answer).await.map_err(|err| {
            VmError::HostError(format!("failed to set webrtc local answer: {err}"))
        })?;
        let _ = gather_complete.recv().await;
        let local =
            io.peer.local_description().await.ok_or_else(|| {
                VmError::HostError("webrtc local answer is unavailable".to_string())
            })?;
        let json = serialize_description_json(&local)?;
        with_connection_state_mut(context, connection, |state| {
            state.set_local_description_json(json.clone(), WebRtcPhase::AnswerCreated);
            Ok(())
        })?;
        Ok(json)
    }

    async fn connect_for_test(
        context: &SharedProxyVmContext,
        connection: i64,
    ) -> Result<bool, VmError> {
        let io = ensure_peer_connection(context, connection).await?;
        with_connection_state_mut(context, connection, |state| {
            state.note_connecting();
            Ok(())
        })?;
        let open = wait_until_open(&io).await?;
        with_connection_state_mut(context, connection, |state| {
            state.refresh_async_state();
            Ok(())
        })?;
        Ok(open)
    }

    async fn send_text_for_test(
        context: &SharedProxyVmContext,
        connection: i64,
        text: &str,
    ) -> Result<(), VmError> {
        let io = ensure_connection_open(context, connection).await?;
        let data_channel = io.current_data_channel().ok_or_else(|| {
            VmError::HostError(
                "webrtc data channel is unavailable before the connection opens".to_string(),
            )
        })?;
        data_channel.send_text(text).await.map_err(|err| {
            VmError::HostError(format!("failed to send webrtc text message: {err}"))
        })?;
        Ok(())
    }

    async fn read_text_for_test(
        context: &SharedProxyVmContext,
        connection: i64,
    ) -> Result<String, VmError> {
        let io = ensure_connection_open(context, connection).await?;
        match pop_next_message(&io).await? {
            Some(WebRtcMessage::Text(text)) => Ok(text),
            Some(WebRtcMessage::Binary(_)) => Err(VmError::HostError(
                "next webrtc message is binary; expected text".to_string(),
            )),
            None => Ok(String::new()),
        }
    }
}
