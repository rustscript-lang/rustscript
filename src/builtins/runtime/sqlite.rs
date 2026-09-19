//! Scoped SQLite host functions (optional `sqlite` feature).
//!
//! Each public SQLite function is an ordinary macro-owned async host function.
//! Calls capture owned SQL, parameters, and connection context before awaiting
//! `tokio-rusqlite`, which owns the blocking SQLite execution thread. The host
//! layer owns no worker, operation driver, mailbox, or manual wakeup state.
//!
//! Connections remain typed [`HostResource`] values in the VM execution scope.
//! A resource stores the adapter handle, immutable policy/limits, close lifecycle,
//! and open/in-flight accounting needed for configured limits. Explicit close and
//! reusable scope teardown interrupt active work and await the adapter's own
//! `Connection::close` confirmation before releasing the resource permit. Dropping
//! the VM remains nonblocking, while cancellation of an individual submitted future
//! retains its operation lease in the adapter closure until that work finishes or
//! is discarded.

use std::fs;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};

use super::VmMap;
use super::typed::{VmArrayHandle, VmArrayRef};
use crate::host_api::{HostApiCatalog, ResourceTypeKey};
use crate::vm::resource::close::{CloseProgress, HostResource};
use crate::vm::resource::error::{ResourceError, ResourceErrorCode, ResourceResult};
use crate::vm::resource::{ResourceCloseReason, ResourceHandle};
use crate::vm::{
    CaptureAsyncHostContext, HostFunctionRegistry, HostFutureOutput, Value, Vm, VmError, VmResult,
};

/// SQLite `progress_handler` step cadence used to enforce transaction deadlines.
const SQLITE_PROGRESS_STEPS: i32 = 1_000;

/// Maximum adapter close attempts, including the initial request.
const SQLITE_CLOSE_MAX_ATTEMPTS: usize = 3;

/// Bounded SQLite connection/query limits, mirroring the published surface.
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

struct ConnectionCountPermit {
    open_connections: Arc<AtomicUsize>,
}

impl Drop for ConnectionCountPermit {
    fn drop(&mut self) {
        self.open_connections.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve_connection(
    open_connections: Arc<AtomicUsize>,
    limit: usize,
) -> VmResult<ConnectionCountPermit> {
    open_connections
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < limit).then_some(count + 1)
        })
        .map_err(|_| VmError::HostError(format!("SQLite connection limit {limit} reached")))?;
    Ok(ConnectionCountPermit { open_connections })
}

struct SqliteOperationLease {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for SqliteOperationLease {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

enum SqliteCloseAttempt {
    Closed,
    Retryable {
        connection: tokio_rusqlite::Connection,
        message: String,
    },
    Failed(String),
}

type SqliteCloseFuture = Pin<Box<dyn Future<Output = SqliteCloseAttempt> + Send + 'static>>;

enum SqliteCloseState {
    Open,
    Closing {
        future: SqliteCloseFuture,
        attempts: usize,
    },
    // A terminal error cannot be reported as resource-close completion:
    // `ResourceTable` reclaims every Ready resource, including cleanup errors.
    // Retain a returned adapter handle when available and park the resource so
    // its connection permit and the VM reuse guard remain held.
    Terminal {
        _connection: Option<tokio_rusqlite::Connection>,
        message: String,
    },
    Closed,
}

struct SqliteCloseLifecycle {
    state: Mutex<SqliteCloseState>,
    #[cfg(test)]
    injected_failures: AtomicUsize,
    #[cfg(test)]
    failures_seen: Arc<AtomicUsize>,
}

impl SqliteCloseLifecycle {
    fn new() -> Self {
        Self {
            state: Mutex::new(SqliteCloseState::Open),
            #[cfg(test)]
            injected_failures: AtomicUsize::new(0),
            #[cfg(test)]
            failures_seen: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    fn new_with_failures(failures: usize) -> Self {
        Self {
            state: Mutex::new(SqliteCloseState::Open),
            injected_failures: AtomicUsize::new(failures),
            failures_seen: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    fn failures_seen(&self) -> usize {
        self.failures_seen.load(Ordering::Acquire)
    }

    fn close_future(&self, connection: tokio_rusqlite::Connection) -> SqliteCloseFuture {
        #[cfg(test)]
        let inject_failure = self
            .injected_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        #[cfg(test)]
        let failures_seen = Arc::clone(&self.failures_seen);
        Box::pin(async move {
            #[cfg(test)]
            if inject_failure {
                failures_seen.fetch_add(1, Ordering::AcqRel);
                return SqliteCloseAttempt::Retryable {
                    connection,
                    message: "injected retryable SQLite close failure".to_string(),
                };
            }
            match connection.close().await {
                Ok(()) | Err(tokio_rusqlite::Error::ConnectionClosed) => SqliteCloseAttempt::Closed,
                Err(tokio_rusqlite::Error::Close((connection, error))) => {
                    SqliteCloseAttempt::Retryable {
                        connection,
                        message: sqlite_error_message(error),
                    }
                }
                Err(error) => SqliteCloseAttempt::Failed(adapter_close_error_message(error)),
            }
        })
    }

    fn begin(
        &self,
        connection: tokio_rusqlite::Connection,
        interrupt: &rusqlite::InterruptHandle,
        closed: &AtomicBool,
    ) -> Result<bool, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*state {
            SqliteCloseState::Open => {
                closed.store(true, Ordering::Release);
                interrupt.interrupt();
                *state = SqliteCloseState::Closing {
                    future: self.close_future(connection),
                    attempts: 1,
                };
                Ok(false)
            }
            SqliteCloseState::Closing { .. } => Ok(false),
            SqliteCloseState::Terminal { message, .. } => Err(message.clone()),
            SqliteCloseState::Closed => Ok(true),
        }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<Result<(), String>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            match &mut *state {
                SqliteCloseState::Open => return Poll::Pending,
                SqliteCloseState::Closing { future, attempts } => {
                    match future.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(SqliteCloseAttempt::Closed) => {
                            *state = SqliteCloseState::Closed;
                            return Poll::Ready(Ok(()));
                        }
                        Poll::Ready(SqliteCloseAttempt::Retryable {
                            connection,
                            message: _,
                        }) if *attempts < SQLITE_CLOSE_MAX_ATTEMPTS => {
                            let attempts = *attempts + 1;
                            *state = SqliteCloseState::Closing {
                                future: self.close_future(connection),
                                attempts,
                            };
                            // Poll the replacement now so it either registers
                            // the caller's waker or consumes another bounded,
                            // immediately-ready retry. Never self-wake here.
                        }
                        Poll::Ready(SqliteCloseAttempt::Retryable {
                            connection,
                            message,
                        }) => {
                            let result = Err(message.clone());
                            *state = SqliteCloseState::Terminal {
                                _connection: Some(connection),
                                message,
                            };
                            return Poll::Ready(result);
                        }
                        Poll::Ready(SqliteCloseAttempt::Failed(message)) => {
                            let result = Err(message.clone());
                            *state = SqliteCloseState::Terminal {
                                _connection: None,
                                message,
                            };
                            return Poll::Ready(result);
                        }
                    }
                }
                SqliteCloseState::Terminal { message, .. } => {
                    return Poll::Ready(Err(message.clone()));
                }
                SqliteCloseState::Closed => return Poll::Ready(Ok(())),
            }
        }
    }

    fn has_terminal_failure(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(&*state, SqliteCloseState::Terminal { .. })
    }
}

/// The one script-visible SQLite connection resource.
struct SqliteResource {
    connection: tokio_rusqlite::Connection,
    interrupt: Arc<rusqlite::InterruptHandle>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
    closed: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
    close_lifecycle: Arc<SqliteCloseLifecycle>,
    _connection_permit: ConnectionCountPermit,
}

impl HostResource for SqliteResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        ResourceTypeKey::new(super::sqlite_schema::SQLITE_CONNECTION_KEY).ok()
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        if reason == ResourceCloseReason::VmDrop {
            self.closed.store(true, Ordering::Release);
            self.interrupt.interrupt();
            return Ok(CloseProgress::Ready);
        }
        match self.close_lifecycle.begin(
            self.connection.clone(),
            self.interrupt.as_ref(),
            self.closed.as_ref(),
        ) {
            Ok(true) => Ok(CloseProgress::Ready),
            Ok(false) => Ok(CloseProgress::Pending),
            Err(_) if self.close_lifecycle.has_terminal_failure() => Ok(CloseProgress::Pending),
            Err(message) => Err(sqlite_close_resource_error(message)),
        }
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        match self.close_lifecycle.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) if self.close_lifecycle.has_terminal_failure() => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.map_err(sqlite_close_resource_error)),
        }
    }
}

#[derive(Default)]
pub(crate) struct SqliteState {
    pub(crate) open_connections: Arc<AtomicUsize>,
}

fn sqlite_state(vm: &mut Vm) -> VmResult<&mut SqliteState> {
    vm.execution_scope()
        .scope_state_or_insert_with(SqliteState::default)
        .map_err(|error| VmError::HostError(format!("sqlite scope state unavailable: {error}")))
}

#[derive(Clone)]
pub(super) struct SqliteOpenContext {
    policy: SqlitePolicy,
    open_connections: Arc<AtomicUsize>,
}

impl CaptureAsyncHostContext for SqliteOpenContext {
    fn capture(vm: &mut Vm) -> VmResult<Self> {
        let policy = current_policy(vm).clone();
        let open_connections = Arc::clone(&sqlite_state(vm)?.open_connections);
        Ok(Self {
            policy,
            open_connections,
        })
    }
}

#[derive(Clone)]
pub(super) struct SqliteConnectionContext {
    handle: ResourceHandle,
    connection: tokio_rusqlite::Connection,
    interrupt: Arc<rusqlite::InterruptHandle>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
    closed: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
    close_lifecycle: Arc<SqliteCloseLifecycle>,
}

impl SqliteConnectionContext {
    fn ensure_open(&self) -> VmResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(VmError::HostError(
                "SQLite database is already closed".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    fn begin_operation(&self) -> VmResult<SqliteOperationLease> {
        self.ensure_open()?;
        let limit = self.limits.max_pending_operations;
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < limit).then_some(count + 1)
            })
            .map_err(|_| {
                VmError::HostError(format!("SQLite pending operation limit {limit} reached"))
            })?;
        if self.closed.load(Ordering::Acquire) {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(VmError::HostError(
                "SQLite database is already closed".to_string(),
            ));
        }
        Ok(SqliteOperationLease {
            in_flight: Arc::clone(&self.in_flight),
        })
    }
}

impl CaptureAsyncHostContext for SqliteConnectionContext {
    fn capture(_vm: &mut Vm) -> VmResult<Self> {
        Err(VmError::HostError(
            "SQLite connection context requires call arguments".to_string(),
        ))
    }

    fn capture_with_args(vm: &mut Vm, args: &[Value]) -> VmResult<Self> {
        let db_id = match args.first() {
            Some(Value::Int(value)) => *value,
            Some(_) => return Err(VmError::TypeMismatch("int")),
            None => {
                return Err(VmError::HostError(
                    "missing SQLite database argument".to_string(),
                ));
            }
        };
        lookup_connection(vm, db_id)
    }
}

/// The default SQLite embedding policy used when no policy has been
/// configured through [`SqliteHostExt::configure_sqlite`]. This is the
/// value `SqlitePolicy::default()` produces, constructed explicitly so it can
/// live in a `static` and serve as the fallback behind the persistent
/// module-state lookup.
static DEFAULT_POLICY: SqlitePolicy = SqlitePolicy {
    database_root: None,
    allow_unsafe_sql: false,
    limits: SqliteLimits {
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
    },
};

/// Returns the current persistent SQLite embedding policy, falling back to the
/// adapter default when none was configured through [`SqliteHostExt`].
fn current_policy(vm: &Vm) -> &SqlitePolicy {
    vm.host
        .get_module_state::<SqlitePolicy>()
        .unwrap_or(&DEFAULT_POLICY)
}

fn sqlite_error_message(error: rusqlite::Error) -> String {
    let code = error
        .sqlite_error()
        .map(|value| value.extended_code.to_string())
        .unwrap_or_else(|| "non_sqlite".to_string());
    let name = error
        .sqlite_error_code()
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "RusqliteError".to_string());
    format!("SQLite error {name} ({code}): {error}")
}

fn sqlite_error(error: rusqlite::Error) -> VmError {
    VmError::HostError(sqlite_error_message(error))
}

fn adapter_call_error(error: tokio_rusqlite::Error<VmError>) -> VmError {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => {
            VmError::HostError("SQLite connection was closed".to_string())
        }
        tokio_rusqlite::Error::Close((_, error)) => sqlite_error(error),
        tokio_rusqlite::Error::Error(error) => error,
        _ => VmError::HostError(format!("SQLite adapter error: {error}")),
    }
}

fn adapter_close_error_message(error: tokio_rusqlite::Error) -> String {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => "SQLite connection was closed".to_string(),
        tokio_rusqlite::Error::Close((_, error)) | tokio_rusqlite::Error::Error(error) => {
            sqlite_error_message(error)
        }
        _ => format!("SQLite adapter error: {error}"),
    }
}

fn sqlite_close_resource_error(message: String) -> ResourceError {
    ResourceError::new(
        ResourceErrorCode::ResourceCleanupFailed,
        "sqlite::close",
        message,
    )
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

/// Lifts a guest-visible integer handle into owned adapter call context.
fn lookup_connection(vm: &mut Vm, handle_id: i64) -> VmResult<SqliteConnectionContext> {
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
    if resource.closed.load(Ordering::Acquire) {
        return Err(VmError::HostError(
            "SQLite database is already closed".to_string(),
        ));
    }
    Ok(SqliteConnectionContext {
        handle,
        connection: resource.connection.clone(),
        interrupt: Arc::clone(&resource.interrupt),
        limits: resource.limits,
        allow_unsafe_sql: resource.allow_unsafe_sql,
        closed: Arc::clone(&resource.closed),
        in_flight: Arc::clone(&resource.in_flight),
        close_lifecycle: Arc::clone(&resource.close_lifecycle),
    })
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
        Some(Value::Null) | None => Err(VmError::HostError(format!("missing SQLite {key}"))),
        Some(_) => Err(VmError::TypeMismatch("SQLite option string")),
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
    if matches!(value, Value::Null) {
        return Ok(ceiling);
    }
    let Value::Map(map) = value else {
        return Err(VmError::TypeMismatch("SQLite limits map"));
    };
    let mut limits = ceiling;
    for (key, value) in map.iter() {
        let Value::String(key) = key else {
            return Err(VmError::TypeMismatch("SQLite limit name"));
        };
        if matches!(value, Value::Null) {
            continue;
        }
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
    connection
        .set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            sqlite_limit(max_value_bytes, "value byte limit")?,
        )
        .map_err(sqlite_error)?;
    connection
        .set_limit(
            Limit::SQLITE_LIMIT_SQL_LENGTH,
            sqlite_limit(limits.max_statement_bytes, "statement byte limit")?,
        )
        .map_err(sqlite_error)?;
    connection
        .set_limit(
            Limit::SQLITE_LIMIT_COLUMN,
            sqlite_limit(limits.max_columns, "column limit")?,
        )
        .map_err(sqlite_error)?;
    connection
        .set_limit(
            Limit::SQLITE_LIMIT_VARIABLE_NUMBER,
            sqlite_limit(limits.max_parameters, "parameter count limit")?,
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn install_authorizer(connection: &Connection, allow_unsafe_sql: bool) -> VmResult<()> {
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
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
        }))
        .map_err(sqlite_error)
}

async fn open_connection(
    options: &OpenOptions,
) -> VmResult<(tokio_rusqlite::Connection, Arc<rusqlite::InterruptHandle>)> {
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
        Some(path) => tokio_rusqlite::Connection::open_with_flags(path, flags).await,
        None => tokio_rusqlite::Connection::open_in_memory_with_flags(flags).await,
    }
    .map_err(sqlite_error)?;
    let limits = options.limits;
    let allow_unsafe_sql = options.allow_unsafe_sql;
    let interrupt = connection
        .call(move |connection| {
            connection
                .busy_timeout(Duration::from_millis(limits.busy_timeout_ms))
                .map_err(sqlite_error)?;
            install_connection_limits(connection, limits)?;
            install_authorizer(connection, allow_unsafe_sql)?;
            Ok(connection.get_interrupt_handle())
        })
        .await
        .map_err(adapter_call_error)?;
    Ok((connection, Arc::new(interrupt)))
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

fn reject_unexpected_fields(map: &VmMap, allowed: &[&str], context: &'static str) -> VmResult<()> {
    for (key, _) in map {
        let Value::String(key) = key else {
            return Err(VmError::TypeMismatch(context));
        };
        if !allowed.iter().any(|allowed| *allowed == key.as_str()) {
            return Err(VmError::HostError(format!(
                "{context} contains unknown field '{key}'"
            )));
        }
    }
    Ok(())
}

fn present_payload<'a>(map: &'a VmMap, key: &str) -> Option<&'a Value> {
    match map_value(map, key) {
        Some(Value::Null) | None => None,
        Some(value) => Some(value),
    }
}

fn sqlite_parameter_value(
    value: &Value,
    parameter_bytes: &mut usize,
    limits: SqliteLimits,
) -> VmResult<SqlValue> {
    let Value::Map(map) = value else {
        return Err(VmError::TypeMismatch("SQLite value"));
    };
    reject_unexpected_fields(
        map,
        &[
            "kind",
            "int_value",
            "float_value",
            "text_value",
            "blob_value",
        ],
        "SQLite value",
    )?;
    let kind = match map_value(map, "kind") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(Value::Null) | None => {
            return Err(VmError::HostError("missing SQLite value kind".to_string()));
        }
        Some(_) => return Err(VmError::TypeMismatch("SQLite value kind")),
    };
    let payload_count = ["int_value", "float_value", "text_value", "blob_value"]
        .into_iter()
        .filter(|key| present_payload(map, key).is_some())
        .count();
    if payload_count > 1 {
        return Err(VmError::HostError(
            "SQLite value has multiple non-null payloads".to_string(),
        ));
    }
    let selected_payload = match kind {
        "null" => {
            if payload_count != 0 {
                return Err(VmError::HostError(
                    "SQLite null value must not have a payload".to_string(),
                ));
            }
            return Ok(SqlValue::Null);
        }
        "int" => present_payload(map, "int_value")
            .ok_or_else(|| VmError::HostError("missing SQLite int_value".to_string()))?,
        "float" => present_payload(map, "float_value")
            .ok_or_else(|| VmError::HostError("missing SQLite float_value".to_string()))?,
        "text" => present_payload(map, "text_value")
            .ok_or_else(|| VmError::HostError("missing SQLite text_value".to_string()))?,
        "blob" => present_payload(map, "blob_value")
            .ok_or_else(|| VmError::HostError("missing SQLite blob_value".to_string()))?,
        _ => {
            return Err(VmError::HostError(format!(
                "unknown SQLite value kind '{kind}'"
            )));
        }
    };
    match kind {
        "int" => match selected_payload {
            Value::Int(value) => Ok(SqlValue::Integer(*value)),
            _ => Err(VmError::TypeMismatch("SQLite int_value payload")),
        },
        "float" => match selected_payload {
            Value::Float(value) => Ok(SqlValue::Real(*value)),
            _ => Err(VmError::TypeMismatch("SQLite float_value payload")),
        },
        "text" => match selected_payload {
            Value::String(value) => {
                *parameter_bytes = parameter_bytes.saturating_add(value.len());
                if *parameter_bytes > limits.max_parameter_bytes {
                    return Err(VmError::HostError(format!(
                        "SQLite parameters exceed the configured {} byte limit",
                        limits.max_parameter_bytes
                    )));
                }
                Ok(SqlValue::Text(value.as_ref().clone()))
            }
            _ => Err(VmError::TypeMismatch("SQLite text_value payload")),
        },
        "blob" => match selected_payload {
            Value::Bytes(value) => {
                *parameter_bytes = parameter_bytes.saturating_add(value.len());
                if *parameter_bytes > limits.max_parameter_bytes {
                    return Err(VmError::HostError(format!(
                        "SQLite parameters exceed the configured {} byte limit",
                        limits.max_parameter_bytes
                    )));
                }
                Ok(SqlValue::Blob(value.as_ref().clone()))
            }
            _ => Err(VmError::TypeMismatch("SQLite blob_value payload")),
        },
        "null" => unreachable!("null SQLite values return before payload decoding"),
        _ => unreachable!("unknown SQLite value kinds return before payload decoding"),
    }
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
        params.push(sqlite_parameter_value(value, &mut bytes, limits)?);
    }
    Ok(params)
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

fn sqlite_value_map(
    kind: &str,
    int_value: Option<i64>,
    float_value: Option<f64>,
    text_value: Option<String>,
    blob_value: Option<Vec<u8>>,
) -> Value {
    Value::Map(Arc::new(VmMap::from_entries(vec![
        (Value::string("kind"), Value::string(kind)),
        (
            Value::string("int_value"),
            int_value.map_or(Value::Null, Value::Int),
        ),
        (
            Value::string("float_value"),
            float_value.map_or(Value::Null, Value::Float),
        ),
        (
            Value::string("text_value"),
            text_value.map_or(Value::Null, Value::string),
        ),
        (
            Value::string("blob_value"),
            blob_value.map_or(Value::Null, Value::bytes),
        ),
    ])))
}

struct EncodedSqliteValue {
    value: Value,
    bytes: usize,
    integer: Option<i64>,
}

fn value_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<EncodedSqliteValue, rusqlite::Error> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(EncodedSqliteValue {
            value: sqlite_value_map("null", None, None, None, None),
            bytes: 1,
            integer: None,
        }),
        ValueRef::Integer(value) => Ok(EncodedSqliteValue {
            value: sqlite_value_map("int", Some(value), None, None, None),
            bytes: 8,
            integer: Some(value),
        }),
        ValueRef::Real(value) => Ok(EncodedSqliteValue {
            value: sqlite_value_map("float", None, Some(value), None, None),
            bytes: 8,
            integer: None,
        }),
        ValueRef::Text(value) => match std::str::from_utf8(value) {
            Ok(value) => Ok(EncodedSqliteValue {
                value: sqlite_value_map("text", None, None, Some(value.to_string()), None),
                bytes: value.len(),
                integer: None,
            }),
            Err(_) => Ok(EncodedSqliteValue {
                value: sqlite_value_map("blob", None, None, None, Some(value.to_vec())),
                bytes: value.len(),
                integer: None,
            }),
        },
        ValueRef::Blob(value) => Ok(EncodedSqliteValue {
            value: sqlite_value_map("blob", None, None, None, Some(value.to_vec())),
            bytes: value.len(),
            integer: None,
        }),
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
        let mut row_cursor = None;
        for index in 0..column_count {
            let encoded = value_from_row(row, index)?;
            row_bytes = row_bytes.saturating_add(encoded.bytes);
            if index == 0 {
                row_cursor = encoded.integer;
            }
            cells.push(encoded.value);
        }
        if result_bytes.saturating_add(row_bytes) > limits.max_result_bytes {
            truncated = true;
            break;
        }
        if let Some(cursor) = row_cursor {
            next_cursor = Some(cursor);
        }
        result_bytes = result_bytes.saturating_add(row_bytes);
        values.push(Value::Map(Arc::new(VmMap::from_entries(vec![(
            Value::string("cells"),
            Value::array(cells),
        )]))));
    }
    let mut entries = vec![
        (Value::string("columns"), Value::array(columns)),
        (Value::string("rows"), Value::array(values)),
        (Value::string("truncated"), Value::Bool(truncated)),
    ];
    entries.push((
        Value::string("next_cursor"),
        match next_cursor {
            Some(next_cursor) => Value::Int(next_cursor),
            None => Value::Null,
        },
    ));
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

fn transaction_result_value(kind: &str, execute: Option<VmMap>, query: Option<VmMap>) -> Value {
    Value::Map(Arc::new(VmMap::from_entries(vec![
        (Value::string("kind"), Value::string(kind)),
        (
            Value::string("execute"),
            execute.map_or(Value::Null, |value| Value::Map(Arc::new(value))),
        ),
        (
            Value::string("query"),
            query.map_or(Value::Null, |value| Value::Map(Arc::new(value))),
        ),
    ])))
}

fn parse_open_options(policy: &SqlitePolicy, options: &VmMap) -> VmResult<OpenOptions> {
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
#[pd_host_function(name = "sqlite::open", contract = super::sqlite_schema::sqlite_open_contract)]
pub(super) async fn builtin_sqlite_open_impl(
    #[pd_host_context] context: SqliteOpenContext,
    options: VmMap,
) -> VmResult<HostFutureOutput<i64>> {
    let options = parse_open_options(&context.policy, &options)?;
    let connection_permit = reserve_connection(
        Arc::clone(&context.open_connections),
        options.limits.max_connections,
    )?;
    let (connection, interrupt) = open_connection(&options).await?;
    let resource = SqliteResource {
        connection,
        interrupt,
        limits: options.limits,
        allow_unsafe_sql: options.allow_unsafe_sql,
        closed: Arc::new(AtomicBool::new(false)),
        in_flight: Arc::new(AtomicUsize::new(0)),
        close_lifecycle: Arc::new(SqliteCloseLifecycle::new()),
        _connection_permit: connection_permit,
    };
    Ok(HostFutureOutput::complete(move |vm| {
        let token = vm
            .execution_scope()
            .push_resource(resource)
            .map_err(|error| {
                VmError::HostError(format!("failed to open SQLite database: {error}"))
            })?;
        Ok(handle_value(token.handle()))
    }))
}

/// Executes one parameterized SQLite statement asynchronously.
#[pd_host_function(name = "sqlite::execute", contract = super::sqlite_schema::sqlite_execute_contract)]
pub(super) async fn builtin_sqlite_execute_impl(
    #[pd_host_context] context: SqliteConnectionContext,
    _db_id: i64,
    sql: String,
    params: VmArrayHandle,
) -> VmResult<VmMap> {
    let lease = context.begin_operation()?;
    validate_sql(&sql, context.limits, context.allow_unsafe_sql)?;
    let params = sqlite_params(params.as_ref(), context.limits)?;
    let closed = Arc::clone(&context.closed);
    let value = context
        .connection
        .call(move |connection| {
            let _lease = lease;
            if closed.load(Ordering::Acquire) {
                return Err(VmError::HostError(
                    "SQLite database is already closed".to_string(),
                ));
            }
            execute_with_connection(connection, &sql, &params).map_err(sqlite_error)
        })
        .await
        .map_err(adapter_call_error)?;
    context.ensure_open()?;
    Ok(value)
}

/// Runs one parameterized SQLite query with row and result-byte bounds.
#[pd_host_function(name = "sqlite::query", contract = super::sqlite_schema::sqlite_query_contract)]
pub(super) async fn builtin_sqlite_query_impl(
    #[pd_host_context] context: SqliteConnectionContext,
    _db_id: i64,
    sql: String,
    params: VmArrayHandle,
    limits: VmMap,
) -> VmResult<VmMap> {
    let lease = context.begin_operation()?;
    let query_limits = parse_query_limits(&limits, context.limits)?;
    validate_sql(&sql, query_limits, context.allow_unsafe_sql)?;
    let params = sqlite_params(params.as_ref(), context.limits)?;
    let closed = Arc::clone(&context.closed);
    let value = context
        .connection
        .call(move |connection| {
            let _lease = lease;
            if closed.load(Ordering::Acquire) {
                return Err(VmError::HostError(
                    "SQLite database is already closed".to_string(),
                ));
            }
            query_with_connection(connection, &sql, &params, query_limits).map_err(sqlite_error)
        })
        .await
        .map_err(adapter_call_error)?;
    context.ensure_open()?;
    Ok(value)
}

struct TransactionStatement {
    sql: String,
    params: Vec<SqlValue>,
    query: bool,
    limits: SqliteLimits,
    #[cfg(test)]
    after_execute: Option<Box<dyn Fn() + Send>>,
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
                Some(Value::Null) | None => Vec::new(),
                Some(_) => return Err(VmError::TypeMismatch("SQLite parameter array")),
            };
            let query = match map_value(statement, "query") {
                Some(Value::Bool(query)) => *query,
                Some(Value::Null) | None => false,
                Some(_) => return Err(VmError::TypeMismatch("SQLite query flag")),
            };
            let statement_limits = match map_value(statement, "limits") {
                Some(Value::Map(statement_limits)) => parse_query_limits(statement_limits, limits)?,
                Some(Value::Null) | None => limits,
                Some(_) => return Err(VmError::TypeMismatch("SQLite limits map")),
            };
            Ok(TransactionStatement {
                sql,
                params,
                query,
                limits: statement_limits,
                #[cfg(test)]
                after_execute: None,
            })
        })
        .collect()
}

fn transaction_with_connection(
    connection: &mut Connection,
    statements: Vec<TransactionStatement>,
    max_transaction_ms: u64,
) -> VmResult<Vec<Value>> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(max_transaction_ms))
        .ok_or_else(|| {
            VmError::HostError("SQLite transaction deadline is out of range".to_string())
        })?;
    transaction_with_connection_until(connection, statements, deadline, max_transaction_ms)
}

fn transaction_with_connection_until(
    connection: &mut Connection,
    statements: Vec<TransactionStatement>,
    deadline: Instant,
    max_transaction_ms: u64,
) -> VmResult<Vec<Value>> {
    connection
        .progress_handler(
            SQLITE_PROGRESS_STEPS,
            Some(move || Instant::now() >= deadline),
        )
        .map_err(sqlite_error)?;
    let result = (|| {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            if Instant::now() >= deadline {
                return Err(VmError::HostError(format!(
                    "SQLite transaction exceeded the configured {max_transaction_ms} ms deadline"
                )));
            }
            let result = if statement.query {
                let value = query_with_connection(
                    &transaction,
                    &statement.sql,
                    &statement.params,
                    statement.limits,
                )
                .map_err(sqlite_error)?;
                transaction_result_value("query", None, Some(value))
            } else {
                let value =
                    execute_with_connection(&transaction, &statement.sql, &statement.params)
                        .map_err(sqlite_error)?;
                transaction_result_value("execute", Some(value), None)
            };
            results.push(result);
            #[cfg(test)]
            if let Some(after_execute) = statement.after_execute.as_ref() {
                after_execute();
            }
        }
        if Instant::now() >= deadline {
            return Err(VmError::HostError(format!(
                "SQLite transaction exceeded the configured {max_transaction_ms} ms deadline"
            )));
        }
        transaction.commit().map_err(sqlite_error)?;
        Ok(results)
    })();
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .map_err(sqlite_error)?;
    if Instant::now() >= deadline && result.is_err() {
        return Err(VmError::HostError(format!(
            "SQLite transaction exceeded the configured {max_transaction_ms} ms deadline"
        )));
    }
    result
}

/// Runs ordered statements atomically and returns ordered result envelopes.
#[pd_host_function(name = "sqlite::transaction", contract = super::sqlite_schema::sqlite_transaction_contract)]
pub(super) async fn builtin_sqlite_transaction_impl(
    #[pd_host_context] context: SqliteConnectionContext,
    _db_id: i64,
    statements: VmArrayHandle,
) -> VmResult<Vec<Value>> {
    let lease = context.begin_operation()?;
    let statements = parse_transaction_statements(
        statements.as_ref(),
        context.limits,
        context.allow_unsafe_sql,
    )?;
    let max_transaction_ms = context.limits.max_transaction_ms;
    let closed = Arc::clone(&context.closed);
    let value = context
        .connection
        .call(move |connection| {
            let _lease = lease;
            if closed.load(Ordering::Acquire) {
                return Err(VmError::HostError(
                    "SQLite database is already closed".to_string(),
                ));
            }
            transaction_with_connection(connection, statements, max_transaction_ms)
        })
        .await
        .map_err(adapter_call_error)?;
    context.ensure_open()?;
    Ok(value)
}

/// Closes the adapter connection, then removes its VM resource.
#[pd_host_function(name = "sqlite::close", contract = super::sqlite_schema::sqlite_close_contract)]
pub(super) async fn builtin_sqlite_close_impl(
    #[pd_host_context] context: SqliteConnectionContext,
    _db_id: i64,
) -> VmResult<HostFutureOutput<()>> {
    let lease = context.begin_operation()?;
    if context.closed.swap(true, Ordering::AcqRel) {
        return Err(VmError::HostError(
            "SQLite database is already closed".to_string(),
        ));
    }
    context.interrupt.interrupt();
    let _ = context
        .connection
        .call(move |_connection| {
            drop(lease);
            Ok::<(), VmError>(())
        })
        .await;
    if let Err(message) = context.close_lifecycle.begin(
        context.connection.clone(),
        context.interrupt.as_ref(),
        context.closed.as_ref(),
    ) {
        return Err(VmError::HostError(message));
    }
    if let Err(message) = std::future::poll_fn(|cx| context.close_lifecycle.poll(cx)).await {
        return Err(VmError::HostError(message));
    }
    let handle = context.handle;
    Ok(HostFutureOutput::complete(move |vm| {
        let progress = vm
            .execution_scope()
            .close_resource::<SqliteResource>(handle, ResourceCloseReason::Requested)
            .map_err(|error| VmError::HostError(format!("unknown SQLite database: {error}")))?;
        if progress != CloseProgress::Ready {
            return Err(VmError::HostError(
                "SQLite resource removal remained pending".to_string(),
            ));
        }
        Ok(())
    }))
}

/// Every SQLite catalog function the feature-enabled build owns.
pub(super) const SQLITE_CATALOG_FUNCTIONS:
    &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    builtin_sqlite_open_descriptor,
    builtin_sqlite_execute_descriptor,
    builtin_sqlite_query_descriptor,
    builtin_sqlite_transaction_descriptor,
    builtin_sqlite_close_descriptor,
];

impl crate::host_extension::HostResourceType for SqliteResource {
    const KEY: &'static str = super::sqlite_schema::SQLITE_CONNECTION_KEY;
    const DESCRIPTION: &'static str = super::sqlite_schema::SQLITE_CONNECTION_DESCRIPTION;
}

/// The canonical declaration for the concrete `sqlite.connection` resource.
pub(super) fn concrete_sqlite_connection_resource() -> crate::host_extension::HostResourceTypeMeta {
    crate::host_extension::HostResourceTypeMeta::of::<SqliteResource>()
}

/// Registers SQLite host functions from [`super::standard_host_catalog`].
pub fn register_sqlite_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = super::standard_host_catalog();
    register_sqlite_builtin_module_from_catalog(registry, catalog.as_ref())
}

/// Registers SQLite host functions using schemas from `catalog`.
///
/// `catalog` must declare the same SQLite named structs and function overloads
/// as [`super::sqlite_host_catalog`]; the registered adapters are the module
/// descriptors, so a compile/bind pair always agrees on identity.
pub fn register_sqlite_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    super::sqlite_schema::sqlite_standard_host_module()
        .catalog_module()
        .expect("the SQLite module publishes a catalog surface")
        .install_from_catalog(registry, catalog)
        .map(|_| ())
}

/// Adapter-owned SQLite embedding-control surface.
///
/// The concrete SQLite `configure`/`clear`/policy-read control API lives in
/// this adapter module (implemented on the public [`Vm`]) rather than on the
/// generic `vm` layer, so `src/vm/**` never names the SQLite policy type or a
/// concrete control method. Replaces `configure_policy` / `clear_policy`
/// free functions and the SQLite methods that used to live in `src/vm/mod.rs`.
pub trait SqliteHostExt {
    /// Replaces the adapter-owned SQLite embedding policy.
    ///
    /// Open connections keep the limits they were opened with; new opens use
    /// this policy. The policy is stored in the persistent, reset-surviving
    /// `ModuleStateStore`: it stays in force across `reset_for_reuse` while
    /// the adapter's per-invocation scope state is destroyed and recreated.
    fn configure_sqlite(&mut self, policy: SqlitePolicy);

    /// Restores the default SQLite embedding policy.
    ///
    /// Pending operations and open connections are unaffected (they carry
    /// their own state); a VM reset or explicit `sqlite::close` retires them
    /// through the generic scope close. The persistent policy entry is
    /// removed, so subsequent opens fall back to the adapter default.
    fn clear_sqlite(&mut self);

    /// Returns the current SQLite embedding policy.
    fn sqlite_policy(&self) -> &SqlitePolicy;
}

impl SqliteHostExt for Vm {
    fn configure_sqlite(&mut self, policy: SqlitePolicy) {
        // Adapter-declared policy stored in the generic module-state store:
        // module-level policy survives execution-scope reset (an embedder's
        // root/limits remain in force across `reset_for_reuse`), while the
        // adapter's per-invocation runtime state lives in the scope arena.
        self.host.set_module_state(policy);
    }

    fn clear_sqlite(&mut self) {
        self.host.remove_module_state::<SqlitePolicy>();
    }

    fn sqlite_policy(&self) -> &SqlitePolicy {
        current_policy(self)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::vm::resource::ResourceTable;

    struct CountingWake(Arc<AtomicUsize>);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn transaction_deadline_rolls_back_an_observed_write() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustscript-sqlite-observed-rollback-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temporary SQLite root should be created");
        let database_path = root.join("state.db");
        Connection::open(&database_path)
            .expect("setup connection should open")
            .execute_batch("CREATE TABLE items (value INTEGER)")
            .expect("setup table should be created");

        let (write_observed_tx, write_observed_rx) = mpsc::sync_channel(0);
        let worker_path = database_path.clone();
        let transaction = std::thread::spawn(move || {
            let mut connection =
                Connection::open(worker_path).expect("transaction connection should open");
            let limits = SqliteLimits::default();
            let statements = vec![
                TransactionStatement {
                    sql: "INSERT INTO items (value) VALUES (1)".to_string(),
                    params: Vec::new(),
                    query: false,
                    limits,
                    after_execute: Some(Box::new(move || {
                        write_observed_tx
                            .send(())
                            .expect("write observation receiver should remain open");
                    })),
                },
                TransactionStatement {
                    sql: "WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 10000000) SELECT sum(value) FROM numbers".to_string(),
                    params: Vec::new(),
                    query: true,
                    limits,
                    after_execute: None,
                },
            ];
            transaction_with_connection_until(
                &mut connection,
                statements,
                Instant::now() + Duration::from_millis(500),
                500,
            )
        });

        write_observed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the INSERT must execute before the expensive statement starts");
        let error = transaction
            .join()
            .expect("transaction worker should not panic")
            .expect_err("the expensive statement should cross the deadline");
        assert!(
            error.to_string().contains("500 ms deadline"),
            "transaction deadline must surface explicitly, got: {error}"
        );

        let verifier = Connection::open(&database_path)
            .expect("verification connection should reopen the database");
        let count: i64 = verifier
            .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
            .expect("verification query should succeed");
        assert_eq!(count, 0, "the observed write must be rolled back");
        drop(verifier);
        fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
    }

    #[tokio::test]
    async fn retryable_close_failure_retains_resource_and_connection_permit() {
        let limits = SqliteLimits::default();
        let options = OpenOptions {
            path: ":memory:".to_string(),
            mode: OpenMode::Memory,
            root: None,
            limits,
            allow_unsafe_sql: false,
        };
        let (connection, interrupt) = open_connection(&options)
            .await
            .expect("adapter connection should open");
        let open_connections = Arc::new(AtomicUsize::new(1));
        let close_lifecycle = Arc::new(SqliteCloseLifecycle::new_with_failures(1));
        let resource = SqliteResource {
            connection,
            interrupt,
            limits,
            allow_unsafe_sql: false,
            closed: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            close_lifecycle: Arc::clone(&close_lifecycle),
            _connection_permit: ConnectionCountPermit {
                open_connections: Arc::clone(&open_connections),
            },
        };
        let mut table = ResourceTable::new().expect("resource table should initialize");
        let token = table.push(resource).expect("SQLite resource should insert");
        assert_eq!(
            table
                .begin_close(token, ResourceCloseReason::VmReset)
                .expect("close should begin"),
            CloseProgress::Pending
        );

        let mut cx = Context::from_waker(Waker::noop());
        let first_poll = table.poll_close(token, &mut cx);
        assert_eq!(close_lifecycle.failures_seen(), 1);
        match first_poll {
            Poll::Pending => {
                assert_eq!(table.len(), 1, "pending close must retain the resource");
                assert_eq!(
                    open_connections.load(Ordering::Acquire),
                    1,
                    "pending close must retain the connection permit"
                );
            }
            Poll::Ready(Ok(())) => {
                assert!(table.is_empty());
                assert_eq!(open_connections.load(Ordering::Acquire), 0);
                return;
            }
            Poll::Ready(Err(error)) => panic!("transient retry must close cleanly: {error}"),
        };

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut cx = Context::from_waker(Waker::noop());
                match table.poll_close(token, &mut cx) {
                    Poll::Ready(result) => break result,
                    Poll::Pending => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("retry should eventually confirm adapter closure")
        .expect("retry should close the resource");
        assert!(table.is_empty());
        assert_eq!(open_connections.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn persistent_close_failure_parks_without_self_waking_or_releasing_the_permit() {
        let limits = SqliteLimits::default();
        let options = OpenOptions {
            path: ":memory:".to_string(),
            mode: OpenMode::Memory,
            root: None,
            limits,
            allow_unsafe_sql: false,
        };
        let (connection, interrupt) = open_connection(&options)
            .await
            .expect("adapter connection should open");
        let open_connections = Arc::new(AtomicUsize::new(1));
        let close_lifecycle = Arc::new(SqliteCloseLifecycle::new_with_failures(usize::MAX));
        let resource = SqliteResource {
            connection,
            interrupt,
            limits,
            allow_unsafe_sql: false,
            closed: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            close_lifecycle: Arc::clone(&close_lifecycle),
            _connection_permit: ConnectionCountPermit {
                open_connections: Arc::clone(&open_connections),
            },
        };
        let program = crate::compile_source("null;")
            .expect("test program should compile")
            .program;
        let mut vm = Vm::new(program);
        vm.execution_scope()
            .push_resource(resource)
            .expect("SQLite resource should insert");
        vm.reset_for_reuse().expect("reset should start");
        assert!(
            vm.scope_reset_pending(),
            "failed cleanup must keep reset pending"
        );
        assert!(!vm.is_reusable(), "pending cleanup must block VM reuse");

        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWake(Arc::clone(&wake_count))));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(vm.poll_reset_for_reuse(&mut cx), Poll::Pending));
        assert_eq!(
            close_lifecycle.failures_seen(),
            3,
            "persistent failure must stop after the finite close-attempt budget"
        );
        assert_eq!(
            wake_count.load(Ordering::SeqCst),
            0,
            "terminal cleanup failure must not self-wake"
        );

        for _ in 0..16 {
            assert!(matches!(vm.poll_reset_for_reuse(&mut cx), Poll::Pending));
        }
        assert_eq!(
            close_lifecycle.failures_seen(),
            3,
            "polling a parked failure must not start another close"
        );
        assert_eq!(wake_count.load(Ordering::SeqCst), 0);
        assert_eq!(
            vm.host_context().resource_count(),
            1,
            "failed close must retain the resource"
        );
        assert_eq!(
            open_connections.load(Ordering::Acquire),
            1,
            "failed close must retain the connection permit"
        );
        assert!(vm.scope_reset_pending());
        assert!(!vm.is_reusable());
        assert_eq!(
            close_lifecycle.failures_seen(),
            3,
            "repeated reset polls must not double-close a parked connection"
        );
    }

    #[tokio::test]
    async fn canceled_host_future_holds_operation_slot_until_adapter_closure_finishes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rustscript-sqlite-operation-lease-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temporary SQLite root should be created");
        let database_path = root.join("state.db");
        let blocker = Connection::open(&database_path).expect("blocking connection should open");
        blocker
            .execute_batch("CREATE TABLE items (value INTEGER); BEGIN IMMEDIATE")
            .expect("blocking transaction should hold the writer lock");

        let limits = SqliteLimits::default();
        let options = OpenOptions {
            path: "state.db".to_string(),
            mode: OpenMode::ReadWriteCreate,
            root: Some(root.clone()),
            limits,
            allow_unsafe_sql: false,
        };
        let (connection, interrupt) = open_connection(&options)
            .await
            .expect("adapter connection should open");
        let in_flight = Arc::new(AtomicUsize::new(0));
        let context = SqliteConnectionContext {
            handle: ResourceHandle::encode(1, 0, 1).expect("test handle should encode"),
            connection: connection.clone(),
            interrupt,
            limits,
            allow_unsafe_sql: false,
            closed: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::clone(&in_flight),
            close_lifecycle: Arc::new(SqliteCloseLifecycle::new()),
        };
        let mut operation = Box::pin(builtin_sqlite_execute_impl(
            context,
            1,
            "INSERT INTO items (value) VALUES (1)".to_string(),
            Arc::new(Vec::new()),
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(operation.as_mut().poll(&mut cx), Poll::Pending));
        drop(operation);

        assert_eq!(
            in_flight.load(Ordering::Acquire),
            1,
            "canceling the host waiter must not release a queued adapter operation slot"
        );

        blocker
            .execute_batch("ROLLBACK")
            .expect("blocking transaction should release the writer lock");
        tokio::time::timeout(Duration::from_secs(5), async {
            while in_flight.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("adapter closure should eventually release its operation slot");
        connection
            .close()
            .await
            .expect("adapter connection should close");
        drop(blocker);
        fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
    }

    #[tokio::test]
    async fn operation_slots_release_on_validation_and_adapter_send_errors() {
        let limits = SqliteLimits::default();
        let options = OpenOptions {
            path: ":memory:".to_string(),
            mode: OpenMode::Memory,
            root: None,
            limits,
            allow_unsafe_sql: false,
        };
        let (connection, interrupt) = open_connection(&options)
            .await
            .expect("adapter connection should open");
        let in_flight = Arc::new(AtomicUsize::new(0));
        let context = SqliteConnectionContext {
            handle: ResourceHandle::encode(1, 0, 1).expect("test handle should encode"),
            connection: connection.clone(),
            interrupt,
            limits,
            allow_unsafe_sql: false,
            closed: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::clone(&in_flight),
            close_lifecycle: Arc::new(SqliteCloseLifecycle::new()),
        };

        builtin_sqlite_execute_impl(context.clone(), 1, String::new(), Arc::new(Vec::new()))
            .await
            .expect_err("empty SQL should fail validation");
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            0,
            "validation failure must release its reserved operation slot"
        );

        connection
            .close()
            .await
            .expect("adapter connection should close");
        builtin_sqlite_execute_impl(context, 1, "SELECT 1".to_string(), Arc::new(Vec::new()))
            .await
            .expect_err("sending a closure to a closed adapter should fail");
        assert_eq!(
            in_flight.load(Ordering::Acquire),
            0,
            "adapter send failure must drop the closure-owned operation slot"
        );
    }
}
