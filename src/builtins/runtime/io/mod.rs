//! IO builtin host implementation.
//!
//! This module is the single canonical owner of the standard `io` host module:
//! the guest contracts, the descriptor ownership list, the `io.file` resource
//! metadata, and [`io_host_module`]. Only the function *implementations* are
//! selected per target and per feature:
//!
//! - `async` (non-wasm32): `async_io` awaits Tokio file/process operations
//!   through ordinary annotated async host functions.
//! - default (non-wasm32): `blocking` performs synchronous IO inline without
//!   worker threads or private operation machinery.
//! - wasm32: `wasm` (the `io_wasm.rs` backend) keeps the catalog surface and
//!   rejects every IO call with a host error; the target has no file system.
//!
//! Native backends retain only script-visible file/process handles as typed
//! execution-scope resources. Every backend declares the same [`IO_FILE_KEY`]
//! and [`IO_FILE_DESCRIPTION`] for its concrete handle type, so the guest
//! contract and the target it compiles for cannot drift.

use super::borrow_arg;
#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
use super::{CallOutcome, CaptureAsyncHostContext, return_one};
#[cfg(not(target_arch = "wasm32"))]
use crate::vm::Vm;

/// The synchronous pending-call channel used only by the wasm32 stub backend.
#[cfg(target_arch = "wasm32")]
pub(super) use super::HostCallResult;

/// The canonical catalog key of the `io.file` resource type.
///
/// Declared once for every backend: each backend implements
/// [`HostResourceType`](crate::host_extension::HostResourceType) for its own
/// concrete handle type but names this key and
/// [`IO_FILE_DESCRIPTION`], so the guest contract cannot drift between
/// targets.
pub(crate) const IO_FILE_KEY: &str = "io.file";

/// The canonical catalog description of the `io.file` resource type.
pub(crate) const IO_FILE_DESCRIPTION: &str = "An open file handle";

// ---- adapter policy (native backends) -------------------------------------
//
// The policy surface is enforced by the native backends: the wasm32 backend
// rejects every IO call regardless of policy, so the type and its accessors
// exist only where they are consulted.

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoPolicy {
    pub allowed_roots: Vec<String>,
    pub allow_write: bool,
    pub allow_process: bool,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
}

#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
pub trait IoHostExt {
    fn configure_io(&mut self, policy: IoPolicy);
    fn clear_io_configuration(&mut self);
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn io_policy(vm: &Vm) -> Option<IoPolicy> {
    vm.host
        .get_module_state::<IoPolicy>()
        .cloned()
        .or_else(|| (!vm.host.default_builtin_capabilities_enabled()).then(IoPolicy::default))
}

// ---- cfg-selected implementations -----------------------------------------

#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
mod async_io;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
mod blocking;
/// The wasm32 backend lives beside the native ones in the runtime module and is
/// pulled in here so it inherits this module's contracts, catalog, ownership
/// list, and resource metadata.
#[cfg(target_arch = "wasm32")]
#[path = "../io_wasm.rs"]
mod wasm;

#[cfg(all(feature = "async", not(target_arch = "wasm32")))]
pub(crate) use async_io::*;
#[cfg(all(not(feature = "async"), not(target_arch = "wasm32")))]
pub(crate) use blocking::*;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::*;

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
