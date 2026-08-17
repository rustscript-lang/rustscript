extern crate vm as rustscript_vm;

pub mod vm {
    use std::any::{Any, TypeId};
    use std::collections::HashMap;

    pub use crate::builtins::runtime::sqlite::{SqliteLimits, SqlitePolicy};
    pub use crate::rustscript_vm::{
        CallReturn, HostCallResult, HostOpId, OpCode, Program, Value, VmError, VmMap, VmResult,
    };

    use crate::builtins::runtime::cancellation::{CancellationToken, OperationRegistry};
    use crate::builtins::runtime::resource::ResourceArena;

    pub(crate) struct TestHostRuntime {
        pub(crate) runtime_resources: ResourceArena,
        pub(crate) runtime_operations: OperationRegistry,
        host_function_states: HashMap<TypeId, Box<dyn Any + Send>>,
    }

    impl TestHostRuntime {
        pub(crate) fn set_host_function_state<T: Any + Send>(&mut self, state: T) {
            self.host_function_states
                .insert(TypeId::of::<T>(), Box::new(state));
        }

        pub(crate) fn host_function_state<T: Any + Send>(&self) -> Option<&T> {
            self.host_function_states
                .get(&TypeId::of::<T>())?
                .downcast_ref()
        }

        #[allow(dead_code)]
        pub(crate) fn remove_host_function_state<T: Any + Send>(&mut self) -> Option<T> {
            self.host_function_states
                .remove(&TypeId::of::<T>())?
                .downcast::<T>()
                .ok()
                .map(|state| *state)
        }
    }

    pub(crate) struct TestRunContext {
        pub(crate) cancellation: CancellationToken,
    }

    pub struct Vm {
        pub(crate) host: TestHostRuntime,
        pub(crate) run_ctx: TestRunContext,
    }

    impl Vm {
        pub fn new(_program: Program) -> Self {
            Self {
                host: TestHostRuntime {
                    runtime_resources: ResourceArena::default(),
                    runtime_operations: OperationRegistry::default(),
                    host_function_states: HashMap::new(),
                },
                run_ctx: TestRunContext {
                    cancellation: CancellationToken::root(),
                },
            }
        }
    }
}

mod builtins {
    pub use crate::vm::{Value, Vm, VmResult};

    pub mod runtime {
        pub use crate::vm::{HostCallResult, VmMap};

        pub mod error {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/builtins/runtime/error.rs"
            ));
        }

        #[allow(dead_code)]
        pub mod cancellation {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/builtins/runtime/cancellation.rs"
            ));
        }

        #[allow(dead_code)]
        pub mod resource {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/builtins/runtime/resource.rs"
            ));
        }

        pub(crate) fn cancel_runtime_operation(
            vm: &mut crate::vm::Vm,
            op_id: cancellation::OperationId,
            reason: cancellation::CancellationReason,
        ) {
            let payload = vm
                .host
                .runtime_operations
                .get(op_id)
                .ok()
                .and_then(|operation| operation.payload());
            let _ = vm.host.runtime_operations.cancel(op_id, reason);
            if let Some(payload) = payload {
                let _ = close_runtime_resource(vm, payload, reason);
            }
        }

        pub(crate) fn close_runtime_resource(
            vm: &mut crate::vm::Vm,
            handle: resource::ResourceHandle,
            reason: cancellation::CancellationReason,
        ) -> error::RuntimeResult<resource::CloseStatus> {
            let operations = vm
                .host
                .runtime_operations
                .operations_for_resource(handle)
                .into_iter()
                .map(|operation| {
                    let payload = operation.payload();
                    (operation, payload)
                })
                .collect::<Vec<_>>();
            for (operation, _) in &operations {
                operation.token().mark_cancelled(reason);
            }
            for (operation, _) in &operations {
                let _ = vm.host.runtime_operations.cancel(operation.id(), reason);
            }
            for (_, payload) in operations {
                if let Some(payload) = payload {
                    let _ = close_runtime_resource(vm, payload, reason);
                }
            }
            vm.host.runtime_resources.close(handle, reason)
        }

        pub(crate) fn cancel_operations_by_owner(
            vm: &mut crate::vm::Vm,
            owner: cancellation::OperationOwner,
            reason: cancellation::CancellationReason,
        ) {
            let operations = vm.host.runtime_operations.operations_by_owner(owner);
            for operation in operations {
                cancel_runtime_operation(vm, operation.id(), reason);
            }
        }

        pub(crate) fn close_resources_by_type(
            vm: &mut crate::vm::Vm,
            resource_type: resource::ResourceTypeId,
            reason: cancellation::CancellationReason,
        ) {
            let handles = vm.host.runtime_resources.handles_of_type(resource_type);
            for handle in handles {
                let _ = close_runtime_resource(vm, handle, reason);
            }
        }

        pub mod typed {
            pub type VmArrayRef<'a> = &'a [crate::vm::Value];
            pub type VmMapRef<'a> = &'a crate::vm::VmMap;
        }

        pub trait TestBorrowArg<'a>: Sized {
            fn borrow_arg(
                args: &'a [crate::vm::Value],
                index: usize,
                label: &'static str,
            ) -> crate::vm::VmResult<Self>;
        }

        impl<'a> TestBorrowArg<'a> for crate::vm::Value {
            fn borrow_arg(
                args: &'a [crate::vm::Value],
                index: usize,
                label: &'static str,
            ) -> crate::vm::VmResult<Self> {
                args.get(index)
                    .cloned()
                    .ok_or(crate::vm::VmError::HostError(label.to_string()))
            }
        }

        impl<'a> TestBorrowArg<'a> for i64 {
            fn borrow_arg(
                args: &'a [crate::vm::Value],
                index: usize,
                label: &'static str,
            ) -> crate::vm::VmResult<Self> {
                match args.get(index) {
                    Some(crate::vm::Value::Int(value)) => Ok(*value),
                    _ => Err(crate::vm::VmError::HostError(label.to_string())),
                }
            }
        }

        impl<'a> TestBorrowArg<'a> for &'a str {
            fn borrow_arg(
                args: &'a [crate::vm::Value],
                index: usize,
                label: &'static str,
            ) -> crate::vm::VmResult<Self> {
                match args.get(index) {
                    Some(crate::vm::Value::String(value)) => Ok(value.as_str()),
                    _ => Err(crate::vm::VmError::HostError(label.to_string())),
                }
            }
        }

        impl<'a> TestBorrowArg<'a> for &'a [crate::vm::Value] {
            fn borrow_arg(
                args: &'a [crate::vm::Value],
                index: usize,
                label: &'static str,
            ) -> crate::vm::VmResult<Self> {
                match args.get(index) {
                    Some(crate::vm::Value::Array(value)) => Ok(value.as_slice()),
                    _ => Err(crate::vm::VmError::HostError(label.to_string())),
                }
            }
        }

        impl<'a> TestBorrowArg<'a> for &'a crate::vm::VmMap {
            fn borrow_arg(
                args: &'a [crate::vm::Value],
                index: usize,
                label: &'static str,
            ) -> crate::vm::VmResult<Self> {
                match args.get(index) {
                    Some(crate::vm::Value::Map(value)) => Ok(value.as_ref()),
                    _ => Err(crate::vm::VmError::HostError(label.to_string())),
                }
            }
        }

        pub fn borrow_arg<'a, T: TestBorrowArg<'a>>(
            args: &'a [crate::vm::Value],
            index: usize,
            label: &'static str,
        ) -> crate::vm::VmResult<T> {
            T::borrow_arg(args, index, label)
        }

        pub mod sqlite {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/builtins/runtime/sqlite.rs"
            ));
        }

        pub mod test_api {
            use std::task::{Context, Poll};

            use super::cancellation::{
                CancellationReason, OperationId, OperationOwner, OperationStatus,
            };
            use super::resource::{ResourceHandle, ResourceTypeId};
            use super::{HostCallResult, VmMap};
            use crate::vm::{CallReturn, HostOpId, Value, Vm, VmResult};

            pub fn open(vm: &mut Vm, args: &[Value]) -> VmResult<i64> {
                super::sqlite::builtin_sqlite_open(vm, args)
            }

            pub fn execute(vm: &mut Vm, args: &[Value]) -> VmResult<HostCallResult<VmMap>> {
                super::sqlite::builtin_sqlite_execute(vm, args)
            }

            pub fn query(vm: &mut Vm, args: &[Value]) -> VmResult<HostCallResult<VmMap>> {
                super::sqlite::builtin_sqlite_query(vm, args)
            }

            pub fn transaction(
                vm: &mut Vm,
                args: &[Value],
            ) -> VmResult<HostCallResult<Vec<Value>>> {
                super::sqlite::builtin_sqlite_transaction(vm, args)
            }

            pub fn close(vm: &mut Vm, args: &[Value]) -> VmResult<()> {
                super::sqlite::builtin_sqlite_close(vm, args)
            }

            pub fn poll(
                vm: &mut Vm,
                op_id: HostOpId,
                cx: &mut Context<'_>,
            ) -> Poll<VmResult<CallReturn>> {
                super::sqlite::poll_pending_op(vm, op_id, cx)
            }

            pub fn cancel(vm: &mut Vm, op_id: HostOpId) {
                let Ok(id) = OperationId::from_raw(op_id) else {
                    return;
                };
                let payload = vm
                    .host
                    .runtime_operations
                    .get(id)
                    .ok()
                    .filter(|operation| operation.owner() == OperationOwner::Sqlite)
                    .and_then(|operation| operation.payload());
                let _ = vm
                    .host
                    .runtime_operations
                    .cancel(id, CancellationReason::Requested);
                if let Some(payload) = payload {
                    let _ = vm
                        .host
                        .runtime_resources
                        .close(payload, CancellationReason::Requested);
                }
            }

            pub fn active_operation_id(vm: &Vm, resource_id: i64) -> Option<HostOpId> {
                super::sqlite::active_operation_id(vm, resource_id)
            }

            pub fn has_pending(vm: &Vm, op_id: HostOpId) -> bool {
                OperationId::from_raw(op_id).is_ok_and(|id| {
                    vm.host.runtime_operations.get(id).is_ok_and(|operation| {
                        operation.owner() == OperationOwner::Sqlite
                            && matches!(operation.status(), OperationStatus::Pending)
                            && operation.payload().is_some()
                    })
                })
            }

            pub fn close_all(vm: &mut Vm) {
                let _ = vm
                    .host
                    .runtime_operations
                    .cancel_all(CancellationReason::VmReset);
                let _ = vm
                    .host
                    .runtime_resources
                    .close_all(CancellationReason::VmReset);
            }

            pub fn has_sqlite_operation_owner(vm: &Vm, op_id: HostOpId) -> bool {
                OperationId::from_raw(op_id)
                    .ok()
                    .and_then(|id| vm.host.runtime_operations.get(id).ok())
                    .map(|operation| operation.owner())
                    == Some(OperationOwner::Sqlite)
            }

            pub fn is_sqlite_resource(handle: i64) -> bool {
                ResourceHandle::from_value(&Value::Int(handle))
                    .is_ok_and(|handle| handle.resource_type() == ResourceTypeId::SQLITE_CONNECTION)
            }

            pub fn insert_wrong_type_resource(vm: &mut Vm) -> i64 {
                let handle = vm
                    .host
                    .runtime_resources
                    .insert(ResourceTypeId::IO_FILE, 7_i64)
                    .expect("test resource should be inserted");
                match handle.as_value() {
                    Value::Int(value) => value,
                    _ => unreachable!(),
                }
            }
        }
    }
}

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{SystemTime, UNIX_EPOCH};

use builtins::runtime::sqlite::SqliteHostExt;
use builtins::runtime::test_api as sqlite;
use vm::{CallReturn, HostCallResult, OpCode, Program, Value, Vm, VmError};

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

fn new_vm() -> Vm {
    Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
}

fn temporary_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "rustscript-sqlite-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temporary SQLite root should be created");
    root
}

fn map_value(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::map(
        entries
            .into_iter()
            .map(|(key, value)| (Value::string(key), value))
            .collect(),
    )
}

fn field<'a>(map: &'a vm::VmMap, key: &str) -> &'a Value {
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("missing SQLite result field {key}"))
}

fn open_options(root: &Path, path: &str, mode: &str, limits: Value) -> Value {
    map_value([
        ("root", Value::string(root.to_string_lossy().into_owned())),
        ("path", Value::string(path)),
        ("mode", Value::string(mode)),
        ("limits", limits),
    ])
}

fn limits(entries: impl IntoIterator<Item = (&'static str, i64)>) -> Value {
    map_value(
        entries
            .into_iter()
            .map(|(key, value)| (key, Value::Int(value))),
    )
}

fn empty_params() -> Value {
    Value::array(Vec::new())
}

fn wait_pending(vm: &mut Vm, op_id: vm::HostOpId) -> Result<Value, VmError> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match sqlite::poll(vm, op_id, &mut cx) {
            Poll::Pending => std::thread::yield_now(),
            Poll::Ready(Ok(CallReturn::None)) => return Ok(Value::Null),
            Poll::Ready(Ok(CallReturn::One(value))) => return Ok(value),
            Poll::Ready(Err(error)) => return Err(error),
        }
    }
}

fn map_from_value(value: Value) -> vm::VmMap {
    let Value::Map(map) = value else {
        panic!("SQLite host result should be a map");
    };
    (*map).clone()
}

fn host_map(
    vm: &mut Vm,
    result: Result<HostCallResult<vm::VmMap>, VmError>,
) -> Result<vm::VmMap, VmError> {
    let result = result?;
    match result {
        HostCallResult::Return(map) => Ok(map),
        HostCallResult::Pending(op_id) => Ok(map_from_value(wait_pending(vm, op_id)?)),
    }
}

fn host_array(
    vm: &mut Vm,
    result: Result<HostCallResult<Vec<Value>>, VmError>,
) -> Result<Vec<Value>, VmError> {
    let result = result?;
    match result {
        HostCallResult::Return(values) => Ok(values),
        HostCallResult::Pending(op_id) => {
            let Value::Array(values) = wait_pending(vm, op_id)? else {
                panic!("SQLite transaction result should be an array");
            };
            Ok((*values).clone())
        }
    }
}

fn open_db(vm: &mut Vm, options: Value) -> i64 {
    if let Value::Map(options_map) = &options
        && let Some(Value::String(root)) = options_map.get(&Value::string("root"))
    {
        vm.configure_sqlite(vm::SqlitePolicy {
            database_root: Some(root.as_ref().clone()),
            ..vm::SqlitePolicy::default()
        });
    }
    sqlite::open(vm, &[options]).expect("SQLite open should return")
}

fn execute(vm: &mut Vm, db_id: i64, sql: &str, params: Value) -> Result<vm::VmMap, VmError> {
    let result = sqlite::execute(vm, &[Value::Int(db_id), Value::string(sql), params]);
    host_map(vm, result)
}

fn query(
    vm: &mut Vm,
    db_id: i64,
    sql: &str,
    params: Value,
    query_limits: Value,
) -> Result<vm::VmMap, VmError> {
    let result = sqlite::query(
        vm,
        &[Value::Int(db_id), Value::string(sql), params, query_limits],
    );
    host_map(vm, result)
}

#[test]
fn sqlite_round_trip_supports_typed_values_and_ordered_transactions() {
    let root = temporary_root("round-trip");
    let mut vm = new_vm();
    let db_id = open_db(
        &mut vm,
        open_options(
            &root,
            "state.db",
            "read_write_create",
            limits([
                ("max_rows", 128),
                ("max_result_bytes", 64 * 1024),
                ("max_statements", 16),
                ("max_transaction_ms", 5_000),
            ]),
        ),
    );

    execute(
        &mut vm,
        db_id,
        "CREATE TABLE values_table (id INTEGER PRIMARY KEY, n INTEGER, r REAL, s TEXT, b BLOB, z TEXT)",
        empty_params(),
    )
    .expect("table creation should succeed");
    execute(
        &mut vm,
        db_id,
        "INSERT INTO values_table (n, r, s, b, z) VALUES (?1, ?2, ?3, ?4, ?5)",
        Value::array(vec![
            Value::Int(7),
            Value::Float(1.5),
            Value::string("hello"),
            Value::bytes(vec![0, 1, 2]),
            Value::Null,
        ]),
    )
    .expect("typed parameter insert should succeed");

    let rowset = query(
        &mut vm,
        db_id,
        "SELECT n, r, s, b, z FROM values_table ORDER BY id",
        empty_params(),
        limits([("max_rows", 8), ("max_result_bytes", 64 * 1024)]),
    )
    .expect("typed query should succeed");
    assert_eq!(
        field(&rowset, "columns"),
        &Value::array(vec![
            Value::string("n"),
            Value::string("r"),
            Value::string("s"),
            Value::string("b"),
            Value::string("z"),
        ])
    );
    assert_eq!(field(&rowset, "truncated"), &Value::Bool(false));
    let Value::Array(rows) = field(&rowset, "rows") else {
        panic!("SQLite rows should be an array");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        Value::array(vec![
            Value::Int(7),
            Value::Float(1.5),
            Value::string("hello"),
            Value::bytes(vec![0, 1, 2]),
            Value::Null,
        ])
    );

    let statements = Value::array(vec![
        map_value([
            (
                "sql",
                Value::string("INSERT INTO values_table (n) VALUES (?1)"),
            ),
            ("params", Value::array(vec![Value::Int(8)])),
        ]),
        map_value([
            (
                "sql",
                Value::string("INSERT INTO values_table (n) VALUES (?1)"),
            ),
            ("params", Value::array(vec![Value::Int(9)])),
        ]),
    ]);
    let transaction = sqlite::transaction(&mut vm, &[Value::Int(db_id), statements])
        .expect("transaction should return");
    let transaction_value =
        Value::array(host_array(&mut vm, Ok(transaction)).expect("transaction should complete"));
    let Value::Array(results) = transaction_value else {
        panic!("transaction should return ordered results");
    };
    assert_eq!(results.len(), 2);
    for result in results.iter() {
        let Value::Map(result) = result else {
            panic!("transaction result should be a map");
        };
        assert_eq!(field(result, "rows_affected"), &Value::Int(1));
    }

    sqlite::close(&mut vm, &[Value::Int(db_id)]).expect("SQLite close should succeed");
    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_enforces_read_only_vm_local_ids_and_sql_safety() {
    let root = temporary_root("policy");
    let mut writer_vm = new_vm();
    let db_id = open_db(
        &mut writer_vm,
        open_options(&root, "state.db", "read_write_create", limits([])),
    );
    execute(
        &mut writer_vm,
        db_id,
        "CREATE TABLE items (value INTEGER)",
        empty_params(),
    )
    .expect("table creation should succeed");

    let mut other_vm = new_vm();
    let cross_vm_error = sqlite::execute(
        &mut other_vm,
        &[Value::Int(db_id), Value::string("SELECT 1"), empty_params()],
    )
    .expect_err("a SQLite id must not cross VM instances");
    assert!(
        cross_vm_error
            .to_string()
            .contains("unknown SQLite database")
    );

    let unsafe_sql = [
        "ATTACH DATABASE 'other.db' AS other",
        "PRAGMA writable_schema = ON",
        "SELECT load_extension('not-available')",
        "CREATE TABLE first (id INTEGER); CREATE TABLE second (id INTEGER)",
    ];
    for sql in unsafe_sql {
        let error = sqlite::execute(
            &mut writer_vm,
            &[Value::Int(db_id), Value::string(sql), empty_params()],
        )
        .expect_err("unsafe SQL should be rejected before execution");
        assert!(
            error.to_string().contains("not allowed")
                || error.to_string().contains("multiple statements")
                || error.to_string().contains("disabled"),
            "unexpected SQLite policy error: {error}"
        );
    }

    let mut read_only_vm = new_vm();
    let read_only_id = open_db(
        &mut read_only_vm,
        open_options(&root, "state.db", "read_only", limits([])),
    );
    let read_only_result = sqlite::execute(
        &mut read_only_vm,
        &[
            Value::Int(read_only_id),
            Value::string("INSERT INTO items (value) VALUES (1)"),
            empty_params(),
        ],
    );
    let read_only_error = host_map(&mut read_only_vm, read_only_result)
        .expect_err("read-only SQLite handles must reject writes");
    assert!(
        read_only_error.to_string().contains("ReadOnly")
            || read_only_error.to_string().contains("readonly")
            || read_only_error.to_string().contains("read-only"),
        "unexpected read-only error: {read_only_error}"
    );

    sqlite::close_all(&mut writer_vm);
    sqlite::close_all(&mut read_only_vm);
    sqlite::close_all(&mut other_vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_query_reports_row_and_result_byte_truncation() {
    let root = temporary_root("limits");
    let mut vm = new_vm();
    let db_id = open_db(
        &mut vm,
        open_options(
            &root,
            "state.db",
            "read_write_create",
            limits([("max_rows", 32), ("max_result_bytes", 32)]),
        ),
    );
    execute(
        &mut vm,
        db_id,
        "CREATE TABLE items (value TEXT)",
        empty_params(),
    )
    .expect("table creation should succeed");
    for value in ["one", "two", "three"] {
        execute(
            &mut vm,
            db_id,
            "INSERT INTO items (value) VALUES (?1)",
            Value::array(vec![Value::string(value)]),
        )
        .expect("row insertion should succeed");
    }

    let row_limited = query(
        &mut vm,
        db_id,
        "SELECT value FROM items ORDER BY rowid",
        empty_params(),
        limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
    )
    .expect("limited query should succeed");
    assert_eq!(field(&row_limited, "truncated"), &Value::Bool(true));
    let Value::Array(rows) = field(&row_limited, "rows") else {
        panic!("SQLite rows should be an array");
    };
    assert_eq!(rows.len(), 1);

    let byte_limited = query(
        &mut vm,
        db_id,
        "SELECT value FROM items ORDER BY rowid",
        empty_params(),
        limits([("max_rows", 32), ("max_result_bytes", 8)]),
    )
    .expect("byte-limited query should succeed");
    assert_eq!(field(&byte_limited, "truncated"), &Value::Bool(true));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_pending_operations_can_be_cancelled_and_cleaned_up() {
    let root = temporary_root("cancel");
    let mut vm = new_vm();
    let db_id = open_db(
        &mut vm,
        open_options(
            &root,
            "state.db",
            "read_write_create",
            limits([
                ("max_transaction_ms", 10_000),
                ("max_result_bytes", 64 * 1024),
            ]),
        ),
    );
    let pending = sqlite::query(
        &mut vm,
        &[
            Value::Int(db_id),
            Value::string(
                "WITH RECURSIVE numbers(value) AS (\
                    SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 100000000\
                ) SELECT sum(value) FROM numbers",
            ),
            empty_params(),
            limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
        ],
    )
    .expect("long SQLite query should be scheduled");
    let HostCallResult::Pending(op_id) = pending else {
        panic!("long SQLite query should return a pending operation");
    };
    assert!(sqlite::has_pending(&vm, op_id));
    sqlite::cancel(&mut vm, op_id);
    assert!(!sqlite::has_pending(&vm, op_id));
    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn cancelling_queued_sqlite_operation_does_not_interrupt_active_sibling() {
    let root = temporary_root("queued_cancel");
    let mut vm = new_vm();
    let db_id = open_db(
        &mut vm,
        open_options(
            &root,
            "state.db",
            "read_write_create",
            limits([
                ("max_transaction_ms", 10_000),
                ("max_result_bytes", 64 * 1024),
            ]),
        ),
    );
    let active = sqlite::query(
        &mut vm,
        &[
            Value::Int(db_id),
            Value::string(
                "WITH RECURSIVE numbers(value) AS (\
                    SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 2000000\
                ) SELECT sum(value) FROM numbers",
            ),
            empty_params(),
            limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
        ],
    )
    .expect("active query should schedule");
    let HostCallResult::Pending(active_id) = active else {
        panic!("active query should be pending");
    };
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while sqlite::active_operation_id(&vm, db_id) != Some(active_id) {
        assert!(
            std::time::Instant::now() < wait_deadline,
            "active query should enter SQLite execution"
        );
        std::thread::yield_now();
    }

    let queued = sqlite::query(
        &mut vm,
        &[
            Value::Int(db_id),
            Value::string("SELECT 42"),
            empty_params(),
            limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
        ],
    )
    .expect("queued query should schedule");
    let HostCallResult::Pending(queued_id) = queued else {
        panic!("queued query should be pending");
    };
    sqlite::cancel(&mut vm, queued_id);
    assert_eq!(sqlite::active_operation_id(&vm, db_id), Some(active_id));

    wait_pending(&mut vm, active_id).expect("active sibling should complete successfully");
    assert!(!sqlite::has_pending(&vm, active_id));
    assert!(!sqlite::has_pending(&vm, queued_id));
    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

fn assert_sqlite_shutdown_cancels_all_siblings(close_all: bool) {
    let root = temporary_root(if close_all {
        "cancel_all_two_phase"
    } else {
        "close_two_phase"
    });
    let mut vm = new_vm();
    let options = open_options(
        &root,
        "state.db",
        "read_write_create",
        limits([
            ("max_transaction_ms", 10_000),
            ("max_result_bytes", 64 * 1024),
        ]),
    );
    let db_id = open_db(&mut vm, options.clone());
    execute(
        &mut vm,
        db_id,
        "CREATE TABLE items (value INTEGER)",
        empty_params(),
    )
    .expect("table creation should succeed");

    let active = sqlite::query(
        &mut vm,
        &[
            Value::Int(db_id),
            Value::string(
                "WITH RECURSIVE numbers(value) AS (\
                    SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 2000000\
                ) SELECT sum(value) FROM numbers",
            ),
            empty_params(),
            limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
        ],
    )
    .expect("active query should schedule");
    let HostCallResult::Pending(active_id) = active else {
        panic!("active query should be pending");
    };
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while sqlite::active_operation_id(&vm, db_id) != Some(active_id) {
        assert!(std::time::Instant::now() < wait_deadline);
        std::thread::yield_now();
    }

    let queued = sqlite::execute(
        &mut vm,
        &[
            Value::Int(db_id),
            Value::string("INSERT INTO items (value) VALUES (42)"),
            empty_params(),
        ],
    )
    .expect("queued insert should schedule");
    let HostCallResult::Pending(queued_id) = queued else {
        panic!("queued insert should be pending");
    };

    if close_all {
        sqlite::close_all(&mut vm);
    } else {
        sqlite::close(&mut vm, &[Value::Int(db_id)]).expect("close should succeed");
    }
    assert!(!sqlite::has_pending(&vm, active_id));
    assert!(!sqlite::has_pending(&vm, queued_id));

    let reopened = open_db(&mut vm, options);
    let result = query(
        &mut vm,
        reopened,
        "SELECT count(*) AS count FROM items",
        empty_params(),
        limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
    )
    .expect("count query should succeed");
    let Value::Array(rows) = field(&result, "rows") else {
        panic!("rows should be an array");
    };
    assert_eq!(rows[0], Value::array(vec![Value::Int(0)]));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_close_cancels_all_siblings_before_waiting_for_workers() {
    assert_sqlite_shutdown_cancels_all_siblings(false);
}

#[test]
fn sqlite_cancel_all_broadcasts_before_waiting_for_workers() {
    assert_sqlite_shutdown_cancels_all_siblings(true);
}

#[test]
fn sqlite_uses_typed_generation_checked_resource_handles() {
    let root = temporary_root("resource_handles");
    let mut vm = new_vm();
    let first = open_db(
        &mut vm,
        open_options(&root, "handles.db", "read_write_create", limits([])),
    );
    assert!(sqlite::is_sqlite_resource(first));

    sqlite::close(&mut vm, &[Value::Int(first)]).expect("first handle should close");
    let second = open_db(
        &mut vm,
        open_options(&root, "handles.db", "read_write_create", limits([])),
    );
    assert_ne!(
        first, second,
        "slot reuse must advance the handle generation"
    );

    let stale = sqlite::execute(
        &mut vm,
        &[Value::Int(first), Value::string("SELECT 1"), empty_params()],
    )
    .expect_err("a closed generation must stay invalid after slot reuse");
    assert!(stale.to_string().contains("unknown SQLite database"));

    let wrong_type = sqlite::insert_wrong_type_resource(&mut vm);
    let wrong_type_error = sqlite::execute(
        &mut vm,
        &[
            Value::Int(wrong_type),
            Value::string("SELECT 1"),
            empty_params(),
        ],
    )
    .expect_err("a handle from another resource type must be rejected");
    assert!(wrong_type_error.to_string().contains("wrong resource type"));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_pending_work_is_registered_with_the_shared_owner() {
    let root = temporary_root("operation_owner");
    let mut vm = new_vm();
    let db_id = open_db(
        &mut vm,
        open_options(&root, "operations.db", "read_write_create", limits([])),
    );

    let operation = sqlite::execute(
        &mut vm,
        &[
            Value::Int(db_id),
            Value::string("CREATE TABLE items(id INTEGER PRIMARY KEY)"),
            empty_params(),
        ],
    )
    .expect("execute should schedule");
    let HostCallResult::Pending(op_id) = operation else {
        panic!("execute should return a pending operation");
    };
    assert!(sqlite::has_sqlite_operation_owner(&vm, op_id));
    let _ = wait_pending(&mut vm, op_id).expect("shared operation should complete");
    assert!(!sqlite::has_pending(&vm, op_id));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}
