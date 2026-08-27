//! Dedicated tests for the external host-extension SDK surface restored into
//! PR18: the host-API catalog model, the generic host-context boundary, and
//! the host-extension register/install lifecycle — all exercised through the
//! public crate API (the same surface an external host crate consumes).

use std::sync::Arc;

use vm::{
    BytecodeBuilder, CallOutcome, CallableKind, CallablePrototype, CallableTarget, HostApiBuilder,
    HostApiCatalog, HostContextErrorKind, HostExtension, HostFunction, HostFunctionRegistry,
    HostFunctionSchema, HostImport, HostImportSchema, HostParamPassing, HostTypeSchema, Program,
    ResourceTypeKey, ResourceTypeSchema, Value, ValueType, Vm, VmError, VmResult,
    catalog_import_schemas, operation, register_catalog_static_function, resource,
};

struct ReturnValue {
    value: Value,
}

impl HostFunction for ReturnValue {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::CallReturn::one(self.value.clone())))
    }
}

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

fn overloaded_int(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    Ok(CallOutcome::Return(vm::return_one(11_i64)))
}

fn overloaded_string(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    Ok(CallOutcome::Return(vm::return_one("string overload")))
}

#[test]
fn catalog_binding_keeps_same_name_overloads_by_full_schema() {
    let mut builder = HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::overloaded",
        vec![vm::HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::overloaded",
        vec![vm::HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::String,
    ));
    let catalog = builder.build().expect("overload catalog");
    let schemas = catalog_import_schemas(&catalog, "demo::overloaded");
    assert_eq!(schemas.len(), 2);

    let mut registry = HostFunctionRegistry::empty();
    register_catalog_static_function(
        &mut registry,
        &catalog,
        "demo::overloaded",
        schemas[0].clone(),
        overloaded_int,
    )
    .expect("integer overload registration");
    register_catalog_static_function(
        &mut registry,
        &catalog,
        "demo::overloaded",
        schemas[1].clone(),
        overloaded_string,
    )
    .expect("string overload registration");

    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ldc(1);
    bytecode.call(1, 1);
    bytecode.ret();
    let imports = vec![
        HostImport {
            name: "demo::overloaded".to_string(),
            arity: 1,
            return_type: ValueType::Int,
        },
        HostImport {
            name: "demo::overloaded".to_string(),
            arity: 1,
            return_type: ValueType::String,
        },
    ];
    let program = Program::with_imports_and_debug(
        vec![Value::Int(1), Value::string("x")],
        bytecode.finish(),
        imports,
        None,
    )
    .with_host_import_schemas(vec![schemas[0].clone(), schemas[1].clone()])
    .expect("schema metadata should align with imports");
    let encoded = vm::encode_program(&program).expect("overload VMBC should encode");
    let decoded = vm::decode_program(&encoded).expect("overload VMBC should decode");
    assert_eq!(decoded.host_import_schemas(), program.host_import_schemas());
    let mut vm = Vm::new(decoded);
    registry
        .bind_vm_cached(&mut vm)
        .expect("full schemas should resolve both overloads");

    assert_eq!(
        vm.run().expect("overloads should execute"),
        vm::VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(11), Value::string("string overload")]
    );

    let untyped_program = Program::with_imports_and_debug(
        Vec::new(),
        vec![vm::OpCode::Ret as u8],
        vec![HostImport {
            name: "demo::overloaded".to_string(),
            arity: 1,
            return_type: ValueType::Int,
        }],
        None,
    );
    let mut untyped_vm = Vm::new(untyped_program);
    let error = registry
        .bind_vm_cached(&mut untyped_vm)
        .expect_err("an overloaded import without full identity must be rejected");
    assert!(error.to_string().contains("full schema and fingerprint"));
}

struct WrongDynamicReturn;

impl vm::HostFunction for WrongDynamicReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::return_one(true)))
    }
}

#[test]
fn dynamic_host_return_is_checked_against_import_type() {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: "demo::wrong_return".to_string(),
            arity: 0,
            return_type: ValueType::Int,
        }],
        None,
    );
    let mut vm = Vm::new(program);
    let mut registry = HostFunctionRegistry::empty();
    registry.register("demo::wrong_return", 0, || Box::new(WrongDynamicReturn));
    registry
        .bind_vm_cached(&mut vm)
        .expect("dynamic host function should bind");
    assert!(matches!(vm.run(), Err(VmError::TypeMismatch("int"))));
}

struct WrongArgsReturn;

#[test]
fn callable_host_returns_validate_nested_authoritative_prototype_schema() {
    let expected_callable = HostTypeSchema::Callable {
        params: vec![HostTypeSchema::Int],
        result: Box::new(HostTypeSchema::Bool),
    };
    let expected_return = HostTypeSchema::Optional(Box::new(HostTypeSchema::Map(Box::new(
        HostTypeSchema::Array(Box::new(expected_callable)),
    ))));
    let function =
        HostFunctionSchema::with_return("demo::nested_callable", Vec::new(), expected_return);
    let mut builder = HostApiBuilder::new();
    builder.function(function.clone());
    let catalog = builder.build().expect("callable catalog");
    let schema = HostImportSchema::from_function(&catalog, &function);

    let actual_callable_schema = vm::compiler::TypeSchema::Callable {
        params: vec![vm::compiler::TypeSchema::String],
        result: Box::new(vm::compiler::TypeSchema::Int),
    };
    let callable = Value::Callable(Arc::new(vm::CallableValue {
        prototype_id: 0,
        kind: CallableKind::FunctionItem,
        env: None,
    }));
    let returned = Value::map(vec![(Value::string("items"), Value::array(vec![callable]))]);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: schema.name.clone(),
            arity: 0,
            return_type: ValueType::Map,
        }],
        None,
    )
    .with_host_import_schemas(vec![schema.clone()])
    .expect("schema metadata")
    .with_callable_metadata(
        Vec::new(),
        vec![CallablePrototype {
            kind: CallableKind::FunctionItem,
            target: CallableTarget::ScriptFunction(0),
            arity: 1,
            frame_local_count: 1,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: Some(actual_callable_schema),
        }],
        Vec::new(),
        Vec::new(),
    );
    let mut vm = Vm::new(program);
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog(schema, move || {
            Box::new(ReturnValue {
                value: returned.clone(),
            })
        })
        .expect("callable host registration");
    registry
        .bind_vm_cached(&mut vm)
        .expect("callable host binding");

    let error = vm
        .run()
        .expect_err("nested callable schema mismatch must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("callable")),
        "{error:?}"
    );
}

impl vm::HostArgsFunction for WrongArgsReturn {
    fn call(&mut self, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::return_one(true)))
    }
}

struct WrongStackReturn;

impl vm::HostStackFunction for WrongStackReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::return_one(true)))
    }
}

fn wrong_static_return(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    Ok(CallOutcome::Return(vm::return_one(true)))
}

fn wrong_static_args_return(_args: &[Value]) -> VmResult<CallOutcome> {
    Ok(CallOutcome::Return(vm::return_one(true)))
}

fn call_zero_arg_import(name: &str) -> Program {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: name.to_string(),
            arity: 0,
            return_type: ValueType::Int,
        }],
        None,
    )
}

#[test]
fn every_dynamic_dispatch_kind_checks_return_values() {
    let mut dynamic_vm = Vm::new(call_zero_arg_import("demo::wrong_dynamic"));
    let mut dynamic_registry = HostFunctionRegistry::empty();
    dynamic_registry.register("demo::wrong_dynamic", 0, || Box::new(WrongDynamicReturn));
    dynamic_registry
        .bind_vm_cached(&mut dynamic_vm)
        .expect("dynamic host function should bind");
    assert!(matches!(
        dynamic_vm.run(),
        Err(VmError::TypeMismatch("int"))
    ));

    let mut args_vm = Vm::new(call_zero_arg_import("demo::wrong_args"));
    let mut args_registry = HostFunctionRegistry::empty();
    args_registry.register_args("demo::wrong_args", 0, || Box::new(WrongArgsReturn));
    args_registry
        .bind_vm_cached(&mut args_vm)
        .expect("args host function should bind");
    assert!(matches!(args_vm.run(), Err(VmError::TypeMismatch("int"))));

    let mut stack_vm = Vm::new(call_zero_arg_import("demo::wrong_stack"));
    let mut stack_registry = HostFunctionRegistry::empty();
    stack_registry.register_stack("demo::wrong_stack", 0, || Box::new(WrongStackReturn));
    stack_registry
        .bind_vm_cached(&mut stack_vm)
        .expect("stack host function should bind");
    assert!(matches!(stack_vm.run(), Err(VmError::TypeMismatch("int"))));

    let mut static_vm = Vm::new(call_zero_arg_import("demo::wrong_static"));
    let mut static_registry = HostFunctionRegistry::empty();
    static_registry.register_static("demo::wrong_static", 0, wrong_static_return);
    static_registry
        .bind_vm_cached(&mut static_vm)
        .expect("static host function should bind");
    assert!(matches!(static_vm.run(), Err(VmError::TypeMismatch("int"))));

    let mut static_args_vm = Vm::new(call_zero_arg_import("demo::wrong_static_args"));
    let mut static_args_registry = HostFunctionRegistry::empty();
    static_args_registry.register_static_args(
        "demo::wrong_static_args",
        0,
        wrong_static_args_return,
    );
    static_args_registry
        .bind_vm_cached(&mut static_args_vm)
        .expect("static args host function should bind");
    assert!(matches!(
        static_args_vm.run(),
        Err(VmError::TypeMismatch("int"))
    ));
}

struct ManyReturn;

impl vm::HostFunction for ManyReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::CallReturn::many(vec![
            Value::Int(1),
            Value::Int(2),
        ])))
    }
}

#[test]
fn host_return_cardinality_is_checked_before_values_reach_guest() {
    let mut vm = Vm::new(call_zero_arg_import("demo::many_return"));
    let mut registry = HostFunctionRegistry::empty();
    registry.register("demo::many_return", 0, || Box::new(ManyReturn));
    registry
        .bind_vm_cached(&mut vm)
        .expect("many-return host function should bind");
    let error = vm.run().expect_err("multiple values must be rejected");
    assert!(error.to_string().contains("cardinality"), "{error}");
}

#[test]
fn catalog_duplicate_registration_is_rejected_without_replacing_the_original() {
    let mut builder = HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "demo::duplicate",
        vec![vm::HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    let catalog = builder.build().expect("catalog");
    let schema = catalog_import_schemas(&catalog, "demo::duplicate")
        .pop()
        .expect("schema");
    let mut registry = HostFunctionRegistry::empty();
    register_catalog_static_function(
        &mut registry,
        &catalog,
        "demo::duplicate",
        schema.clone(),
        overloaded_int,
    )
    .expect("first registration");
    let duplicate = register_catalog_static_function(
        &mut registry,
        &catalog,
        "demo::duplicate",
        schema.clone(),
        overloaded_string,
    )
    .expect_err("duplicate full schema must fail");
    assert!(duplicate.to_string().contains("already registered"));

    let mut conflict_schema = schema.clone();
    conflict_schema.return_type = HostTypeSchema::Bool;
    let conflict = registry
        .register_catalog_static(conflict_schema, overloaded_string)
        .expect_err("same dispatch shape with a different return is ambiguous");
    assert!(matches!(
        conflict,
        vm::RegistrySchemaError::DispatchConflict { .. }
    ));

    let imports = vec![HostImport {
        name: "demo::duplicate".to_string(),
        arity: 1,
        return_type: ValueType::Int,
    }];
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ret();
    let program =
        Program::with_imports_and_debug(vec![Value::Int(9)], bytecode.finish(), imports, None)
            .with_host_import_schemas(vec![schema])
            .expect("metadata");
    let mut vm = Vm::new(program);
    registry
        .bind_vm_cached(&mut vm)
        .expect("original registration remains bindable");
    vm.run().expect("original registration runs");
    assert_eq!(vm.stack(), &[Value::Int(11)]);
}
