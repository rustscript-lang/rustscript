//! External host-extension fixture.
//!
//! A standalone crate that consumes only the **public** host-extension SDK of
//! `pd-vm` (crate name `vm`): the [`HostApiCatalog`] model, typed per-VM
//! module state through [`HostContext`], the [`HostExtension`] register /
//! install lifecycle, external [`HostResource`] insertion and typed borrows
//! through the generic host boundary, and external concrete
//! [`HostOperation`] drivers started into the VM's execution scope.
//!
//! It is deliberately **not** a member of the pd-vm workspace: this proves
//! the extension surface works from a genuinely separate crate with no
//! crate-private access.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vm::{
    CallOutcome, HostApiCatalog, HostContextError, HostContextErrorKind, HostExtension,
    HostFunctionRegistry, HostImportSchema, HostParamPassing, HostTypeSchema, ResourceTypeKey,
    ResourceTypeSchema, Value, Vm, VmError, VmResult, catalog_import_schemas, resource, return_one,
};

/// Number of times the external `Counter` resource was closed.
pub static CLOSED_COUNTERS: AtomicUsize = AtomicUsize::new(0);
/// Number of times the external `Widget` resource was closed.
pub static CLOSED_WIDGETS: AtomicUsize = AtomicUsize::new(0);

/// Serializes tracker-dependent tests (the close counters are process-global).
static TRACKER_LOCK: Mutex<()> = Mutex::new(());

fn reset_trackers() {
    CLOSED_COUNTERS.store(0, Ordering::SeqCst);
    CLOSED_WIDGETS.store(0, Ordering::SeqCst);
}

/// A typed external resource: closed through the generic poll-based close
/// contract, identified by a catalog `ResourceTypeKey`.
#[derive(Debug)]
pub struct Counter(pub u64);

impl resource::HostResource for Counter {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("demo.counter").expect("static key"))
    }

    fn begin_close(
        &mut self,
        _reason: resource::ResourceCloseReason,
    ) -> resource::ResourceResult<resource::CloseProgress> {
        CLOSED_COUNTERS.fetch_add(1, Ordering::SeqCst);
        Ok(resource::CloseProgress::Ready)
    }
}

/// A second typed external resource with its own key.
#[derive(Debug)]
pub struct Widget(pub i64);

impl resource::HostResource for Widget {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("demo.widget").expect("static key"))
    }

    fn begin_close(
        &mut self,
        _reason: resource::ResourceCloseReason,
    ) -> resource::ResourceResult<resource::CloseProgress> {
        CLOSED_WIDGETS.fetch_add(1, Ordering::SeqCst);
        Ok(resource::CloseProgress::Ready)
    }
}

/// Persistent per-VM module state: survives execution-scope reset and never
/// participates in resource close. Covered by `HostModule`'s blanket impl.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemoPolicy {
    pub max_counters: u64,
}

/// An external concrete [`HostOperation`] driver. Polling advances the
/// operation; cancellation records the reason and completes promptly.
#[derive(Debug)]
pub struct CounterOp {
    pub remaining: u64,
    pub cancelled: Arc<AtomicUsize>,
}

impl vm::operation::HostOperation for CounterOp {
    fn poll(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<vm::operation::OperationResult<()>> {
        if self.remaining == 0 {
            std::task::Poll::Ready(Ok(()))
        } else {
            self.remaining -= 1;
            std::task::Poll::Pending
        }
    }

    fn cancel(
        &mut self,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        self.remaining = 0;
        Ok(())
    }
}

// ---- catalog ---------------------------------------------------------------

/// The external extension's catalog: one resource key per concrete type and
/// one declared function per registered host callable.
pub fn demo_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiCatalog::builder();
    builder.resource(ResourceTypeSchema::new(
        ResourceTypeKey::new("demo.counter").expect("key"),
        "An external counter resource",
    ));
    builder.resource(ResourceTypeSchema::new(
        ResourceTypeKey::new("demo.widget").expect("key"),
        "An external widget resource",
    ));
    builder.function(vm::HostFunctionSchema::new(
        "demo::make_counter",
        vec![vm::HostParamSchema::value("seed", HostTypeSchema::Int)],
    ));
    builder.function(vm::HostFunctionSchema::new(
        "demo::make_widget",
        vec![vm::HostParamSchema::value("seed", HostTypeSchema::Int)],
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::read_counter",
        vec![vm::HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(ResourceTypeKey::new("demo.counter").expect("key")),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(vm::HostFunctionSchema::new("demo::spawn_op", vec![]));
    Arc::new(builder.build().expect("catalog must build"))
}

fn decode_handle(raw: i64) -> Result<resource::ResourceHandle, VmError> {
    resource::ResourceHandle::from_raw(raw as u64)
        .map_err(|error| VmError::HostError(error.to_string()))
}

fn host_error(error: HostContextError) -> VmError {
    VmError::HostError(error.to_string())
}

/// External host function: inserts a `Counter` into the VM's execution scope
/// and returns its raw handle to the guest.
fn make_counter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let seed = match args.first() {
        Some(Value::Int(seed)) => *seed,
        _ => return Err(VmError::TypeMismatch("int seed")),
    };
    let token = vm
        .host_context()
        .push_resource(Counter(seed as u64))
        .map_err(host_error)?;
    Ok(CallOutcome::Return(return_one(token.handle().raw() as i64)))
}

/// External host function: inserts a `Widget` into the scope.
fn make_widget(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let seed = match args.first() {
        Some(Value::Int(seed)) => *seed,
        _ => return Err(VmError::TypeMismatch("int seed")),
    };
    let token = vm
        .host_context()
        .push_resource(Widget(seed))
        .map_err(host_error)?;
    Ok(CallOutcome::Return(return_one(token.handle().raw() as i64)))
}

/// External host function: borrows a `Counter` through the typed host
/// boundary and reads its value.
fn read_counter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let raw = match args.first() {
        Some(Value::Int(raw)) => *raw,
        _ => return Err(VmError::TypeMismatch("int handle")),
    };
    let decoded = decode_handle(raw)?;
    let value = vm
        .host_context()
        .borrow_resource::<Counter>(decoded)
        .map_err(host_error)?
        .0;
    Ok(CallOutcome::Return(return_one(value as i64)))
}

/// External host function: starts a concrete [`HostOperation`] driver in the
/// VM's execution scope and returns a non-zero id.
fn spawn_op(vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    let cancelled = Arc::new(AtomicUsize::new(0));
    let spec = vm::operation::OperationSpec::new(CounterOp {
        remaining: 2,
        cancelled: Arc::clone(&cancelled),
    });
    let id = vm
        .host_context()
        .start_operation(spec)
        .map_err(host_error)?;
    Ok(CallOutcome::Return(return_one(id.raw() as i64)))
}

fn register_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    arity: u8,
    function: vm::StaticHostFunction,
) -> VmResult<()> {
    // Validate the declaration against the catalog before registering.
    let schemas: Vec<HostImportSchema> = catalog_import_schemas(catalog, name);
    if schemas.is_empty() {
        return Err(VmError::HostError(format!(
            "catalog declares no function '{name}'"
        )));
    }
    let _ = schemas;
    registry.register_static(name, arity, function);
    Ok(())
}

/// External host extension: registers host functions and installs persistent
/// per-VM module state through the public [`HostExtension`] surface.
pub struct DemoExtension;

impl HostExtension for DemoExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        let catalog = demo_catalog();
        register_from_catalog(registry, &catalog, "demo::make_counter", 1, make_counter)?;
        register_from_catalog(registry, &catalog, "demo::make_widget", 1, make_widget)?;
        register_from_catalog(registry, &catalog, "demo::read_counter", 1, read_counter)?;
        register_from_catalog(registry, &catalog, "demo::spawn_op", 0, spawn_op)?;
        Ok(())
    }

    fn install(&self, vm: &mut Vm) {
        let mut context = vm.host_context();
        context.set_module_state(DemoPolicy { max_counters: 3 });
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
fn installed_vm() -> Vm {
    let program = vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    vm.install_extension(&DemoExtension)
        .expect("extension should install");
    vm
}

#[test]
fn extension_installs_module_state_and_registers_host_functions() {
    let mut vm = installed_vm();
    // The infallible install phase registered typed per-VM module state.
    assert_eq!(
        vm.host_context()
            .module_state::<DemoPolicy>()
            .map(|policy| policy.max_counters),
        Some(3)
    );
    // The catalog-derived registration surface is exercised by register().
    assert!(vm.host_context().is_scope_active());
    drop(vm);
}

#[test]
fn external_resource_insert_borrow_and_close_through_scope() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    let counter = {
        let token = vm
            .host_context()
            .push_resource(Counter(7))
            .expect("insert counter");
        token
    };
    assert_eq!(vm.host_context().resource_count(), 1);

    // Typed borrow reads the value through the SDK (borrow is call-scoped).
    {
        let context = vm.host_context();
        let borrowed = context
            .borrow_resource::<Counter>(counter.handle())
            .expect("borrow counter");
        assert_eq!(borrowed.0, 7);
    }

    // Mut borrow writes through the SDK.
    {
        let mut context = vm.host_context();
        let mut borrowed = context
            .borrow_resource_mut::<Counter>(counter.handle())
            .expect("mut borrow counter");
        borrowed.0 = 9;
    }
    {
        let context = vm.host_context();
        let borrowed = context
            .borrow_resource::<Counter>(counter.handle())
            .expect("re-borrow counter");
        assert_eq!(borrowed.0, 9);
    }

    // Vm drop closes the resource through the scope close sweep.
    drop(vm);
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 1);
}

#[test]
fn typed_wrong_resource_rejection_is_structured() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    let token = vm
        .host_context()
        .push_resource(Widget(2))
        .expect("insert widget");

    // Wrong concrete type is rejected with a structured resource error.
    let error = vm
        .host_context()
        .borrow_resource::<Counter>(token.handle())
        .unwrap_err();
    assert_eq!(error.namespace(), "host::resource");
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Resource(resource_error)
            if resource_error.code() == resource::ResourceErrorCode::ResourceTypeMismatch
    ));
    drop(vm);
}

#[test]
fn reset_driven_scope_cleanup_closes_resources_and_cancels_operations() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    vm.host_context()
        .push_resource(Counter(1))
        .expect("counter");
    vm.host_context().push_resource(Widget(2)).expect("widget");
    let cancelled = Arc::new(AtomicUsize::new(0));
    let spec = vm::operation::OperationSpec::new(CounterOp {
        remaining: 200,
        cancelled: Arc::clone(&cancelled),
    });
    vm.host_context().start_operation(spec).expect("op start");
    assert_eq!(vm.host_context().resource_count(), 2);
    assert_eq!(vm.host_context().operation_count(), 1);

    // Reset drives the scope to quiescence: resources close, op cancels.
    vm.reset_for_reuse();
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 1);
    assert_eq!(CLOSED_WIDGETS.load(Ordering::SeqCst), 1);
    assert!(
        cancelled.load(Ordering::SeqCst) > 0,
        "the pending operation driver must be cancelled by the scope close"
    );
}

#[test]
fn module_state_survives_reset_and_never_participates_in_close() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    vm.reset_for_reuse();
    assert!(
        vm.host_context().module_state::<DemoPolicy>().is_some(),
        "module state must survive reset"
    );
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 0);
    assert_eq!(CLOSED_WIDGETS.load(Ordering::SeqCst), 0);
    drop(vm);
}

#[test]
fn catalog_validates_declarations_and_fingerprint() {
    let catalog = demo_catalog();
    assert!(catalog.has_resource(&ResourceTypeKey::new("demo.counter").expect("key")));
    assert!(catalog.has_resource(&ResourceTypeKey::new("demo.widget").expect("key")));
    assert!(!catalog.functions_named("demo::make_counter").is_empty());
    // Fingerprint is deterministic.
    assert_eq!(catalog.fingerprint(), catalog.fingerprint());
}
