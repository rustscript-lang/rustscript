//! SQLite host tests.
//!
//! SQLite is exercised here as a *generic* host-SDK consumer: connections are
//! [`HostResource`]s pushed into the execution scope through its
//! `host_context()`, and every async activity is a generic [`HostOperation`]
//! associated with the connection resource handle. The mock `Vm` below
//! therefore exposes the same generic scope / module-state surface the
//! production `Vm` does, so the very same `src/builtins/runtime/sqlite.rs`
//! source (via `include!`) runs against the real generic SDK types.
//!
//! The suite preserves every historical SQLite scenario (round-trips,
//! transactions, policy/limits, read-only + SQL safety, truncation, pending
//! cancellation, sibling isolation, close/cancel-all, generational handles,
//! resource association) and adds generic-scope tests: policy persistence
//! across reset, connection lifecycle through the scope, and the typed
//! cancellation reason delivered on both connection close and scope reset.

extern crate vm as rustscript_vm;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{SystemTime, UNIX_EPOCH};

/// The generic host surface the included sqlite implementation is compiled
/// against. Data types and the generic resource/operation SDK come from the
/// real `vm` crate; the VM shell itself is mocked with a real
/// [`ExecutionScope`] plus a real typed module-state store.
pub mod vm {
    use std::any::{Any, TypeId};
    use std::collections::HashMap;

    pub use rustscript_vm::vm::execution_scope::{ExecutionScope, ScopeCloseOutcome, ScopeState};
    pub use rustscript_vm::vm::{
        CallOutcome, CallReturn, HostContextError, HostContextErrorKind, HostContextResult,
        HostModule, HostModuleState, HostOpId,
    };

    /// Mock registry: registration only compiles the included sqlite
    /// registration path against the mock `Vm`. Real binding/binding-absence
    /// behaviour is exercised through the production crate in the integration
    /// tests below.
    #[derive(Default)]
    pub struct HostFunctionRegistry;

    impl HostFunctionRegistry {
        pub fn register_exact_static(
            &mut self,
            _name: impl Into<String>,
            _arity: u8,
            _schema: rustscript_vm::bytecode::HostImportSchema,
            _function: fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>,
        ) -> VmResult<u16> {
            Ok(0)
        }

        pub fn authorize_registered_builtin_import(&mut self, _name: &str) {}

        /// Mock transaction surface: the included registration path compiles
        /// against this stub; every registration method is a no-op, so the
        /// staging closure can run against the mock directly.
        pub fn transactionally<F>(&mut self, stage: F) -> VmResult<()>
        where
            F: FnOnce(&mut HostFunctionRegistry) -> VmResult<()>,
        {
            stage(self)
        }
    }
    pub mod operation {
        pub use rustscript_vm::vm::operation::{
            HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationId,
            OperationOutcome, OperationResult, OperationSpec, OperationStatus,
        };
    }
    pub use rustscript_vm::vm::operation::{
        HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationId,
        OperationOutcome, OperationResult, OperationSpec, OperationStatus,
    };
    pub mod resource {
        pub use rustscript_vm::vm::resource::{
            CloseProgress, HostResource, Resource, ResourceCloseReason, ResourceError,
            ResourceHandle, ResourceRef, ResourceResult, ResourceTypeKey,
        };
    }
    pub use rustscript_vm::host_extension;
    pub use rustscript_vm::vm::resource::{
        CloseProgress, HostResource, Resource, ResourceCloseReason, ResourceError, ResourceHandle,
        ResourceRef, ResourceResult, ResourceTypeKey,
    };
    pub use rustscript_vm::{HostCallResult, OpCode, Program, Value, VmError, VmMap, VmResult};
    // Sqlite policy/limits types come from the included sqlite source (defined
    // in `builtins::runtime::sqlite`), mirroring the production crate root.
    pub use crate::builtins::runtime::sqlite::{SqliteLimits, SqlitePolicy};

    /// Mock host-extension surface bound to the mock `Vm` (the production
    /// `HostExtension` trait is bound to the real `Vm`, which the mock cannot
    /// satisfy).
    pub trait HostExtension: Send + Sync + 'static {
        fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
            let _ = registry;
            Ok(())
        }

        fn install(&self, vm: &mut Vm) {
            let _ = vm;
        }
    }

    /// Mock per-VM host runtime: a real execution scope plus a real typed
    /// module-state store (the only surfaces sqlite uses).
    pub(crate) type PendingOpResult = Box<dyn FnOnce(&mut Vm) -> VmResult<CallReturn> + Send>;

    pub(crate) struct TestHostRuntime {
        pub(crate) execution_scope: ExecutionScope,
        pub(crate) module_states: HashMap<TypeId, Box<dyn Any + Send>>,
    }

    impl TestHostRuntime {
        fn new() -> Self {
            Self {
                execution_scope: ExecutionScope::new().expect("scope"),
                module_states: HashMap::new(),
            }
        }

        fn set_module_state<M: HostModule>(&mut self, state: M) -> bool {
            self.module_states
                .insert(TypeId::of::<M>(), Box::new(state))
                .is_some()
        }

        fn take_module_state<M: HostModule>(&mut self) -> Option<M> {
            self.module_states
                .remove(&TypeId::of::<M>())?
                .downcast::<M>()
                .ok()
                .map(|value| *value)
        }

        fn get_module_state<M: HostModule>(&self) -> Option<&M> {
            self.module_states.get(&TypeId::of::<M>())?.downcast_ref()
        }

        fn get_module_state_mut<M: HostModule>(&mut self) -> Option<&mut M> {
            self.module_states
                .get_mut(&TypeId::of::<M>())?
                .downcast_mut()
        }

        pub(crate) fn register_pending_op_result(&mut self, _raw: u64, _provider: PendingOpResult) {
            // The mock surfaces the value directly through `take_pending_result`
            // (mirroring the production module side channel); it does not need
            // to store the adapter — this only keeps the union surface in sync.
        }
    }

    /// The mock `Vm` mirrors the production `Vm::host_context()` surface with
    /// exactly the methods the sqlite implementation uses.
    pub struct Vm {
        pub(crate) host: TestHostRuntime,
    }

    impl Vm {
        pub fn new(_program: Program) -> Self {
            Self {
                host: TestHostRuntime::new(),
            }
        }

        pub fn host_context(&mut self) -> TestHostContext<'_> {
            TestHostContext::new(&mut self.host)
        }
    }

    /// Generic boundary over the mock runtime, exposing the same surface the
    /// production [`HostContext`](rustscript_vm::vm::HostContext) does.
    pub struct TestHostContext<'a> {
        host: &'a mut TestHostRuntime,
    }

    impl<'a> TestHostContext<'a> {
        fn new(host: &'a mut TestHostRuntime) -> Self {
            Self { host }
        }

        fn from_scope<T>(result: rustscript_vm::VmResult<T>) -> HostContextResult<T> {
            result.map_err(|error| HostContextError::new("host::scope", error.to_string()))
        }

        fn from_resource<T>(result: ResourceResult<T>) -> HostContextResult<T> {
            result.map_err(|error| HostContextError::new("host::resource", error.to_string()))
        }

        pub fn set_module_state<M: HostModule>(&mut self, state: M) -> bool {
            self.host.set_module_state(state)
        }

        pub fn take_module_state<M: HostModule>(&mut self) -> Option<M> {
            self.host.take_module_state()
        }

        pub fn module_state<M: HostModule>(&self) -> Option<&M> {
            self.host.get_module_state()
        }

        pub fn module_state_mut<M: HostModule>(&mut self) -> Option<&mut M> {
            self.host.get_module_state_mut()
        }

        pub fn execution_scope(&self) -> &ExecutionScope {
            &self.host.execution_scope
        }

        pub fn push_resource_with_key<T: HostResource>(
            &mut self,
            value: T,
            key: ResourceTypeKey,
        ) -> HostContextResult<Resource<T>> {
            Self::from_scope(
                self.host
                    .execution_scope
                    .push_resource_with_key(value, key)
                    .map_err(|error| rustscript_vm::VmError::HostError(error.to_string())),
            )
        }

        pub fn start_operation(&mut self, spec: OperationSpec) -> HostContextResult<OperationId> {
            Self::from_scope(
                self.host
                    .execution_scope
                    .start_operation(spec)
                    .map_err(|error| rustscript_vm::VmError::HostError(error.to_string())),
            )
        }

        pub fn close_resource<T: HostResource>(
            &mut self,
            handle: ResourceHandle,
            reason: ResourceCloseReason,
        ) -> HostContextResult<CloseProgress> {
            Self::from_scope(
                self.host
                    .execution_scope
                    .close_resource::<T>(handle, reason)
                    .map_err(|error| rustscript_vm::VmError::HostError(error.to_string())),
            )
        }

        pub fn typed_resource<T: HostResource>(
            &self,
            handle: ResourceHandle,
        ) -> HostContextResult<Resource<T>> {
            Self::from_resource(self.host.execution_scope.resources().typed(handle))
        }

        pub fn resource<T: HostResource>(
            &self,
            token: &Resource<T>,
        ) -> HostContextResult<ResourceRef<'_, T>> {
            Self::from_resource(self.host.execution_scope.resources().get(token))
        }
    }
}

/// Mirrors the production `crate::host_api` path used by the included source.
pub mod host_api {
    pub use rustscript_vm::host_api::{
        HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
        HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
    };
}

pub mod builtins {
    pub use crate::vm::{
        CallOutcome, CallReturn, HostCallResult, Value, Vm, VmError, VmMap, VmResult,
    };

    pub mod runtime {
        pub use crate::vm::{HostCallResult, VmMap};

        pub use rustscript_vm::standard_host_catalog;

        pub use self::typed::borrow_arg;

        pub mod error {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/builtins/runtime/error.rs"
            ));
        }

        pub mod typed {
            pub type VmArrayRef<'a> = &'a [crate::vm::Value];
            pub type VmMapRef<'a> = &'a crate::vm::VmMap;

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
        }

        pub mod sqlite {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/builtins/runtime/sqlite.rs"
            ));
        }

        /// Test-side wrappers driving the included sqlite implementation
        /// through its generic scope surface.
        pub mod test_api {
            use super::sqlite;
            use crate::vm::{
                CallReturn, HostCallResult, HostOpId, OperationCancelReason, OperationId,
                OperationOutcome, ResourceCloseReason, ResourceHandle, Value, Vm, VmError, VmMap,
                VmResult,
            };
            use std::sync::Arc;
            use std::task::{Context, Poll, Wake, Waker};

            struct NoopWake;

            impl Wake for NoopWake {
                fn wake(self: Arc<Self>) {}
            }

            pub fn open(vm: &mut Vm, args: &[Value]) -> VmResult<i64> {
                sqlite::builtin_sqlite_open(vm, args)
            }

            pub fn execute(vm: &mut Vm, args: &[Value]) -> VmResult<HostCallResult<VmMap>> {
                sqlite::builtin_sqlite_execute(vm, args)
            }

            pub fn query(vm: &mut Vm, args: &[Value]) -> VmResult<HostCallResult<VmMap>> {
                sqlite::builtin_sqlite_query(vm, args)
            }

            pub fn transaction(
                vm: &mut Vm,
                args: &[Value],
            ) -> VmResult<HostCallResult<Vec<Value>>> {
                sqlite::builtin_sqlite_transaction(vm, args)
            }

            pub fn close(vm: &mut Vm, args: &[Value]) -> VmResult<()> {
                sqlite::builtin_sqlite_close(vm, args)
            }

            /// Polls one generic scope operation to terminal and returns the
            /// value the sqlite driver produced, mapping a cancelled
            /// operation back onto the same typed cancellation error the
            /// production runtime surfaces.
            pub fn poll(
                vm: &mut Vm,
                op_id: HostOpId,
                cx: &mut Context<'_>,
            ) -> Poll<VmResult<CallReturn>> {
                let Ok(id) = OperationId::from_raw(op_id) else {
                    return Poll::Ready(Err(VmError::HostError(
                        "invalid SQLite operation id".to_string(),
                    )));
                };
                // Capture the association before polling: a terminal poll
                // consumes the registry entry.
                let connection = vm
                    .host
                    .execution_scope
                    .operations()
                    .resource_of(id)
                    .ok()
                    .flatten()
                    .map(|handle| handle.raw() as i64);
                match vm.host.execution_scope.poll_operation(id, cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        Poll::Ready(Err(VmError::HostError(error.to_string())))
                    }
                    Poll::Ready(Ok(outcome)) => match outcome {
                        OperationOutcome::Cancelled(reason) => Poll::Ready(Err(
                            VmError::HostError(format!("SQLite operation cancelled ({reason})")),
                        )),
                        _ => {
                            let value = connection
                                .and_then(|raw| sqlite::take_pending_result(vm, op_id, raw))
                                .unwrap_or_else(|| {
                                    Err(VmError::HostError(
                                        "SQLite operation produced no result".to_string(),
                                    ))
                                });
                            Poll::Ready(value)
                        }
                    },
                }
            }

            pub fn cancel(vm: &mut Vm, op_id: HostOpId) {
                if let Ok(id) = OperationId::from_raw(op_id) {
                    let _ = vm
                        .host
                        .execution_scope
                        .cancel_operation(id, OperationCancelReason::Requested);
                }
            }

            /// Whether the connection identified by `resource_id` still has a
            /// live sqlite worker (the query actually entered execution).
            pub fn live_worker_count(vm: &mut Vm, resource_id: i64) -> usize {
                sqlite::live_worker_count(vm, resource_id)
            }

            pub fn has_pending(vm: &Vm, op_id: HostOpId) -> bool {
                OperationId::from_raw(op_id).is_ok_and(|id| {
                    vm.host
                        .execution_scope
                        .operations()
                        .status(id)
                        .is_ok_and(|status| status == crate::vm::OperationStatus::Pending)
                })
            }

            /// Drives the whole execution scope to quiescence (VmReset).
            pub fn close_all(vm: &mut Vm) {
                let _ = vm
                    .host
                    .execution_scope
                    .begin_close(ResourceCloseReason::VmReset);
                drive_quiescent(&mut vm.host.execution_scope);
            }

            /// Resets the execution scope (mimicking the production
            /// `Vm::reset_for_reuse`): drives the current scope to quiescence,
            /// then installs a fresh Active scope so the VM can run again.
            pub fn reset_all(vm: &mut Vm) {
                let _ = vm
                    .host
                    .execution_scope
                    .begin_close(ResourceCloseReason::VmReset);
                drive_quiescent(&mut vm.host.execution_scope);
                vm.host.execution_scope = crate::vm::ExecutionScope::new().expect("scope");
            }

            /// Whether the operation is registered in the scope and
            /// associated with the given connection handle.
            pub fn is_associated_with(vm: &Vm, op_id: HostOpId, connection: i64) -> bool {
                let Ok(id) = OperationId::from_raw(op_id) else {
                    return false;
                };
                let Ok(handle) = ResourceHandle::from_value(&Value::Int(connection)) else {
                    return false;
                };
                vm.host
                    .execution_scope
                    .operations()
                    .resource_of(id)
                    .ok()
                    .flatten()
                    == Some(handle)
            }

            /// Pushes a resource of a different concrete type into the scope,
            /// returning its raw handle (used to prove sqlite rejects it).
            pub fn insert_wrong_type_resource(vm: &mut Vm) -> i64 {
                let token = vm
                    .host
                    .execution_scope
                    .push_resource(TestNonSqliteResource)
                    .expect("test resource should be inserted");
                token.into_handle().raw() as i64
            }

            fn drive_quiescent(scope: &mut crate::vm::ExecutionScope) {
                let waker = Waker::from(Arc::new(NoopWake));
                let mut cx = Context::from_waker(&waker);
                loop {
                    match scope.poll_close(&mut cx) {
                        Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(2)),
                        Poll::Ready(result) => {
                            let _ = result.expect("scope close should succeed");
                            break;
                        }
                    }
                }
                assert!(
                    scope.is_quiescent(),
                    "mock scope must reach quiescence after close_all"
                );
            }

            struct TestNonSqliteResource;

            impl crate::vm::HostResource for TestNonSqliteResource {}
        }
    }
}

use builtins::runtime::sqlite::SqliteHostExt;
use builtins::runtime::test_api as sqlite;
use vm::{CallReturn, HostCallResult, OpCode, Program, Value, Vm, VmError};

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn new_vm() -> Vm {
    // `Vm` here is the sqlite test mock (`pub mod vm` in this file), a
    // test-only double that cannot allocate a production arena identity, so
    // its own infallible `new` is the correct constructor.
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
    let waker = std::sync::Arc::new(NoopWake).into();
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

/// Waits until the connection has a live worker (a query actually entered
/// SQLite execution), bounded by `deadline`.
fn wait_for_worker(vm: &mut Vm, db_id: i64, deadline: std::time::Instant) {
    while sqlite::live_worker_count(vm, db_id) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "sqlite query should enter execution"
        );
        std::thread::yield_now();
    }
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
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
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
    wait_for_worker(&mut vm, db_id, wait_deadline);

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

    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
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
    wait_for_worker(&mut vm, db_id, wait_deadline);

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
        sqlite::reset_all(&mut vm);
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
    assert!(
        wrong_type_error
            .to_string()
            .contains("unknown SQLite database")
    );

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_pending_work_is_associated_with_its_connection_resource() {
    let root = temporary_root("operation_association");
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
    assert!(sqlite::is_associated_with(&vm, op_id, db_id));
    let _ = wait_pending(&mut vm, op_id).expect("associated operation should complete");
    assert!(!sqlite::has_pending(&vm, op_id));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

/// Reconfiguring the SQLite policy must be pure configuration: it replaces
/// the persistent module-state policy without touching any live connection
/// resource or its in-flight operation (the production semantics that
/// replaced the removed legacy
/// `sqlite_reconfiguration_only_closes_sqlite_owned_state` behaviour).
///
/// A pending query started before the reconfiguration must keep running on
/// the *original* connection, never be cancelled with any close/reset
/// reason, and still complete normally; the replacement policy must be the
/// one in force afterwards. The shared open-connection accounting must also
/// survive the swap: while the original connection is still live, a new open
/// is bound by the replacement policy's tightened `max_connections`, and once
/// the original is closed a new open proceeds under the replacement policy.
#[test]
fn sqlite_reconfiguration_preserves_live_connection_and_its_pending_operation() {
    let root = temporary_root("reconfiguration_preserves_connection");
    let mut vm = new_vm();
    vm.configure_sqlite(vm::SqlitePolicy {
        database_root: Some(root.to_string_lossy().into_owned()),
        allow_unsafe_sql: true,
        // Deliberately wide: `PRAGMA` is unsafe SQL (used to prove the
        // replacement policy is active) and the recursive query below needs a
        // generous transaction window.
        limits: vm::SqliteLimits {
            max_connections: 4,
            max_transaction_ms: 10_000,
            max_result_bytes: 64 * 1024,
            ..vm::SqliteLimits::default()
        },
    });

    // Open one connection against the configured root (the counter is shared
    // with the persistent module state).
    let original_options = open_options(
        &root,
        "state.db",
        "read_write_create",
        limits([
            ("max_transaction_ms", 10_000),
            ("max_result_bytes", 64 * 1024),
        ]),
    );
    let db_id = sqlite::open(&mut vm, std::slice::from_ref(&original_options))
        .expect("SQLite open should succeed under the original policy");

    // A slow query guarantees a genuinely pending operation associated with
    // the original connection when the policy is replaced below.
    let pending = sqlite::query(
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
    .expect("long SQLite query should be scheduled");
    let HostCallResult::Pending(op_id) = pending else {
        panic!("long SQLite query should return a pending operation");
    };
    assert!(sqlite::has_pending(&vm, op_id));
    assert!(sqlite::is_associated_with(&vm, op_id, db_id));
    wait_for_worker(
        &mut vm,
        db_id,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    );

    // Replace the policy *while the query is mid-flight*. This is the
    // regression under test: configuration must not reach into the execution
    // scope (no resource close, no operation cancellation) — the old
    // owner/type-dispatch reconfiguration closed exactly the sqlite-owned
    // state, and the generic-scope replacement must not resurrect that.
    vm.configure_sqlite(vm::SqlitePolicy {
        database_root: Some(root.to_string_lossy().into_owned()),
        allow_unsafe_sql: true,
        limits: vm::SqliteLimits {
            max_connections: 1,
            max_transaction_ms: 10_000,
            max_result_bytes: 64 * 1024,
            ..vm::SqliteLimits::default()
        },
    });

    // The pending operation is untouched: still pending, still associated
    // with the original connection, and its completion is a normal success —
    // not a cancellation with any close/reset reason.
    assert!(sqlite::has_pending(&vm, op_id));
    assert!(sqlite::is_associated_with(&vm, op_id, db_id));
    let completed = wait_pending(&mut vm, op_id)
        .expect("the pending query must complete normally after reconfiguration");
    let completed = map_from_value(completed);
    let Value::Array(rows) = field(&completed, "rows") else {
        panic!("query rows should be an array");
    };
    assert_eq!(rows.len(), 1);
    let Value::Array(cells) = &rows[0] else {
        panic!("query row should be an array");
    };
    // sum(1..=2_000_000) = 2_000_001_000_000.
    assert_eq!(cells[0], Value::Int(2_000_001_000_000));
    assert!(!sqlite::has_pending(&vm, op_id));

    // The original connection is still live and usable after the swap.
    let result = query(
        &mut vm,
        db_id,
        "PRAGMA table_info(state_db)",
        empty_params(),
        limits([("max_rows", 8), ("max_result_bytes", 64 * 1024)]),
    )
    .expect("the original connection must remain usable after reconfiguration");
    assert_eq!(field(&result, "truncated"), &Value::Bool(false));

    // Accounting under the replacement policy: with the original connection
    // still live, a second open must be rejected — the shared counter (1)
    // meets the replacement `max_connections` (1) — proving the tightened
    // limit is active against the preserved accounting.
    let second_options = open_options(
        &root,
        "other.db",
        "read_write_create",
        limits([("max_result_bytes", 64 * 1024)]),
    );
    let error = sqlite::open(&mut vm, &[second_options])
        .expect_err("a second open must be rejected while the original connection is live");
    assert!(
        error.to_string().contains("connection limit"),
        "rejection must name the connection limit: {error}"
    );

    // Closing the original connection releases the accounting; a new open now
    // proceeds under the replacement policy.
    sqlite::close(&mut vm, &[Value::Int(db_id)]).expect("close should succeed");
    let db_id = sqlite::open(&mut vm, &[original_options])
        .expect("a new open must proceed once the original connection is closed");
    let result = query(
        &mut vm,
        db_id,
        "SELECT 42",
        empty_params(),
        limits([("max_rows", 1), ("max_result_bytes", 64 * 1024)]),
    )
    .expect("the reopened connection should be usable");
    let Value::Array(rows) = field(&result, "rows") else {
        panic!("query rows should be an array");
    };
    let Value::Array(cells) = &rows[0] else {
        panic!("query row should be an array");
    };
    assert_eq!(cells[0], Value::Int(42));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

// ---------------------------------------------------------------------------
// Generic scope lifecycle: policy persistence, connection cleanup, and the
// typed cancellation reason delivered uniformly on close and on reset.
// ---------------------------------------------------------------------------

/// A connection resource must be driven out of the scope by the generic close
/// machinery: after closing, the scope no longer holds it.
#[test]
fn sqlite_connection_is_closed_with_the_execution_scope() {
    let root = temporary_root("scope_close");
    let mut vm = new_vm();
    let db_id = open_db(
        &mut vm,
        open_options(&root, "state.db", "read_write_create", limits([])),
    );
    assert!(!vm.host.execution_scope.resources().is_empty());

    sqlite::close_all(&mut vm);

    assert!(
        vm.host.execution_scope.resources().is_empty(),
        "the generic scope close must reclaim the sqlite connection resource"
    );
    let error = sqlite::execute(
        &mut vm,
        &[Value::Int(db_id), Value::string("SELECT 1"), empty_params()],
    )
    .expect_err("a handle whose connection was closed with the scope must be rejected");
    assert!(error.to_string().contains("unknown SQLite database"));
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

/// The sqlite policy is persistent module state: it must survive a scope
/// reset (all resources/operations cleared) and even a reset that closed a
/// live connection.
#[test]
fn sqlite_policy_survives_scope_reset() {
    let root = temporary_root("policy_survives_reset");
    let mut vm = new_vm();
    vm.configure_sqlite(vm::SqlitePolicy {
        database_root: Some(root.to_string_lossy().into_owned()),
        allow_unsafe_sql: true,
        ..vm::SqlitePolicy::default()
    });

    // A live connection keeps the module state untouched but exercises a
    // worker before the reset.
    let options = open_options(
        &root,
        "state.db",
        "read_write_create",
        limits([("max_result_bytes", 64 * 1024)]),
    );
    let db_id =
        sqlite::open(&mut vm, std::slice::from_ref(&options)).expect("SQLite open should succeed");
    execute(
        &mut vm,
        db_id,
        "CREATE TABLE items (value INTEGER)",
        empty_params(),
    )
    .expect("table creation should succeed");

    // Reset through the generic scope: closes the connection and drains ops.
    sqlite::reset_all(&mut vm);

    // Policy still installed (persistent module state): opening again uses
    // the same configured database root and unsafe SQL remains allowed.
    let db_id = sqlite::open(&mut vm, &[options]).expect("SQLite open should succeed");
    let result = query(
        &mut vm,
        db_id,
        "PRAGMA table_info(items)",
        empty_params(),
        limits([("max_rows", 8), ("max_result_bytes", 64 * 1024)]),
    )
    .expect("unsafe SQL (PRAGMA) must still be allowed per the persisted policy");
    assert_eq!(field(&result, "truncated"), &Value::Bool(false));

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

/// A pending query delivered to the driver must observe the *same typed*
/// cancellation reason whether the connection was closed explicitly or the
/// whole scope was reset — both travel through the generic association logic,
/// never a SQLite-specific owner/poller dispatch.
#[test]
fn sqlite_query_gets_identical_typed_cancellation_on_close_and_reset() {
    for (explicit_close, expected) in [(true, "resource_closed"), (false, "vm_reset")] {
        let root = temporary_root("typed_cancel_reason");
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

        // A generic recording driver stands in for the sqlite query driver:
        // it is registered as an operation *associated with the connection
        // handle*, exactly like `sqlite::query` does, so the generic
        // association logic is what forwards the cancellation reason.
        let recorded: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let connection_handle =
            vm::ResourceHandle::from_value(&Value::Int(db_id)).expect("valid connection handle");
        let spec = vm::OperationSpec::new(RecordingDriver {
            recorded: Arc::clone(&recorded),
        })
        .with_resource(connection_handle);
        vm.host
            .execution_scope
            .start_operation(spec)
            .expect("recording operation should start");

        if explicit_close {
            sqlite::close(&mut vm, &[Value::Int(db_id)]).expect("close should succeed");
            assert_eq!(
                recorded.lock().expect("reason cell").as_deref(),
                Some(expected),
                "connection close must cancel associated operations with {expected}"
            );
        } else {
            sqlite::close_all(&mut vm);
            assert_eq!(
                recorded.lock().expect("reason cell").as_deref(),
                Some(expected),
                "scope reset must cancel associated operations with {expected}"
            );
        }

        fs::remove_dir_all(&root).expect("temporary SQLite root should be removed");
    }
}

#[test]
fn sqlite_pending_operation_is_woken_by_its_worker_completion() {
    let root = temporary_root("waker_regression");
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

    // A slow query guarantees the worker is still executing when the first
    // poll registers its waker, so the wake-under-test is deterministic.
    let pending = sqlite::query(
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
    .expect("query should be scheduled");
    let HostCallResult::Pending(op_id) = pending else {
        panic!("query should return a pending operation");
    };
    assert!(sqlite::has_pending(&vm, op_id));
    wait_for_worker(
        &mut vm,
        db_id,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
    );

    // First poll registers a *real* (counting) waker and must report Pending:
    // the worker is mid-query and has not published yet.
    let wake_state = Arc::new(WakeState::default());
    let waker = Waker::from(Arc::new(WakeOnDrop {
        state: Arc::clone(&wake_state),
    }));
    let mut cx = Context::from_waker(&waker);
    assert!(
        matches!(sqlite::poll(&mut vm, op_id, &mut cx), Poll::Pending),
        "an in-flight sqlite query must poll Pending"
    );

    // No busy-spin, no sleep: block until the operation's worker wakes the
    // registered waker (the notification under test).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut guard = wake_state
        .woken
        .lock()
        .expect("wake state mutex should not be poisoned");
    while !*guard {
        let now = std::time::Instant::now();
        assert!(
            now < deadline,
            "worker completion must wake a pending sqlite operation"
        );
        let (new_guard, _) = wake_state
            .condvar
            .wait_timeout(guard, deadline - now)
            .expect("wake condvar wait should not be poisoned");
        guard = new_guard;
    }

    // The wake must be followed by a Ready poll carrying the published value.
    match sqlite::poll(&mut vm, op_id, &mut cx) {
        Poll::Ready(Ok(CallReturn::One(Value::Map(result)))) => {
            let Value::Array(rows) = field(&result, "rows") else {
                panic!("query rows should be an array");
            };
            assert_eq!(rows.len(), 1);
            let Value::Array(cells) = &rows[0] else {
                panic!("query row should be an array");
            };
            // sum(1..=2_000_000) = 2_000_001_000_000.
            assert_eq!(cells[0], Value::Int(2_000_001_000_000));
        }
        Poll::Ready(Ok(other)) => panic!("query should return a map value, got {other:?}"),
        Poll::Ready(Err(error)) => panic!("query should complete successfully: {error}"),
        Poll::Pending => panic!("completed sqlite operation must poll Ready"),
    }

    sqlite::close_all(&mut vm);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

/// Counting waker: records that `wake` was invoked and notifies a condvar, so
/// the test can block on an actual wake instead of busy-spinning or sleeping.
struct WakeState {
    woken: std::sync::Mutex<bool>,
    condvar: std::sync::Condvar,
}

impl Default for WakeState {
    fn default() -> Self {
        Self {
            woken: std::sync::Mutex::new(false),
            condvar: std::sync::Condvar::new(),
        }
    }
}

struct WakeOnDrop {
    state: Arc<WakeState>,
}

impl Wake for WakeOnDrop {
    fn wake(self: Arc<Self>) {
        *self.state.woken.lock().expect("wake state mutex") = true;
        self.state.condvar.notify_one();
    }
}

struct RecordingDriver {
    recorded: Arc<std::sync::Mutex<Option<String>>>,
}

impl vm::HostOperation for RecordingDriver {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<vm::OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(&mut self, reason: vm::OperationCancelReason) -> vm::OperationResult<()> {
        *self.recorded.lock().expect("reason cell") = Some(reason.as_str().to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Production-crate integration: SQLite installed through the exact
// HostFunctionRegistry / HostExtension path on a *real* `Vm`. Registration,
// binding, capability gating, coexistence with another host module, and
// policy persistence across the real reset are exercised here (the mock
// harness above never touches the real registry).
// ---------------------------------------------------------------------------

mod production_crate {
    use super::temporary_root;
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};

    fn compile_with_catalog(source: &str) -> rustscript_vm::CompiledProgram {
        let catalog = rustscript_vm::standard_host_catalog();
        rustscript_vm::compile_source_with_flavor_and_options(
            source,
            rustscript_vm::SourceFlavor::RustScript,
            rustscript_vm::CompileSourceFileOptions::default()
                .with_host_api_catalog(Arc::clone(&catalog)),
        )
        .expect("sqlite source should compile against the standard catalog")
    }

    /// The standard combined catalog augmented with a custom marker function,
    /// so its fingerprint differs from the standard snapshot. Exact imports
    /// compiled against this catalog are carried under the custom fingerprint
    /// and are therefore *not* auto-bound by the fresh-VM default path (which
    /// only stages the standard snapshot). This lets the tests below assert
    /// that a caller-supplied catalog's imports require an explicit
    /// registration / extension to bind.
    fn compile_with_custom_catalog(source: &str) -> rustscript_vm::CompiledProgram {
        let standard = rustscript_vm::standard_host_catalog();
        let mut builder = rustscript_vm::HostApiBuilder::new();
        for resource in standard.resources() {
            builder.resource(resource.clone());
        }
        for function in standard.functions() {
            builder.function(function.clone());
        }
        builder.function(rustscript_vm::HostFunctionSchema::with_return(
            "custom::marker",
            Vec::new(),
            rustscript_vm::HostTypeSchema::Int,
        ));
        let custom = Arc::new(builder.build().expect("custom catalog must build"));
        assert_ne!(
            custom.fingerprint(),
            standard.fingerprint(),
            "custom catalog must have a distinct fingerprint"
        );
        rustscript_vm::compile_source_with_flavor_and_options(
            source,
            rustscript_vm::SourceFlavor::RustScript,
            rustscript_vm::CompileSourceFileOptions::default()
                .with_host_api_catalog(Arc::clone(&custom)),
        )
        .expect("sqlite source should compile against the custom catalog")
    }

    fn real_vm(program: rustscript_vm::Program) -> rustscript_vm::vm::Vm {
        rustscript_vm::vm::Vm::try_new(program).expect("test VM construction must not fail")
    }

    fn noop_waker() -> Waker {
        struct LocalNoop;
        impl Wake for LocalNoop {
            fn wake(self: Arc<Self>) {}
        }
        Waker::from(Arc::new(LocalNoop))
    }

    use rustscript_vm::{HostExtension, SqliteHostExt};

    #[test]
    fn sqlite_imports_are_not_bound_without_the_extension() {
        // Compiled against a custom (non-standard) catalog: the exact sqlite
        // imports carry the custom fingerprint, so the fresh-VM default path
        // (which only stages the standard snapshot) must not auto-bind them.
        // Without the extension, running must surface a structured binding
        // error naming the sqlite import.
        let compiled = compile_with_custom_catalog(
            "use sqlite;\n\
             let db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });\n\
             sqlite::close(&db);\n",
        );
        let mut vm = rustscript_vm::vm::Vm::try_new(compiled.program)
            .expect("test VM construction must not fail");
        vm.set_standard_composition(rustscript_vm::standard_composition());
        let error = vm
            .run()
            .expect_err("sqlite imports must not bind when the extension is absent");
        assert!(
            error.to_string().contains("sqlite"),
            "unbound sqlite import must surface a binding error naming the import: {error}"
        );
    }

    #[test]
    fn sqlite_extension_binds_exact_functions_and_runs_memory_open_close() {
        let compiled = compile_with_catalog(
            "use sqlite;\n\
             let db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });\n\
             sqlite::close(&db);\n",
        );
        let mut vm = rustscript_vm::vm::Vm::try_new(compiled.program)
            .expect("test VM construction must not fail");
        vm.install_extension(&rustscript_vm::SqliteExtension)
            .expect("sqlite extension should install exact functions + module state");
        assert_eq!(
            vm.run().expect("memory sqlite open/close should run"),
            rustscript_vm::vm::VmStatus::Halted
        );
    }

    #[test]
    fn sqlite_restricted_registry_requires_an_explicit_grant() {
        let compiled = compile_with_catalog(
            "use sqlite;\n\
             let db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });\n\
             sqlite::close(&db);\n",
        );
        // A restricted registry with the sqlite functions registered but no
        // grant: binding the VM must be rejected by the capability profile.
        let mut restricted = rustscript_vm::vm::HostFunctionRegistry::restricted();
        rustscript_vm::register_sqlite_builtin_module(&mut restricted)
            .expect("registration on a restricted registry must succeed");
        let mut vm = rustscript_vm::vm::Vm::try_new(compiled.program)
            .expect("test VM construction must not fail");
        let error = restricted
            .bind_vm_cached(&mut vm)
            .expect_err("ungranted sqlite import must be rejected");
        assert!(
            error.to_string().contains("capability"),
            "missing grant must surface the capability-profile rejection: {error}"
        );

        // Explicit grant binds and runs.
        let compiled_granted = compile_with_catalog(
            "use sqlite;\n\
             let db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });\n\
             sqlite::close(&db);\n",
        );
        let mut granted = rustscript_vm::vm::HostFunctionRegistry::restricted();
        rustscript_vm::register_sqlite_builtin_module(&mut granted)
            .expect("registration on a restricted registry must succeed");
        let profile = rustscript_vm::vm::CapabilityProfile::builder()
            .allow_host_import("sqlite::open")
            .allow_host_import("sqlite::close")
            .build();
        granted.set_capability_profile(profile);
        let mut vm = rustscript_vm::vm::Vm::try_new(compiled_granted.program)
            .expect("test VM construction must not fail");
        granted
            .bind_vm_cached(&mut vm)
            .expect("granted sqlite import must bind");
        assert_eq!(
            vm.run().expect("granted sqlite open/close should run"),
            rustscript_vm::vm::VmStatus::Halted
        );
    }

    /// A second, unrelated host module coexisting with sqlite in one registry
    /// (proving the core adds no dispatch branch — each exact import simply
    /// resolves against its declared name/schema).
    struct PingPolicy {
        max: u64,
    }

    struct PingExtension;

    impl rustscript_vm::HostExtension for PingExtension {
        fn register(
            &self,
            registry: &mut rustscript_vm::vm::HostFunctionRegistry,
        ) -> rustscript_vm::VmResult<()> {
            let mut builder = rustscript_vm::HostApiBuilder::new();
            builder.function(rustscript_vm::HostFunctionSchema::with_return(
                "acme::ping",
                Vec::new(),
                rustscript_vm::HostTypeSchema::Int,
            ));
            let catalog = Arc::new(builder.build().expect("ping catalog must build"));
            for schema in rustscript_vm::catalog_import_schemas(&catalog, "acme::ping") {
                registry.register_exact_static("acme::ping", 0, schema, |_vm, _args| {
                    Ok(rustscript_vm::vm::CallOutcome::Return(
                        rustscript_vm::vm::CallReturn::One(rustscript_vm::Value::Int(11)),
                    ))
                })?;
            }
            Ok(())
        }

        fn install(&self, vm: &mut rustscript_vm::vm::Vm) {
            vm.host_context().set_module_state(PingPolicy { max: 7 });
        }
    }

    #[test]
    fn sqlite_coexists_with_another_host_module_in_one_registry() {
        // sqlite plus the acme::ping module, both exact-schema registered.
        let compiled_sqlite = compile_with_catalog(
            "use sqlite;\n\
             let db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });\n\
             sqlite::close(&db);\n",
        );
        let mut registry = rustscript_vm::vm::HostFunctionRegistry::new();
        rustscript_vm::register_sqlite_builtin_module(&mut registry)
            .expect("sqlite registration should succeed");
        PingExtension
            .register(&mut registry)
            .expect("ping registration should succeed");

        let mut vm = rustscript_vm::vm::Vm::try_new(compiled_sqlite.program)
            .expect("test VM construction must not fail");
        registry
            .bind_vm_cached(&mut vm)
            .expect("sqlite + ping registry should bind");
        PingExtension.install(&mut vm);
        assert_eq!(
            vm.run().expect("sqlite + ping vm should run"),
            rustscript_vm::vm::VmStatus::Halted
        );

        // The fake module's state coexists with the sqlite module state in
        // the same typed store.
        assert_eq!(
            vm.host_context()
                .module_state::<PingPolicy>()
                .expect("ping policy")
                .max,
            7
        );
    }

    #[test]
    fn sqlite_policy_survives_the_real_vm_reset() {
        let root = temporary_root("real_policy_reset");
        // The script opens a real file under the configured root, so it only
        // succeeds while the database_root policy is installed.
        let source = format!(
            "use sqlite;\n\
             let db = sqlite::open({{ root: {:?}, path: \"state.db\", mode: \"read_write_create\", limits: {{}} }});\n\
             sqlite::close(&db);\n",
            root.to_string_lossy()
        );
        let compiled = compile_with_catalog(&source);
        let mut vm = real_vm(compiled.program);
        vm.install_extension(&rustscript_vm::SqliteExtension)
            .expect("sqlite extension should install");
        vm.configure_sqlite(rustscript_vm::SqlitePolicy {
            database_root: Some(root.to_string_lossy().into_owned()),
            ..rustscript_vm::SqlitePolicy::default()
        });

        // First run proves the policy-driven file open works.
        assert_eq!(
            vm.run().expect("first run"),
            rustscript_vm::vm::VmStatus::Halted
        );

        // Real reset: scope closed + recycled; the scripted run must still
        // succeed, i.e. the persistent SqlitePolicy survived the reset (the
        // module-state store is deliberately kept across invocation resets).
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        vm.begin_reset_for_reuse(
            rustscript_vm::vm::resource::ResourceCloseReason::VmReset,
            None,
        )
        .expect("begin reset");
        let mut stuck = 0u32;
        loop {
            match vm.poll_reset_for_reuse(&mut cx, std::time::Instant::now()) {
                std::task::Poll::Pending => {
                    stuck += 1;
                    assert!(stuck < 100_000, "real reset should drain promptly");
                    std::thread::yield_now();
                }
                std::task::Poll::Ready(result) => {
                    result.expect("reset should succeed without scope-cleanup errors");
                    break;
                }
            }
        }
        assert!(vm.is_reusable());

        assert_eq!(
            vm.run().expect("rerun after reset"),
            rustscript_vm::vm::VmStatus::Halted,
            "the persisted sqlite policy (database_root) must survive the real reset"
        );

        // Memory-only open would work regardless; prove the ROOT policy is
        // what persisted by checking the module state is still non-empty.
        assert!(
            !vm.host_context().is_module_state_empty(),
            "sqlite module state must survive the real reset"
        );
        fs::remove_dir_all(&root).expect("temporary SQLite root should be removed");
    }

    #[test]
    fn sqlite_async_query_executes_through_the_real_vm_pending_await() {
        // A *real* async round-trip through the production VM: the sqlite
        // extension's async host functions return execution-scope pending
        // operations, which the VM must await through the generic scope
        // registry and materialize the produced value back into the script.
        let compiled = compile_with_catalog(
            "use sqlite;\n\
             let db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });\n\
             let created = sqlite::execute(&db, \"CREATE TABLE t (a INTEGER)\", []);\n\
             let inserted = sqlite::execute(&db, \"INSERT INTO t VALUES (7)\", []);\n\
             let queried = sqlite::query(&db, \"SELECT a FROM t\", [], { max_rows: 100 });\n\
             sqlite::close(&db);\n\
             sqlite::rows_affected(inserted);\n",
        );
        let mut vm = rustscript_vm::vm::Vm::try_new(compiled.program)
            .expect("test VM construction must not fail");
        vm.install_extension(&rustscript_vm::SqliteExtension)
            .expect("sqlite extension should install");

        // Drive run/await until the VM halts: the pending host calls are
        // awaited via the generic execution-scope operation registry and the
        // awaited values are delivered back to the script.
        loop {
            match vm.run() {
                Ok(rustscript_vm::vm::VmStatus::Halted) => break,
                Ok(rustscript_vm::vm::VmStatus::Waiting(_)) => {
                    let waker = noop_waker();
                    let mut cx = std::task::Context::from_waker(&waker);
                    let mut stuck = 0u64;
                    loop {
                        match vm.poll_waiting_host_op(&mut cx) {
                            std::task::Poll::Ready(Ok(())) => break,
                            std::task::Poll::Ready(Err(error)) => {
                                panic!("sqlite async await failed: {error}")
                            }
                            std::task::Poll::Pending => {
                                stuck += 1;
                                assert!(stuck < 1_000_000, "sqlite async await stuck");
                                std::thread::yield_now();
                            }
                        }
                    }
                }
                Ok(other) => panic!("sqlite async run yielded unexpected status: {other:?}"),
                Err(error) => panic!("sqlite async run failed: {error}"),
            }
        }

        // The awaited `execute` envelope's `rows_affected` (the final
        // expression the script returned) must be 1: the produced value was
        // truly materialized back into the guest script.
        assert_eq!(
            vm.stack(),
            &[rustscript_vm::Value::Int(1)],
            "the awaited sqlite execute result must be delivered back to the script"
        );
    }

    use std::fs;
}
