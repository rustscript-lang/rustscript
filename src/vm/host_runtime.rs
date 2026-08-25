//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, builtin overrides, resolved call slots,
//! the IO subsystem state, host operation id allocation, the async bridge,
//! and the print sink. Interpreter state and run budgets live outside this
//! struct (see [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! The unified host-lifecycle plan migrates individual subsystems behind this
//! shell; for now it groups their ownership and their reset/drop behavior.
//! This mechanical decomposition only moves existing fields: capability
//! allow-lists, resource arenas, and operation registries are intentionally
//! left out of this commit.

use std::collections::HashMap;

use crate::builtins::runtime::IoState;
use crate::vm::host::{HostAsyncBridge, HostOpId, VmHostFunction};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions and IO state are
/// mutable and not shareable) and not shared; one facade owns one host
/// runtime. Clone semantics: not `Clone` — host bindings and IO handles must
/// not be duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
    pub(crate) io_state: IoState,
    pub(crate) next_host_op_id: HostOpId,
}

impl HostRuntime {
    /// Creates an empty host runtime with no bound functions, no IO state, and
    /// no async bridge or print sink.
    pub(crate) fn new() -> Self {
        Self {
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            async_bridge: None,
            runtime_print_sink: None,
            io_state: IoState::default(),
            next_host_op_id: 1,
        }
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
