//! External host-extension fixture crate.
//!
//! A standalone crate — a *separate* Cargo package with no crate-private
//! access to `pd-vm` — that consumes only the public host-extension SDK:
//!
//! - defines two resource classes (`Counter`, `Widget`) not present in any
//!   `pd-vm` enum or poller table, plus a pending operation and a `HostModule`
//!   policy state type;
//! - registers exact host functions through [`HostExtension::register`] using
//!   the catalog schema identity + fingerprint surfaced by the public
//!   [`vm::catalog_import_schemas`] adapter;
//! - installs persistent module state through [`HostExtension::install`];
//! - proves typed wrong-resource rejection, macro-generated absolute SDK paths
//!   (`crate = "vm"`), and reset-driven scope cleanup.
//!
//! It is compiled by `cargo check --manifest-path tests/fixtures/external-host-extension/Cargo.toml`
//! and its unit tests by `cargo test --manifest-path ...`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
#[cfg(test)]
use std::{collections::HashMap, sync::Mutex, task::{Wake, Waker}};

use pd_host_function::pd_host_function;
use vm::operation::{HostOperation, OperationSpec};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceResult, ResourceTypeKey,
};
#[cfg(test)]
use vm::{
    HostAsyncBridge, HostFuture, HostOpId,
};
use vm::{
    CallOutcome, CallReturn, CaptureAsyncHostContext, HostApiBuilder, HostApiCatalog,
    HostExtension, HostFunctionRegistry, HostFunctionSchema, HostFutureOutput, HostParamSchema,
    HostTypeSchema, ResourceTypeSchema, Value, Vm, VmError, VmResult,
};
#[cfg(test)]
use vm::{
    HostContextErrorKind, VmStatus, compile_source_with_flavor_and_options,
};

/// Number of counters / widgets whose `begin_close` has run in this process.
///
/// The core never names a concrete resource class; close counts are only
/// observable through these extension-owned counters.
pub static CLOSED_COUNTERS: AtomicUsize = AtomicUsize::new(0);
pub static CLOSED_WIDGETS: AtomicUsize = AtomicUsize::new(0);
/// Number of times a pending operation was cancelled (reset-driven).
pub static CANCELLED_OPS: AtomicUsize = AtomicUsize::new(0);
/// Async completion latch shared by `echo_async` and the async tests: the
/// generated wrapper parks a dynamic `HostOperation` until the test sets
/// `ASYNC_READY` (then the future completes and the script resumes).
static ASYNC_READY: AtomicBool = AtomicBool::new(false);

/// Serializes tests that observe or mutate the close/cancel trackers. The
/// test harness runs tests concurrently; without this, a close driven by one
/// test's `Drop` can race another test's counter assertions.
#[cfg(test)]
static TRACKER_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn reset_trackers() {
    CLOSED_COUNTERS.store(0, Ordering::SeqCst);
    CLOSED_WIDGETS.store(0, Ordering::SeqCst);
    CANCELLED_OPS.store(0, Ordering::SeqCst);
    ASYNC_READY.store(false, Ordering::SeqCst);
}

/// External resource class #1 — never enumerated by the core.
#[derive(Debug)]
pub struct Counter(u64);

impl HostResource for Counter {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("demo.counter").expect("valid key"))
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        CLOSED_COUNTERS.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

/// External resource class #2 — a distinct concrete type with its own key.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Widget(i64);

impl HostResource for Widget {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("demo.widget").expect("valid key"))
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        CLOSED_WIDGETS.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

/// Persistent per-VM module state: survives execution-scope reset and never
/// participates in resource close. Covered by `HostModule`'s blanket impl for
/// `Any + Send + 'static`.
#[derive(Clone, Debug)]
pub struct DemoPolicy {
    pub max_counters: u64,
}

/// A pending operation owned by the extension. The core drives it generically.
pub struct CounterOp;

impl HostOperation for CounterOp {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<vm::operation::OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(&mut self, _reason: vm::operation::OperationCancelReason) -> vm::operation::OperationResult<()> {
        CANCELLED_OPS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---- catalog + exact-schema identity --------------------------------------
//
// The exact schema (labels, type schemas, passing, catalog fingerprint) must
// match what the compiler embeds at the call site. Both sides derive it from
// the SAME public catalog, so registration can never drift.

pub fn demo_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        ResourceTypeKey::new("demo.counter").expect("valid key"),
        "counter resource",
    ));
    builder.resource(ResourceTypeSchema::new(
        ResourceTypeKey::new("demo.widget").expect("valid key"),
        "widget resource",
    ));
    builder.function(HostFunctionSchema::with_return(
        "demo::make_counter",
        vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "demo::make_widget",
        vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "demo::read_counter",
        vec![HostParamSchema::value("handle", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "demo::spawn_op",
        Vec::new(),
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "demo::echo_async",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("catalog must build"))
}

fn decode_handle(raw: i64) -> Result<vm::resource::ResourceHandle, VmError> {
    u64::try_from(raw)
        .ok()
        .and_then(|raw| vm::resource::ResourceHandle::from_raw(raw).ok())
        .ok_or_else(|| VmError::HostError(format!("invalid resource handle {raw}")))
}

fn host_error(error: vm::HostContextError) -> VmError {
    VmError::HostError(format!("{}: {}", error.namespace(), error.message()))
}

// ---- host functions (macro, external crate path) --------------------------
//
// `crate = "vm"` makes every generated path an absolute public-SDK path; no
// mirroring of pd-vm's internal module nesting and no copied wrappers.

#[pd_host_function(name = "demo::make_counter", crate = "vm")]
/// Creates a counter resource in the current scope, returning its raw handle.
fn make_counter(vm: &mut Vm, seed: i64) -> VmResult<CallOutcome> {
    let token = vm
        .host_context()
        .insert_resource(Counter(seed as u64))
        .map_err(host_error)?;
    Ok(CallOutcome::Return(CallReturn::One(
        token.handle().as_value(),
    )))
}

#[pd_host_function(name = "demo::make_widget", crate = "vm")]
/// Creates a widget resource in the current scope, returning its raw handle.
fn make_widget(vm: &mut Vm, seed: i64) -> VmResult<CallOutcome> {
    let token = vm
        .host_context()
        .insert_resource(Widget(seed))
        .map_err(host_error)?;
    Ok(CallOutcome::Return(CallReturn::One(
        token.handle().as_value(),
    )))
}

#[pd_host_function(name = "demo::read_counter", crate = "vm")]
/// Reads a counter resource through a typed borrow; wrong types are rejected.
fn read_counter(vm: &mut Vm, handle: i64) -> VmResult<CallOutcome> {
    let handle = decode_handle(handle)?;
    let value = {
        let context = vm.host_context();
        let counter = context.borrow_resource::<Counter>(handle).map_err(host_error)?;
        counter.0 as i64
    };
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(value))))
}

#[pd_host_function(name = "demo::spawn_op", crate = "vm")]
/// Starts an extension-owned pending operation in the current scope.
fn spawn_op(vm: &mut Vm) -> VmResult<CallOutcome> {
    let id = vm
        .host_context()
        .start_operation(OperationSpec::new(CounterOp))
        .map_err(host_error)?;
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(
        id.raw() as i64,
    ))))
}

/// A `#[pd_host_function]` whose resource parameter uses the generated typed
/// resource machinery (proving it compiles against absolute public SDK paths).
#[pd_host_function(name = "demo::peek_counter", crate = "vm")]
/// Peek a counter value through the generated typed resource parameter.
fn peek_counter(resource: vm::resource::ResourceRef<'_, Counter>) -> i64 {
    resource.0 as i64
}

/// Owned per-call state captured by the async adapter through the public
/// `CaptureAsyncHostContext` surface (`vm::CaptureAsyncHostContext`).
#[derive(Clone, Debug)]
pub struct EchoContext {
    prefix: i64,
}

impl CaptureAsyncHostContext for EchoContext {
    fn capture(_vm: &mut Vm) -> VmResult<Self> {
        Ok(EchoContext { prefix: 100 })
    }
}

/// Awaits the externally-driven async latch used to park `echo_async` until
/// the test releases it.
async fn await_async_signal() -> VmResult<()> {
    std::future::poll_fn(|_| {
        if ASYNC_READY.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await
}

/// External async host function: the generated wrapper captures owned context
/// and submits a dynamic `HostOperation` (through `vm::submit_host_future` →
/// `CallOutcome::Pending`) using only absolute public-SDK adapters
/// (`CaptureAsyncHostContext`, `HostFutureOutput`, `IntoHostCallOutcome`,
/// `return_one`), then completes with `prefix + value` once the test releases
/// the latch.
#[pd_host_function(name = "demo::echo_async", crate = "vm")]
async fn echo_async(
    #[pd_host_context] context: EchoContext,
    value: i64,
) -> VmResult<HostFutureOutput<i64>> {
    await_async_signal().await?;
    Ok(HostFutureOutput::returning(context.prefix + value))
}

// ---- async bridge (test driver) ------------------------------------------

/// A `HostAsyncBridge` that parks submitted futures and counts cancellations,
/// letting the tests drive a genuinely pending external async operation.
#[cfg(test)]
struct FixtureAsyncBridge {
    futures: Mutex<HashMap<HostOpId, HostFuture>>,
    cancelled: Arc<AtomicUsize>,
}

#[cfg(test)]
impl FixtureAsyncBridge {
    fn new(cancelled: Arc<AtomicUsize>) -> Self {
        Self {
            futures: Mutex::new(HashMap::new()),
            cancelled,
        }
    }
}

#[cfg(test)]
impl HostAsyncBridge for FixtureAsyncBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        if self
            .futures
            .lock()
            .expect("futures lock")
            .insert(op_id, future)
            .is_some()
        {
            return Err(VmError::HostError(format!("duplicate async op {op_id}")));
        }
        Ok(())
    }

    fn poll_op(&mut self, _op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        Poll::Pending
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let poll = {
            let mut guard = self.futures.lock().expect("futures lock");
            let future = match guard.get_mut(&op_id) {
                Some(future) => future,
                None => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "unknown async op {op_id}"
                    ))));
                }
            };
            future.as_mut().poll(cx)
        };
        if poll.is_ready() {
            self.futures.lock().expect("futures lock").remove(&op_id);
        }
        poll
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        self.futures.lock().expect("futures lock").remove(&op_id);
    }
}

#[cfg(test)]
struct NoopWake;

#[cfg(test)]
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

#[cfg(test)]
fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

// ---- extension ------------------------------------------------------------

fn register_exact(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    arity: u8,
    function: vm::StaticHostFunction,
) -> VmResult<()> {
    for schema in vm::catalog_import_schemas(catalog, name) {
        registry.register_exact_static(name, arity, schema, function)?;
    }
    Ok(())
}

/// External host extension: registers exact host functions and installs
/// persistent module state through the public [`HostExtension`] surface.
pub struct DemoExtension;

impl HostExtension for DemoExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        let catalog = demo_catalog();
        register_exact(registry, &catalog, "demo::make_counter", 1, make_counter)?;
        register_exact(registry, &catalog, "demo::make_widget", 1, make_widget)?;
        register_exact(registry, &catalog, "demo::read_counter", 1, read_counter)?;
        register_exact(registry, &catalog, "demo::spawn_op", 0, spawn_op)?;
        register_exact(registry, &catalog, "demo::echo_async", 1, echo_async)?;
        Ok(())
    }

    fn install(&self, vm: &mut Vm) {
        let mut context = vm.host_context();
        context.set_module_state(DemoPolicy { max_counters: 3 });
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
fn compiled(catalog: &HostApiCatalog, source: &str) -> vm::compiler::CompiledProgram {
    compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::new(catalog.clone())),
    )
    .expect("catalog source should compile")
}

#[cfg(test)]
fn installed_vm(catalog: &HostApiCatalog, source: &str) -> Vm {
    let compiled = compiled(catalog, source);
    let mut vm = Vm::new(compiled.program);
    vm.install_extension(&DemoExtension)
        .expect("extension should install");
    vm
}

#[test]
fn external_extension_registers_runs_and_returns_raw_handles() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let mut vm = installed_vm(
        &catalog,
        "use demo;\nlet a = demo::make_counter(7);\nlet b = demo::make_counter(9);\n\
         let r = demo::read_counter(a);\nlet s = demo::spawn_op();\n[r, s != 0];\n",
    );
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::array(vec![Value::Int(7), Value::Bool(true)])]
    );
    // Two counters created; scope close (on Vm drop) closes them.
    drop(vm);
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 2);
}

#[test]
fn typed_wrong_resource_rejection_is_structured() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let mut vm = installed_vm(
        &catalog,
        "use demo;\nlet c = demo::make_counter(1);\nlet w = demo::make_widget(2);\n\
         let bad = demo::read_counter(w);\n0;\n",
    );
    // Passing a Widget handle to a Counter-typed borrow must fail through the
    // registered external host function with the preserved structured
    // resource-layer namespace.
    let error = vm.run().expect_err("wrong-typed read must be rejected");
    let text = error.to_string();
    assert!(
        text.contains("host::resource"),
        "structured resource error namespace must survive the boundary: {text}"
    );
    drop(vm);

    // SDK-level typed recovery also rejects the wrong concrete type.
    let mut vm = installed_vm(
        &catalog,
        "use demo;\nlet c = demo::make_counter(1);\nc;\n",
    );
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let Value::Int(c_raw) = vm.stack()[0] else {
        panic!("expected an int handle on the stack");
    };
    let counter_handle = vm::resource::ResourceHandle::from_raw(c_raw as u64).expect("real handle");
    let error = vm
        .host_context()
        .typed_resource::<Widget>(counter_handle)
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Resource(resource)
            if resource.code() == vm::resource::ResourceErrorCode::ResourceTypeMismatch
    ));
}

#[test]
fn macro_typed_resource_parameter_uses_absolute_public_sdk_paths() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let mut vm = installed_vm(&catalog, "use demo;\nlet c = demo::make_counter(41);\nc;\n");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let Value::Int(raw) = vm.stack()[0] else {
        panic!("expected an int handle on the stack");
    };
    // Directly call the `#[pd_host_function]`-generated wrapper (same crate):
    // the generated resource-parameter adapter compiles and runs externally.
    let value = peek_counter(&mut vm, &[Value::Int(raw)]).expect("peek counter");
    assert_eq!(value, 41);
}

#[test]
fn reset_driven_scope_cleanup_closes_resources_and_cancels_operations() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let mut vm = installed_vm(
        &catalog,
        "use demo;\nlet c = demo::make_counter(1);\nlet w = demo::make_widget(2);\n\
         let _ = demo::spawn_op();\n0;\n",
    );
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.host_context().resource_count(), 2);
    assert_eq!(vm.host_context().operation_count(), 1);

    // Reset drives the scope to quiescence: resources close, op cancels.
    vm.reset_for_reuse();
    assert!(vm.is_reusable(), "clean reset leaves the VM reusable");
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 1);
    assert_eq!(CLOSED_WIDGETS.load(Ordering::SeqCst), 1);
    assert_eq!(CANCELLED_OPS.load(Ordering::SeqCst), 1);

    // The VM remains usable: a fresh run on the same installed extension works.
    assert_eq!(
        vm.run().expect("second run"),
        VmStatus::Halted,
        "the same installed extension must serve the next invocation"
    );
    assert_eq!(
        vm.host_context().resource_count(),
        2,
        "the second invocation re-creates its own scoped resources"
    );
    assert_eq!(
        CLOSED_COUNTERS.load(Ordering::SeqCst),
        1,
        "the second invocation's counters are still open"
    );
}

#[test]
fn module_state_survives_reset_and_never_participates_in_close() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let mut vm = installed_vm(&catalog, "use demo; 0;\n");
    assert_eq!(
        vm.host_context()
            .module_state::<DemoPolicy>()
            .map(|policy| policy.max_counters),
        Some(3)
    );
    vm.reset_for_reuse();
    assert!(
        vm.host_context().module_state::<DemoPolicy>().is_some(),
        "module state must survive reset"
    );
    // Module state is storage only — it never registers/participates in close.
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 0);
    assert_eq!(CLOSED_WIDGETS.load(Ordering::SeqCst), 0);
}

#[test]
fn external_async_function_parks_then_completes_a_dynamic_operation() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let cancelled = Arc::new(AtomicUsize::new(0));
    let mut vm = installed_vm(
        &catalog,
        "use demo;\nlet r = demo::echo_async(7);\nr;\n",
    );
    vm.set_async_bridge(Box::new(FixtureAsyncBridge::new(Arc::clone(&cancelled))));

    // The external async wrapper captures owned context and submits a dynamic
    // HostOperation; the script parks on it.
    let status = vm.run().expect("first run parks");
    let VmStatus::Waiting(op_id) = status else {
        panic!("expected a dynamic waiting op, got {status:?}");
    };
    assert_eq!(vm.waiting_host_op_id(), Some(op_id));

    // The operation stays genuinely pending until the latch is released.
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(
        matches!(vm.poll_waiting_host_op(&mut cx), Poll::Pending),
        "the external async operation must stay pending until released"
    );

    // Release the latch: the parked value (context.prefix + 7 = 107) is
    // delivered through the public SDK return path and the script resumes.
    ASYNC_READY.store(true, Ordering::SeqCst);
    assert!(
        matches!(vm.poll_waiting_host_op(&mut cx), Poll::Ready(Ok(()))),
        "the external async operation must complete once released"
    );
    assert_eq!(vm.resume().expect("resumed run halts"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(107)],
        "the script receives the value returned by the async host function"
    );
    assert_eq!(
        cancelled.load(Ordering::SeqCst),
        0,
        "completing normally must not cancel the dynamic operation"
    );
}

#[test]
fn external_async_function_cancels_on_reset_and_vm_stays_reusable() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let catalog = demo_catalog();
    let cancelled = Arc::new(AtomicUsize::new(0));
    let mut vm = installed_vm(
        &catalog,
        "use demo;\nlet _ = demo::echo_async(7);\n0;\n",
    );
    vm.set_async_bridge(Box::new(FixtureAsyncBridge::new(Arc::clone(&cancelled))));

    let status = vm.run().expect("run parks");
    let VmStatus::Waiting(op_id) = status else {
        panic!("expected a dynamic waiting op, got {status:?}");
    };
    assert_eq!(vm.waiting_host_op_id(), Some(op_id));

    // Reset drives the pending dynamic operation to cancellation.
    vm.reset_for_reuse();
    assert!(
        vm.is_reusable(),
        "cancelling a parked async op resets the VM to a reusable state"
    );
    assert_eq!(vm.waiting_host_op_id(), None);
    assert_eq!(
        cancelled.load(Ordering::SeqCst),
        1,
        "reset must cancel the bridge-owned pending operation exactly once"
    );

    // A fresh invocation on the same installed extension parks a new dynamic
    // operation and still completes normally once released.
    let status = vm.run().expect("second run parks again");
    assert!(
        matches!(status, VmStatus::Waiting(_)),
        "the reinstalled extension must submit a fresh dynamic operation"
    );
    ASYNC_READY.store(true, Ordering::SeqCst);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        vm.poll_waiting_host_op(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(vm.resume().expect("final resume"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(0)],
        "the second invocation completes normally after reset"
    );
}
