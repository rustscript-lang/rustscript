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

struct IoHostState {
    policy: IoPolicy,
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
        self.host.set_host_function_state(IoHostState { policy });
    }

    fn clear_io_configuration(&mut self) {
        self.host.remove_host_function_state::<IoHostState>();
    }
}

pub(super) fn io_policy(vm: &Vm) -> Option<IoPolicy> {
    vm.host
        .host_function_state::<IoHostState>()
        .map(|state| state.policy.clone())
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
