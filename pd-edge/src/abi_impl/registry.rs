use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use edge_abi::FUNCTIONS as EDGE_ABI_FUNCTIONS;
use vm::{
    HostFunctionRegistry, StaticHostArgsFunction, StaticHostStackFunction, Vm, VmError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeHostScope {
    Runtime,
    Http,
    HttpExtension,
    Io,
    Transport,
    #[cfg(feature = "mqtt")]
    Mqtt,
    WebSocket,
    #[cfg(feature = "webrtc")]
    WebRtc,
    Proxy,
    #[cfg(feature = "console")]
    Console,
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) enum EdgeHostRegistrationFunction {
    StackStatic(StaticHostStackFunction),
    ArgsStatic(StaticHostArgsFunction),
}

pub(crate) struct EdgeHostRegistration {
    pub scope: EdgeHostScope,
    pub name: &'static str,
    pub arity: u8,
    #[cfg_attr(not(test), allow(dead_code))]
    pub docs: &'static str,
    pub function: EdgeHostRegistrationFunction,
}

#[::linkme::distributed_slice]
pub(crate) static PD_EDGE_HOST_FUNCTIONS: [EdgeHostRegistration];

pub(crate) const EDGE_HOST_SCOPE_MASK_RUNTIME: u16 = 1 << 0;
pub(crate) const EDGE_HOST_SCOPE_MASK_HTTP: u16 = 1 << 1;
pub(crate) const EDGE_HOST_SCOPE_MASK_HTTP_EXTENSION: u16 = 1 << 2;
pub(crate) const EDGE_HOST_SCOPE_MASK_IO: u16 = 1 << 3;
pub(crate) const EDGE_HOST_SCOPE_MASK_TRANSPORT: u16 = 1 << 4;
#[cfg(feature = "mqtt")]
pub(crate) const EDGE_HOST_SCOPE_MASK_MQTT: u16 = 1 << 5;
pub(crate) const EDGE_HOST_SCOPE_MASK_WEBSOCKET: u16 = 1 << 6;
#[cfg(feature = "webrtc")]
pub(crate) const EDGE_HOST_SCOPE_MASK_WEBRTC: u16 = 1 << 7;
pub(crate) const EDGE_HOST_SCOPE_MASK_PROXY: u16 = 1 << 8;
#[cfg(feature = "console")]
pub(crate) const EDGE_HOST_SCOPE_MASK_CONSOLE: u16 = 1 << 9;

fn scope_mask(scope: EdgeHostScope) -> u16 {
    match scope {
        EdgeHostScope::Runtime => EDGE_HOST_SCOPE_MASK_RUNTIME,
        EdgeHostScope::Http => EDGE_HOST_SCOPE_MASK_HTTP,
        EdgeHostScope::HttpExtension => EDGE_HOST_SCOPE_MASK_HTTP_EXTENSION,
        EdgeHostScope::Io => EDGE_HOST_SCOPE_MASK_IO,
        EdgeHostScope::Transport => EDGE_HOST_SCOPE_MASK_TRANSPORT,
        #[cfg(feature = "mqtt")]
        EdgeHostScope::Mqtt => EDGE_HOST_SCOPE_MASK_MQTT,
        EdgeHostScope::WebSocket => EDGE_HOST_SCOPE_MASK_WEBSOCKET,
        #[cfg(feature = "webrtc")]
        EdgeHostScope::WebRtc => EDGE_HOST_SCOPE_MASK_WEBRTC,
        EdgeHostScope::Proxy => EDGE_HOST_SCOPE_MASK_PROXY,
        #[cfg(feature = "console")]
        EdgeHostScope::Console => EDGE_HOST_SCOPE_MASK_CONSOLE,
    }
}

fn scopes_mask(scopes: &[EdgeHostScope]) -> u16 {
    let mut mask = 0u16;
    for scope in scopes {
        mask |= scope_mask(*scope);
    }
    mask
}

fn registration_matches_scope_mask(
    registration: &EdgeHostRegistration,
    scope_mask_bits: u16,
) -> bool {
    scope_mask_bits & scope_mask(registration.scope) != 0
}

fn cached_registry_for_scope_mask(scope_mask_bits: u16) -> HostFunctionRegistry {
    static REGISTRY_CACHE: OnceLock<RwLock<HashMap<u16, HostFunctionRegistry>>> = OnceLock::new();
    let cache = REGISTRY_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Some(registry) = cache
        .read()
        .expect("edge host registry cache read lock should not be poisoned")
        .get(&scope_mask_bits)
        .cloned()
    {
        return registry;
    }

    let mut registry = HostFunctionRegistry::new();
    for function in EDGE_ABI_FUNCTIONS {
        registry.register_static(
            function.name,
            function.arity,
            super::unbound_edge_abi_function,
        );
    }
    for registration in PD_EDGE_HOST_FUNCTIONS {
        if registration_matches_scope_mask(registration, scope_mask_bits) {
            match registration.function {
                EdgeHostRegistrationFunction::StackStatic(function) => {
                    registry.register_static_stack(registration.name, registration.arity, function);
                }
                EdgeHostRegistrationFunction::ArgsStatic(function) => {
                    registry.register_static_args(registration.name, registration.arity, function);
                }
            }
        }
    }

    let mut write_guard = cache
        .write()
        .expect("edge host registry cache write lock should not be poisoned");
    write_guard
        .entry(scope_mask_bits)
        .or_insert_with(|| registry)
        .clone()
}

fn apply_cached_builtin_overrides(vm: &mut Vm, scopes: &[EdgeHostScope]) {
    if scopes
        .iter()
        .any(|scope| matches!(scope, EdgeHostScope::Io))
    {
        bind_host_scopes_direct(vm, &[EdgeHostScope::Io]);
    }
}

pub(crate) fn bind_host_scopes(vm: &mut Vm, scopes: &[EdgeHostScope]) -> Result<(), VmError> {
    let scope_mask_bits = scopes_mask(scopes);
    if scope_mask_bits == 0 {
        return Ok(());
    }
    cached_registry_for_scope_mask(scope_mask_bits).bind_vm_cached(vm)?;
    apply_cached_builtin_overrides(vm, scopes);
    Ok(())
}

pub(crate) fn bind_host_scopes_direct(vm: &mut Vm, scopes: &[EdgeHostScope]) {
    let scope_mask_bits = scopes_mask(scopes);
    for registration in PD_EDGE_HOST_FUNCTIONS {
        if registration_matches_scope_mask(registration, scope_mask_bits) {
            match registration.function {
                EdgeHostRegistrationFunction::StackStatic(function) => {
                    vm.bind_static_stack_function(registration.name, function);
                }
                EdgeHostRegistrationFunction::ArgsStatic(function) => {
                    vm.bind_static_args_function(registration.name, function);
                }
            }
        }
    }
}
