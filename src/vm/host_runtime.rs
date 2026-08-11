//! Host runtime shell.
//!
//! [`HostRuntime`] owns the host-facing capability surface: bound host
//! functions and their symbol table, capability allow-lists, builtin
//! overrides, resolved call slots, the opaque resource arena, the pending
//! operation registry, and the IO/HTTP/SQLite subsystem state plus the async
//! bridge and print sink. Interpreter state and run budgets live outside this
//! struct (see [`Instance`](super::instance::Instance) and
//! [`RunContext`](super::run_context::RunContext)).
//!
//! The unified host-lifecycle plan migrates individual subsystems behind this
//! shell; for now it groups their ownership and their reset/drop behavior.

use std::collections::{HashMap, HashSet};

use crate::builtins::runtime::HttpState;
use crate::builtins::runtime::cancellation::{
    CancellationReason, DEFAULT_MAX_PENDING_OPERATIONS, OperationRegistry,
};
use crate::builtins::runtime::resource::{DEFAULT_MAX_RESOURCES, ResourceArena};

use crate::vm::IoPolicy;
#[cfg(feature = "sqlite")]
use crate::vm::SqlitePolicy;
use crate::vm::host::{HostAsyncBridge, VmHostFunction};

/// Embedder-supplied print sink for `print`/`debug` output.
pub(crate) type RuntimePrintSink = dyn FnMut(String) + Send;

/// Host-owned capabilities, resources, operations, and subsystem state.
///
/// Thread safety: `HostRuntime` is `!Sync` (host functions, resources, and
/// operations are mutable and not shareable) and not shared; one facade owns
/// one host runtime. Clone semantics: not `Clone` — host bindings and resource
/// handles must not be duplicated across VMs.
pub(crate) struct HostRuntime {
    pub(super) host_functions: Vec<VmHostFunction>,
    pub(crate) host_function_symbols: HashMap<String, u16>,
    pub(crate) allow_default_host_fallback: bool,
    pub(crate) allowed_builtin_calls: Vec<u16>,
    pub(crate) allow_default_builtin_capabilities: bool,
    pub(crate) allowed_host_function_slots: Vec<u16>,
    pub(crate) allow_default_host_capabilities: bool,
    pub(crate) builtin_overrides: HashMap<u16, u16>,
    pub(crate) runtime_owned_pending_host_slots: HashSet<u16>,
    pub(crate) resolved_calls: Vec<u16>,
    pub(crate) resolved_calls_dirty: bool,
    pub(crate) runtime_resources: ResourceArena,
    pub(crate) runtime_operations: OperationRegistry,
    pub(crate) io_policy: Option<IoPolicy>,
    #[cfg(feature = "sqlite")]
    pub(crate) sqlite_policy: SqlitePolicy,
    pub(crate) http_state: HttpState,
    pub(crate) async_bridge: Option<Box<dyn HostAsyncBridge>>,
    pub(crate) runtime_print_sink: Option<Box<RuntimePrintSink>>,
}

impl HostRuntime {
    /// Creates an empty host runtime with default capability and resource
    /// limits and no bound functions.
    pub(crate) fn new() -> Self {
        Self {
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            allow_default_host_fallback: true,
            allowed_builtin_calls: Vec::new(),
            allow_default_builtin_capabilities: true,
            allowed_host_function_slots: Vec::new(),
            allow_default_host_capabilities: true,
            builtin_overrides: HashMap::new(),
            runtime_owned_pending_host_slots: HashSet::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            runtime_resources: ResourceArena::with_limit(DEFAULT_MAX_RESOURCES)
                .expect("default runtime resource limit should be valid"),
            runtime_operations: OperationRegistry::with_limit(DEFAULT_MAX_PENDING_OPERATIONS)
                .expect("default runtime operation limit should be valid"),
            io_policy: None,
            #[cfg(feature = "sqlite")]
            sqlite_policy: SqlitePolicy::default(),
            http_state: HttpState::default(),
            async_bridge: None,
            runtime_print_sink: None,
        }
    }

    /// Closes run-scoped host state between runs: pending operations are
    /// cancelled, resources are closed, and the IO subsystem is recreated.
    /// Host bindings, capability allow-lists, and the async bridge are
    /// preserved (documented reusable state).
    pub(crate) fn reset_for_reuse(&mut self) {
        let _ = self
            .runtime_operations
            .cancel_all(CancellationReason::VmReset);
        let _ = self
            .runtime_resources
            .close_all(CancellationReason::VmReset);
        self.http_state.reset_for_reuse();
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}
