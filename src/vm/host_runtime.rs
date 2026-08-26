//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, builtin overrides, resolved call slots,
//! host operation id allocation, the async bridge, and the print sink.
//! Interpreter state and run budgets live outside this struct (see
//! [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! This mechanical decomposition groups host-facing ownership and reset/drop
//! behavior. Concrete adapter runtime state (currently the legacy IO
//! completion mailbox) deliberately stays on the `Vm` facade in this commit;
//! it moves onto the generic execution-scope lifecycle in a later commit.

use std::collections::HashMap;

use crate::vm::host::{HostAsyncBridge, HostOpId, VmHostFunction};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions are mutable and
/// not shareable) and not shared; one facade owns one host runtime. Clone
/// semantics: not `Clone` — host bindings must not be duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
    pub(crate) next_host_op_id: HostOpId,
}

impl HostRuntime {
    /// Creates an empty host runtime with no bound functions, no async bridge
    /// or print sink.
    pub(crate) fn new() -> Self {
        Self {
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            async_bridge: None,
            runtime_print_sink: None,
            next_host_op_id: 1,
        }
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
