//! IO builtin host implementation.
//!
//! At this layer IO is blocking-only: it drives IO through worker threads
//! registered as concrete [`HostOperation`] drivers in the execution scope.
//! Live handles are [`IoResource`]s owned by the VM's execution scope and
//! in-flight IO work is driven by concrete operation drivers registered in
//! the same scope.
//!
//! The capability system (restricted registries and explicit grants) is
//! introduced by the public host SDK layer; before that layer exists,
//! [`io_policy`] returns only the configured persistent [`IoPolicy`] held in
//! the generic module-state store.

use super::borrow_arg;
use crate::vm::Vm;

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
        // roots remain in force across `reset_for_reuse`), while the adapter's
        // per-invocation runtime state lives in the scope arena.
        self.host.set_module_state(policy);
    }

    fn clear_io_configuration(&mut self) {
        self.host.remove_module_state::<IoPolicy>();
    }
}

pub(super) fn io_policy(vm: &Vm) -> Option<IoPolicy> {
    vm.host.get_module_state::<IoPolicy>().cloned()
}

#[cfg(target_arch = "wasm32")]
pub(super) use super::io_wasm::*;
#[cfg(not(target_arch = "wasm32"))]
mod blocking;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use blocking::*;
