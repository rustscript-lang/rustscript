use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};

use super::cancellation::{
    CancellationReason, CancellationToken, OperationId, OperationOwner, OperationStatus,
};
use super::error::{RuntimeError, RuntimeErrorCode};
use super::resource::{ResourceHandle, ResourceTypeId};
use super::typed::{VmArrayRef, VmMapRef};
use super::{HostCallResult, VmMap};
use crate::vm::{CallReturn, HostOpId, SqliteLimits, Value, Vm, VmError, VmResult};

const SQLITE_PROGRESS_STEPS: i32 = 1_000;
const SQLITE_CLOSE_GRACE: Duration = Duration::from_millis(100);

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
    active_operation: Mutex<Option<OperationId>>,
    interrupt: Arc<rusqlite::InterruptHandle>,
    limits: SqliteLimits,
    allow_unsafe_sql: bool,
}

struct PendingResult {
    receiver: mpsc::Receiver<VmResult<CallReturn>>,
    worker: Option<JoinHandle<()>>,
    waker: Arc<Mutex<Option<Waker>>>,
}

fn runtime_error(error: RuntimeError) -> VmError {
    VmError::HostError(error.to_string())
}

fn operation_id(op_id: HostOpId) -> VmResult<OperationId> {
    OperationId::from_raw(op_id).map_err(runtime_error)
}

fn handle_value(handle: ResourceHandle) -> i64 {
    match handle.as_value() {
        Value::Int(value) => value,
        _ => unreachable!("resource handles are integer values"),
    }
}

fn sqlite_handle(raw: i64) -> VmResult<ResourceHandle> {
    let handle = ResourceHandle::from_value(&Value::Int(raw))
        .map_err(|error| VmError::HostError(format!("unknown SQLite database handle: {error}")))?;
    if handle.resource_type() != ResourceTypeId::SQLITE_CONNECTION {
        return Err(VmError::HostError(
            "unknown SQLite database handle (wrong resource type)".to_string(),
        ));
    }
    Ok(handle)
}

fn lookup_connection(vm: &Vm, raw: i64) -> VmResult<(ResourceHandle, Arc<ConnectionSlot>)> {
    let handle = sqlite_handle(raw)?;
    let slot = vm
        .host
        .runtime_resources
        .get::<Arc<ConnectionSlot>>(handle, ResourceTypeId::SQLITE_CONNECTION)
        .map_err(|error| VmError::HostError(format!("unknown SQLite database: {error}")))?;
    Ok((handle, Arc::clone(slot)))
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

fn parse_open_options(vm: &Vm, options: &VmMap) -> VmResult<OpenOptions> {
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
    let configured_root = vm
        .host
        .sqlite_policy
        .database_root
        .as_deref()
        .map(PathBuf::from);
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
    let limits = parse_limits(map_value(options, "limits"), vm.host.sqlite_policy.limits)?;
    Ok(OpenOptions {
        path,
        mode,
        root: configured_root,
        limits,
        allow_unsafe_sql: vm.host.sqlite_policy.allow_unsafe_sql,
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

fn cancellation_error(token: &CancellationToken) -> VmError {
    let reason = token
        .reason()
        .unwrap_or(CancellationReason::Requested)
        .as_str();
    VmError::HostError(format!("SQLite operation cancelled ({reason})"))
}

fn with_connection<T>(
    slot: &ConnectionSlot,
    token: &CancellationToken,
    operation: impl FnOnce(&mut Connection) -> Result<T, rusqlite::Error>,
) -> VmResult<T> {
    token.check().map_err(runtime_error)?;
    let mut connection = slot
        .connection
        .lock()
        .map_err(|_| VmError::HostError("SQLite connection lock is poisoned".to_string()))?;
    token.check().map_err(runtime_error)?;
    let callback_token = token.clone();
    connection.progress_handler(
        SQLITE_PROGRESS_STEPS,
        Some(move || callback_token.is_cancelled()),
    );
    let result = operation(&mut connection);
    connection.progress_handler(0, None::<fn() -> bool>);
    if token.is_cancelled() {
        return Err(cancellation_error(token));
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

fn pending_count_for_resource(vm: &Vm, resource: ResourceHandle) -> usize {
    vm.host
        .runtime_operations
        .operations_for_resource(resource)
        .into_iter()
        .filter(|operation| operation.owner() == OperationOwner::Sqlite)
        .count()
}

fn schedule_operation(
    vm: &mut Vm,
    resource: ResourceHandle,
    slot: Arc<ConnectionSlot>,
    operation: impl FnOnce(Arc<ConnectionSlot>, CancellationToken) -> VmResult<CallReturn>
    + Send
    + 'static,
) -> VmResult<HostOpId> {
    if pending_count_for_resource(vm, resource) >= slot.limits.max_pending_operations {
        return Err(VmError::HostError(format!(
            "SQLite pending operation limit {} reached",
            slot.limits.max_pending_operations
        )));
    }
    let deadline =
        Instant::now().checked_add(Duration::from_millis(slot.limits.max_transaction_ms));
    let operation_state = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Sqlite,
            Some(&vm.run_ctx.cancellation),
            deadline,
            None,
        )
        .map_err(runtime_error)?;
    let id = operation_state.id();
    let token = operation_state.token();
    let cleanup_slot = Arc::clone(&slot);
    operation_state
        .set_cleanup(Box::new(move |end| {
            if matches!(end, super::cancellation::OperationEnd::Cancelled(_))
                && cleanup_slot
                    .active_operation
                    .lock()
                    .expect("SQLite active operation lock should not be poisoned")
                    .is_some_and(|active| active == id)
            {
                cleanup_slot.interrupt.interrupt();
            }
            Ok(())
        }))
        .map_err(runtime_error)?;
    let worker_operation = operation_state.clone();
    let (sender, receiver) = mpsc::channel();
    let waker = Arc::new(Mutex::new(None::<Waker>));
    let worker_waker = Arc::clone(&waker);
    let worker = thread::Builder::new()
        .name(format!("rustscript-sqlite-{}", id.raw()))
        .spawn(move || {
            let _execution = slot
                .execution
                .lock()
                .expect("SQLite execution lock should not be poisoned");
            *slot
                .active_operation
                .lock()
                .expect("SQLite active operation lock should not be poisoned") = Some(id);
            let result = operation(Arc::clone(&slot), token);
            *slot
                .active_operation
                .lock()
                .expect("SQLite active operation lock should not be poisoned") = None;
            match &result {
                Ok(_) => {
                    let _ = worker_operation.complete();
                }
                Err(error) => {
                    let _ = worker_operation.fail(
                        RuntimeError::new(
                            RuntimeErrorCode::OperationFailed,
                            "sqlite::operation",
                            error.to_string(),
                        )
                        .with_value(id.raw()),
                    );
                }
            }
            let _ = sender.send(result);
            if let Ok(mut waker) = worker_waker.lock()
                && let Some(waker) = waker.take()
            {
                waker.wake();
            }
        })
        .map_err(|error| {
            let _ = vm
                .host
                .runtime_operations
                .cancel(id, CancellationReason::Requested);
            VmError::HostError(format!("failed to start SQLite worker: {error}"))
        })?;
    let pending = PendingResult {
        receiver,
        worker: Some(worker),
        waker,
    };
    let payload = match vm.host.runtime_resources.insert_with_cleanup(
        ResourceTypeId::CALLBACK,
        pending,
        |pending, _reason| {
            wait_worker_bounded(pending);
            Ok(())
        },
    ) {
        Ok(payload) => payload,
        Err(error) => {
            let _ = vm
                .host
                .runtime_operations
                .cancel(id, CancellationReason::ResourceClosed);
            return Err(runtime_error(error));
        }
    };
    operation_state.set_resource(resource);
    operation_state.set_payload(payload);
    Ok(id.raw())
}

fn wait_worker_bounded(mut pending: PendingResult) {
    let deadline = Instant::now() + SQLITE_CLOSE_GRACE;
    if let Some(worker) = pending.worker.take() {
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn active_operation_id(vm: &Vm, resource_id: i64) -> Option<HostOpId> {
    let handle = ResourceHandle::from_value(&Value::Int(resource_id)).ok()?;
    let slot = vm
        .host
        .runtime_resources
        .get::<Arc<ConnectionSlot>>(handle, ResourceTypeId::SQLITE_CONNECTION)
        .ok()?;
    let active = *slot
        .active_operation
        .lock()
        .expect("SQLite active operation lock should not be poisoned");
    active.map(OperationId::raw)
}

fn cancel_operation(vm: &mut Vm, id: OperationId, reason: CancellationReason) {
    let Ok(operation) = vm.host.runtime_operations.get(id) else {
        return;
    };
    if operation.owner() != OperationOwner::Sqlite {
        return;
    }
    super::cancel_runtime_operation(vm, id, reason);
}

pub(super) fn poll_pending_op(
    vm: &mut Vm,
    op_id: HostOpId,
    cx: &mut Context<'_>,
) -> Poll<VmResult<CallReturn>> {
    let id = match operation_id(op_id) {
        Ok(id) => id,
        Err(error) => return Poll::Ready(Err(error)),
    };
    let operation = match vm.host.runtime_operations.get(id) {
        Ok(operation) if operation.owner() == OperationOwner::Sqlite => operation,
        Ok(_) => {
            return Poll::Ready(Err(VmError::HostError(format!(
                "host operation {op_id} is not owned by SQLite"
            ))));
        }
        Err(error) => return Poll::Ready(Err(runtime_error(error))),
    };
    let Some(payload) = operation.payload() else {
        return Poll::Ready(Err(VmError::HostError(format!(
            "SQLite operation {op_id} has no completion payload"
        ))));
    };
    if operation.token().is_cancelled() {
        let reason = operation
            .token()
            .reason()
            .unwrap_or(CancellationReason::Requested);
        let error = cancellation_error(&operation.token());
        cancel_operation(vm, id, reason);
        return Poll::Ready(Err(error));
    }
    let (received, worker) = {
        let pending = match vm
            .host
            .runtime_resources
            .get_mut::<PendingResult>(payload, ResourceTypeId::CALLBACK)
        {
            Ok(pending) => pending,
            Err(error) => return Poll::Ready(Err(runtime_error(error))),
        };
        if let Ok(mut waker) = pending.waker.lock() {
            *waker = Some(cx.waker().clone());
        }
        let received = pending.receiver.try_recv();
        let worker = if matches!(received, Err(mpsc::TryRecvError::Empty)) {
            None
        } else {
            pending.worker.take()
        };
        (received, worker)
    };
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    match received {
        Err(mpsc::TryRecvError::Empty) => {
            if operation.token().is_cancelled() {
                let reason = operation
                    .token()
                    .reason()
                    .unwrap_or(CancellationReason::Requested);
                let error = cancellation_error(&operation.token());
                cancel_operation(vm, id, reason);
                Poll::Ready(Err(error))
            } else {
                Poll::Pending
            }
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            let _ = super::close_runtime_resource(vm, payload, CancellationReason::ResourceClosed);
            Poll::Ready(Err(VmError::HostError(
                "SQLite worker ended without a result".to_string(),
            )))
        }
        Ok(result) => {
            let _ = super::close_runtime_resource(vm, payload, CancellationReason::ResourceClosed);
            match result {
                Ok(value) => {
                    if let OperationStatus::Cancelled(_) = operation.status() {
                        Poll::Ready(Err(cancellation_error(&operation.token())))
                    } else {
                        Poll::Ready(Ok(value))
                    }
                }
                Err(error) => {
                    if operation.token().is_cancelled() {
                        Poll::Ready(Err(cancellation_error(&operation.token())))
                    } else {
                        Poll::Ready(Err(error))
                    }
                }
            }
        }
    }
}

/// Opens a SQLite database under the embedding-owned path and limit policy.
#[pd_host_function(name = "sqlite::open")]
pub(super) fn builtin_sqlite_open_impl(vm: &mut Vm, options: VmMapRef<'_>) -> VmResult<i64> {
    let options = parse_open_options(vm, options)?;
    let open_count = vm
        .host
        .runtime_resources
        .count_type(ResourceTypeId::SQLITE_CONNECTION);
    if open_count >= options.limits.max_connections {
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
        active_operation: Mutex::new(None),
        interrupt: Arc::clone(&interrupt),
        limits: options.limits,
        allow_unsafe_sql: options.allow_unsafe_sql,
    });
    let cleanup_interrupt = Arc::clone(&interrupt);
    let handle = vm
        .host
        .runtime_resources
        .insert_with_cleanup(
            ResourceTypeId::SQLITE_CONNECTION,
            slot,
            move |_slot, _reason| {
                cleanup_interrupt.interrupt();
                Ok(())
            },
        )
        .map_err(runtime_error)?;
    Ok(handle_value(handle))
}

/// Executes one parameterized SQLite statement asynchronously.
#[pd_host_function(name = "sqlite::execute")]
pub(super) fn builtin_sqlite_execute_impl(
    vm: &mut Vm,
    db_id: i64,
    sql: &str,
    params: VmArrayRef<'_>,
) -> VmResult<HostCallResult<VmMap>> {
    let (resource, slot) = lookup_connection(vm, db_id)?;
    validate_sql(sql, slot.limits, slot.allow_unsafe_sql)?;
    let sql = sql.to_string();
    let params = sqlite_params(params, slot.limits)?;
    let op_id = schedule_operation(vm, resource, slot, move |slot, token| {
        with_connection(&slot, &token, |connection| {
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
    let (resource, slot) = lookup_connection(vm, db_id)?;
    let query_limits = parse_query_limits(limits, slot.limits)?;
    validate_sql(sql, query_limits, slot.allow_unsafe_sql)?;
    let sql = sql.to_string();
    let params = sqlite_params(params, slot.limits)?;
    let op_id = schedule_operation(vm, resource, slot, move |slot, token| {
        with_connection(&slot, &token, |connection| {
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

/// Executes ordered statements atomically and returns ordered result envelopes.
#[pd_host_function(name = "sqlite::transaction")]
pub(super) fn builtin_sqlite_transaction_impl(
    vm: &mut Vm,
    db_id: i64,
    statements: VmArrayRef<'_>,
) -> VmResult<HostCallResult<Vec<Value>>> {
    let (resource, slot) = lookup_connection(vm, db_id)?;
    let statements = parse_transaction_statements(statements, slot.limits, slot.allow_unsafe_sql)?;
    let op_id = schedule_operation(vm, resource, slot, move |slot, token| {
        with_connection(&slot, &token, |connection| {
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

/// Closes a SQLite resource and cancels operations using it.
#[pd_host_function(name = "sqlite::close")]
pub(super) fn builtin_sqlite_close_impl(vm: &mut Vm, db_id: i64) -> VmResult<()> {
    let handle = sqlite_handle(db_id)?;
    super::close_runtime_resource(vm, handle, CancellationReason::ResourceClosed)
        .map_err(|error| VmError::HostError(format!("unknown SQLite database: {error}")))?;
    Ok(())
}
