use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::{VmMap, VmMapHandle, borrow_arg, take_arg};
use crate::HostCallResult;
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeSchema,
};
use crate::vm::operation::{OperationCancelReason, OperationError};
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceError, ResourceHandle,
    ResourceResult, ResourceTypeKey,
};
use crate::vm::{
    CallOutcome, CallReturn, HostContextError, HostFunctionRegistry, Value, Vm, VmError, VmResult,
};

mod config;
pub(super) mod policy;
pub(super) mod request;
pub(super) mod sse;

pub use config::HttpConfig;
use policy::{ConnectionAdmission, ConnectionPermit};
pub use request::{HttpRequestResource, HttpResponseResource, SseStreamResource};

const DEFAULT_MAX_HTTP_IN_FLIGHT: usize = 64;

/// Persistent, per-VM HTTP module state.
///
/// Lives outside the invocation execution scope: it is installed through the
/// generic module-state store and deliberately survives
/// [`Vm::reset_for_reuse`] and scope close. The in-flight admission counter is
/// shared (via [`Arc`]) with every live connection permit; the last one to
/// drop decrements it, so it stays authoritative across resets without the
/// core ever counting connections by class.
struct HttpHostState {
    config: Option<HttpConfig>,
    admission: ConnectionAdmission,
}

impl Default for HttpHostState {
    fn default() -> Self {
        Self {
            config: None,
            admission: ConnectionAdmission::new(DEFAULT_MAX_HTTP_IN_FLIGHT),
        }
    }
}

/// HTTP host configuration owned by the HTTP host implementation.
///
/// Configuration is persistent module state, *outside* invocation resources:
/// [`configure_http`](Self::configure_http) replaces the policy without
/// touching the execution scope, and the policy survives
/// [`Vm::reset_for_reuse`]. Requests and streams are closed/cancelled by the
/// generic execution-scope lifecycle, never by an HTTP-specific owner/type
/// dispatch.
pub trait HttpHostExt {
    fn configure_http(&mut self, config: HttpConfig) -> VmResult<()>;
    fn set_http_max_in_flight(&mut self, max_in_flight: usize);
    fn http_max_in_flight(&mut self) -> usize;
    fn clear_http_configuration(&mut self);
    fn http_is_configured(&mut self) -> bool;
}

impl HttpHostExt for Vm {
    fn configure_http(&mut self, config: HttpConfig) -> VmResult<()> {
        config.validate()?;
        let mut ctx = self.host_context();
        let admission = ctx
            .module_state::<HttpHostState>()
            .map(|state| state.admission.clone())
            .unwrap_or_else(|| ConnectionAdmission::new(DEFAULT_MAX_HTTP_IN_FLIGHT));
        ctx.set_module_state(HttpHostState {
            config: Some(config),
            admission,
        });
        Ok(())
    }

    fn set_http_max_in_flight(&mut self, max_in_flight: usize) {
        let mut ctx = self.host_context();
        if ctx.module_state::<HttpHostState>().is_none() {
            ctx.set_module_state(HttpHostState::default());
        }
        ctx.module_state_mut::<HttpHostState>()
            .expect("HTTP host state was inserted")
            .admission
            .set_max_in_flight(max_in_flight);
    }

    fn http_max_in_flight(&mut self) -> usize {
        self.host_context()
            .module_state::<HttpHostState>()
            .map_or(DEFAULT_MAX_HTTP_IN_FLIGHT, |state| {
                state.admission.max_in_flight()
            })
    }

    fn clear_http_configuration(&mut self) {
        let mut ctx = self.host_context();
        if let Some(state) = ctx.module_state_mut::<HttpHostState>() {
            state.config = None;
        }
    }

    fn http_is_configured(&mut self) -> bool {
        self.host_context()
            .module_state::<HttpHostState>()
            .and_then(|state| state.config.as_ref())
            .is_some()
    }
}

/// Captured HTTP configuration plus a connection permit, used to open a
/// request/stream without re-entering the VM.
pub(super) struct HttpRequestContext {
    pub(super) config: HttpConfig,
    permit: ConnectionPermit,
}

impl HttpRequestContext {
    /// Captures the persistent HTTP policy plus a shared in-flight permit for
    /// one connection-oriented adapter.
    ///
    /// The deadline is validated *before* the permit is acquired, preserving
    /// the historical ordering guarantee (a script timeout that cannot form a
    /// deadline is rejected even when the in-flight capacity is exhausted).
    fn capture(
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
        let permit = state.admission.acquire()?;
        Ok((Self { config, permit }, deadline))
    }

    /// Consumes the captured permit, transferring it to the caller (e.g. the
    /// SSE driver that releases it when the stream finishes).
    pub(super) fn into_permit(self) -> ConnectionPermit {
        self.permit
    }
}

/// Maps a generic resource-close reason onto the parallel operation-cancellation
/// vocabulary (the same stable 1:1 mapping the execution scope uses).
pub(super) fn operation_reason(reason: ResourceCloseReason) -> OperationCancelReason {
    match reason {
        ResourceCloseReason::Requested => OperationCancelReason::Requested,
        ResourceCloseReason::Deadline => OperationCancelReason::Deadline,
        ResourceCloseReason::VmReset => OperationCancelReason::VmReset,
        ResourceCloseReason::Parent => OperationCancelReason::Parent,
        ResourceCloseReason::ResourceClosed => OperationCancelReason::ResourceClosed,
        ResourceCloseReason::OwnershipRelease => OperationCancelReason::Requested,
    }
}

fn host_boundary_error(error: HostContextError) -> VmError {
    VmError::HostError(error.to_string())
}

fn operation_error(error: OperationError) -> VmError {
    VmError::HostError(error.to_string())
}

fn http_handle(raw: i64) -> VmResult<ResourceHandle> {
    ResourceHandle::from_value(&Value::Int(raw))
        .map_err(|error| VmError::HostError(format!("unknown HTTP handle: {error}")))
}

fn resource_error(error: ResourceError) -> VmError {
    VmError::HostError(error.to_string())
}

/// The shared [`HostApiCatalog`] describing every HTTP host function.
///
/// The compiler and the runtime registry consume this same catalog, so the
/// fingerprints embedded in compiled `HostImport`s match the schemas
/// registered by [`HttpExtension`] byte-for-byte.
pub fn http_host_catalog() -> Arc<HostApiCatalog> {
    let request_key = HttpRequestResource::resource_type_key()
        .expect("http.request resource type key must be valid");
    let response_key = HttpResponseResource::resource_type_key()
        .expect("http.response resource type key must be valid");
    let sse_key =
        SseStreamResource::resource_type_key().expect("http.sse resource type key must be valid");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        request_key.clone(),
        "An in-flight HTTP request under the configured network policy",
    ));
    builder.resource(ResourceTypeSchema::new(
        response_key.clone(),
        "An open HTTP response body stream",
    ));
    builder.resource(ResourceTypeSchema::new(
        sse_key.clone(),
        "An incremental SSE stream reader over an open response body stream",
    ));

    // The dynamic request map is accepted as `unknown` because RustScript
    // object literals are exact record types; the HTTP implementation
    // validates the concrete contents at runtime. Schemas, keys, passing
    // modes and fingerprints still come from this one catalog, so compiler
    // and registry agree byte-for-byte.
    builder.function(HostFunctionSchema::with_return(
        "http::client::request",
        vec![HostParamSchema::value("request", HostTypeSchema::Unknown)],
        HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
    ));
    builder.function(HostFunctionSchema::with_return(
        "http::client::sse",
        vec![
            HostParamSchema::value("request", HostTypeSchema::Unknown),
            HostParamSchema::with_passing(
                "on_event",
                HostTypeSchema::Callable {
                    params: vec![HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown))],
                    result: Box::new(HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown))),
                },
                HostParamPassing::Value,
            ),
        ],
        HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
    ));

    Arc::new(builder.build().expect("http catalog must build"))
}

/// Registers every HTTP host function into `registry` using the exact
/// catalog schema path.
pub fn register_http_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = http_host_catalog();
    for schema in
        crate::vm::host_extension::catalog_import_schemas(&catalog, "http::client::request")
    {
        registry.register_exact_static("http::client::request", 1, schema, request_adapter)?;
    }
    for schema in crate::vm::host_extension::catalog_import_schemas(&catalog, "http::client::sse") {
        registry.register_exact_static("http::client::sse", 2, schema, sse_adapter)?;
    }
    // The async host functions return *generic execution-scope* pending
    // operations (registered through `HostContext::start_operation`), so the
    // VM awaits them through the scope registry rather than the async
    // bridge. Marking the exact slots runtime-owned keeps
    // `http::client::request` / `http::client::sse` on the scope-await path
    // (no shadow host-bridge operation is created for them).
    for name in ["http::client::request", "http::client::sse"] {
        registry.mark_exact_runtime_owned_pending(name)?;
    }
    Ok(())
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

fn request_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    match builtin_http_client_request(vm, args)? {
        HostCallResult::Return(value) => Ok(CallOutcome::Return(CallReturn::One(Value::Map(
            Arc::new(value),
        )))),
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn sse_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    match sse::builtin_http_client_sse(vm, args)? {
        HostCallResult::Return(value) => Ok(CallOutcome::Return(CallReturn::One(Value::Map(
            Arc::new(value),
        )))),
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

/// Starts an HTTP request under the VM's configured network policy.
///
/// The request map accepts `method`, `url`, optional `headers`, and optional
/// `body`. The response map contains `status`, `headers`, `body`, and the
/// final `url`.
#[pd_host_function(name = "http::client::request")]
pub(super) fn builtin_http_client_request(
    vm: &mut Vm,
    request: VmMapHandle,
) -> VmResult<HostCallResult<VmMap>> {
    request::perform_buffered_request(vm, request)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::policy::{
        SchemeFamily, is_restricted_ip, validate_resolved_addresses, validate_url,
        validate_url_policy,
    };
    use super::{HttpConfig, HttpHostExt};
    use crate::vm::{Value, VmStatus};

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
        vm.configure_http(HttpConfig::default())
            .expect("default config should be valid");

        let error = super::HttpRequestContext::capture(&mut vm, Some(Duration::MAX), "SSE")
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

    #[test]
    fn pinned_resolution_preserves_the_original_host_and_validated_address() {
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![8080],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let url = "http://127.0.0.1:8080/".parse().expect("valid pinned URL");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        let target = runtime
            .block_on(super::policy::resolve_url(
                &config,
                SchemeFamily::Http,
                &url,
            ))
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
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.configure_http(HttpConfig::default())
            .expect("default config should be valid");
        assert!(vm.http_is_configured());

        vm.reset_for_reuse();
        assert!(
            vm.http_is_configured(),
            "the persistent HTTP config must survive reset"
        );

        vm.clear_http_configuration();
        assert!(!vm.http_is_configured());
        // A VM that never runs keeps working after config removal.
    }
}
