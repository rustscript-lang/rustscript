use std::sync::{Arc, OnceLock};
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
use policy::{ConnectionAdmission, ConnectionPermit};

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
    client: request::HttpClient,
}

impl Default for HttpHostState {
    fn default() -> Self {
        let config = HttpConfig::default();
        Self {
            config: None,
            admission: ConnectionAdmission::new(DEFAULT_MAX_HTTP_IN_FLIGHT),
            client: request::build_client(&config),
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
        let client = request::build_client(&config);
        ctx.set_module_state(HttpHostState {
            config: Some(config),
            admission,
            client,
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

/// Captured HTTP configuration, shared Hyper client, and one in-flight permit.
pub(super) struct HttpRequestContext {
    config: HttpConfig,
    client: request::HttpClient,
    permit: ConnectionPermit,
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
        Ok((
            Self {
                config,
                client: state.client.clone(),
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
    request::perform_buffered_request(&client, &config, &request, deadline).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::policy::{
        SchemeFamily, is_restricted_ip, validate_resolved_addresses, validate_url,
        validate_url_policy,
    };
    use super::{HttpConfig, HttpHostExt};

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
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.configure_http(HttpConfig::default())
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
