use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};

use super::typed::{VmArrayRef, VmMapRef};
use super::{HostCallResult, VmMap};
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeSchema,
};
use crate::vm::operation::{
    HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationResult,
    OperationSpec,
};
use crate::vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceHandle, ResourceResult,
    ResourceTypeKey,
};
use crate::vm::{
    CallOutcome, CallReturn, HostContextError, HostFunctionRegistry, HostOpId, Value, Vm, VmError,
    VmResult,
};

const SQLITE_PROGRESS_STEPS: i32 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqlitePolicy {
    pub database_root: Option<String>,
    pub allow_unsafe_sql: bool,
    pub limits: SqliteLimits,
}

/// Persistent, per-VM SQLite module state.
///
/// Lives outside the invocation execution scope: it is installed through the
/// generic module-state store and deliberately survives
/// [`Vm::reset_for_reuse`] and scope close. The open-connection counter is
/// shared (via [`Arc`]) with every live connection resource; the last one to
/// close decrements it, so it stays authoritative across resets without the
/// core ever counting resources by class.
struct SqliteHostState {
    policy: SqlitePolicy,
    open_connections: Arc<AtomicUsize>,
}

impl Default for SqliteHostState {
    fn default() -> Self {
        Self {
            policy: SqlitePolicy::default(),
            open_connections: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// SQLite host configuration owned by the SQLite host implementation.
///
/// Configuration is persistent module state, *outside* invocation resources:
/// [`configure_sqlite`](Self::configure_sqlite) replaces the policy without
/// touching the execution scope, and the policy survives
/// [`Vm::reset_for_reuse`]. Connections and in-flight queries are
/// closed/cancelled by the generic execution-scope lifecycle, never by a
/// SQLite-specific owner/type dispatch.
#[allow(dead_code)]
pub trait SqliteHostExt {
    fn configure_sqlite(&mut self, policy: SqlitePolicy);
    fn clear_sqlite_configuration(&mut self);
}

impl SqliteHostExt for Vm {
    fn configure_sqlite(&mut self, policy: SqlitePolicy) {
        let mut ctx = self.host_context();
        let open_connections = ctx
            .module_state::<SqliteHostState>()
            .map(|state| Arc::clone(&state.open_connections))
            .unwrap_or_default();
        ctx.set_module_state(SqliteHostState {
            policy,
            open_connections,
        });
    }

    fn clear_sqlite_configuration(&mut self) {
        let _ = self.host_context().take_module_state::<SqliteHostState>();
    }
}

fn sqlite_policy(vm: &mut Vm) -> SqlitePolicy {
    vm.host_context()
        .module_state::<SqliteHostState>()
        .map_or_else(SqlitePolicy::default, |state| state.policy.clone())
}

fn sqlite_connection_key() -> ResourceTypeKey {
    SqliteConnectionResource::resource_type_key()
        .expect("sqlite.connection resource type key must be valid")
}

/// Maps a generic resource-close reason onto the parallel operation-cancellation
/// vocabulary (the same stable 1:1 mapping the execution scope uses).
fn operation_reason(reason: ResourceCloseReason) -> OperationCancelReason {
    match reason {
        ResourceCloseReason::Requested => OperationCancelReason::Requested,
        ResourceCloseReason::Deadline => OperationCancelReason::Deadline,
        ResourceCloseReason::VmReset => OperationCancelReason::VmReset,
        ResourceCloseReason::Parent => OperationCancelReason::Parent,
        ResourceCloseReason::ResourceClosed => OperationCancelReason::ResourceClosed,
        ResourceCloseReason::OwnershipRelease => OperationCancelReason::Requested,
        ResourceCloseReason::VmDrop => OperationCancelReason::VmDrop,
    }
}

/// Returns the affected-row count from a SQLite result envelope.
#[pd_host_function(name = "sqlite::rows_affected")]
pub(super) fn builtin_sqlite_rows_affected_impl(value: VmMapRef<'_>) -> VmResult<i64> {
    match value.get(&Value::string("rows_affected")) {
        Some(Value::Int(value)) => Ok(*value),
        Some(_) => Err(VmError::TypeMismatch("SQLite rows_affected integer")),
        None => Ok(0),
    }
}

/// Returns the truncation flag from a SQLite query result envelope.
#[pd_host_function(name = "sqlite::truncated")]
pub(super) fn builtin_sqlite_truncated_impl(value: VmMapRef<'_>) -> VmResult<bool> {
    match value.get(&Value::string("truncated")) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(VmError::TypeMismatch("SQLite truncated boolean")),
        None => Ok(false),
    }
}

/// Returns the continuation cursor from a SQLite query result envelope.
#[pd_host_function(name = "sqlite::next_cursor")]
pub(super) fn builtin_sqlite_next_cursor_impl(value: VmMapRef<'_>) -> VmResult<i64> {
    match value.get(&Value::string("next_cursor")) {
        Some(Value::Int(value)) => Ok(*value),
        Some(_) => Err(VmError::TypeMismatch("SQLite next_cursor integer")),
        None => Ok(0),
    }
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

struct ConnectionSlot {
    connection: Mutex<Connection>,
    execution: Mutex<()>,
    interrupt: Arc<rusqlite::InterruptHandle>,
    /// Set on close/cancel; the cooperative progress handler aborts the running
    /// statement as soon as it fires.
    closing: Arc<AtomicBool>,
    /// First cancellation reason (operation vocabulary) recorded against this
    /// connection, for diagnostics when a worker aborts mid-statement.
    closing_reason: Arc<AtomicU8>,
    /// Number of worker slots reserved or running for this connection.
    live_workers: Arc<AtomicUsize>,
    /// Number of operation slots reserved or live for this connection.
    pending_operations: Arc<AtomicUsize>,
    /// Completion waker for the connection resource's poll-based close.
    close_waker: Mutex<Option<Waker>>,
    /// Per-operation completion cells, keyed by raw operation id. Owned by the
    /// connection resource: dropped with it on close, so nothing leaks on reset.
    pending_results: Mutex<std::collections::HashMap<u64, Arc<OperationCell>>>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
}

/// Shared per-operation completion state: the produced result value plus the
/// poll waker.
///
/// The result and the waker live under a single `Mutex` so the worker's
/// publish-then-wake step is atomic with the driver's read-then-register
/// step. A result published between the driver's read and its waker
/// registration is therefore never missed, so an operation polled to
/// [`Poll::Pending`] is always woken when its worker completes.
struct OperationCell {
    state: Mutex<OperationCellState>,
}

struct OperationCellState {
    /// Completed value of the sqlite query/execute/transaction operation.
    value: Option<VmResult<CallReturn>>,
    /// Waker registered by the latest pending [`poll`](SqliteOperationDriver::poll).
    waker: Option<Waker>,
}

impl OperationCell {
    fn new() -> Self {
        Self {
            state: Mutex::new(OperationCellState {
                value: None,
                waker: None,
            }),
        }
    }
}

/// A SQLite connection modelled as a generic [`HostResource`].
///
/// The resource carries the connection slot (connection/interrupt/limits) and
/// owns the close progression: [`begin_close`](Self::begin_close) issues the
/// cooperative interrupt and reports `Pending` while any worker is still
/// alive; [`poll_close`](Self::poll_close) completes (and drops the pending
/// result cells) once every worker has drained. The core never dispatches a
/// SQLite interrupt — it only drives this generic close contract.
struct SqliteConnectionResource {
    slot: Arc<ConnectionSlot>,
    open_connections: Arc<AtomicUsize>,
    counted: bool,
}

impl HostResource for SqliteConnectionResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        ResourceTypeKey::new("sqlite.connection").ok()
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.slot.closing.store(true, Ordering::SeqCst);
        self.slot
            .closing_reason
            .store(operation_reason(reason).raw(), Ordering::SeqCst);
        self.slot.interrupt.interrupt();
        if self.slot.live_workers.load(Ordering::SeqCst) == 0 {
            Ok(CloseProgress::Ready)
        } else {
            Ok(CloseProgress::Pending)
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        if self.slot.live_workers.load(Ordering::SeqCst) == 0 {
            self.slot
                .pending_results
                .lock()
                .expect("sqlite result lock")
                .clear();
            self.release_counter();
            return Poll::Ready(Ok(()));
        }
        *self
            .slot
            .close_waker
            .lock()
            .expect("sqlite close waker lock") = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl SqliteConnectionResource {
    fn new(slot: Arc<ConnectionSlot>, open_connections: Arc<AtomicUsize>) -> Self {
        Self {
            slot,
            open_connections,
            counted: true,
        }
    }

    fn release_counter(&mut self) {
        if std::mem::take(&mut self.counted) {
            self.open_connections.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for SqliteConnectionResource {
    fn drop(&mut self) {
        // Last-resort guard: the counter is released even if the scope never
        // polls the close to completion.
        self.release_counter();
    }
}

fn host_boundary_error(error: HostContextError) -> VmError {
    VmError::HostError(error.to_string())
}

fn unknown_database_error(error: HostContextError) -> VmError {
    VmError::HostError(format!("unknown SQLite database: {error}"))
}

fn sqlite_handle(raw: i64) -> VmResult<ResourceHandle> {
    ResourceHandle::from_value(&Value::Int(raw))
        .map_err(|error| VmError::HostError(format!("unknown SQLite database handle: {error}")))
}

fn lookup_connection(vm: &mut Vm, raw: i64) -> VmResult<(ResourceHandle, Arc<ConnectionSlot>)> {
    let handle = sqlite_handle(raw)?;
    let slot = {
        let ctx = vm.host_context();
        let token = ctx
            .typed_resource::<SqliteConnectionResource>(handle)
            .map_err(unknown_database_error)?;
        ctx.resource(&token)
            .map_err(unknown_database_error)?
            .get()
            .slot
            .clone()
    };
    Ok((handle, slot))
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

fn parse_open_options(vm: &mut Vm, options: &VmMap) -> VmResult<OpenOptions> {
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
    let policy = sqlite_policy(vm);
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

fn cancellation_error(slot: &ConnectionSlot) -> VmError {
    let reason = OperationCancelReason::from_raw(slot.closing_reason.load(Ordering::SeqCst))
        .unwrap_or(OperationCancelReason::Requested);
    VmError::HostError(format!("SQLite operation cancelled ({reason})"))
}

fn with_connection<T>(
    slot: &ConnectionSlot,
    cancelled: &Arc<AtomicBool>,
    operation: impl FnOnce(&mut Connection) -> Result<T, rusqlite::Error>,
) -> VmResult<T> {
    if slot.closing.load(Ordering::SeqCst) || cancelled.load(Ordering::SeqCst) {
        // A worker that was cancelled (its own operation, or the whole
        // connection closing) before it could run must not execute (and
        // auto-commit) its statement.
        return Err(cancellation_error(slot));
    }
    let mut connection = slot
        .connection
        .lock()
        .map_err(|_| VmError::HostError("SQLite connection lock is poisoned".to_string()))?;
    let closing = Arc::clone(&slot.closing);
    let cancelled_hook = Arc::clone(cancelled);
    connection.progress_handler(
        SQLITE_PROGRESS_STEPS,
        Some(move || closing.load(Ordering::SeqCst) || cancelled_hook.load(Ordering::SeqCst)),
    );
    let result = operation(&mut connection);
    connection.progress_handler(0, None::<fn() -> bool>);
    if slot.closing.load(Ordering::SeqCst) || cancelled.load(Ordering::SeqCst) {
        return Err(cancellation_error(slot));
    }
    result.map_err(sqlite_error)
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

/// One atomic capacity reservation. The counter is decremented exactly once
/// when the reservation owner is dropped.
struct CounterReservation {
    counter: Arc<AtomicUsize>,
}

impl CounterReservation {
    fn acquire(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        let mut current = counter.load(Ordering::SeqCst);
        loop {
            if current >= limit {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(Self {
                        counter: Arc::clone(counter),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for CounterReservation {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Driver for one async SQLite activity (query / execute / transaction).
///
/// The worker thread runs the statement on the shared connection slot and
/// stores the completed value in the shared [`OperationCell`]; the driver's
/// [`poll`](Self::poll) observes the cell and registers the caller's waker.
/// [`cancel`](Self::cancel) issues the cooperative interrupt on the shared
/// connection — the only cancellation mechanism; the core never dispatches a
/// SQLite interrupt directly.
struct SqliteOperationDriver {
    slot: Arc<ConnectionSlot>,
    cell: Arc<OperationCell>,
    running: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    operation_id: Arc<AtomicU64>,
    _pending_reservation: CounterReservation,
}

impl HostOperation for SqliteOperationDriver {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        let mut state = self.cell.state.lock().expect("sqlite operation cell lock");
        match state.value.as_ref() {
            Some(Ok(_)) => Poll::Ready(Ok(())),
            Some(Err(error)) => Poll::Ready(Err(OperationError::new(
                OperationErrorCode::OperationDriverFailed,
                "sqlite::operation",
                error.to_string(),
            ))),
            None => {
                // Register the current waker. The worker publishes its result
                // into this same cell and wakes this waker once the result is
                // visible, so a pending waiter is always re-polled on
                // completion.
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn cancel(&mut self, reason: OperationCancelReason) -> OperationResult<()> {
        // This operation is cancelled: always abort its own worker, before it
        // runs (the `cancelled` flag is checked by `with_connection`) or while
        // it runs (interrupt).
        self.cancelled.store(true, Ordering::SeqCst);
        if self.running.load(Ordering::SeqCst) {
            self.slot.interrupt.interrupt();
        }
        // Connection-level reasons (the connection itself is closing, or the
        // whole scope is resetting) also flip the shared closing flag so every
        // worker aborts; an individual `Requested`/`Deadline` cancel must stay
        // scoped to its own operation.
        if !matches!(
            reason,
            OperationCancelReason::Requested | OperationCancelReason::Deadline
        ) {
            self.slot.closing.store(true, Ordering::SeqCst);
            self.slot
                .closing_reason
                .store(reason.raw(), Ordering::SeqCst);
        }
        Ok(())
    }
}

impl Drop for SqliteOperationDriver {
    fn drop(&mut self) {
        let operation_id = self.operation_id.load(Ordering::SeqCst);
        if operation_id == 0 {
            return;
        }
        let preserve_for_success_adapter = self
            .cell
            .state
            .lock()
            .expect("sqlite operation cell lock")
            .value
            .as_ref()
            .is_some_and(Result::is_ok);
        if !preserve_for_success_adapter {
            self.slot
                .pending_results
                .lock()
                .expect("sqlite pending results lock")
                .remove(&operation_id);
        }
    }
}

/// Maximum live sqlite worker threads per connection (safety valve).
const SQLITE_MAX_WORKERS_PER_SLOT: usize = 8;

fn schedule_operation(
    vm: &mut Vm,
    handle: ResourceHandle,
    slot: Arc<ConnectionSlot>,
    cancelled: Arc<AtomicBool>,
    operation: impl FnOnce(Arc<ConnectionSlot>, Arc<AtomicBool>) -> VmResult<CallReturn>
    + Send
    + 'static,
) -> VmResult<HostOpId> {
    let pending_reservation =
        CounterReservation::acquire(&slot.pending_operations, slot.limits.max_pending_operations)
            .ok_or_else(|| {
            VmError::HostError(format!(
                "SQLite pending operation limit {} reached",
                slot.limits.max_pending_operations
            ))
        })?;
    let worker_reservation =
        CounterReservation::acquire(&slot.live_workers, SQLITE_MAX_WORKERS_PER_SLOT)
            .ok_or_else(|| VmError::HostError("SQLite worker limit reached".to_string()))?;

    let cell: Arc<OperationCell> = Arc::new(OperationCell::new());
    let running: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let operation_id = Arc::new(AtomicU64::new(0));
    let deadline =
        Instant::now().checked_add(Duration::from_millis(slot.limits.max_transaction_ms));

    let driver = SqliteOperationDriver {
        slot: Arc::clone(&slot),
        cell: Arc::clone(&cell),
        running: Arc::clone(&running),
        cancelled: Arc::clone(&cancelled),
        operation_id: Arc::clone(&operation_id),
        _pending_reservation: pending_reservation,
    };
    let mut spec = OperationSpec::new(driver).with_resource(handle);
    if let Some(deadline) = deadline {
        spec = spec.with_deadline(deadline);
    }
    let op_id = vm
        .host_context()
        .start_operation(spec)
        .map_err(host_boundary_error)?;
    let raw = op_id.raw();
    operation_id.store(raw, Ordering::SeqCst);
    slot.pending_results
        .lock()
        .expect("sqlite pending results lock")
        .insert(raw, Arc::clone(&cell));

    let conn_raw = handle.raw() as i64;
    vm.host.register_pending_op_result(
        raw,
        Box::new(move |vm: &mut Vm| {
            take_pending_result(vm, raw, conn_raw).unwrap_or_else(|| {
                Err(VmError::HostError(
                    "SQLite operation produced no result".to_string(),
                ))
            })
        }),
    );

    let worker_slot = Arc::clone(&slot);
    let worker_result = Arc::clone(&cell);
    let worker_running = Arc::clone(&running);
    let worker_cancelled = Arc::clone(&cancelled);
    let spawn_result = thread::Builder::new()
        .name(format!("rustscript-sqlite-worker-{raw}"))
        .spawn(move || {
            let _execution = worker_slot
                .execution
                .lock()
                .expect("SQLite execution lock should not be poisoned");
            worker_running.store(true, Ordering::SeqCst);
            let result = operation(Arc::clone(&worker_slot), worker_cancelled);
            worker_running.store(false, Ordering::SeqCst);
            let wake = {
                let mut state = worker_result
                    .state
                    .lock()
                    .expect("SQLite result cell lock should not be poisoned");
                state.value = Some(result);
                state.waker.take()
            };
            drop(worker_reservation);
            if let Some(waker) = wake {
                waker.wake();
            }
            if let Ok(mut waker) = worker_slot.close_waker.lock()
                && let Some(waker) = waker.take()
            {
                waker.wake();
            }
        });

    if let Err(error) = spawn_result {
        let cause = VmError::HostError(format!("failed to start SQLite worker: {error}"));
        return match vm
            .host_context()
            .abort_operation(op_id, OperationCancelReason::Requested)
        {
            Ok(_) => Err(cause),
            Err(cleanup) => Err(VmError::HostError(format!(
                "{cause}; operation rollback failed: {cleanup}"
            ))),
        };
    }

    Ok(raw)
}

/// Removes and returns the completed value of one sqlite operation, if the
/// operation's driver produced one. The cell is registered on the connection
/// resource (keyed by raw operation id) and cleaned up when the connection
/// closes. The caller supplies the connection handle captured before the
/// operation's terminal state consumed its registry entry.
pub(super) fn take_pending_result(
    vm: &mut Vm,
    op_raw: u64,
    conn_raw: i64,
) -> Option<VmResult<CallReturn>> {
    let slot = lookup_connection(vm, conn_raw).ok()?.1;
    let cell = slot
        .pending_results
        .lock()
        .expect("sqlite pending results lock")
        .remove(&op_raw)?;
    cell.state
        .lock()
        .expect("sqlite result cell lock")
        .value
        .take()
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn pending_result_count(vm: &mut Vm, resource_id: i64) -> usize {
    lookup_connection(vm, resource_id)
        .map(|(_, slot)| {
            slot.pending_results
                .lock()
                .expect("sqlite pending results lock")
                .len()
        })
        .unwrap_or(0)
}

/// Number of worker threads still alive for the connection identified by
/// `resource_id`.
///
/// Test-only: lets the sqlite host tests observe that a query has actually
/// entered execution before exercising cancellation. Compiled out of the
/// production crate.
#[cfg(test)]
#[allow(dead_code)]
pub(super) fn live_worker_count(vm: &mut Vm, resource_id: i64) -> usize {
    lookup_connection(vm, resource_id)
        .map(|(_, slot)| slot.live_workers.load(Ordering::SeqCst))
        .unwrap_or(0)
}

/// Opens a SQLite database under the embedding-owned path and limit policy.
#[pd_host_function(name = "sqlite::open")]
pub(super) fn builtin_sqlite_open_impl(vm: &mut Vm, options: VmMapRef<'_>) -> VmResult<i64> {
    let options = parse_open_options(vm, options)?;
    let open_connections = {
        let mut ctx = vm.host_context();
        if ctx.module_state::<SqliteHostState>().is_none() {
            // Progressive install: opening without an explicit configuration
            // still binds the persistent module state so connection counting
            // stays authoritative and the policy a future
            // `configure_sqlite` replaces it later.
            ctx.set_module_state(SqliteHostState::default());
        }
        Arc::clone(
            &ctx.module_state::<SqliteHostState>()
                .expect("sqlite module state installed above")
                .open_connections,
        )
    };
    if open_connections.load(Ordering::SeqCst) >= options.limits.max_connections {
        return Err(VmError::HostError(format!(
            "SQLite connection limit {} reached",
            options.limits.max_connections
        )));
    }
    let connection = open_connection(&options)?;
    let interrupt = Arc::new(connection.get_interrupt_handle());
    let slot = Arc::new(ConnectionSlot {
        connection: Mutex::new(connection),
        execution: Mutex::new(()),
        interrupt: Arc::clone(&interrupt),
        closing: Arc::new(AtomicBool::new(false)),
        closing_reason: Arc::new(AtomicU8::new(0)),
        live_workers: Arc::new(AtomicUsize::new(0)),
        pending_operations: Arc::new(AtomicUsize::new(0)),
        close_waker: Mutex::new(None),
        pending_results: Mutex::new(std::collections::HashMap::new()),
        limits: options.limits,
        allow_unsafe_sql: options.allow_unsafe_sql,
    });
    open_connections.fetch_add(1, Ordering::SeqCst);
    let token = vm
        .host_context()
        .push_resource_with_key(
            SqliteConnectionResource::new(slot, Arc::clone(&open_connections)),
            sqlite_connection_key(),
        )
        .map_err(host_boundary_error)?;
    Ok(token.into_handle().raw() as i64)
}

/// Executes one parameterized SQLite statement asynchronously.
#[pd_host_function(name = "sqlite::execute")]
pub(super) fn builtin_sqlite_execute_impl(
    vm: &mut Vm,
    db_id: i64,
    sql: &str,
    params: VmArrayRef<'_>,
) -> VmResult<HostCallResult<VmMap>> {
    let (handle, slot) = lookup_connection(vm, db_id)?;
    validate_sql(sql, slot.limits, slot.allow_unsafe_sql)?;
    let sql = sql.to_string();
    let params = sqlite_params(params, slot.limits)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let op_id = schedule_operation(
        vm,
        handle,
        slot,
        Arc::clone(&cancelled),
        move |slot, cancelled| {
            with_connection(&slot, &cancelled, |connection| {
                execute_with_connection(connection, &sql, &params)
            })
            .map(|value| CallReturn::one(Value::Map(Arc::new(value))))
        },
    )?;
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
    let (handle, slot) = lookup_connection(vm, db_id)?;
    let query_limits = parse_query_limits(limits, slot.limits)?;
    validate_sql(sql, query_limits, slot.allow_unsafe_sql)?;
    let sql = sql.to_string();
    let params = sqlite_params(params, slot.limits)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let op_id = schedule_operation(
        vm,
        handle,
        slot,
        Arc::clone(&cancelled),
        move |slot, cancelled| {
            with_connection(&slot, &cancelled, |connection| {
                query_with_connection(connection, &sql, &params, query_limits)
            })
            .map(|value| CallReturn::one(Value::Map(Arc::new(value))))
        },
    )?;
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

/// Executes ordered statements atomically and returns ordered result envelopes.
#[pd_host_function(name = "sqlite::transaction")]
pub(super) fn builtin_sqlite_transaction_impl(
    vm: &mut Vm,
    db_id: i64,
    statements: VmArrayRef<'_>,
) -> VmResult<HostCallResult<Vec<Value>>> {
    let (handle, slot) = lookup_connection(vm, db_id)?;
    let statements = parse_transaction_statements(statements, slot.limits, slot.allow_unsafe_sql)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let op_id = schedule_operation(
        vm,
        handle,
        slot,
        Arc::clone(&cancelled),
        move |slot, cancelled| {
            with_connection(&slot, &cancelled, |connection| {
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
        },
    )?;
    Ok(HostCallResult::Pending(op_id))
}

/// Closes a SQLite resource and cancels operations using it.
#[pd_host_function(name = "sqlite::close")]
pub(super) fn builtin_sqlite_close_impl(vm: &mut Vm, db_id: i64) -> VmResult<()> {
    let handle = sqlite_handle(db_id)?;
    vm.host_context()
        .close_resource::<SqliteConnectionResource>(handle, ResourceCloseReason::ResourceClosed)
        .map_err(host_boundary_error)?;
    Ok(())
}

/// The shared [`HostApiCatalog`] describing every SQLite host function.
///
/// This is the SQLite *subcatalog* surface. The standard extensions and the
/// standard compile entry use the combined [`standard_host_catalog`]
/// snapshot, not this subcatalog, so a standard compile does NOT match
/// [`SqliteExtension`]'s default registration. Custom embedders who compile
/// against this subcatalog must register with
/// [`register_sqlite_builtin_module_from_catalog`] (or
/// [`SqliteExtension`] against the combined snapshot) so the registered
/// fingerprint matches the compiled imports.
pub fn sqlite_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(SQLITE_HOST_CATALOG.get_or_init(build_sqlite_host_catalog))
}

static SQLITE_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

fn build_sqlite_host_catalog() -> Arc<HostApiCatalog> {
    let key = sqlite_connection_key();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        key.clone(),
        "An open SQLite database connection",
    ));

    // The dynamic option/parameter/statement/envelope containers are accepted
    // as `unknown` because RustScript object/array literals are exact record /
    // array types; the sqlite implementation validates the concrete contents
    // at runtime. Schemas, keys, passing modes and fingerprints still come
    // from this one catalog, so compiler and registry agree byte-for-byte.

    builder.function(HostFunctionSchema::with_return(
        "sqlite::open",
        vec![HostParamSchema::value("options", HostTypeSchema::Unknown)],
        HostTypeSchema::Resource(key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::execute",
        vec![
            borrow_connection(&key),
            HostParamSchema::value("sql", HostTypeSchema::String),
            HostParamSchema::value("params", HostTypeSchema::Unknown),
        ],
        HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::query",
        vec![
            borrow_connection(&key),
            HostParamSchema::value("sql", HostTypeSchema::String),
            HostParamSchema::value("params", HostTypeSchema::Unknown),
            HostParamSchema::value("limits", HostTypeSchema::Unknown),
        ],
        HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::transaction",
        vec![
            borrow_connection(&key),
            HostParamSchema::value("statements", HostTypeSchema::Unknown),
        ],
        HostTypeSchema::Array(Box::new(HostTypeSchema::Unknown)),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::close",
        vec![borrow_connection(&key)],
        HostTypeSchema::Null,
    ));
    for (name, result) in [
        ("sqlite::rows_affected", HostTypeSchema::Int),
        ("sqlite::truncated", HostTypeSchema::Bool),
        ("sqlite::next_cursor", HostTypeSchema::Int),
    ] {
        builder.function(HostFunctionSchema::with_return(
            name,
            vec![HostParamSchema::value("envelope", HostTypeSchema::Unknown)],
            result,
        ));
    }

    Arc::new(builder.build().expect("sqlite catalog must build"))
}

fn borrow_connection(key: &ResourceTypeKey) -> HostParamSchema {
    HostParamSchema::with_passing(
        "connection",
        HostTypeSchema::Resource(key.clone()),
        HostParamPassing::Borrow,
    )
}

pub(super) struct SqliteAdapterContract {
    pub(super) name: &'static str,
    pub(super) arity: u8,
    pub(super) adapter: fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>,
}

pub(super) const SQLITE_ADAPTER_CONTRACTS: &[SqliteAdapterContract] = &[
    SqliteAdapterContract {
        name: "sqlite::open",
        arity: 1,
        adapter: open_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::execute",
        arity: 3,
        adapter: execute_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::query",
        arity: 4,
        adapter: query_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::transaction",
        arity: 2,
        adapter: transaction_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::close",
        arity: 1,
        adapter: close_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::rows_affected",
        arity: 1,
        adapter: rows_affected_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::truncated",
        arity: 1,
        adapter: truncated_adapter,
    },
    SqliteAdapterContract {
        name: "sqlite::next_cursor",
        arity: 1,
        adapter: next_cursor_adapter,
    },
];

/// Registers every SQLite host function into `registry` using the exact
/// catalog schema path and the authoritative [`standard_host_catalog`]
/// snapshot.
///
/// The standard extensions all register against this single combined
/// snapshot, so a standard combined-catalog compile exact-binds the standard
/// SQLite surface byte-for-byte. Callers that compose their own custom
/// catalog or a SQLite *subcatalog* snapshot must use
/// [`register_sqlite_builtin_module_from_catalog`] instead.
pub fn register_sqlite_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = crate::builtins::runtime::standard_host_catalog();
    register_sqlite_builtin_module_from_catalog(registry, &catalog)
}

/// Registers every SQLite host function into `registry` using the exact
/// schema path derived from a caller-supplied, validated
/// [`HostApiCatalog`] snapshot.
///
/// This is the public register-forwarding API for custom embedders who
/// compile against a SQLite subcatalog (or their own composite) rather than
/// the standard combined snapshot: the schemas are extracted from the
/// supplied `catalog`, so the registered exact fingerprint matches what the
/// matching compile emitted. Every static and pending member is preflighted
/// against its adapter contract (including labels, passing modes, resource keys
/// and return schema), and all mutations are published atomically. Missing or
/// incompatible members return a typed
/// [`crate::vm::HostImportBindingError`] before registry state changes.
pub fn register_sqlite_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    let contract = sqlite_host_catalog();
    let catalog_fingerprint = catalog.fingerprint();
    let contract_fingerprint = contract.fingerprint();
    let schemas = SQLITE_ADAPTER_CONTRACTS
        .iter()
        .map(|entry| {
            crate::vm::host_extension::validate_catalog_import_schemas_with_fingerprints(
                catalog,
                &contract,
                entry.name,
                catalog_fingerprint,
                contract_fingerprint,
            )
            .map(|schemas| (entry, schemas))
        })
        .collect::<VmResult<Vec<_>>>()?;

    registry.transactionally(|staged| {
        for (entry, schemas) in &schemas {
            for schema in schemas.iter().cloned() {
                staged.register_exact_static(entry.name, entry.arity, schema, entry.adapter)?;
            }
            staged.authorize_registered_builtin_import(entry.name);
        }
        Ok(())
    })
}

/// Standard [`HostExtension`] registering SQLite through the exact catalog
/// path and installing the persistent policy module state.
pub struct SqliteExtension;

impl crate::vm::HostExtension for SqliteExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        register_sqlite_builtin_module(registry)
    }

    fn install(&self, vm: &mut Vm) {
        vm.host_context()
            .set_module_state(SqliteHostState::default());
    }
}

fn open_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    builtin_sqlite_open(vm, args).map(|raw| CallOutcome::Return(CallReturn::One(Value::Int(raw))))
}

fn execute_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    match builtin_sqlite_execute(vm, args)? {
        HostCallResult::Return(value) => Ok(CallOutcome::Return(CallReturn::One(Value::Map(
            Arc::new(value),
        )))),
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn query_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    match builtin_sqlite_query(vm, args)? {
        HostCallResult::Return(value) => Ok(CallOutcome::Return(CallReturn::One(Value::Map(
            Arc::new(value),
        )))),
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn transaction_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    match builtin_sqlite_transaction(vm, args)? {
        HostCallResult::Return(values) => {
            Ok(CallOutcome::Return(CallReturn::one(Value::array(values))))
        }
        HostCallResult::Pending(op_id) => Ok(CallOutcome::Pending(op_id)),
    }
}

fn close_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    builtin_sqlite_close(vm, args).map(|()| CallOutcome::Return(CallReturn::None))
}

fn rows_affected_adapter(_vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    builtin_sqlite_rows_affected(args)
        .map(|value| CallOutcome::Return(CallReturn::One(Value::Int(value))))
}

fn truncated_adapter(_vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    builtin_sqlite_truncated(args)
        .map(|value| CallOutcome::Return(CallReturn::One(Value::Bool(value))))
}

fn next_cursor_adapter(_vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    builtin_sqlite_next_cursor(args)
        .map(|value| CallOutcome::Return(CallReturn::One(Value::Int(value))))
}
