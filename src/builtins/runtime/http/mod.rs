use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::typed::{VmMap, VmMapHandle};
use super::{CallOutcome, CaptureAsyncHostContext, borrow_arg, return_one};
use crate::host_api::{
    HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema, HostStructField,
    HostStructSchema, HostTypeSchema,
};
use crate::vm::{HostFunctionRegistry, HostFutureOutput, Value, Vm, VmError, VmResult};

mod config;
pub(super) mod policy;
pub(super) mod request;
pub(super) mod sse;

pub use config::HttpConfig;
use policy::{CompositeConnectionPermit, ConnectionAdmission};

const DEFAULT_MAX_HTTP_IN_FLIGHT: usize = 64;
const DEFAULT_MAX_WORKER_HTTP_IN_FLIGHT: usize = usize::MAX;

struct HttpClientEntry {
    config: HttpConfig,
    client: request::HttpClient,
    worker_admission: ConnectionAdmission,
}

/// Opaque, cloneable access to one embedding-owned Hyper client.
///
/// Cloning this lease shares the client, connector, connection pool, and
/// VM-local admission scope. Every lease from the same worker entry retains the
/// shared worker admission gate, while each `client_for` call gets a distinct
/// local scope. The lease does not expose Hyper types to embedders and can be
/// safely captured by request-owned futures.
#[derive(Clone)]
pub struct HttpClientLease {
    entry: Arc<HttpClientEntry>,
    local_admission: ConnectionAdmission,
}

impl HttpClientLease {
    fn from_entry(entry: &Arc<HttpClientEntry>) -> Self {
        Self {
            entry: Arc::clone(entry),
            local_admission: ConnectionAdmission::new(DEFAULT_MAX_HTTP_IN_FLIGHT),
        }
    }

    fn matches_config(&self, config: &HttpConfig) -> bool {
        self.entry.config == *config
    }

    fn acquire(&self) -> VmResult<CompositeConnectionPermit> {
        let worker_permit = self.entry.worker_admission.acquire()?;
        let local_permit = match self.local_admission.acquire() {
            Ok(permit) => permit,
            Err(error) => {
                drop(worker_permit);
                return Err(error);
            }
        };
        Ok(CompositeConnectionPermit::new(worker_permit, local_permit))
    }

    fn client(&self) -> &request::HttpClient {
        &self.entry.client
    }

    fn set_local_max_in_flight(&self, max_in_flight: usize) {
        self.local_admission.set_max_in_flight(max_in_flight);
    }

    fn local_max_in_flight(&self) -> usize {
        self.local_admission.max_in_flight()
    }

    fn worker_max_in_flight(&self) -> usize {
        self.entry.worker_admission.max_in_flight()
    }
}

/// Embedding-owned HTTP clients and their worker-level admission gates.
///
/// One resource owner may be held for a worker lifetime. A client is built once
/// for each full [`HttpConfig`] identity and is reused by every VM that receives
/// a lease from this resource. Each returned lease has its own VM-local
/// admission scope. Hyper and hyper-util remain the owners of connectors,
/// sockets, pools, and connection drivers.
pub struct HttpWorkerResources {
    clients: HashMap<HttpConfig, Arc<HttpClientEntry>>,
    max_in_flight: usize,
    admission_open: bool,
}

impl HttpWorkerResources {
    /// Creates an empty worker resource owner with no practical aggregate cap.
    pub fn new() -> Self {
        Self::with_max_in_flight(DEFAULT_MAX_WORKER_HTTP_IN_FLIGHT)
    }

    /// Creates an empty worker resource owner with an explicit aggregate cap.
    pub fn with_max_in_flight(max_in_flight: usize) -> Self {
        Self {
            clients: HashMap::new(),
            max_in_flight,
            admission_open: true,
        }
    }

    /// Returns the shared client lease for a complete HTTP policy identity.
    ///
    /// Configuration validation runs before client lookup. Once admission is
    /// closed, no new lease or request permit can be created; existing leases
    /// remain owned by their callers until they are dropped.
    pub fn client_for(&mut self, config: &HttpConfig) -> VmResult<HttpClientLease> {
        config.validate()?;
        if !self.admission_open {
            return Err(VmError::HostError(
                "HTTP worker resource admission is closed".to_string(),
            ));
        }
        if let Some(entry) = self.clients.get(config) {
            return Ok(HttpClientLease::from_entry(entry));
        }
        let entry = Arc::new(HttpClientEntry {
            config: config.clone(),
            client: request::build_client(config),
            worker_admission: ConnectionAdmission::new(self.max_in_flight),
        });
        self.clients.insert(config.clone(), Arc::clone(&entry));
        Ok(HttpClientLease::from_entry(&entry))
    }

    /// Updates the shared admission cap for existing and future policy keys.
    pub fn set_max_in_flight(&mut self, max_in_flight: usize) {
        self.max_in_flight = max_in_flight;
        for entry in self.clients.values() {
            entry.worker_admission.set_max_in_flight(max_in_flight);
        }
    }

    /// Returns the configured worker-level admission cap.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// Stops new leases and request permits while allowing existing permits to
    /// retire normally.
    pub fn close_admission(&mut self) {
        self.admission_open = false;
        for entry in self.clients.values() {
            entry.worker_admission.close();
        }
    }

    /// Moves the worker-owned clients into an owned, non-blocking shutdown
    /// future. The future contains no VM, request, or raw-pointer borrow.
    pub fn into_shutdown(mut self) -> HttpWorkerShutdown {
        self.close_admission();
        let clients = std::mem::take(&mut self.clients);
        HttpWorkerShutdown {
            clients: Some(clients),
        }
    }
}

impl Drop for HttpWorkerResources {
    fn drop(&mut self) {
        self.close_admission();
    }
}

impl Default for HttpWorkerResources {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned cleanup state for one [`HttpWorkerResources`] instance.
///
/// The current Hyper legacy client has no separate caller task set to await;
/// polling this future releases the resource owner's client map immediately.
/// Any outstanding [`HttpClientLease`] keeps its shared client alive until its
/// own request work retires.
pub struct HttpWorkerShutdown {
    clients: Option<HashMap<HttpConfig, Arc<HttpClientEntry>>>,
}

impl Future for HttpWorkerShutdown {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().clients.take();
        Poll::Ready(())
    }
}

/// Persistent, per-VM HTTP module state.
///
/// The VM stores only request policy, an optional admission override, and an
/// opaque lease injected by its embedding. It never constructs or owns a
/// Hyper client. The state survives [`Vm::reset_for_reuse`], while clearing
/// configuration drops only this VM's lease clone.
#[derive(Default)]
struct HttpHostState {
    config: Option<HttpConfig>,
    client: Option<HttpClientLease>,
    max_in_flight: Option<usize>,
}

/// HTTP host configuration and explicit worker-client injection.
///
/// The embedding owns [`HttpWorkerResources`], obtains a policy-matched lease,
/// and passes that lease into [`Self::configure_http`]. There is no implicit
/// per-VM client fallback.
pub trait HttpHostExt {
    fn configure_http(&mut self, config: HttpConfig, client: HttpClientLease) -> VmResult<()>;
    fn set_http_max_in_flight(&mut self, max_in_flight: usize);
    fn http_max_in_flight(&mut self) -> usize;
    fn clear_http_configuration(&mut self);
    fn http_is_configured(&mut self) -> bool;
}

impl HttpHostExt for Vm {
    fn configure_http(&mut self, config: HttpConfig, client: HttpClientLease) -> VmResult<()> {
        config.validate()?;
        if !client.matches_config(&config) {
            return Err(VmError::HostError(
                "HTTP client lease does not match HTTP configuration".to_string(),
            ));
        }
        let mut ctx = self.host_context();
        let max_in_flight = ctx
            .module_state::<HttpHostState>()
            .and_then(|state| state.max_in_flight);
        if let Some(max_in_flight) = max_in_flight {
            client.set_local_max_in_flight(max_in_flight);
        }
        ctx.set_module_state(HttpHostState {
            config: Some(config),
            client: Some(client),
            max_in_flight,
        });
        Ok(())
    }

    fn set_http_max_in_flight(&mut self, max_in_flight: usize) {
        let mut ctx = self.host_context();
        if ctx.module_state::<HttpHostState>().is_none() {
            ctx.set_module_state(HttpHostState::default());
        }
        let state = ctx
            .module_state_mut::<HttpHostState>()
            .expect("HTTP host state was inserted");
        state.max_in_flight = Some(max_in_flight);
        if let Some(client) = state.client.as_ref() {
            client.set_local_max_in_flight(max_in_flight);
        }
    }

    fn http_max_in_flight(&mut self) -> usize {
        self.host_context()
            .module_state::<HttpHostState>()
            .map(|state| {
                let local = match state.client.as_ref() {
                    Some(client) => client.local_max_in_flight(),
                    None => state.max_in_flight.unwrap_or(DEFAULT_MAX_HTTP_IN_FLIGHT),
                };
                state
                    .client
                    .as_ref()
                    .map_or(local, |client| local.min(client.worker_max_in_flight()))
            })
            .unwrap_or(DEFAULT_MAX_HTTP_IN_FLIGHT)
    }

    fn clear_http_configuration(&mut self) {
        let mut ctx = self.host_context();
        if let Some(state) = ctx.module_state_mut::<HttpHostState>() {
            state.config = None;
            state.client = None;
        }
    }

    fn http_is_configured(&mut self) -> bool {
        self.host_context()
            .module_state::<HttpHostState>()
            .is_some_and(|state| state.config.is_some() && state.client.is_some())
    }
}

/// Captured HTTP configuration, shared Hyper client lease, and one in-flight permit.
pub(super) struct HttpRequestContext {
    config: HttpConfig,
    client: HttpClientLease,
    permit: CompositeConnectionPermit,
    prepared_request: Option<(request::HttpRequest, Instant)>,
}

impl HttpRequestContext {
    /// Captures persistent HTTP state without leaving a VM borrow in the
    /// macro-owned future.
    ///
    /// Deadline validation precedes admission so an unrepresentable script
    /// timeout remains the first reported error even when capacity is full.
    pub(super) fn capture_for(
        vm: &mut Vm,
        script_timeout: Option<Duration>,
        protocol: &str,
    ) -> VmResult<(Self, Instant)> {
        let ctx = vm.host_context();
        let state = ctx
            .module_state::<HttpHostState>()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let config = state
            .config
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let client = state
            .client
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP client lease is not configured".to_string()))?;
        let admitted_at = Instant::now();
        if script_timeout.is_some_and(|timeout| admitted_at.checked_add(timeout).is_none()) {
            return Err(VmError::HostError(format!(
                "{protocol} timeout_ms cannot form a deadline"
            )));
        }
        let duration = script_timeout.map_or(config.max_stream_duration, |timeout| {
            timeout.min(config.max_stream_duration)
        });
        let deadline = admitted_at.checked_add(duration).ok_or_else(|| {
            VmError::HostError("HTTP max_stream_duration cannot form a deadline".to_string())
        })?;
        let permit = client.acquire()?;
        Ok((
            Self {
                config,
                client,
                permit,
                prepared_request: None,
            },
            deadline,
        ))
    }
}

impl CaptureAsyncHostContext for HttpRequestContext {
    fn capture(vm: &mut Vm) -> VmResult<Self> {
        Self::capture_for(vm, None, "HTTP").map(|(context, _)| context)
    }

    fn capture_with_args(vm: &mut Vm, args: &[Value]) -> VmResult<Self> {
        let mut context = Self::capture(vm)?;
        let request = match args.first() {
            Some(Value::Map(request)) => request,
            Some(_) => return Err(VmError::TypeMismatch("http request map")),
            None => return Err(VmError::StackUnderflow),
        };
        context.prepared_request =
            Some(request::prepare_buffered_request(&context.config, request)?);
        Ok(context)
    }
}

/// The shared [`HostApiCatalog`] describing every HTTP host function.
///
/// The compiler and the runtime registry consume this same catalog, so the
/// fingerprints embedded in compiled `HostImport`s match the schemas
/// registered by [`HttpExtension`] byte-for-byte.
pub fn http_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(HTTP_HOST_CATALOG.get_or_init(|| {
        super::host_modules::module_catalog("http", HTTP_CATALOG_FUNCTIONS, &[], HTTP_NAMED_STRUCTS)
    }))
}

static HTTP_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

/// Guest contract for `http::client::request`.
///
/// The request resolves directly to a typed `HttpResponse`; transport and pool
/// state stay hidden in the host implementation.
fn http_request_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::client::request",
        vec![HostParamSchema::value(
            "request",
            http_request_struct(&http_request_header_struct(), &http_request_body_struct())
                .as_type(),
        )],
        http_response_struct(&http_response_header_struct(&http_header_value_struct())).as_type(),
    )
}

/// Guest contract for `http::client::sse`.
///
/// The callback observes one typed `SseEvent` and returns a typed
/// `SseCallbackAction`; the stream resolves to a typed `SseSummary`.
fn http_sse_contract() -> HostFunctionSchema {
    let response_header = http_response_header_struct(&http_header_value_struct());
    let callback = HostTypeSchema::Callable {
        params: vec![sse_event_struct(&response_header).as_type()],
        result: Box::new(sse_callback_action_struct().as_type()),
    };
    HostFunctionSchema::with_return(
        "http::client::sse",
        vec![
            HostParamSchema::value(
                "request",
                sse_request_struct(&http_request_header_struct(), &http_request_body_struct())
                    .as_type(),
            ),
            HostParamSchema::with_passing("on_event", callback, HostParamPassing::Value),
        ],
        sse_summary_struct(&response_header).as_type(),
    )
}

/// The HTTP named structs, in the published declaration order.
///
/// The bodies come from the descriptors; this list is the published order and
/// documentation, so a descriptor-derived catalog renders identically.
const HTTP_NAMED_STRUCTS: &[(&str, &str)] = &[
    ("HttpRequestHeader", ""),
    ("HttpHeaderValue", ""),
    ("HttpResponseHeader", ""),
    ("HttpRequestBody", ""),
    ("SseEvent", ""),
    ("HttpRequest", ""),
    ("SseRequest", ""),
    ("HttpResponse", ""),
    ("SseCallbackAction", ""),
    ("SseSummary", ""),
];

/// The HTTP host catalog surface: one descriptor per `http::client::*` member.
const HTTP_CATALOG_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    builtin_http_client_request_descriptor,
    sse::builtin_http_client_sse_descriptor,
];

fn http_catalog_module() -> crate::host_extension::HostModuleDescriptor {
    super::host_modules::catalog_module("http", HTTP_CATALOG_FUNCTIONS, &[])
}

/// The standard `http` host module.
pub(super) fn http_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::StandardHostModule;

    StandardHostModule {
        name: "http",
        catalog: http_catalog_module,
        owned: HTTP_CATALOG_FUNCTIONS,
        named_structs: HTTP_NAMED_STRUCTS,
    }
}

fn opt(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Optional(Box::new(inner))
}

pub(super) fn array(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(inner))
}

pub(super) fn http_request_header_struct() -> HostStructSchema {
    HostStructSchema::new(
        "HttpRequestHeader",
        vec![
            HostStructField::new("name", HostTypeSchema::String),
            HostStructField::new("value", HostTypeSchema::String),
        ],
    )
}

pub(super) fn http_header_value_struct() -> HostStructSchema {
    HostStructSchema::new(
        "HttpHeaderValue",
        vec![
            HostStructField::new("kind", HostTypeSchema::String),
            HostStructField::new("text", opt(HostTypeSchema::String)),
            HostStructField::new("bytes", opt(HostTypeSchema::Bytes)),
        ],
    )
}

pub(super) fn http_response_header_struct(header_value: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "HttpResponseHeader",
        vec![
            HostStructField::new("name", HostTypeSchema::String),
            HostStructField::new("value", header_value.as_type()),
        ],
    )
}

pub(super) fn http_request_body_struct() -> HostStructSchema {
    HostStructSchema::new(
        "HttpRequestBody",
        vec![
            HostStructField::new("kind", HostTypeSchema::String),
            HostStructField::new("text", opt(HostTypeSchema::String)),
            HostStructField::new("bytes", opt(HostTypeSchema::Bytes)),
        ],
    )
}

pub(super) fn sse_event_struct(response_header: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SseEvent",
        vec![
            HostStructField::new("kind", HostTypeSchema::String),
            HostStructField::new("status", opt(HostTypeSchema::Int)),
            HostStructField::new("headers", opt(array(response_header.as_type()))),
            HostStructField::new("url", opt(HostTypeSchema::String)),
            HostStructField::new("event", opt(HostTypeSchema::String)),
            HostStructField::new("data", opt(HostTypeSchema::String)),
            HostStructField::new("id", opt(HostTypeSchema::String)),
            HostStructField::new("retry_ms", opt(HostTypeSchema::Int)),
        ],
    )
}

pub(super) fn http_request_struct(
    request_header: &HostStructSchema,
    request_body: &HostStructSchema,
) -> HostStructSchema {
    HostStructSchema::new(
        "HttpRequest",
        vec![
            HostStructField::new("method", HostTypeSchema::String),
            HostStructField::new("url", HostTypeSchema::String),
            HostStructField::new("headers", opt(array(request_header.as_type()))),
            HostStructField::new("body", opt(request_body.as_type())),
        ],
    )
}

pub(super) fn sse_request_struct(
    request_header: &HostStructSchema,
    request_body: &HostStructSchema,
) -> HostStructSchema {
    let mut fields = http_request_struct(request_header, request_body).fields;
    fields.push(HostStructField::new("timeout_ms", opt(HostTypeSchema::Int)));
    HostStructSchema::new("SseRequest", fields)
}

pub(super) fn http_response_struct(response_header: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "HttpResponse",
        vec![
            HostStructField::new("status", HostTypeSchema::Int),
            HostStructField::new("headers", array(response_header.as_type())),
            HostStructField::new("body", HostTypeSchema::Bytes),
            HostStructField::new("url", HostTypeSchema::String),
        ],
    )
}

pub(super) fn sse_callback_action_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SseCallbackAction",
        vec![HostStructField::new("action", HostTypeSchema::String)],
    )
}

pub(super) fn sse_summary_struct(response_header: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SseSummary",
        vec![
            HostStructField::new("outcome", HostTypeSchema::String),
            HostStructField::new("status", HostTypeSchema::Int),
            HostStructField::new("headers", array(response_header.as_type())),
            HostStructField::new("url", HostTypeSchema::String),
            HostStructField::new("items", HostTypeSchema::Int),
            HostStructField::new("bytes_received", HostTypeSchema::Int),
            HostStructField::new("bytes_sent", HostTypeSchema::Int),
        ],
    )
}

/// Registers every HTTP host function into `registry` using the exact
/// catalog schema path and the authoritative [`standard_host_catalog`]
/// snapshot.
///
/// The standard extensions all register against this single combined
/// snapshot, so a standard combined-catalog compile exact-binds the standard
/// HTTP surface byte-for-byte. Callers that compose their own custom catalog
/// or an HTTP *subcatalog* snapshot must use
/// [`register_http_builtin_module_from_catalog`] instead.
pub fn register_http_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = crate::builtins::runtime::standard_host_catalog();
    register_http_builtin_module_from_catalog(registry, &catalog)
}

/// Registers every HTTP host function into `registry` using the exact
/// schema path derived from a caller-supplied, validated [`HostApiCatalog`]
/// snapshot.
///
/// This is the public register-forwarding API for custom embedders who
/// compile against an HTTP subcatalog (or their own composite) rather than
/// the standard combined snapshot: the schemas are extracted from the
/// supplied `catalog`, so the registered exact fingerprint matches what the
/// matching compile emitted, and the adapters are the module descriptors
/// themselves. Every required request/SSE member is preflighted against its
/// descriptor contract (labels, passing modes, resource keys and return
/// schema), and all mutations are published atomically. Missing or
/// incompatible members return a typed
/// [`crate::vm::HostImportBindingError`] before registry state changes.
pub fn register_http_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    http_host_module()
        .catalog_module()
        .expect("the HTTP module publishes a catalog surface")
        .install_from_catalog(registry, catalog)
        .map(|_| ())
}

/// Standard [`HostExtension`] registering HTTP through the exact catalog
/// path and installing the persistent policy module state.
pub struct HttpExtension;

impl crate::vm::HostExtension for HttpExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        register_http_builtin_module(registry)
    }

    fn install(&self, vm: &mut Vm) {
        vm.host_context().set_module_state(HttpHostState::default());
    }
}

/// Starts an HTTP request under the VM's configured network policy.
#[pd_host_function(name = "http::client::request", contract = http_request_contract)]
pub(super) async fn builtin_http_client_request(
    #[pd_host_context] context: HttpRequestContext,
    request: VmMapHandle,
) -> VmResult<VmMap> {
    let HttpRequestContext {
        config,
        client,
        permit,
        prepared_request,
    } = context;
    let _permit = permit;
    let _ = request;
    let (request, deadline) = prepared_request.ok_or_else(|| {
        VmError::HostError("HTTP request capture did not prepare the request".to_string())
    })?;
    request::perform_buffered_request(client.client(), &config, &request, deadline).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::policy::{
        SchemeFamily, is_restricted_ip, validate_resolved_addresses, validate_url,
        validate_url_policy,
    };
    use super::{HttpConfig, HttpHostExt, HttpRequestContext, HttpWorkerResources};

    #[test]
    fn default_http_policy_denies_all_hosts() {
        let config = HttpConfig::default();
        assert_eq!(config.allowed_schemes, ["https"]);
        assert!(config.allowed_hosts.is_empty());
        assert!(config.allowed_ports.is_empty());
        assert!(!config.allow_private_ips);
        config.validate().expect("default bounds should be valid");
    }

    #[test]
    fn stream_timeout_validation_precedes_permit_admission() {
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.set_http_max_in_flight(0);
        let mut resources = HttpWorkerResources::new();
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        vm.configure_http(config, lease)
            .expect("default config should be valid");

        let error = super::HttpRequestContext::capture_for(&mut vm, Some(Duration::MAX), "SSE")
            .err()
            .expect("an unrepresentable script timeout should be rejected");
        assert!(error.to_string().contains("timeout_ms"), "{error}");
        assert!(
            !error.to_string().contains("in-flight request limit"),
            "deadline validation must happen before permit admission: {error}"
        );
    }

    #[test]
    fn http_scheme_family_rejects_non_http_schemes() {
        let config = HttpConfig {
            allowed_schemes: vec!["http".into(), "https".into(), "ftp".into()],
            allowed_hosts: vec!["example.com".into()],
            allowed_ports: vec![80, 443],
            ..HttpConfig::default()
        };
        let http: url::Url = "https://example.com/".parse().expect("valid URL");
        let ftp: url::Url = "ftp://example.com/".parse().expect("valid URL");
        assert!(validate_url_policy(&config, SchemeFamily::Http, &http).is_ok());
        assert!(validate_url_policy(&config, SchemeFamily::Http, &ftp).is_err());
    }

    #[test]
    fn empty_port_allowlist_rejects_explicit_and_default_ports() {
        let config = HttpConfig {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: vec!["example.com".to_string()],
            ..HttpConfig::default()
        };
        let explicit = "https://example.com:443/".parse().expect("valid URL");
        let default_port = "https://example.com/".parse().expect("valid URL");
        assert!(validate_url(&config, SchemeFamily::Http, &explicit).is_err());
        assert!(validate_url(&config, SchemeFamily::Http, &default_port).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pinned_resolution_preserves_the_original_host_and_validated_address() {
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![8080],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let url = "http://127.0.0.1:8080/".parse().expect("valid pinned URL");

        let target = super::policy::resolve_url(&config, SchemeFamily::Http, &url)
            .await
            .expect("target should resolve under policy");

        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.address, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn special_use_networks_and_mixed_dns_answers_are_restricted() {
        for address in [
            "0.1.2.3",
            "100.64.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.175.48.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "2620:4f:8000::1",
            "3fff::1",
            "fc00::1",
        ] {
            assert!(
                is_restricted_ip(address.parse().expect("valid IP")),
                "{address} must be restricted"
            );
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                !is_restricted_ip(address.parse().expect("valid IP")),
                "{address} must remain globally routable"
            );
        }

        let config = HttpConfig::default();
        let addresses = [
            "8.8.8.8:443".parse().expect("valid socket address"),
            "100.64.0.1:443".parse().expect("valid socket address"),
        ];
        assert!(validate_resolved_addresses(&config, &addresses).is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_is_restricted() {
        assert!(is_restricted_ip(
            "::ffff:127.0.0.1".parse().expect("valid IP")
        ));
    }

    #[test]
    fn http_config_persists_across_scope_reset() {
        let mut resources = HttpWorkerResources::new();
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.configure_http(config, lease)
            .expect("default config should be valid");
        assert!(vm.http_is_configured());

        vm.reset_for_reuse()
            .expect("reset should complete for an idle VM");
        assert!(
            vm.http_is_configured(),
            "the persistent HTTP config must survive reset"
        );

        vm.clear_http_configuration();
        assert!(!vm.http_is_configured());
        // A VM that never runs keeps working after config removal.
    }

    #[test]
    fn same_worker_policy_reuses_client_across_vms_and_reset() {
        let mut resources = HttpWorkerResources::new();
        let config = HttpConfig::default();
        let first = resources
            .client_for(&config)
            .expect("default config should be valid");
        let second = resources
            .client_for(&config)
            .expect("same policy should reuse the worker client");
        assert!(std::sync::Arc::ptr_eq(&first.entry, &second.entry));

        let mut first_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        first_vm
            .configure_http(config.clone(), first)
            .expect("first lease injection should succeed");
        let mut second_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        second_vm
            .configure_http(config.clone(), second.clone())
            .expect("second lease injection should succeed");
        first_vm
            .reset_for_reuse()
            .expect("first VM reset should complete while the worker client remains owned");
        second_vm
            .reset_for_reuse()
            .expect("second VM reset should complete while the worker client remains owned");
        drop(first_vm);
        drop(second_vm);

        let third = resources
            .client_for(&config)
            .expect("worker client should survive VM reset and drop");
        assert!(std::sync::Arc::ptr_eq(&second.entry, &third.entry));
    }

    #[test]
    fn different_full_http_policy_identity_isolated() {
        let mut resources = HttpWorkerResources::new();
        let first_config = HttpConfig::default();
        let second_config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["example.com".to_string()],
            allowed_ports: vec![80],
            max_redirects: 0,
            max_request_body_bytes: 2,
            max_request_header_count: 1,
            max_request_header_bytes: 1,
            max_response_body_bytes: 1,
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            allow_private_ips: true,
            max_stream_item_bytes: 1,
            max_stream_total_bytes: 1,
            max_sse_line_bytes: 1,
            max_stream_duration: Duration::from_secs(1),
            stream_idle_timeout: Duration::from_secs(1),
        };
        let first = resources
            .client_for(&first_config)
            .expect("first config should be valid");
        let second = resources
            .client_for(&second_config)
            .expect("second config should be valid");
        assert!(!std::sync::Arc::ptr_eq(&first.entry, &second.entry));
    }

    #[test]
    fn distinct_vm_leases_have_independent_local_admission_scopes() {
        let mut resources = HttpWorkerResources::with_max_in_flight(usize::MAX);
        let config = HttpConfig::default();
        let mut active = Vec::new();

        for _ in 0..65 {
            let lease = resources
                .client_for(&config)
                .expect("default config should be valid");
            let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
            vm.set_http_max_in_flight(64);
            vm.configure_http(config.clone(), lease)
                .expect("lease injection should succeed");
            active.push(
                HttpRequestContext::capture_for(&mut vm, None, "HTTP")
                    .expect("each VM should admit one request")
                    .0,
            );
        }

        assert_eq!(active.len(), 65);
    }

    #[test]
    fn one_vm_local_admission_rejects_its_65th_request() {
        let mut resources = HttpWorkerResources::with_max_in_flight(usize::MAX);
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.set_http_max_in_flight(64);
        vm.configure_http(config, lease)
            .expect("lease injection should succeed");
        let mut active = Vec::new();
        for _ in 0..64 {
            active.push(
                HttpRequestContext::capture_for(&mut vm, None, "HTTP")
                    .expect("the VM should admit up to its local limit")
                    .0,
            );
        }

        let error = match HttpRequestContext::capture_for(&mut vm, None, "HTTP") {
            Ok(_) => panic!("the VM should reject its 65th request"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("in-flight request limit"));
    }

    #[test]
    fn sibling_vm_local_limits_do_not_share_in_flight_counts() {
        let mut resources = HttpWorkerResources::with_max_in_flight(usize::MAX);
        let config = HttpConfig::default();
        let first_lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let second_lease = resources
            .client_for(&config)
            .expect("same policy should reuse the worker client");
        let mut first_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        first_vm.set_http_max_in_flight(1);
        first_vm
            .configure_http(config.clone(), first_lease)
            .expect("first lease injection should succeed");
        let mut second_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        second_vm.set_http_max_in_flight(2);
        second_vm
            .configure_http(config, second_lease)
            .expect("second lease injection should succeed");

        let _first = HttpRequestContext::capture_for(&mut first_vm, None, "HTTP")
            .expect("first VM should admit its local request");
        let _second = HttpRequestContext::capture_for(&mut second_vm, None, "HTTP")
            .expect("second VM should admit its first local request");
        let _second_again = HttpRequestContext::capture_for(&mut second_vm, None, "HTTP")
            .expect("second VM should admit its second local request");
        let error = match HttpRequestContext::capture_for(&mut second_vm, None, "HTTP") {
            Ok(_) => panic!("second VM should reject only its own third request"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("in-flight request limit"));
    }

    #[test]
    fn composite_permit_holds_worker_and_local_admission() {
        let mut resources = HttpWorkerResources::with_max_in_flight(1);
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        lease.set_local_max_in_flight(1);

        let permit = lease
            .acquire()
            .expect("the composite permit should admit both scopes");
        assert!(
            lease.entry.worker_admission.acquire().is_err(),
            "the worker permit must remain held"
        );
        assert!(
            lease.local_admission.acquire().is_err(),
            "the VM-local permit must remain held"
        );
        drop(permit);

        let worker_permit = lease
            .entry
            .worker_admission
            .acquire()
            .expect("dropping the composite permit should release worker admission");
        let local_permit = lease
            .local_admission
            .acquire()
            .expect("dropping the composite permit should release local admission");
        drop(local_permit);
        drop(worker_permit);
    }

    #[test]
    fn local_admission_failure_releases_the_worker_permit() {
        let mut resources = HttpWorkerResources::with_max_in_flight(1);
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        lease.set_local_max_in_flight(0);

        assert!(lease.acquire().is_err(), "zero local capacity must reject");
        let worker_permit = lease
            .entry
            .worker_admission
            .acquire()
            .expect("a failed local admission must release worker capacity");
        drop(worker_permit);
    }

    #[test]
    fn cloned_lease_shares_its_vm_local_admission_scope() {
        let mut resources = HttpWorkerResources::with_max_in_flight(usize::MAX);
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let clone = lease.clone();
        clone.set_local_max_in_flight(1);

        let permit = lease
            .acquire()
            .expect("the cloned lease should share local capacity");
        assert!(
            clone.acquire().is_err(),
            "a clone must observe the same VM-local in-flight count"
        );
        drop(permit);
    }

    #[test]
    fn cloned_vm_leases_report_and_enforce_shared_local_cap() {
        let mut resources = HttpWorkerResources::with_max_in_flight(3);
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let first_lease = lease.clone();
        let second_lease = lease;
        let mut first_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        first_vm
            .configure_http(config.clone(), first_lease)
            .expect("first lease injection should succeed");
        let mut second_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        second_vm
            .configure_http(config, second_lease)
            .expect("second lease injection should succeed");

        first_vm.set_http_max_in_flight(8);
        assert_eq!(
            first_vm.http_max_in_flight(),
            3,
            "the worker cap must limit the shared local cap"
        );
        assert_eq!(
            second_vm.http_max_in_flight(),
            3,
            "cloned VMs must report the same effective cap"
        );

        second_vm.set_http_max_in_flight(1);
        assert_eq!(first_vm.http_max_in_flight(), 1);
        assert_eq!(second_vm.http_max_in_flight(), 1);

        let first_permit = HttpRequestContext::capture_for(&mut first_vm, None, "HTTP")
            .expect("the shared local cap should admit one request")
            .0;
        assert!(
            HttpRequestContext::capture_for(&mut second_vm, None, "HTTP").is_err(),
            "the cloned VM must enforce the updated shared local cap"
        );
        drop(first_permit);
        let second_permit = HttpRequestContext::capture_for(&mut second_vm, None, "HTTP")
            .expect("releasing the shared permit should restore capacity")
            .0;
        drop(second_permit);
    }

    #[test]
    fn set_max_in_flight_updates_existing_and_future_entries_without_cross_config_corruption() {
        let mut resources = HttpWorkerResources::with_max_in_flight(2);
        let first_config = HttpConfig::default();
        let second_config = HttpConfig {
            max_redirects: 4,
            ..HttpConfig::default()
        };
        let first = resources
            .client_for(&first_config)
            .expect("first config should be valid");
        let second = resources
            .client_for(&second_config)
            .expect("second config should be valid");
        assert!(!std::sync::Arc::ptr_eq(&first.entry, &second.entry));
        assert_eq!(first.worker_max_in_flight(), 2);
        assert_eq!(second.worker_max_in_flight(), 2);

        let first_active = first
            .acquire()
            .expect("the first config should admit an active request");
        resources.set_max_in_flight(1);
        assert_eq!(resources.max_in_flight(), 1);
        assert_eq!(first.worker_max_in_flight(), 1);
        assert_eq!(second.worker_max_in_flight(), 1);
        assert!(
            first.acquire().is_err(),
            "a cap reduction must account for an existing active permit"
        );
        let second_permit = second
            .acquire()
            .expect("a distinct config must retain an independent active count");
        drop(second_permit);

        resources.set_max_in_flight(3);
        assert_eq!(first.worker_max_in_flight(), 3);
        assert_eq!(second.worker_max_in_flight(), 3);
        let first_after_raise = first
            .acquire()
            .expect("raising the cap must allow a new permit beside the active one");
        drop(first_after_raise);

        let future_config = HttpConfig {
            max_redirects: 3,
            ..HttpConfig::default()
        };
        let future = resources
            .client_for(&future_config)
            .expect("future config should use the current worker cap");
        assert_eq!(future.worker_max_in_flight(), 3);
        let future_permits = [
            future.acquire().expect("future entry permit 1"),
            future.acquire().expect("future entry permit 2"),
            future.acquire().expect("future entry permit 3"),
        ];
        assert!(
            future.acquire().is_err(),
            "a future entry must enforce the current worker cap"
        );
        drop(future_permits);
        drop(first_active);
    }

    #[test]
    fn vm_admission_override_does_not_change_a_sibling_vm() {
        let mut resources = HttpWorkerResources::with_max_in_flight(3);
        let config = HttpConfig::default();
        let first_lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let second_lease = resources
            .client_for(&config)
            .expect("same policy should reuse the worker client");
        let mut first_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        first_vm
            .configure_http(config.clone(), first_lease.clone())
            .expect("first lease injection should succeed");
        let mut second_vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        second_vm
            .configure_http(config, second_lease.clone())
            .expect("second lease injection should succeed");

        first_vm.set_http_max_in_flight(1);
        assert_eq!(first_vm.http_max_in_flight(), 1);
        assert_eq!(
            second_vm.http_max_in_flight(),
            3,
            "a VM-local override must not report the sibling's shared admission cap"
        );
        let _first = HttpRequestContext::capture_for(&mut first_vm, None, "HTTP")
            .expect("the first VM should admit its local cap");
        let _second = HttpRequestContext::capture_for(&mut second_vm, None, "HTTP")
            .expect("the sibling VM should retain its own local cap");
        let _second_again = HttpRequestContext::capture_for(&mut second_vm, None, "HTTP")
            .expect("the sibling VM should admit another request");
        let error = match HttpRequestContext::capture_for(&mut second_vm, None, "HTTP") {
            Ok(_) => panic!("the worker aggregate cap should reject the fourth request"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("in-flight request limit"));
    }

    #[test]
    fn dropping_worker_resources_closes_stale_lease_admission() {
        let mut resources = HttpWorkerResources::new();
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let permit = lease
            .acquire()
            .expect("an active permit should be admitted before owner drop");
        drop(resources);
        assert!(
            lease.acquire().is_err(),
            "stale leases must fail closed after their owner is dropped"
        );
        drop(permit);
    }

    #[test]
    fn closing_worker_admission_rejects_leases_and_new_requests() {
        let mut resources = HttpWorkerResources::new();
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        resources.close_admission();
        assert!(resources.client_for(&config).is_err());
        assert!(lease.acquire().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_shutdown_is_owned_and_does_not_borrow_a_vm() {
        let mut resources = HttpWorkerResources::new();
        let config = HttpConfig::default();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let permit = lease
            .acquire()
            .expect("active permit should be admitted before owner drop");
        drop(resources);
        assert!(lease.acquire().is_err());
        drop(permit);

        let mut resources = HttpWorkerResources::new();
        let lease = resources
            .client_for(&config)
            .expect("default config should be valid");
        let permit = lease
            .acquire()
            .expect("active permit should be admitted before explicit shutdown");
        resources.into_shutdown().await;
        assert!(lease.acquire().is_err());
        drop(permit);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::bytecode::{HostImport, ValueType};

    #[test]
    fn adapter_contract_covers_catalog_and_every_registered_schema() {
        let catalog = http_host_catalog();
        let contract_names: std::collections::BTreeSet<String> = HTTP_CATALOG_FUNCTIONS
            .iter()
            .map(|factory| factory().schema.name)
            .collect();
        let catalog_names: std::collections::BTreeSet<String> = catalog
            .functions()
            .iter()
            .map(|function| function.name.clone())
            .collect();
        assert_eq!(contract_names, catalog_names);

        let mut registry = HostFunctionRegistry::empty();
        register_http_builtin_module_from_catalog(&mut registry, &catalog).expect("register HTTP");
        for name in &contract_names {
            let schemas = crate::vm::host_extension::catalog_import_schemas(&catalog, name);
            let imports = schemas
                .iter()
                .map(|schema| HostImport {
                    name: schema.name.clone(),
                    arity: schema.arity() as u8,
                    return_type: ValueType::Map,
                })
                .collect::<Vec<_>>();
            let schema_slots = schemas.into_iter().map(Some).collect::<Vec<_>>();
            assert!(
                registry
                    .prepare_plan_with_schemas(&imports, &schema_slots)
                    .is_ok(),
                "{}",
                name
            );
        }
    }
}
