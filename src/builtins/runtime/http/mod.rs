use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::{VmMap, VmMapHandle, borrow_arg, take_arg};
use crate::HostCallResult;
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeSchema,
};
use crate::vm::resource::HostResource;
use crate::vm::{CallOutcome, CallReturn, HostFunctionRegistry, Value, Vm, VmError, VmResult};

mod config;
pub(super) mod policy;
pub(super) mod request;
pub(super) mod sse;

pub use config::HttpConfig;
use policy::{ConnectionAdmission, ConnectionPermit};
pub use request::{HttpRequestResource, HttpResponseResource};
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
    Arc::clone(HTTP_HOST_CATALOG.get_or_init(build_http_host_catalog))
}

static HTTP_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

fn build_http_host_catalog() -> Arc<HostApiCatalog> {
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

struct HttpAdapterContract {
    name: &'static str,
    arity: u8,
    adapter: fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>,
    runtime_owned_pending: bool,
}

const HTTP_ADAPTER_CONTRACTS: &[HttpAdapterContract] = &[
    HttpAdapterContract {
        name: "http::client::request",
        arity: 1,
        adapter: request_adapter,
        runtime_owned_pending: true,
    },
    HttpAdapterContract {
        name: "http::client::sse",
        arity: 2,
        adapter: sse_adapter,
        runtime_owned_pending: true,
    },
];
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
/// matching compile emitted. Every required request/SSE member is preflighted
/// against its adapter contract (including labels, passing modes, resource keys
/// and return schema), and all mutations are published atomically. Missing or
/// incompatible members return a typed
/// [`crate::vm::HostImportBindingError`] before registry state changes.
pub fn register_http_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    let contract = http_host_catalog();
    let catalog_fingerprint = catalog.fingerprint();
    let contract_fingerprint = contract.fingerprint();
    let schemas = HTTP_ADAPTER_CONTRACTS
        .iter()
        .map(|entry| {
            crate::vm::host_extension::validate_catalog_import_schemas_with_fingerprints(
                catalog,
                &contract,
                entry.name,
                catalog_fingerprint,
                contract_fingerprint,
            )
            .map(|schemas| (entry, schemas))
        })
        .collect::<VmResult<Vec<_>>>()?;

    registry.transactionally(|staged| {
        for (entry, schemas) in &schemas {
            for schema in schemas.iter().cloned() {
                staged.register_exact_static(entry.name, entry.arity, schema, entry.adapter)?;
            }
            staged.authorize_registered_builtin_import(entry.name);
            if entry.runtime_owned_pending {
                staged.mark_exact_runtime_owned_pending(entry.name)?;
            }
        }
        Ok(())
    })
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
        let mut vm = crate::vm::Vm::try_new(crate::vm::Program::new(Vec::new(), Vec::new()))
            .expect("test VM construction must not fail");
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
        let mut vm = crate::vm::Vm::try_new(crate::vm::Program::new(Vec::new(), Vec::new()))
            .expect("test VM construction must not fail");
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

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::bytecode::HostImport;

    #[test]
    fn adapter_contract_covers_catalog_and_every_registered_schema() {
        let catalog = http_host_catalog();
        let contract_names: std::collections::BTreeSet<&str> = HTTP_ADAPTER_CONTRACTS
            .iter()
            .map(|entry| entry.name)
            .collect();
        let catalog_names: std::collections::BTreeSet<&str> = catalog
            .functions()
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert_eq!(contract_names, catalog_names);

        let mut registry = HostFunctionRegistry::empty();
        register_http_builtin_module_from_catalog(&mut registry, &catalog).expect("register HTTP");
        for entry in HTTP_ADAPTER_CONTRACTS {
            for schema in crate::vm::host_extension::catalog_import_schemas(&catalog, entry.name) {
                let import = HostImport {
                    name: entry.name.to_string(),
                    arity: schema.params.len() as u8,
                    return_type: schema.return_type.coarse_value_type(),
                    schema: Some(schema),
                };
                assert!(registry.resolve_import(&import).is_ok(), "{}", entry.name);
            }
        }
    }
}
