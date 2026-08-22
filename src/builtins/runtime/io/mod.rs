//! Generic scoped I/O host module.
//!
//! The same-crate `io::*` builtins (file, socket/listener, child process,
//! worker thread, stdio pipe) are concrete consumers of the generic scoped
//! host SDK:
//!
//! - file, socket/listener, child process and worker thread are concrete
//!   [`HostResource`] implementations stored in the VM's execution scope;
//! - stdio pipes are child resources of their owning process resource;
//! - read/write/accept/connect/wait and other pending I/O work are dynamic
//!   [`HostOperation`] drivers associated with the relevant resource handle;
//! - [`IoPolicy`] is persistent per-VM module state that survives
//!   [`Vm::reset_for_reuse`](crate::vm::Vm::reset_for_reuse) and scope close
//!   without entering resource cleanup.
//!
//! The VM core (`src/vm`, resource core, operation core and
//! [`ExecutionScope`](crate::vm::execution_scope::ExecutionScope)) never
//! imports or dispatches concrete file / socket / process / thread types; the
//! generic resource/operation/scope protocol is the only bridge.
//!
//! Registration mirrors the sqlite builtin: the exact catalog path
//! ([`HostApiCatalog`] + `HostFunctionRegistry::register_exact_*`) is
//! available through [`register_io_builtin_module`], while the
//! `#[pd_host_function]`-generated namespaced-builtin dispatch keeps the
//! published coarse catalog working with the same RSS names, signatures,
//! errors and capability grants as before.

use std::sync::Arc;

use super::CallOutcome;
use super::borrow_arg;
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};
use crate::vm::Vm;
use crate::vm::{CallReturn, HostFunctionRegistry, Value, VmResult};

/// Persistent I/O host policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoPolicy {
    pub allowed_roots: Vec<String>,
    pub allow_write: bool,
    pub allow_process: bool,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

impl Default for IoPolicy {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            allow_write: false,
            allow_process: false,
            max_read_bytes: 1024 * 1024,
            max_write_bytes: 1024 * 1024,
        }
    }
}

/// Persistent per-VM I/O module state.
///
/// Lives outside the invocation execution scope: it is installed through the
/// generic module-state store and deliberately survives
/// [`Vm::reset_for_reuse`] and scope close. Live IO resources are closed by
/// the generic execution-scope lifecycle, never by an IO-specific
/// owner/type dispatch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IoHostState {
    policy: IoPolicy,
}

impl IoHostState {
    pub fn new(policy: IoPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &IoPolicy {
        &self.policy
    }
}

/// I/O host configuration owned by the I/O host implementation.
///
/// Configuration is persistent module state, *outside* invocation resources:
/// [`configure_io`](Self::configure_io) replaces the policy without touching
/// the execution scope, and the policy survives
/// [`Vm::reset_for_reuse`]. File/socket/process resources and in-flight IO
/// operations are closed/cancelled by the generic execution-scope lifecycle,
/// never by an IO-specific owner/type dispatch.
pub trait IoHostExt {
    fn configure_io(&mut self, policy: IoPolicy);
    fn clear_io_configuration(&mut self);
}

impl IoHostExt for Vm {
    fn configure_io(&mut self, mut policy: IoPolicy) {
        policy.allowed_roots.sort();
        policy.allowed_roots.dedup();
        self.host_context()
            .set_module_state(IoHostState::new(policy));
    }

    fn clear_io_configuration(&mut self) {
        let _ = self.host_context().take_module_state::<IoHostState>();
    }
}

/// The current IO policy: the configured persistent policy, or the
/// deny-by-default fallback when the VM runs a restricted registry with no
/// IO host state installed.
pub(super) fn io_policy(vm: &Vm) -> Option<IoPolicy> {
    vm.host
        .get_module_state::<IoHostState>()
        .map(|state| state.policy().clone())
        .or_else(|| (!vm.host.default_builtin_capabilities_enabled()).then(IoPolicy::default))
}

/// Stable catalog identity for an IO file resource.
pub(crate) fn io_file_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("io.file resource type key must be valid")
}

/// Stable catalog identity for an IO socket resource.
pub(crate) fn io_socket_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.socket").expect("io.socket resource type key must be valid")
}

/// Stable catalog identity for an IO child-process resource.
pub(crate) fn io_process_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.process").expect("io.process resource type key must be valid")
}

/// Stable catalog identity for an IO worker-thread resource.
pub(crate) fn io_worker_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.worker").expect("io.worker resource type key must be valid")
}

/// Stable catalog identity for an IO stdio pipe resource (a child of a
/// process resource).
pub(crate) fn io_pipe_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.pipe").expect("io.pipe resource type key must be valid")
}

/// The shared [`HostApiCatalog`] describing every IO host function.
///
/// The compiler and the runtime registry consume this same catalog, so the
/// fingerprints embedded in compiled `HostImport`s match the schemas
/// registered by [`IoExtension`] byte-for-byte.
pub fn io_host_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        io_file_key(),
        "An open file handle",
    ));
    builder.resource(ResourceTypeSchema::new(
        io_socket_key(),
        "An open socket or listener",
    ));
    builder.resource(ResourceTypeSchema::new(
        io_process_key(),
        "A spawned child process",
    ));
    builder.resource(ResourceTypeSchema::new(
        io_worker_key(),
        "A cooperative worker thread",
    ));
    builder.resource(ResourceTypeSchema::new(
        io_pipe_key(),
        "A stdio pipe of a child process",
    ));

    builder.function(HostFunctionSchema::with_return(
        "io::open",
        vec![
            HostParamSchema::value("path", HostTypeSchema::String),
            HostParamSchema::value("mode", HostTypeSchema::String),
        ],
        HostTypeSchema::Resource(io_file_key()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "io::popen",
        vec![
            HostParamSchema::value("command", HostTypeSchema::String),
            HostParamSchema::value("mode", HostTypeSchema::String),
        ],
        HostTypeSchema::Resource(io_process_key()),
    ));
    for (name, result) in [
        ("io::read_all", HostTypeSchema::String),
        ("io::read_line", HostTypeSchema::String),
        ("io::write", HostTypeSchema::Int),
        ("io::flush", HostTypeSchema::Bool),
    ] {
        builder.function(HostFunctionSchema::with_return(
            name,
            vec![borrow_handle()],
            result,
        ));
    }
    builder.function(HostFunctionSchema::with_return(
        "io::close",
        vec![borrow_handle()],
        HostTypeSchema::Bool,
    ));
    builder.function(HostFunctionSchema::with_return(
        "io::exists",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Bool,
    ));

    Arc::new(builder.build().expect("io catalog must build"))
}

fn borrow_handle() -> HostParamSchema {
    HostParamSchema::with_passing(
        "handle",
        HostTypeSchema::Resource(io_file_key()),
        HostParamPassing::Borrow,
    )
}

/// Registers every IO host function into `registry` using the exact catalog
/// schema path and the authoritative [`standard_host_catalog`] snapshot.
///
/// The exact path is the catalog-driven host-import surface (like sqlite):
/// it binds the same `#[pd_host_function]`-generated adapter functions the
/// namespaced-builtin dispatch uses, so behavior is identical across both
/// compile paths. `mark_exact_runtime_owned_pending` keeps pending IO on the
/// generic execution-scope await path. Available in every build matrix:
/// blocking, async (tokio), and wasm32 (structured unsupported errors) —
/// the adapters always dispatch to the actually-enabled implementation.
///
/// Callers that compose their own custom catalog or an IO *subcatalog*
/// snapshot must use [`register_io_builtin_module_from_catalog`] instead.
pub fn register_io_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = crate::builtins::runtime::standard_host_catalog();
    register_io_builtin_module_from_catalog(registry, &catalog)
}

/// Registers every IO host function into `registry` using the exact schema
/// path derived from a caller-supplied, validated [`HostApiCatalog`]
/// snapshot.
///
/// This is the public register-forwarding API for custom embedders who
/// compile against an IO subcatalog (or their own composite) rather than the
/// standard combined snapshot: the schemas are extracted from the supplied
/// `catalog`, so the registered exact fingerprint matches what the matching
/// compile emitted. Missing/incompatible schemas are rejected
/// deterministically (`MissingExact` at bind time; fingerprint mismatch at
/// compile time).
pub fn register_io_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    for schema in crate::vm::host_extension::catalog_import_schemas(catalog, "io::open") {
        registry.register_exact_static("io::open", 2, schema, open_adapter)?;
    }
    for schema in crate::vm::host_extension::catalog_import_schemas(catalog, "io::popen") {
        registry.register_exact_static("io::popen", 2, schema, popen_adapter)?;
    }
    for (name, adapter) in [
        (
            "io::read_all",
            read_all_adapter as crate::vm::StaticHostFunction,
        ),
        (
            "io::read_line",
            read_line_adapter as crate::vm::StaticHostFunction,
        ),
        ("io::write", write_adapter as crate::vm::StaticHostFunction),
        ("io::flush", flush_adapter as crate::vm::StaticHostFunction),
        ("io::close", close_adapter as crate::vm::StaticHostFunction),
    ] {
        for schema in crate::vm::host_extension::catalog_import_schemas(catalog, name) {
            registry.register_exact_static(name, 1, schema, adapter)?;
        }
    }
    for schema in crate::vm::host_extension::catalog_import_schemas(catalog, "io::exists") {
        registry.register_exact_static(
            "io::exists",
            1,
            schema,
            exists_adapter as crate::vm::StaticHostFunction,
        )?;
    }
    for name in [
        "io::open",
        "io::popen",
        "io::read_all",
        "io::read_line",
        "io::write",
        "io::flush",
        "io::close",
        "io::exists",
    ] {
        registry.mark_exact_runtime_owned_pending(name)?;
    }
    Ok(())
}

// ---- Adapter functions (shared across blocking, async and wasm dispatch) ----
//
// Each adapter decodes the incoming `#[pd_host_function]`-generated wrapper
// result (`VmResult<HostCallResult<T>>`) into a VM `CallOutcome`, exactly as
// the blocking path does. The `io_impl` module is the feature-appropriate
// implementation (blocking / async / wasm), all of which expose the same
// generated wrapper names and signatures, so a single adapter set is used for
// every build matrix. Pending ops are kept on the generic execution-scope
// await path via [`mark_exact_runtime_owned_pending`].

fn open_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_open(vm, args)? {
        HostCallResult::Return(handle) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(handle))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn popen_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_popen(vm, args)? {
        HostCallResult::Return(handle) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(handle))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn read_all_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_read_all(vm, args)? {
        HostCallResult::Return(text) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::string(text))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn read_line_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_read_line(vm, args)? {
        HostCallResult::Return(text) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::string(text))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn write_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_write(vm, args)? {
        HostCallResult::Return(written) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(written))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn flush_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_flush(vm, args)? {
        HostCallResult::Return(ok) => Ok(CallOutcome::Return(CallReturn::one(Value::Bool(ok)))),
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn close_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_close(vm, args)? {
        HostCallResult::Return(ok) => Ok(CallOutcome::Return(CallReturn::one(Value::Bool(ok)))),
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn exists_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    use super::HostCallResult;
    match io_impl::builtin_io_exists(vm, args)? {
        HostCallResult::Return(found) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(found))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

/// Standard [`HostExtension`] registering IO through the exact catalog path
/// and installing the persistent policy module state.
pub struct IoExtension;

impl crate::vm::HostExtension for IoExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        register_io_builtin_module(registry)
    }

    fn install(&self, vm: &mut Vm) {
        vm.host_context().set_module_state(IoHostState::default());
    }
}

// ---- Module declarations ----

/// Feature-appropriate IO implementation module, selected by the build
/// matrix so the exact adapters (and the namespaced dispatch) always bind the
/// actually-available implementation:
///
/// * wasm32 → `io_wasm` (structured "unsupported on wasm32" errors);
/// * `async` → `async_io` (tokio-based worker path);
/// * otherwise → `blocking` (thread-based worker path).
#[cfg(target_arch = "wasm32")]
pub(super) mod io_impl {
    pub(super) use super::super::super::io_wasm::*;
}
#[cfg(all(not(target_arch = "wasm32"), feature = "async"))]
mod io_impl {
    pub(super) use super::async_io::*;
}
#[cfg(all(not(target_arch = "wasm32"), not(feature = "async")))]
mod io_impl {
    pub(super) use super::blocking::*;
}

#[cfg(feature = "async")]
mod async_io;
#[cfg(not(feature = "async"))]
mod blocking;
mod ops;
mod shared;
#[cfg(windows)]
mod windows_process_tree;
mod worker;

#[cfg(target_arch = "wasm32")]
pub(super) use super::io_wasm::*;
#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
pub(super) use async_io::*;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
pub(super) use blocking::*;
