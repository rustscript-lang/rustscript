//! IO builtin host implementation, selected by feature:
//!
//! - `async` (non-wasm32): [`async_io`] drives IO through tokio and submits
//!   async host functions via the generic async host bridge.
//! - default (non-wasm32): [`blocking`] drives IO through worker threads
//!   registered as concrete [`HostOperation`] drivers in the execution scope.
//! - wasm32: the wasm stub implementation.
//!
//! Both non-wasm32 implementations share the same execution-scope resource
//! model: live handles are [`IoResource`]s owned by the VM's execution scope
//! and in-flight IO work is driven by concrete operation drivers registered
//! in the same scope. Only the concurrency mechanism differs.

use super::borrow_arg;
#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
use super::{CallOutcome, CaptureAsyncHostContext, return_one};
use crate::vm::Vm;

#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
pub(super) use super::HostCallResult;

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

/// I/O host configuration owned by the I/O host implementation.
pub trait IoHostExt {
    fn configure_io(&mut self, policy: IoPolicy);
    fn clear_io_configuration(&mut self);
}

impl IoHostExt for Vm {
    fn configure_io(&mut self, mut policy: IoPolicy) {
        policy.allowed_roots.sort();
        policy.allowed_roots.dedup();
        // Adapter-declared policy stored in the generic module-state store:
        // module-level policy survives execution-scope reset (an embedder's
        // roots/capabilities remain in force across `reset_for_reuse`), while
        // the adapter's per-invocation runtime state lives in the scope arena.
        self.host.set_module_state(policy);
    }

    fn clear_io_configuration(&mut self) {
        self.host.remove_module_state::<IoPolicy>();
    }
}

pub(super) fn io_policy(vm: &Vm) -> Option<IoPolicy> {
    vm.host
        .get_module_state::<IoPolicy>()
        .cloned()
        .or_else(|| (!vm.host.default_builtin_capabilities_enabled()).then(IoPolicy::default))
}

#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
mod async_io;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
mod blocking;

#[cfg(target_arch = "wasm32")]
pub(super) use super::io_wasm::*;
#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
pub(crate) use async_io::*;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
pub(crate) use blocking::*;

// ---- guest contracts -------------------------------------------------------

use crate::host_api::{
    HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey,
};

/// The `io.file` resource key, derived from its single canonical declaration.
///
/// The guest contracts below name the key through this helper, so a contract
/// cannot drift from the resource type it refers to.
fn io_file_key() -> ResourceTypeKey {
    io_file_resource().schema.key
}

/// Guest contract for `io::open`.
///
/// The runtime signature carries the raw scope-token `i64` for the opened
/// handle; the guest contract is the typed `io.file` resource it denotes.
fn io_open_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::open",
        vec![
            HostParamSchema::value("path", HostTypeSchema::String),
            HostParamSchema::value("mode", HostTypeSchema::String),
        ],
        HostTypeSchema::Resource(io_file_key()),
    )
}

/// Guest contract for `io::read_all`.
fn io_read_all_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::read_all",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
    )
}

/// Guest contract for `io::close`.
fn io_close_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "io::close",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(io_file_key()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Bool,
    )
}

/// The functions this module publishes as the `io` guest catalog surface.
///
/// The compatibility surface is exactly the resource-bearing set; the
/// remaining `io::*` members stay dispatched through the generated
/// namespaced-builtin path and are owned by this module without a catalog
/// entry.
const IO_CATALOG_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    builtin_io_open_descriptor,
    builtin_io_read_all_descriptor,
    builtin_io_close_descriptor,
];

fn io_catalog_module() -> crate::host_extension::HostModuleDescriptor {
    super::host_modules::catalog_module("io", IO_CATALOG_FUNCTIONS, &[io_file_resource])
}

/// The standard `io` host module: the catalog surface plus every owned
/// function of the selected backend.
pub(super) fn io_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::StandardHostModule;

    const OWNED: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
        builtin_io_open_descriptor,
        builtin_io_popen_descriptor,
        builtin_io_read_all_descriptor,
        builtin_io_read_line_descriptor,
        builtin_io_write_descriptor,
        builtin_io_flush_descriptor,
        builtin_io_close_descriptor,
        builtin_io_exists_descriptor,
    ];

    StandardHostModule {
        name: "io",
        catalog: io_catalog_module,
        owned: OWNED,
        named_structs: &[],
    }
}
