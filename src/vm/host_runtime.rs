//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, builtin overrides, resolved call slots,
//! the IO subsystem state, host operation id allocation, the async bridge,
//! the execution scope (one resource table + one operation registry), and
//! the print sink. Interpreter state and run budgets live outside this
//! struct (see [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! This mechanical decomposition groups host-facing ownership and reset/drop
//! behavior. The execution scope is the isolated resource/operation owner
//! that host code addresses through the generic, host-agnostic
//! [`ExecutionScope`] lifecycle.

use std::collections::HashMap;

use crate::builtins::runtime::IoState;
use crate::vm::execution_scope::ExecutionScope;
use crate::vm::host::{HostAsyncBridge, HostOpId, VmHostFunction};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions, IO state and the
/// execution scope are mutable and not shareable) and not shared; one facade
/// owns one host runtime. Clone semantics: not `Clone` — host bindings and IO
/// handles must not be duplicated across VMs.
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
    /// The isolated execution scope owned by this host runtime.
    pub(super) execution_scope: ExecutionScope,
}

impl HostRuntime {
    /// Creates an empty host runtime with no bound functions, no IO state, and
    /// no async bridge or print sink, plus a fresh active `ExecutionScope`.
    ///
    /// The execution-scope construction is fallible only when a process-unique
    /// identity space (resource arena or operation-registry tag) is exhausted,
    /// which cannot happen in a host runtime owned by a single `Vm` in one
    /// process. The scope-owned `expect` keeps `Vm::new` infallible while
    /// still giving every VM a live, independent scope.
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
            execution_scope: ExecutionScope::new()
                .expect("host runtime execution-scope identity space must be available"),
        }
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
