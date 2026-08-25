//! Scoped SQLite host functions (optional `sqlite` feature).
//!
//! SQLite connections are typed [`HostResource`]s owned by the VM's
//! [`ExecutionScope`](crate::vm::execution_scope::ExecutionScope), exactly
//! like IO handles. Pending `sqlite::execute` / `sqlite::query` /
//! `sqlite::transaction` work is driven by concrete [`HostOperation`]
//! drivers registered in the same scope and polled/cancelled directly by the
//! operation registry. There is no poller table, no operation-owner enum, and
//! no callback-payload resource: the driver holds the shared connection slot
//! and the scope drives its lifecycle.
//!
//! Connection cleanup is adapter-owned: closing the resource (via
//! `sqlite::close`, VM reset, or scope drop) interrupts the connection
//! through the slot the resource owns and marks it closed. Pending drivers on
//! that connection observe the closed state and are retired through the
//! generic scope close, so no `close_resources_by_type` /
//! `cancel_operations_by_owner` helper is needed.
//!
//! Bounds preserved from the PR16 source: statement byte length, parameter
//! count and byte length, result rows/columns/bytes, connection count,
//! transaction statement count, and transaction deadline, plus SQL-safety
//! rejection and read-only enforcement.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};

use super::typed::{VmArrayRef, VmMapRef};
use super::{HostCallResult, VmMap};
use crate::vm::operation::driver::HostOperation;
use crate::vm::operation::error::{OperationError, OperationErrorCode, OperationResult};
use crate::vm::operation::reason::OperationCancelReason;
use crate::vm::operation::{OperationId, OperationSpec};
use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::ResourceResult;
use crate::vm::resource::{ResourceCloseReason, ResourceHandle};
use crate::vm::{CallReturn, HostOpId, Value, Vm, VmError, VmResult};

/// SQLite `progress_handler` step cadence used to surface cancellation while a
/// statement runs.
const SQLITE_PROGRESS_STEPS: i32 = 1_000;

/// Bounded SQLite connection/query limits, mirroring the PR16 source surface.
#[derive(Clone, Copy, Debug)]
pub struct SqliteLimits {
    pub max_connections: usize,
    pub max_statements: usize,
    pub max_rows: usize,
    pub max_columns: usize,
    pub max_result_bytes: usize,
    pub max_statement_bytes: usize,
    pub max_parameters: usize,
    pub max_parameter_bytes: usize,
    pub max_pending_operations: usize,
    pub max_transaction_ms: u64,
    pub busy_timeout_ms: u64,
}

impl Default for SqliteLimits {
    fn default() -> Self {
        Self {
            max_connections: 16,
            max_statements: 128,
            max_rows: 1_000,
            max_columns: 128,
            max_result_bytes: 4 * 1024 * 1024,
            max_statement_bytes: 1024 * 1024,
            max_parameters: 128,
            max_parameter_bytes: 1024 * 1024,
            max_pending_operations: 32,
            max_transaction_ms: 5_000,
            busy_timeout_ms: 5_000,
        }
    }
}

/// Embedding policy for the SQLite namespace.
#[derive(Clone, Debug, Default)]
pub struct SqlitePolicy {
    pub database_root: Option<String>,
    pub allow_unsafe_sql: bool,
    pub limits: SqliteLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenMode {
    Memory,
    ReadOnly,
    ReadWrite,
    ReadWriteCreate,
}

struct OpenOptions {
    path: String,
    mode: OpenMode,
    root: Option<PathBuf>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
}

/// Shared, adapter-owned per-connection state.
///
/// The connection itself is a [`Mutex<Connection>`] (SQLite connections are
/// not thread-safe), serialized by the `execution` mutex so at most one
/// worker uses the connection at a time. The slot records the currently
/// executing operation and every in-flight operation on this connection so
/// close can retire them without a type-dispatched helper.
struct ConnectionSlot {
    connection: Mutex<Connection>,
    execution: Mutex<()>,
    /// The operation currently executing on this connection, if any.
    active_operation: Mutex<Option<OperationId>>,
    /// Every in-flight operation scheduled against this connection.
    pending: Mutex<Vec<OperationId>>,
    /// Workers that have been scheduled but whose completion guard has not
    /// retired yet. This closes the publish/unregister tail window.
    live_workers: AtomicUsize,
    /// Waker for a resource close waiting for `pending` to become empty.
    close_waker: Mutex<Option<Waker>>,
    interrupt: Arc<rusqlite::InterruptHandle>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
    closed: AtomicBool,
}

impl ConnectionSlot {
    fn register(&self, id: OperationId) {
        self.pending.lock().expect("sqlite pending lock").push(id);
        self.live_workers.fetch_add(1, Ordering::Release);
    }

    fn unregister(&self, id: OperationId) {
        let removed = {
            let mut pending = self.pending.lock().expect("sqlite pending lock");
            let before = pending.len();
            pending.retain(|candidate| *candidate != id);
            pending.len() != before
        };
        if !removed {
            return;
        }
        let workers = self.live_workers.fetch_sub(1, Ordering::AcqRel) - 1;
        if self.pending_count() == 0
            && workers == 0
            && let Some(waker) = self
                .close_waker
                .lock()
                .expect("sqlite close waker lock")
                .take()
        {
            waker.wake();
        }
    }

    fn register_close_waker(&self, waker: &Waker) {
        if self.pending_count() == 0 && self.live_workers.load(Ordering::Acquire) == 0 {
            return;
        }
        {
            let mut close_waker = self.close_waker.lock().expect("sqlite close waker lock");
            *close_waker = Some(waker.clone());
        }
        if self.pending_count() == 0
            && self.live_workers.load(Ordering::Acquire) == 0
            && let Some(waker) = self
                .close_waker
                .lock()
                .expect("sqlite close waker lock")
                .take()
        {
            waker.wake();
        }
    }

    fn drained(&self) -> bool {
        self.pending_count() == 0 && self.live_workers.load(Ordering::Acquire) == 0
    }

    fn pending_count(&self) -> usize {
        self.pending.lock().expect("sqlite pending lock").len()
    }
}

/// The typed connection resource stored in the execution scope.
///
/// The slot is `Arc`-shared with worker threads so a closing resource does not
/// free the connection out from under an in-flight worker; the last Arc drops
/// the `Connection`. `begin_close` is exact-once: it marks the slot closed and
/// interrupts any currently executing statement so cancellation is prompt.
struct SqliteResource {
    slot: Arc<ConnectionSlot>,
    /// Adapter-owned live-connection counter (decremented on close).
    open_connections: Arc<AtomicUsize>,
    counter_released: bool,
}

impl SqliteResource {
    fn new(slot: Arc<ConnectionSlot>, open_connections: Arc<AtomicUsize>) -> Self {
        Self {
            slot,
            open_connections,
            counter_released: false,
        }
    }

    fn release_connection(&mut self) {
        if !self.counter_released {
            self.open_connections.fetch_sub(1, Ordering::SeqCst);
            self.counter_released = true;
        }
    }
}

impl HostResource for SqliteResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        if !self.slot.closed.swap(true, Ordering::AcqRel) {
            self.slot.interrupt.interrupt();
        }
        if self.slot.drained() {
            self.release_connection();
            Ok(CloseProgress::Ready)
        } else {
            Ok(CloseProgress::Pending)
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.slot.drained() {
            self.release_connection();
            Poll::Ready(Ok(()))
        } else {
            self.slot.register_close_waker(cx.waker());
            if self.slot.drained() {
                self.release_connection();
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }
}

impl Drop for SqliteResource {
    fn drop(&mut self) {
        self.release_connection();
    }
}

/// Shared state between one SQLite worker, its [`SqliteOpDriver`] operation,
/// and [`poll_pending_op`] on the VM thread.
///
/// The worker writes the terminal signal and guest-visible value; the driver
/// reflects the signal into the operation registry and the VM wrapper reads
/// the value after the registry drive returns terminal.
struct SqliteOpShared {
    cancelled: AtomicBool,
    worker_done: AtomicBool,
    signal: Mutex<Option<Result<(), String>>>,
    value: Mutex<Option<VmResult<CallReturn>>>,
    waker: Mutex<Option<Waker>>,
    quiescence_waker: Mutex<Option<Waker>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SqliteOpShared {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            worker_done: AtomicBool::new(false),
            signal: Mutex::new(None),
            value: Mutex::new(None),
            waker: Mutex::new(None),
            quiescence_waker: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    fn is_quiescent(&self) -> bool {
        self.worker_done.load(Ordering::Acquire)
    }

    fn mark_worker_done(&self) {
        self.worker_done.store(true, Ordering::Release);
        if let Some(waker) = self
            .quiescence_waker
            .lock()
            .expect("sqlite quiescence waker lock")
            .take()
        {
            waker.wake();
        }
    }

    fn register_quiescence_waker(&self, waker: &Waker) {
        let mut guard = self
            .quiescence_waker
            .lock()
            .expect("sqlite quiescence waker lock");
        if self.is_quiescent() {
            return;
        }
        *guard = Some(waker.clone());
        if self.is_quiescent()
            && let Some(waker) = guard.take()
        {
            waker.wake();
        }
    }

    fn set_worker(&self, worker: JoinHandle<()>) {
        *self.worker.lock().expect("sqlite worker lock") = Some(worker);
    }

    fn join_worker(&self) -> bool {
        self.worker
            .lock()
            .expect("sqlite worker lock")
            .take()
            .is_some_and(|worker| worker.join().is_err())
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn publish(&self, signal: Result<(), String>) {
        *self.signal.lock().expect("sqlite signal lock") = Some(signal);
        if let Some(waker) = self.waker.lock().expect("sqlite waker lock").take() {
            waker.wake();
        }
    }

    fn take_signal(&self) -> Option<Result<(), String>> {
        self.signal.lock().expect("sqlite signal lock").take()
    }

    fn register_waker(&self, waker: &Waker) {
        *self.waker.lock().expect("sqlite waker lock") = Some(waker.clone());
    }

    fn fail(&self, error: VmError) {
        let message = error.to_string();
        *self.value.lock().expect("sqlite value lock") = Some(Err(error));
        self.publish(Err(message));
    }

    fn succeed(&self, value: CallReturn) {
        *self.value.lock().expect("sqlite value lock") = Some(Ok(value));
        self.publish(Ok(()));
    }
}

/// A concrete [`HostOperation`] driver for one pending SQLite operation.
///
/// The operation id is filled in by [`schedule_operation`] after
/// [`ExecutionScope::start_operation`](crate::vm::execution_scope::ExecutionScope::start_operation)
/// assigns it, because the registry allocates packed ids internally. The
/// shared cell is written exactly once, before the driver can be polled or
/// cancelled (the operation is registered with the driver already boxed, but
/// the registry only drives it once the scheduler returns).
struct SqliteOpDriver {
    shared: Arc<SqliteOpShared>,
    slot: Arc<ConnectionSlot>,
    id: Arc<Mutex<Option<OperationId>>>,
    name: String,
}

impl SqliteOpDriver {
    fn new(
        shared: Arc<SqliteOpShared>,
        slot: Arc<ConnectionSlot>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            shared,
            slot,
            id: Arc::new(Mutex::new(None)),
            name: name.into(),
        }
    }

    fn worker_failed(&self, message: String) -> Poll<OperationResult<()>> {
        Poll::Ready(Err(OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "sqlite::operation",
            message,
        )))
    }
}

impl HostOperation for SqliteOpDriver {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        if !self.shared.is_quiescent() {
            self.shared.register_waker(cx.waker());
            self.shared.register_quiescence_waker(cx.waker());
            if !self.shared.is_quiescent() {
                return Poll::Pending;
            }
        }
        if self.shared.is_cancelled() {
            return self.worker_failed(format!("{} was cancelled", self.name));
        }
        match self.shared.take_signal() {
            Some(Ok(())) => Poll::Ready(Ok(())),
            Some(Err(message)) => self.worker_failed(message),
            None => self.worker_failed(format!(
                "{} worker terminated without a completion signal",
                self.name
            )),
        }
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        self.shared.cancelled.store(true, Ordering::Release);
        // If this operation is the one currently executing on the connection,
        // interrupt the statement so the worker aborts promptly. Interrupting a
        // connection with no active statement is a harmless no-op.
        let is_active = self
            .id
            .lock()
            .expect("sqlite driver id lock")
            .is_some_and(|id| {
                *self
                    .slot
                    .active_operation
                    .lock()
                    .expect("sqlite active lock")
                    == Some(id)
            });
        if self.slot.closed.load(Ordering::Acquire) || is_active {
            self.slot.interrupt.interrupt();
        }
        Ok(())
    }

    fn is_quiescent(&self) -> bool {
        self.shared.is_quiescent()
    }

    fn register_quiescence_waker(&mut self, cx: &Context<'_>) {
        self.shared.register_quiescence_waker(cx.waker());
    }

    fn cancel_and_wait(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        self.cancel(reason)?;
        if self.shared.join_worker() {
            return Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "sqlite::operation",
                format!("{} worker panicked while cancelling", self.name),
            ));
        }
        Ok(())
    }
}

impl Drop for SqliteOpDriver {
    fn drop(&mut self) {
        if !self.shared.is_quiescent() {
            let _ = self.cancel(OperationCancelReason::VmDrop);
        }
        let _ = self.shared.join_worker();
    }
}

/// The per-VM SQLite adapter state, mirroring the IO subsystem.
///
/// Owns the embedding policy (database root, unsafe-SQL flag, limits) plus
/// the completion mailboxes for pending operations and an adapter-owned
/// counter of live connections used to enforce `max_connections`. The policy
/// is adapter-owned: `configure` replaces it, `clear` restores the default,
/// and VM reset drops the whole state together with the execution scope.
pub(crate) struct SqliteState {
    pending_results: HashMap<HostOpId, Arc<SqliteOpShared>>,
    pub(crate) policy: SqlitePolicy,
    /// Adapter-owned live connection count, shared with each
    /// [`SqliteResource`] so `begin_close` can decrement it. Avoids a generic
    /// by-type close helper.
    pub(crate) open_connections: Arc<AtomicUsize>,
}

impl Default for SqliteState {
    fn default() -> Self {
        Self {
            pending_results: HashMap::new(),
            policy: SqlitePolicy::default(),
            open_connections: Arc::new(AtomicUsize::new(0)),
        }
    }
}

fn sqlite_error(error: rusqlite::Error) -> VmError {
    let code = error
        .sqlite_error()
        .map(|value| value.extended_code.to_string())
        .unwrap_or_else(|| "non_sqlite".to_string());
    let name = error
        .sqlite_error_code()
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "RusqliteError".to_string());
    VmError::HostError(format!("SQLite error {name} ({code}): {error}"))
}

fn cancellation_message(shared: &SqliteOpShared) -> String {
    if shared.is_cancelled() {
        "SQLite operation cancelled".to_string()
    } else {
        "SQLite connection was closed".to_string()
    }
}

/// Cancels one pending SQLite operation through the execution scope.
pub(super) fn cancel_pending_op(vm: &mut Vm, op_id: HostOpId) {
    let Ok(id) = OperationId::from_raw(op_id) else {
        return;
    };
    vm.host.sqlite_state.pending_results.remove(&op_id);
    let _ = vm
        .execution_scope()
        .cancel_operation(id, OperationCancelReason::Requested);
}

/// Polls one pending SQLite operation through the execution scope's operation
/// registry, delivering the worker's guest-visible value.
pub(super) fn poll_pending_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    let id = match OperationId::from_raw(op_id) {
        Ok(id) => id,
        Err(error) => {
            return Poll::Ready(Err(VmError::HostError(format!(
                "invalid builtin sqlite op {op_id}: {error}"
            ))));
        }
    };

    let poll_result = vm.execution_scope().poll_operation(id, cx);
    match poll_result {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Err(error)) => {
            vm.host.sqlite_state.pending_results.remove(&op_id);
            Poll::Ready(Err(VmError::HostError(format!(
                "builtin sqlite op {op_id} failed: {error}"
            ))))
        }
        Poll::Ready(Ok(outcome)) => {
            let shared = match vm.host.sqlite_state.pending_results.remove(&op_id) {
                Some(shared) => shared,
                None => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "builtin sqlite op {op_id} has no completion mailbox"
                    ))));
                }
            };
            // A cancelled/closed operation reports a guest-visible error even if
            // the worker happened to complete concurrently.
            if matches!(
                outcome,
                crate::vm::operation::driver::OperationOutcome::Cancelled(_)
            ) || shared.is_cancelled()
            {
                return Poll::Ready(Err(VmError::HostError(cancellation_message(&shared))));
            }
            let value = shared.value.lock().expect("sqlite value lock").take();
            match value {
                Some(value) => Poll::Ready(value),
                None => Poll::Ready(Err(VmError::HostError(format!(
                    "builtin sqlite op {op_id} completed without a result"
                )))),
            }
        }
    }
}

fn handle_value(handle: ResourceHandle) -> i64 {
    handle.raw() as i64
}

fn sqlite_handle(handle_id: i64) -> VmResult<ResourceHandle> {
    if handle_id <= 0 {
        return Err(VmError::HostError(format!(
            "invalid sqlite handle id {handle_id}; expected positive handle id"
        )));
    }
    ResourceHandle::from_raw(handle_id as u64).map_err(|error| {
        VmError::HostError(format!("invalid sqlite handle id {handle_id}: {error}"))
    })
}

/// Lifts a guest-visible integer handle into a typed, live scope token.
///
/// This validates arena, slot, generation, open state, and `TypeId` through
/// the generic typed table — a foreign, stale, closed, or wrong-typed handle
/// is rejected here before any SQLite state is touched.
fn lookup_connection(vm: &mut Vm, handle_id: i64) -> VmResult<Arc<ConnectionSlot>> {
    let handle = sqlite_handle(handle_id)?;
    let token = vm
        .execution_scope()
        .resources()
        .typed::<SqliteResource>(handle)
        .map_err(|error| VmError::HostError(format!("unknown SQLite database: {error}")))?;
    let resource = vm
        .execution_scope()
        .resources()
        .get::<SqliteResource>(&token)
        .map_err(|error| VmError::HostError(format!("SQLite database borrow failed: {error}")))?;
    if resource.slot.closed.load(Ordering::SeqCst) {
        return Err(VmError::HostError(
            "SQLite database is already closed".to_string(),
        ));
    }
    Ok(Arc::clone(&resource.slot))
}

fn map_value<'a>(map: &'a VmMap, key: &str) -> Option<&'a Value> {
    map.get(&Value::string(key))
}

fn required_string(map: &VmMap, key: &str) -> VmResult<String> {
    match map_value(map, key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.as_ref().clone()),
        Some(Value::String(_)) => Err(VmError::HostError(format!(
            "SQLite {key} must not be empty"
        ))),
        Some(_) => Err(VmError::TypeMismatch("SQLite option string")),
        None => Err(VmError::HostError(format!("missing SQLite {key}"))),
    }
}

fn optional_string(map: &VmMap, key: &str) -> VmResult<Option<String>> {
    match map_value(map, key) {
        Some(Value::String(value)) => Ok(Some(value.as_ref().clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(VmError::TypeMismatch("SQLite option string")),
    }
}

fn parse_positive_usize(value: &Value, label: &str) -> VmResult<usize> {
    let Value::Int(value) = value else {
        return Err(VmError::TypeMismatch("SQLite limit integer"));
    };
    if *value <= 0 {
        return Err(VmError::HostError(format!(
            "SQLite {label} must be positive"
        )));
    }
    usize::try_from(*value).map_err(|_| VmError::HostError(format!("SQLite {label} is too large")))
}

fn parse_positive_u64(value: &Value, label: &str) -> VmResult<u64> {
    let Value::Int(value) = value else {
        return Err(VmError::TypeMismatch("SQLite limit integer"));
    };
    if *value <= 0 {
        return Err(VmError::HostError(format!(
            "SQLite {label} must be positive"
        )));
    }
    u64::try_from(*value).map_err(|_| VmError::HostError(format!("SQLite {label} is too large")))
}

fn parse_limits(value: Option<&Value>, ceiling: SqliteLimits) -> VmResult<SqliteLimits> {
    let Some(value) = value else {
        return Ok(ceiling);
    };
    let Value::Map(map) = value else {
        return Err(VmError::TypeMismatch("SQLite limits map"));
    };
    let mut limits = ceiling;
    for (key, value) in map.iter() {
        let Value::String(key) = key else {
            return Err(VmError::TypeMismatch("SQLite limit name"));
        };
        match key.as_str() {
            "max_connections" => {
                limits.max_connections =
                    parse_positive_usize(value, key)?.min(ceiling.max_connections)
            }
            "max_statements" => {
                limits.max_statements =
                    parse_positive_usize(value, key)?.min(ceiling.max_statements)
            }
            "max_rows" => limits.max_rows = parse_positive_usize(value, key)?.min(ceiling.max_rows),
            "max_columns" => {
                limits.max_columns = parse_positive_usize(value, key)?.min(ceiling.max_columns)
            }
            "max_result_bytes" => {
                limits.max_result_bytes =
                    parse_positive_usize(value, key)?.min(ceiling.max_result_bytes)
            }
            "max_statement_bytes" => {
                limits.max_statement_bytes =
                    parse_positive_usize(value, key)?.min(ceiling.max_statement_bytes)
            }
            "max_parameters" => {
                limits.max_parameters =
                    parse_positive_usize(value, key)?.min(ceiling.max_parameters)
            }
            "max_parameter_bytes" => {
                limits.max_parameter_bytes =
                    parse_positive_usize(value, key)?.min(ceiling.max_parameter_bytes)
            }
            "max_pending_operations" => {
                limits.max_pending_operations =
                    parse_positive_usize(value, key)?.min(ceiling.max_pending_operations)
            }
            "max_transaction_ms" => {
                limits.max_transaction_ms =
                    parse_positive_u64(value, key)?.min(ceiling.max_transaction_ms)
            }
            "busy_timeout_ms" => {
                limits.busy_timeout_ms =
                    parse_positive_u64(value, key)?.min(ceiling.busy_timeout_ms)
            }
            _ => {
                return Err(VmError::HostError(format!("unknown SQLite limit {key}")));
            }
        }
    }
    Ok(limits)
}

fn parse_query_limits(value: &VmMap, ceiling: SqliteLimits) -> VmResult<SqliteLimits> {
    parse_limits(Some(&Value::Map(Arc::new(value.clone()))), ceiling)
}

fn validate_relative_path(path: &Path) -> VmResult<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(VmError::HostError(
            "SQLite database path must be a non-empty relative path".to_string(),
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(VmError::HostError(
            "SQLite database path must stay below its configured root".to_string(),
        ));
    }
    Ok(())
}

fn canonical_root(root: &Path) -> VmResult<PathBuf> {
    if !root.is_absolute() {
        return Err(VmError::HostError(
            "SQLite database root must be absolute".to_string(),
        ));
    }
    fs::canonicalize(root)
        .map_err(|error| VmError::HostError(format!("invalid SQLite database root: {error}")))
}

fn resolve_database_path(options: &OpenOptions) -> VmResult<Option<PathBuf>> {
    if options.mode == OpenMode::Memory {
        if options.path != ":memory:" {
            return Err(VmError::HostError(
                "SQLite memory mode requires path ':memory:'".to_string(),
            ));
        }
        return Ok(None);
    }
    if options.path == ":memory:" {
        return Err(VmError::HostError(
            "SQLite ':memory:' requires memory open mode".to_string(),
        ));
    }
    let root = options
        .root
        .as_deref()
        .ok_or_else(|| VmError::HostError("SQLite database root is required".to_string()))?;
    let root = canonical_root(root)?;
    let relative = Path::new(&options.path);
    validate_relative_path(relative)?;
    let candidate = root.join(relative);
    let canonical = if candidate.exists() {
        fs::canonicalize(&candidate)
            .map_err(|error| VmError::HostError(format!("invalid SQLite database path: {error}")))?
    } else {
        if options.mode != OpenMode::ReadWriteCreate {
            return Err(VmError::HostError(format!(
                "SQLite database does not exist: {}",
                candidate.display()
            )));
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| VmError::HostError("SQLite database path has no parent".to_string()))?;
        let canonical_parent = fs::canonicalize(parent).map_err(|error| {
            VmError::HostError(format!("invalid SQLite database parent: {error}"))
        })?;
        let file_name = candidate.file_name().ok_or_else(|| {
            VmError::HostError("SQLite database path has no file name".to_string())
        })?;
        canonical_parent.join(file_name)
    };
    if !canonical.starts_with(&root) {
        return Err(VmError::HostError(
            "SQLite database path escapes its configured root".to_string(),
        ));
    }
    Ok(Some(canonical))
}

fn sqlite_limit(value: usize, label: &str) -> VmResult<i32> {
    i32::try_from(value)
        .map_err(|_| VmError::HostError(format!("SQLite {label} exceeds engine limits")))
}

fn install_connection_limits(connection: &Connection, limits: SqliteLimits) -> VmResult<()> {
    let max_value_bytes = limits.max_result_bytes.max(limits.max_parameter_bytes);
    connection.set_limit(
        Limit::SQLITE_LIMIT_LENGTH,
        sqlite_limit(max_value_bytes, "value byte limit")?,
    );
    connection.set_limit(
        Limit::SQLITE_LIMIT_SQL_LENGTH,
        sqlite_limit(limits.max_statement_bytes, "statement byte limit")?,
    );
    connection.set_limit(
        Limit::SQLITE_LIMIT_COLUMN,
        sqlite_limit(limits.max_columns, "column limit")?,
    );
    connection.set_limit(
        Limit::SQLITE_LIMIT_VARIABLE_NUMBER,
        sqlite_limit(limits.max_parameters, "parameter count limit")?,
    );
    Ok(())
}

fn install_authorizer(connection: &Connection, allow_unsafe_sql: bool) {
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        if allow_unsafe_sql {
            return Authorization::Allow;
        }
        match context.action {
            AuthAction::Attach { .. }
            | AuthAction::Detach { .. }
            | AuthAction::Pragma { .. }
            | AuthAction::CreateVtable { .. }
            | AuthAction::DropVtable { .. }
            | AuthAction::Unknown { .. } => Authorization::Deny,
            AuthAction::Function { function_name }
                if function_name.eq_ignore_ascii_case("load_extension") =>
            {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }
    }));
}

fn open_connection(options: &OpenOptions) -> VmResult<Connection> {
    let path = resolve_database_path(options)?;
    let flags = match options.mode {
        OpenMode::Memory => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        OpenMode::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
        OpenMode::ReadWriteCreate => {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        }
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = match path {
        Some(path) => Connection::open_with_flags(path, flags),
        None => Connection::open_in_memory_with_flags(flags),
    }
    .map_err(sqlite_error)?;
    connection
        .busy_timeout(Duration::from_millis(options.limits.busy_timeout_ms))
        .map_err(sqlite_error)?;
    install_connection_limits(&connection, options.limits)?;
    install_authorizer(&connection, options.allow_unsafe_sql);
    Ok(connection)
}

fn normalized_sql(sql: &str) -> VmResult<String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut index = 0;
    let mut quote = None;
    let mut statement_ended = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active_quote) = quote {
            if byte == active_quote {
                if index + 1 < bytes.len() && bytes[index + 1] == active_quote {
                    index += 2;
                    continue;
                }
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
            out.push(' ');
            index += 1;
            continue;
        }
        if byte == b'-' && index + 1 < bytes.len() && bytes[index + 1] == b'-' {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            out.push(' ');
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            if index + 1 >= bytes.len() {
                return Err(VmError::HostError(
                    "SQLite SQL contains an unterminated comment".to_string(),
                ));
            }
            index += 2;
            out.push(' ');
            continue;
        }
        if byte == b';' {
            statement_ended = true;
            index += 1;
            continue;
        }
        if statement_ended && !byte.is_ascii_whitespace() {
            return Err(VmError::HostError(
                "multiple SQLite statements are not allowed".to_string(),
            ));
        }
        out.push((byte as char).to_ascii_lowercase());
        index += 1;
    }
    if quote.is_some() {
        return Err(VmError::HostError(
            "SQLite SQL contains an unterminated quote".to_string(),
        ));
    }
    Ok(out)
}

fn validate_sql(sql: &str, limits: SqliteLimits, allow_unsafe_sql: bool) -> VmResult<()> {
    if sql.is_empty() || sql.len() > limits.max_statement_bytes || sql.as_bytes().contains(&0) {
        return Err(VmError::HostError(format!(
            "SQLite statement exceeds the configured {} byte limit or is invalid",
            limits.max_statement_bytes
        )));
    }
    let normalized = normalized_sql(sql)?;
    if allow_unsafe_sql {
        return Ok(());
    }
    let first = normalized.split_whitespace().next().unwrap_or_default();
    if matches!(
        first,
        "attach"
            | "detach"
            | "pragma"
            | "vacuum"
            | "begin"
            | "commit"
            | "rollback"
            | "savepoint"
            | "release"
    ) {
        return Err(VmError::HostError(format!(
            "SQLite statement {first} is not allowed"
        )));
    }
    if normalized
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| token == "load_extension")
    {
        return Err(VmError::HostError(
            "SQLite extension loading is disabled".to_string(),
        ));
    }
    Ok(())
}

fn sqlite_params(values: VmArrayRef<'_>, limits: SqliteLimits) -> VmResult<Vec<SqlValue>> {
    if values.len() > limits.max_parameters {
        return Err(VmError::HostError(
            "SQLite parameter count exceeds the configured limit".to_string(),
        ));
    }
    let mut bytes = 0usize;
    let mut params = Vec::with_capacity(values.len());
    for value in values {
        let sql_value = match value {
            Value::Null => SqlValue::Null,
            Value::Int(value) => SqlValue::Integer(*value),
            Value::Float(value) => SqlValue::Real(*value),
            Value::String(value) => {
                bytes = bytes.saturating_add(value.len());
                SqlValue::Text(value.as_ref().clone())
            }
            Value::Bytes(value) => {
                bytes = bytes.saturating_add(value.len());
                SqlValue::Blob(value.as_ref().clone())
            }
            _ => {
                return Err(VmError::HostError(
                    "SQLite parameters support only null, int, float, string, and bytes"
                        .to_string(),
                ));
            }
        };
        if bytes > limits.max_parameter_bytes {
            return Err(VmError::HostError(format!(
                "SQLite parameters exceed the configured {} byte limit",
                limits.max_parameter_bytes
            )));
        }
        params.push(sql_value);
    }
    Ok(params)
}

/// Runs one synchronous closure against the connection, with cancellation
/// surfaced through the shared cancelled flag.
fn with_connection<T>(
    slot: &ConnectionSlot,
    shared: &Arc<SqliteOpShared>,
    operation: impl FnOnce(&mut Connection) -> Result<T, rusqlite::Error>,
) -> VmResult<T> {
    if slot.closed.load(Ordering::Acquire) || shared.is_cancelled() {
        return Err(VmError::HostError(cancellation_message(shared)));
    }
    let mut connection = slot
        .connection
        .lock()
        .map_err(|_| VmError::HostError("SQLite connection lock is poisoned".to_string()))?;
    if slot.closed.load(Ordering::Acquire) || shared.is_cancelled() {
        return Err(VmError::HostError(cancellation_message(shared)));
    }
    let handler_shared = Arc::clone(shared);
    connection.progress_handler(
        SQLITE_PROGRESS_STEPS,
        Some(move || handler_shared.is_cancelled()),
    );
    let result = operation(&mut connection);
    connection.progress_handler(0, None::<fn() -> bool>);
    if slot.closed.load(Ordering::Acquire) || shared.is_cancelled() {
        return Err(VmError::HostError(cancellation_message(shared)));
    }
    result.map_err(sqlite_error)
}

fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 1,
        Value::Int(_) | Value::Float(_) => 8,
        Value::Bool(_) => 1,
        Value::String(value) => value.len(),
        Value::Bytes(value) => value.len(),
        Value::Array(values) => values.iter().map(estimate_value_bytes).sum(),
        Value::Map(values) => values
            .iter()
            .map(|(key, value)| {
                estimate_value_bytes(key).saturating_add(estimate_value_bytes(value))
            })
            .sum(),
        Value::Callable(_) => 8,
    }
}

fn value_from_row(row: &rusqlite::Row<'_>, index: usize) -> Result<Value, rusqlite::Error> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(Value::Null),
        ValueRef::Integer(value) => Ok(Value::Int(value)),
        ValueRef::Real(value) => Ok(Value::Float(value)),
        ValueRef::Text(value) => match std::str::from_utf8(value) {
            Ok(value) => Ok(Value::string(value)),
            Err(_) => Ok(Value::bytes(value.to_vec())),
        },
        ValueRef::Blob(value) => Ok(Value::bytes(value.to_vec())),
    }
}

fn query_with_connection(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
    limits: SqliteLimits,
) -> Result<VmMap, rusqlite::Error> {
    let mut statement = connection.prepare(sql)?;
    let columns = statement
        .column_names()
        .into_iter()
        .map(Value::string)
        .collect::<Vec<_>>();
    if columns.len() > limits.max_columns {
        return Err(rusqlite::Error::InvalidColumnIndex(columns.len()));
    }
    let column_count = columns.len();
    let mut rows = statement.query(params_from_iter(params.iter()))?;
    let mut values = Vec::new();
    let mut result_bytes = columns.iter().map(estimate_value_bytes).sum::<usize>();
    let mut truncated = false;
    let mut next_cursor = None;
    while let Some(row) = rows.next()? {
        if values.len() >= limits.max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(column_count);
        let mut row_bytes = 0usize;
        for index in 0..column_count {
            let value = value_from_row(row, index)?;
            row_bytes = row_bytes.saturating_add(estimate_value_bytes(&value));
            cells.push(value);
        }
        if result_bytes.saturating_add(row_bytes) > limits.max_result_bytes {
            truncated = true;
            break;
        }
        if let Some(Value::Int(cursor)) = cells.first() {
            next_cursor = Some(*cursor);
        }
        result_bytes = result_bytes.saturating_add(row_bytes);
        values.push(Value::array(cells));
    }
    let mut entries = vec![
        (Value::string("columns"), Value::array(columns)),
        (Value::string("rows"), Value::array(values)),
        (Value::string("truncated"), Value::Bool(truncated)),
    ];
    if let Some(next_cursor) = next_cursor {
        entries.push((Value::string("next_cursor"), Value::Int(next_cursor)));
    }
    Ok(VmMap::from_entries(entries))
}

fn execute_with_connection(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> Result<VmMap, rusqlite::Error> {
    let mut statement = connection.prepare(sql)?;
    let rows_affected = statement.execute(params_from_iter(params.iter()))?;
    drop(statement);
    Ok(VmMap::from_entries(vec![
        (
            Value::string("rows_affected"),
            Value::Int(i64::try_from(rows_affected).unwrap_or(i64::MAX)),
        ),
        (
            Value::string("last_insert_rowid"),
            Value::Int(connection.last_insert_rowid()),
        ),
    ]))
}

struct SqliteWorkerCompletion {
    slot: Arc<ConnectionSlot>,
    shared: Arc<SqliteOpShared>,
    id: OperationId,
}

impl Drop for SqliteWorkerCompletion {
    fn drop(&mut self) {
        if let Ok(mut active) = self.slot.active_operation.lock()
            && *active == Some(self.id)
        {
            *active = None;
        }
        self.shared.mark_worker_done();
        self.slot.unregister(self.id);
    }
}

/// Schedules a worker thread to run one SQLite operation on a connection and
/// registers its [`SqliteOpDriver`] in the VM's execution scope.
///
/// The driver is constructed with a shared id cell that
/// [`ExecutionScope::start_operation`](crate::vm::execution_scope::ExecutionScope::start_operation)
/// fills in after allocating the packed operation id, so the driver's `cancel`
/// can compare against the connection's active operation without a registry
/// fixup. The worker holds the connection's execution mutex for the whole
/// operation (serializing access, since SQLite connections are not
/// thread-safe), records itself as the active operation, and publishes the
/// terminal signal plus the guest-visible value through the shared mailbox.
fn schedule_operation(
    vm: &mut Vm,
    slot: Arc<ConnectionSlot>,
    operation: impl FnOnce(Arc<ConnectionSlot>, Arc<SqliteOpShared>) -> VmResult<CallReturn>
    + Send
    + 'static,
) -> VmResult<HostOpId> {
    if slot.closed.load(Ordering::SeqCst) {
        return Err(VmError::HostError(
            "SQLite database is already closed".to_string(),
        ));
    }
    if slot.pending_count() >= slot.limits.max_pending_operations {
        return Err(VmError::HostError(format!(
            "SQLite pending operation limit {} reached",
            slot.limits.max_pending_operations
        )));
    }

    let shared = Arc::new(SqliteOpShared::new());
    let worker_shared = Arc::clone(&shared);
    let worker_slot = Arc::clone(&slot);
    let worker_name = "sqlite::operation".to_string();
    let driver = SqliteOpDriver::new(Arc::clone(&shared), Arc::clone(&slot), worker_name.clone());
    let driver_id = Arc::clone(&driver.id);

    let deadline =
        Instant::now().checked_add(Duration::from_millis(slot.limits.max_transaction_ms));
    let spec = OperationSpec::new(driver)
        .with_deadline(deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)));

    let op_id = vm
        .execution_scope()
        .start_operation(spec)
        .map_err(|error| {
            VmError::HostError(format!("failed to start sqlite operation: {error}"))
        })?;
    *driver_id
        .lock()
        .expect("sqlite driver id lock should not be poisoned") = Some(op_id);
    slot.register(op_id);
    let raw = op_id.raw();

    let worker = thread::Builder::new()
        .name(format!("rustscript-sqlite-{raw}"))
        .spawn(move || {
            let _completion = SqliteWorkerCompletion {
                slot: Arc::clone(&worker_slot),
                shared: Arc::clone(&worker_shared),
                id: op_id,
            };
            let _execution = worker_slot
                .execution
                .lock()
                .expect("SQLite execution lock should not be poisoned");
            if worker_slot.closed.load(Ordering::Acquire) || worker_shared.is_cancelled() {
                worker_shared.fail(VmError::HostError(cancellation_message(&worker_shared)));
                return;
            }
            *worker_slot
                .active_operation
                .lock()
                .expect("SQLite active operation lock should not be poisoned") = Some(op_id);
            if worker_slot.closed.load(Ordering::Acquire) || worker_shared.is_cancelled() {
                worker_shared.fail(VmError::HostError(cancellation_message(&worker_shared)));
                return;
            }
            let result = operation(Arc::clone(&worker_slot), Arc::clone(&worker_shared));
            match result {
                Ok(value) => worker_shared.succeed(value),
                Err(error) => worker_shared.fail(error),
            }
        })
        .map_err(|error| {
            shared.mark_worker_done();
            let _ = vm
                .execution_scope()
                .abort_operation(op_id, OperationCancelReason::Requested);
            slot.unregister(op_id);
            VmError::HostError(format!("failed to spawn sqlite worker: {error}"))
        })?;
    shared.set_worker(worker);

    vm.host.sqlite_state.pending_results.insert(raw, shared);
    Ok(raw)
}

/// Parses the `sqlite::open` options map against the adapter-owned embedding
/// policy.
fn parse_open_options(vm: &Vm, options: &VmMap) -> VmResult<OpenOptions> {
    let policy = &vm.host.sqlite_state.policy;
    let path = required_string(options, "path")?;
    let mode = match optional_string(options, "mode")?.as_deref() {
        Some("memory") => OpenMode::Memory,
        Some("read_only") => OpenMode::ReadOnly,
        Some("read_write") => OpenMode::ReadWrite,
        Some("read_write_create") | None => OpenMode::ReadWriteCreate,
        Some(mode) => {
            return Err(VmError::HostError(format!(
                "unknown SQLite open mode {mode}"
            )));
        }
    };
    let configured_root = policy.database_root.as_deref().map(PathBuf::from);
    if let Some(requested_root) = optional_string(options, "root")? {
        let requested_root = PathBuf::from(requested_root);
        if configured_root.as_ref() != Some(&requested_root) {
            return Err(VmError::HostError(
                "SQLite root must match the embedding policy".to_string(),
            ));
        }
    }
    if mode != OpenMode::Memory && configured_root.is_none() {
        return Err(VmError::HostError(
            "SQLite database root is not configured".to_string(),
        ));
    }
    let limits = parse_limits(map_value(options, "limits"), policy.limits)?;
    Ok(OpenOptions {
        path,
        mode,
        root: configured_root,
        limits,
        allow_unsafe_sql: policy.allow_unsafe_sql,
    })
}

/// Opens a SQLite database under the embedding-owned path and limit policy.
///
/// The connection is stored as a typed [`SqliteResource`] in the execution
/// scope; the guest-visible handle is the raw scope handle, validated for
/// arena, slot, generation, open state, and type on every later use. The
/// live-connection count is adapter-owned (shared with each resource) so
/// `max_connections` is enforced without a generic by-type helper.
#[pd_host_function(name = "sqlite::open")]
pub(super) fn builtin_sqlite_open_impl(vm: &mut Vm, options: VmMapRef<'_>) -> VmResult<i64> {
    let options = parse_open_options(vm, options)?;
    let open_connections: Arc<AtomicUsize> = Arc::clone(&vm.host.sqlite_state.open_connections);
    if open_connections.load(Ordering::SeqCst) >= options.limits.max_connections {
        return Err(VmError::HostError(format!(
            "SQLite connection limit {} reached",
            options.limits.max_connections
        )));
    }
    let connection = open_connection(&options)?;
    let interrupt = connection.get_interrupt_handle();
    let slot = Arc::new(ConnectionSlot {
        connection: Mutex::new(connection),
        execution: Mutex::new(()),
        active_operation: Mutex::new(None),
        pending: Mutex::new(Vec::new()),
        live_workers: AtomicUsize::new(0),
        close_waker: Mutex::new(None),
        interrupt: Arc::new(interrupt),
        limits: options.limits,
        allow_unsafe_sql: options.allow_unsafe_sql,
        closed: AtomicBool::new(false),
    });
    let resource = vm
        .execution_scope()
        .push_resource(SqliteResource::new(slot, Arc::clone(&open_connections)))
        .map_err(|error| VmError::HostError(format!("failed to open SQLite database: {error}")))?;
    open_connections.fetch_add(1, Ordering::SeqCst);
    Ok(handle_value(resource.handle()))
}

/// Executes one parameterized SQLite statement asynchronously.
#[pd_host_function(name = "sqlite::execute")]
pub(super) fn builtin_sqlite_execute_impl(
    vm: &mut Vm,
    db_id: i64,
    sql: &str,
    params: VmArrayRef<'_>,
) -> VmResult<HostCallResult<VmMap>> {
    let slot = lookup_connection(vm, db_id)?;
    validate_sql(sql, slot.limits, slot.allow_unsafe_sql)?;
    let sql = sql.to_string();
    let params = sqlite_params(params, slot.limits)?;
    let op_id = schedule_operation(vm, slot, move |slot, shared| {
        with_connection(&slot, &shared, |connection| {
            execute_with_connection(connection, &sql, &params)
        })
        .map(|value| CallReturn::one(Value::Map(Arc::new(value))))
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Runs one parameterized SQLite query with row and result-byte bounds.
#[pd_host_function(name = "sqlite::query")]
pub(super) fn builtin_sqlite_query_impl(
    vm: &mut Vm,
    db_id: i64,
    sql: &str,
    params: VmArrayRef<'_>,
    limits: VmMapRef<'_>,
) -> VmResult<HostCallResult<VmMap>> {
    let slot = lookup_connection(vm, db_id)?;
    let query_limits = parse_query_limits(limits, slot.limits)?;
    validate_sql(sql, query_limits, slot.allow_unsafe_sql)?;
    let sql = sql.to_string();
    let params = sqlite_params(params, slot.limits)?;
    let op_id = schedule_operation(vm, slot, move |slot, shared| {
        with_connection(&slot, &shared, |connection| {
            query_with_connection(connection, &sql, &params, query_limits)
        })
        .map(|value| CallReturn::one(Value::Map(Arc::new(value))))
    })?;
    Ok(HostCallResult::Pending(op_id))
}

struct TransactionStatement {
    sql: String,
    params: Vec<SqlValue>,
    query: bool,
    limits: SqliteLimits,
}

fn parse_transaction_statements(
    statements: VmArrayRef<'_>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
) -> VmResult<Vec<TransactionStatement>> {
    if statements.is_empty() {
        return Err(VmError::HostError(
            "SQLite transaction requires at least one statement".to_string(),
        ));
    }
    if statements.len() > limits.max_statements {
        return Err(VmError::HostError(format!(
            "SQLite transaction exceeds the configured {} statement limit",
            limits.max_statements
        )));
    }
    statements
        .iter()
        .map(|statement| {
            let Value::Map(statement) = statement else {
                return Err(VmError::TypeMismatch("SQLite transaction statement map"));
            };
            let sql = required_string(statement, "sql")?;
            validate_sql(&sql, limits, allow_unsafe_sql)?;
            let params = match map_value(statement, "params") {
                Some(Value::Array(params)) => sqlite_params(params, limits)?,
                Some(_) => return Err(VmError::TypeMismatch("SQLite parameter array")),
                None => Vec::new(),
            };
            let query = match map_value(statement, "query") {
                Some(Value::Bool(query)) => *query,
                Some(_) => return Err(VmError::TypeMismatch("SQLite query flag")),
                None => false,
            };
            let statement_limits = match map_value(statement, "limits") {
                Some(Value::Map(statement_limits)) => parse_query_limits(statement_limits, limits)?,
                Some(_) => return Err(VmError::TypeMismatch("SQLite limits map")),
                None => limits,
            };
            Ok(TransactionStatement {
                sql,
                params,
                query,
                limits: statement_limits,
            })
        })
        .collect()
}

/// Runs ordered statements atomically and returns ordered result envelopes.
#[pd_host_function(name = "sqlite::transaction")]
pub(super) fn builtin_sqlite_transaction_impl(
    vm: &mut Vm,
    db_id: i64,
    statements: VmArrayRef<'_>,
) -> VmResult<HostCallResult<Vec<Value>>> {
    let slot = lookup_connection(vm, db_id)?;
    let statements = parse_transaction_statements(statements, slot.limits, slot.allow_unsafe_sql)?;
    let op_id = schedule_operation(vm, slot, move |slot, shared| {
        with_connection(&slot, &shared, |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut results = Vec::with_capacity(statements.len());
            for statement in statements {
                let value = if statement.query {
                    query_with_connection(
                        &transaction,
                        &statement.sql,
                        &statement.params,
                        statement.limits,
                    )?
                } else {
                    execute_with_connection(&transaction, &statement.sql, &statement.params)?
                };
                results.push(Value::Map(Arc::new(value)));
            }
            transaction.commit()?;
            Ok(results)
        })
        .map(|values| CallReturn::one(Value::array(values)))
    })?;
    Ok(HostCallResult::Pending(op_id))
}

/// Closes a SQLite resource through the generic scope close. Pending drivers
/// on the connection observe the closed slot and are retired through the
/// scope's operation registry; no type-dispatched helper is needed.
#[pd_host_function(name = "sqlite::close")]
pub(super) fn builtin_sqlite_close_impl(vm: &mut Vm, db_id: i64) -> VmResult<()> {
    let handle = sqlite_handle(db_id)?;
    vm.execution_scope()
        .close_resource::<SqliteResource>(handle, ResourceCloseReason::Requested)
        .map_err(|error| VmError::HostError(format!("unknown SQLite database: {error}")))?;
    Ok(())
}

/// Adapter-owned cleanup for `configure`/`clear`: replaces the embedding
/// policy. Pending operations and open connections are unaffected; a later
/// `clear` or VM reset retires them through the generic scope close.
pub(crate) fn configure_policy(vm: &mut Vm, policy: SqlitePolicy) {
    vm.host.sqlite_state.policy = policy;
}

/// Adapter-owned cleanup for `clear`: restores the default policy. Open
/// connections stay live (they carry their own limits); a VM reset or
/// explicit `sqlite::close` retires them through the generic scope close.
pub(crate) fn clear_policy(vm: &mut Vm) {
    vm.host.sqlite_state.policy = SqlitePolicy::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_operation_id(slot: u64) -> OperationId {
        OperationId::from_raw((1 << 43) | (slot << 22) | 1).expect("valid test operation id")
    }

    #[test]
    fn close_waits_for_active_and_queued_workers() {
        let connection = Connection::open_in_memory().expect("in-memory SQLite connection");
        let interrupt = connection.get_interrupt_handle();
        let slot = Arc::new(ConnectionSlot {
            connection: Mutex::new(connection),
            execution: Mutex::new(()),
            active_operation: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            live_workers: AtomicUsize::new(0),
            close_waker: Mutex::new(None),
            interrupt: Arc::new(interrupt),
            limits: SqliteLimits::default(),
            allow_unsafe_sql: false,
            closed: AtomicBool::new(false),
        });
        let open_connections = Arc::new(AtomicUsize::new(1));
        let mut resource = SqliteResource::new(Arc::clone(&slot), Arc::clone(&open_connections));
        let active_id = test_operation_id(1);
        let queued_id = test_operation_id(2);
        slot.register(active_id);
        slot.register(queued_id);

        let release_active = Arc::new(AtomicBool::new(false));
        let active_started = Arc::new(AtomicBool::new(false));
        let active_slot = Arc::clone(&slot);
        let active_release = Arc::clone(&release_active);
        let active_started_flag = Arc::clone(&active_started);
        let active = thread::spawn(move || {
            let _execution = active_slot.execution.lock().expect("execution lock");
            active_started_flag.store(true, Ordering::Release);
            while !active_release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            active_slot.unregister(active_id);
        });
        while !active_started.load(Ordering::Acquire) {
            thread::yield_now();
        }

        let queued_started = Arc::new(AtomicBool::new(false));
        let queued_executed = Arc::new(AtomicBool::new(false));
        let queued_slot = Arc::clone(&slot);
        let queued_started_flag = Arc::clone(&queued_started);
        let queued_executed_flag = Arc::clone(&queued_executed);
        let queued = thread::spawn(move || {
            queued_started_flag.store(true, Ordering::Release);
            let _execution = queued_slot.execution.lock().expect("execution lock");
            if !queued_slot.closed.load(Ordering::Acquire) {
                queued_executed_flag.store(true, Ordering::Release);
            }
            queued_slot.unregister(queued_id);
        });
        while !queued_started.load(Ordering::Acquire) {
            thread::yield_now();
        }

        assert_eq!(
            resource
                .begin_close(ResourceCloseReason::Requested)
                .expect("close should begin"),
            CloseProgress::Pending
        );
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(resource.poll_close(&mut cx), Poll::Pending));
        assert!(!queued_executed.load(Ordering::Acquire));

        release_active.store(true, Ordering::Release);
        active.join().expect("active worker should finish");
        queued.join().expect("queued worker should finish");
        assert!(!queued_executed.load(Ordering::Acquire));
        assert!(slot.drained());
        assert!(matches!(resource.poll_close(&mut cx), Poll::Ready(Ok(()))));
        assert_eq!(open_connections.load(Ordering::Acquire), 0);
    }
}
