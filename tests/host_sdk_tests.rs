//! Dedicated tests for the external host-extension SDK surface restored into
//! PR18: the host-API catalog model, the generic host-context boundary, and
//! the host-extension register/install lifecycle — all exercised through the
//! public crate API (the same surface an external host crate consumes).

use vm::{
    CallOutcome, HostApiBuilder, HostApiCatalog, HostContextErrorKind, HostExtension,
    HostFunctionRegistry, HostParamPassing, HostTypeSchema, Program, ResourceTypeKey,
    ResourceTypeSchema, Value, Vm, VmError, VmResult, catalog_import_schemas, operation, resource,
};

fn counter_key() -> ResourceTypeKey {
    ResourceTypeKey::new("demo.counter").expect("static key")
}

fn catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(counter_key(), "A counter"));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::make",
        vec![vm::HostParamSchema::value("seed", HostTypeSchema::Int)],
        HostTypeSchema::Resource(counter_key()),
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::read",
        vec![vm::HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.build().expect("catalog must build")
}

#[test]
fn catalog_import_schemas_carries_fingerprint_and_passing() {
    let catalog = catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::read");
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].fingerprint, catalog.fingerprint());
    assert_eq!(schemas[0].params.len(), 1);
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::Borrow);
}

#[test]
fn catalog_fingerprint_is_stable_and_semantic() {
    let a = catalog();
    let mut b = HostApiBuilder::new();
    b.resource(ResourceTypeSchema::new(counter_key(), "Different docs"));
    b.function(vm::HostFunctionSchema::with_return(
        "demo::make",
        vec![vm::HostParamSchema::value("seed", HostTypeSchema::Int)],
        HostTypeSchema::Resource(counter_key()),
    ));
    b.function(vm::HostFunctionSchema::with_return(
        "demo::read",
        vec![vm::HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    let b = b.build().expect("catalog must build");
    // Documentation is excluded from the fingerprint; semantic fields match.
    assert_eq!(a.fingerprint(), b.fingerprint());
}

#[derive(Debug)]
struct Counter(u64);

impl resource::HostResource for Counter {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(counter_key())
    }

    fn begin_close(
        &mut self,
        _reason: resource::ResourceCloseReason,
    ) -> resource::ResourceResult<resource::CloseProgress> {
        Ok(resource::CloseProgress::Ready)
    }
}

#[derive(Debug)]
struct TickingOp;

impl operation::HostOperation for TickingOp {
    fn poll(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<operation::OperationResult<()>> {
        std::task::Poll::Pending
    }

    fn cancel(
        &mut self,
        _reason: operation::OperationCancelReason,
    ) -> operation::OperationResult<()> {
        Ok(())
    }
}

struct DemoExtension;

impl HostExtension for DemoExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        let catalog = catalog();
        let schemas = catalog_import_schemas(&catalog, "demo::make");
        assert!(!schemas.is_empty());
        registry.register_static("demo::make", 1, make_counter as vm::StaticHostFunction);
        registry.register_static("demo::read", 1, read_counter as vm::StaticHostFunction);
        Ok(())
    }

    fn install(&self, vm: &mut Vm) {
        vm.host_context().set_module_state("installed");
    }
}

fn make_counter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let seed = match args.first() {
        Some(Value::Int(seed)) => *seed,
        _ => return Err(VmError::TypeMismatch("int")),
    };
    let token = vm
        .host_context()
        .push_resource(Counter(seed as u64))
        .map_err(|error| VmError::HostError(error.to_string()))?;
    Ok(CallOutcome::Return(vm::return_one(
        token.handle().raw() as i64
    )))
}

fn read_counter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
    let raw = match args.first() {
        Some(Value::Int(raw)) => *raw,
        _ => return Err(VmError::TypeMismatch("int")),
    };
    let handle = resource::ResourceHandle::from_raw(raw as u64)
        .map_err(|error| VmError::HostError(error.to_string()))?;
    let value = vm
        .host_context()
        .borrow_resource::<Counter>(handle)
        .map_err(|error| VmError::HostError(error.to_string()))?
        .0;
    Ok(CallOutcome::Return(vm::return_one(value as i64)))
}

#[test]
fn extension_register_and_install_are_transactional() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    vm.install_extension(&DemoExtension)
        .expect("extension should install");
    assert_eq!(vm.host_context().module_state::<&str>(), Some(&"installed"));
}

#[test]
fn host_context_inserts_resources_and_starts_operations() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    {
        let token = vm
            .host_context()
            .push_resource(Counter(42))
            .expect("push counter");
        let value = vm
            .host_context()
            .borrow_resource::<Counter>(token.handle())
            .expect("borrow counter")
            .0;
        assert_eq!(value, 42);
    }
    let id = vm
        .host_context()
        .start_operation(operation::OperationSpec::new(TickingOp))
        .expect("start operation");
    assert_eq!(vm.host_context().operation_count(), 1);
    assert_eq!(
        vm.host_context().operation_status(id).expect("status"),
        operation::OperationStatus::Pending
    );
}

#[test]
fn closing_scope_rejects_new_inserts_with_structured_error() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    // Drive the public execution scope into Closing (new inserts sealed).
    vm.execution_scope()
        .begin_close(resource::ResourceCloseReason::Requested)
        .expect("begin close");
    let error = vm
        .host_context()
        .push_resource(Counter(1))
        .expect_err("closing scope must reject inserts");
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Scope(scope_error)
            if matches!(
                scope_error,
                vm::execution_scope::ExecutionScopeError::ScopeClosing
            )
    ));
}

#[test]
fn external_operation_driver_cancels_on_scope_close() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let driver = TickingOpWithFlag(cancelled.clone());
    vm.host_context()
        .start_operation(operation::OperationSpec::new(driver))
        .expect("start");
    drop(vm);
    assert_eq!(cancelled.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct TickingOpWithFlag(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl operation::HostOperation for TickingOpWithFlag {
    fn poll(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<operation::OperationResult<()>> {
        std::task::Poll::Pending
    }

    fn cancel(
        &mut self,
        _reason: operation::OperationCancelReason,
    ) -> operation::OperationResult<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
