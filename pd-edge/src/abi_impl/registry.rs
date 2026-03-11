use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use edge_abi::FUNCTIONS as EDGE_ABI_FUNCTIONS;
use vm::{HostFunctionRegistry, StaticHostFunction, Vm, VmError};

use super::{SharedProxyVmContext, SharedVmAsyncOps};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeHostScope {
    Runtime,
    Http,
    HttpExtension,
    Io,
}

pub(crate) struct EdgeHostRegistration {
    pub scope: EdgeHostScope,
    pub name: &'static str,
    pub arity: u8,
    pub function: StaticHostFunction,
}

#[::linkme::distributed_slice]
pub(crate) static PD_EDGE_HOST_FUNCTIONS: [EdgeHostRegistration];

fn scope_mask(scope: EdgeHostScope) -> u8 {
    match scope {
        EdgeHostScope::Runtime => 1 << 0,
        EdgeHostScope::Http => 1 << 1,
        EdgeHostScope::HttpExtension => 1 << 2,
        EdgeHostScope::Io => 1 << 3,
    }
}

fn scopes_mask(scopes: &[EdgeHostScope]) -> u8 {
    let mut mask = 0u8;
    for scope in scopes {
        mask |= scope_mask(*scope);
    }
    mask
}

fn registration_matches_scope_mask(
    registration: &EdgeHostRegistration,
    scope_mask_bits: u8,
) -> bool {
    scope_mask_bits & scope_mask(registration.scope) != 0
}

fn cached_registry_for_scope_mask(scope_mask_bits: u8) -> HostFunctionRegistry {
    static REGISTRY_CACHE: OnceLock<RwLock<HashMap<u8, HostFunctionRegistry>>> = OnceLock::new();
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
            registry.register_static(registration.name, registration.arity, registration.function);
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

pub(crate) fn bind_host_scopes(vm: &mut Vm, scopes: &[EdgeHostScope]) -> Result<(), VmError> {
    let scope_mask_bits = scopes_mask(scopes);
    if scope_mask_bits == 0 {
        return Ok(());
    }
    if vm.bound_function_count() == 0 {
        return cached_registry_for_scope_mask(scope_mask_bits).bind_vm_cached(vm);
    }
    bind_host_scopes_direct(vm, scopes);
    Ok(())
}

pub(crate) fn bind_host_scopes_direct(vm: &mut Vm, scopes: &[EdgeHostScope]) {
    let scope_mask_bits = scopes_mask(scopes);
    for registration in PD_EDGE_HOST_FUNCTIONS {
        if registration_matches_scope_mask(registration, scope_mask_bits) {
            vm.bind_static_function(registration.name, registration.function);
        }
    }
}

pub(crate) fn register_host_scope(
    vm: &mut Vm,
    _context: &SharedProxyVmContext,
    _async_ops: &SharedVmAsyncOps,
    scope: EdgeHostScope,
) {
    bind_host_scopes_direct(vm, &[scope]);
}
