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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};

use pd_host_function::pd_host_function;
use vm::operation::{HostOperation, OperationSpec};
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult, ResourceTypeKey};
use vm::{
    CallOutcome, CallReturn, HostApiBuilder, HostApiCatalog, HostExtension, HostFunctionRegistry,
    HostFunctionSchema, HostParamSchema, HostTypeSchema, ResourceTypeSchema, Value, Vm, VmError,
    VmResult,
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

/// Serializes tests that observe or mutate the close/cancel trackers. The
/// test harness runs tests concurrently; without this, a close driven by one
/// test's `Drop` can race another test's counter assertions.
static TRACKER_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn reset_trackers() {
    CLOSED_COUNTERS.store(0, Ordering::SeqCst);
    CLOSED_WIDGETS.store(0, Ordering::SeqCst);
    CANCELLED_OPS.store(0, Ordering::SeqCst);
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
        Ok(())
    }

    fn install(&self, vm: &mut Vm) -> VmResult<()> {
        let mut context = vm.host_context();
        context.set_module_state(DemoPolicy { max_counters: 3 });
        Ok(())
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
    reset_trackers();
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
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
    reset_trackers();
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
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
    reset_trackers();
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
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
    reset_trackers();
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
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
    reset_trackers();
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
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
