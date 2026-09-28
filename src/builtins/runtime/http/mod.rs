use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::{arg, borrow_arg};
use crate::HostCallResult;
use crate::host_api::{
    HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
};
use crate::vm::{HostFunctionRegistry, Vm, VmError, VmResult};

mod config;
pub(super) mod policy;
pub(super) mod request;
pub(super) mod resources;
pub(super) mod sse;

pub use config::HttpConfig;
use policy::{ConnectionAdmission, ConnectionPermit};
pub use request::{HttpRequestResource, HttpResponseResource};

impl crate::host_extension::HostResourceType for HttpRequestResource {
    const KEY: &'static str = "http.internal.request_worker";
    const DESCRIPTION: &'static str =
        "An in-flight HTTP request under the configured network policy";
}

impl crate::host_extension::HostResourceType for HttpResponseResource {
    const KEY: &'static str = "http.internal.response_stream";
    const DESCRIPTION: &'static str = "An open HTTP response body stream";
}

pub(crate) use sse::SseStreamResource;

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
    fn into_permit(self) -> ConnectionPermit {
        self.permit
    }
}

/// The shared [`HostApiCatalog`] describing every HTTP host function.
///
/// The compiler and the runtime registry consume this same catalog, so the
/// fingerprints embedded in compiled `HostImport`s match the schemas
/// registered by [`HttpExtension`] byte-for-byte.
pub fn http_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(HTTP_HOST_CATALOG.get_or_init(|| {
        super::host_modules::module_catalog(
            "http",
            HTTP_CATALOG_FUNCTIONS,
            &[
                http_request_resource,
                http_response_resource,
                sse_stream_resource,
                request_builder_resource,
                buffered_response_resource,
                http_headers_resource,
                sse_summary_resource,
            ],
            HTTP_NAMED_STRUCTS,
        )
    }))
}

static HTTP_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

fn buffered_request_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "http::client::request",
        vec![HostParamSchema::with_passing(
            "request",
            HostTypeSchema::Resource(
                crate::host_api::ResourceTypeKey::new("http.request").expect("static key"),
            ),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Resource(
            crate::host_api::ResourceTypeKey::new("http.response").expect("static key"),
        ),
    )
}

/// Guest contract for `http::client::sse`.
///
/// SSE streams return an immutable summary resource and invoke positional callbacks.
fn http_sse_contract() -> HostFunctionSchema {
    sse::sse_contract(false, false)
}

fn http_sse_open_contract() -> HostFunctionSchema {
    sse::sse_contract(true, false)
}

fn http_sse_only_timeout_contract() -> HostFunctionSchema {
    sse::sse_contract(false, true)
}

fn http_sse_timeout_contract() -> HostFunctionSchema {
    sse::sse_contract(true, true)
}

/// The SSE surface uses resource types and positional callbacks; it declares
/// no host named structs.
const HTTP_NAMED_STRUCTS: &[(&str, &str)] = &[];

/// The HTTP host catalog surface: one descriptor per `http::client::*` member.
const HTTP_CATALOG_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    resources::new_descriptor,
    resources::set_header_descriptor,
    resources::set_body_text_descriptor,
    resources::set_body_bytes_descriptor,
    builtin_http_client_request_descriptor,
    resources::status_descriptor,
    resources::url_descriptor,
    resources::response_header_values_descriptor,
    resources::response_header_names_descriptor,
    resources::body_descriptor,
    resources::headers_values_descriptor,
    resources::headers_names_descriptor,
    resources::sse_outcome_descriptor,
    resources::sse_status_descriptor,
    resources::sse_headers_descriptor,
    resources::sse_url_descriptor,
    resources::sse_items_descriptor,
    resources::sse_bytes_received_descriptor,
    resources::sse_bytes_sent_descriptor,
    sse::builtin_http_client_sse_descriptor,
    sse::builtin_http_client_sse_open_descriptor,
    sse::builtin_http_client_sse_only_timeout_descriptor,
    sse::builtin_http_client_sse_timeout_descriptor,
];

fn http_catalog_module() -> crate::host_extension::HostModuleDescriptor {
    super::host_modules::catalog_module(
        "http",
        HTTP_CATALOG_FUNCTIONS,
        &[
            http_request_resource,
            http_response_resource,
            sse_stream_resource,
            request_builder_resource,
            buffered_response_resource,
            http_headers_resource,
            sse_summary_resource,
        ],
    )
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

/// The canonical declarations for the HTTP resource types.
pub(super) fn http_request_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<HttpRequestResource>()
}

pub(super) fn http_response_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<HttpResponseResource>()
}

pub(super) fn sse_stream_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<SseStreamResource>()
}

fn request_builder_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<resources::HttpRequest>()
}

fn buffered_response_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<resources::HttpResponse>()
}

fn http_headers_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<resources::HttpHeaders>()
}

fn sse_summary_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<resources::SseSummary>()
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

/// Starts a buffered request, consuming the request builder.
#[pd_host_function(
    name = "http::client::request",
    contract = buffered_request_contract,
    runtime_owned_pending
)]
pub(super) fn builtin_http_client_request(
    vm: &mut Vm,
    request: crate::vm::resource::ResourceOwned<resources::HttpRequest>,
) -> VmResult<HostCallResult<i64>> {
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
                    return_type: match &schema.return_type {
                        HostTypeSchema::Null => ValueType::Null,
                        HostTypeSchema::Int | HostTypeSchema::Resource(_) => ValueType::Int,
                        HostTypeSchema::String => ValueType::String,
                        HostTypeSchema::Bytes => ValueType::Bytes,
                        HostTypeSchema::Array(_) => ValueType::Array,
                        _ => ValueType::Map,
                    },
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
