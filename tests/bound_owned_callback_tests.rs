#![cfg(all(feature = "runtime", feature = "bind-mode-test-hooks"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vm::{
    BindModeTestScope, BoundHostProgram, BytecodeBuilder, CallOutcome, CallReturn,
    CompileSourceFileOptions, HostApiBuilder, HostApiCatalog, HostFunction, HostFunctionRegistry,
    HostFunctionSchema, HostOwnedFunction, HostParamPassing, HostParamSchema, HostStructField,
    HostStructSchema, HostTypeSchema, OwnedHostCall, Program, SourceFlavor, TimerBackend,
    TimerConfig, TimerHostExt, TimerRegistration, Value, Vm, VmError, VmResult, VmStatus,
    compile_source_with_flavor_and_options, register_timer_builtin_module,
    register_timer_builtin_module_from_catalog, standard_host_catalog,
};

#[derive(Default)]
struct RecordingBackend {
    registrations: Mutex<Vec<TimerRegistration>>,
}

impl TimerBackend for RecordingBackend {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        self.registrations
            .lock()
            .expect("timer registrations")
            .push(registration);
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.registrations
            .lock()
            .expect("timer registrations")
            .len()
    }

    fn running_count(&self) -> usize {
        0
    }

    fn report_callback_error(&self, _error: vm::TimerCallbackError) {}

    fn shutdown(&self) -> VmResult<()> {
        Ok(())
    }
}

fn timer_program(source: &str) -> Arc<vm::Program> {
    let catalog = standard_host_catalog();
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("timer source must compile");
    Arc::new(compiled.program)
}

fn timer_bound_vm(
    program: Arc<vm::Program>,
    backend: &Arc<RecordingBackend>,
    config: TimerConfig,
) -> VmResult<Vm> {
    let mut registry = HostFunctionRegistry::new();
    register_timer_builtin_module(&mut registry)?;
    let bound = registry.bind_program_once(program)?;
    let mut vm = Vm::new_bound(bound)?;
    let backend: Arc<dyn TimerBackend> = backend.clone();
    vm.install_timer_runtime(backend, config);
    Ok(vm)
}

#[test]
fn bound_timer_callbacks_use_fresh_bound_vms_without_full_bind() {
    let program = timer_program(
        "use timer; timer::at(1, |premature| timer::pending_count()); timer::every(2, |premature| timer::running_count());",
    );
    let backend = Arc::new(RecordingBackend::default());
    let scope = BindModeTestScope::enter();
    let mut vm = timer_bound_vm(Arc::clone(&program), &backend, TimerConfig::default())
        .expect("bound timer VM");

    assert_eq!(vm.run().expect("bound timer root run"), VmStatus::Halted);
    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 0);
    assert_eq!(snapshot.bound_vm_instantiations, 3);
    let schemas = program
        .host_import_schemas()
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            matches!(
                program.imports[*index].name.as_str(),
                "timer::at" | "timer::every"
            )
        })
        .map(|(_, schema)| schema.as_ref().expect("timer schema"))
        .collect::<Vec<_>>();
    assert_eq!(schemas.len(), 2);
    for schema in schemas {
        assert_eq!(schema.params.len(), 2);
        assert_eq!(schema.params[1].passing, HostParamPassing::TakeOwned);
        assert!(matches!(
            &schema.params[1].schema,
            HostTypeSchema::Callable { params, result }
                if params == &[HostTypeSchema::Bool]
                    && matches!(result.as_ref(), HostTypeSchema::Unknown)
        ));
        assert_eq!(schema.return_type, HostTypeSchema::Bool);
    }
    let mut registrations =
        std::mem::take(&mut *backend.registrations.lock().expect("timer registrations"));
    assert_eq!(registrations.len(), 2);
    assert_eq!(registrations[0].interval, None);
    assert!(registrations[1].interval.is_some());
    assert_eq!(
        registrations[0]
            .callback
            .start(false)
            .expect("one-shot callback"),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(
        registrations[1]
            .callback
            .start(false)
            .expect("repeating callback"),
        vm::TimerCallbackStatus::Complete
    );
}

#[test]
fn ten_thousand_bound_timer_callback_constructions_reuse_one_preparation() {
    let program = timer_program(
        "use timer; let mut index = 0; while index < 10000 { timer::at(1, |premature| null); index = index + 1; }",
    );
    let backend = Arc::new(RecordingBackend::default());
    let scope = BindModeTestScope::enter();
    let mut vm = timer_bound_vm(
        Arc::clone(&program),
        &backend,
        TimerConfig {
            max_pending: 10001,
            max_running: 10001,
        },
    )
    .expect("bound timer VM");

    assert_eq!(
        vm.run().expect("10,000 timer registrations"),
        VmStatus::Halted
    );
    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 0);
    assert_eq!(snapshot.bound_vm_instantiations, 10001);
    assert_eq!(
        backend
            .registrations
            .lock()
            .expect("timer registrations")
            .len(),
        10000
    );
}

struct CountingHost {
    calls: Arc<AtomicUsize>,
}

impl HostFunction for CountingHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
    }
}

fn timer_catalog_with_overloads_and_named_record() -> Arc<HostApiCatalog> {
    let standard = standard_host_catalog();
    let record = HostStructSchema::new(
        "TimerRecord",
        vec![HostStructField::new("value", HostTypeSchema::Int)],
    );
    let overloaded_int = HostFunctionSchema::with_return(
        "test::overloaded",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Bool,
    );
    let overloaded_string = HostFunctionSchema::with_return(
        "test::overloaded",
        vec![HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::Bool,
    );
    let named = HostFunctionSchema::with_return(
        "test::named",
        vec![HostParamSchema::value("record", record.as_type())],
        HostTypeSchema::Bool,
    );
    let mut builder = HostApiBuilder::new();
    for resource in standard.resources() {
        builder.resource(resource.clone());
    }
    for structure in standard.structs() {
        builder.named_struct(structure.clone());
    }
    for function in standard.functions() {
        builder.function(function.clone());
    }
    builder.named_struct(record);
    builder.function(overloaded_int);
    builder.function(overloaded_string);
    builder.function(named);
    Arc::new(builder.build().expect("combined timer catalog"))
}

#[test]
fn bound_timer_callback_executes_overloads_and_named_struct_schema() {
    let catalog = timer_catalog_with_overloads_and_named_record();
    let source = "use timer; use test; timer::at(1, |premature| if true => { test::overloaded(7); test::overloaded(\"text\"); test::named({ value: 7 }); } else => { null });";
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("combined timer source must compile");
    let program = Arc::new(compiled.program);
    assert!(program.host_import_schemas().iter().any(|schema| {
        schema.as_ref().is_some_and(|schema| {
            schema.name == "test::named"
                && matches!(
                    &schema.params[0].schema,
                    HostTypeSchema::Named { name, .. } if name == "TimerRecord"
                )
        })
    }));

    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::empty();
    registry
        .install_named_struct_schemas(vm::catalog_named_struct_schemas(catalog.as_ref()))
        .expect("named struct schemas");
    register_timer_builtin_module_from_catalog(&mut registry, catalog.as_ref())
        .expect("timer registration");
    for name in ["test::overloaded", "test::named"] {
        for schema in vm::catalog_import_schemas(catalog.as_ref(), name) {
            let calls = Arc::clone(&calls);
            registry
                .register_catalog(schema, move || {
                    Box::new(CountingHost {
                        calls: Arc::clone(&calls),
                    })
                })
                .expect("custom catalog registration");
        }
    }
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("combined bound program");
    let backend = Arc::new(RecordingBackend::default());
    let scope = BindModeTestScope::enter();
    let mut vm = Vm::new_bound(bound).expect("combined bound VM");
    let backend_trait: Arc<dyn TimerBackend> = backend.clone();
    vm.install_timer_runtime(backend_trait, TimerConfig::default());
    assert_eq!(vm.run().expect("combined timer root run"), VmStatus::Halted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let mut registrations =
        std::mem::take(&mut *backend.registrations.lock().expect("timer registrations"));
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0]
            .callback
            .start(false)
            .expect("combined callback"),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 0);
    assert_eq!(snapshot.bound_vm_instantiations, 2);
}

#[test]
fn standalone_owned_timer_uses_registry_fallback_and_counts_full_bind() {
    let program = timer_program("use timer; timer::at(1, |premature| null);");
    let backend = Arc::new(RecordingBackend::default());
    let mut registry = HostFunctionRegistry::new();
    register_timer_builtin_module(&mut registry).expect("timer registration");
    let mut vm = Vm::new_shared(Arc::clone(&program));
    registry
        .bind_vm_cached(&mut vm)
        .expect("registry-only root binding");
    let backend_trait: Arc<dyn TimerBackend> = backend.clone();
    vm.install_timer_runtime(backend_trait, TimerConfig::default());

    let scope = BindModeTestScope::enter();
    assert_eq!(
        vm.run().expect("registry-only timer root run"),
        VmStatus::Halted
    );
    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 1);
    assert_eq!(snapshot.bound_vm_instantiations, 0);

    let mut registrations =
        std::mem::take(&mut *backend.registrations.lock().expect("timer registrations"));
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0]
            .callback
            .start(false)
            .expect("registry-only callback"),
        vm::TimerCallbackStatus::Complete
    );
}

#[test]
fn registry_only_binding_is_explicit_and_counted_as_full_bind() {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let code = bytecode.finish();
    let plan_registry = HostFunctionRegistry::empty();
    let plan = plan_registry.prepare_plan(&[]).expect("registry-only plan");
    let scope = BindModeTestScope::enter();
    let mut vm = Vm::new(Program::new(Vec::new(), code.clone()));
    HostFunctionRegistry::empty()
        .bind_vm_cached(&mut vm)
        .expect("registry-only binding");
    let mut planned_vm = Vm::new(Program::new(Vec::new(), code));
    plan_registry
        .bind_vm_with_plan(&mut planned_vm, &plan)
        .expect("registry-only explicit plan binding");

    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 2);
    assert_eq!(snapshot.bound_vm_instantiations, 0);
}

#[test]
fn stale_bound_artifact_fails_closed_without_counting_an_instantiation() {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = Arc::new(Program::new(Vec::new(), bytecode.finish()));
    let mut registry = HostFunctionRegistry::empty();
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("bound artifact");
    registry.register_static("stale::mutation", 0, |_vm, _args| {
        Ok(CallOutcome::Return(CallReturn::none()))
    });

    let scope = BindModeTestScope::enter();
    let error = match Vm::new_bound(bound) {
        Ok(_) => panic!("stale artifact must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stale"));
    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 0);
    assert_eq!(snapshot.bound_vm_instantiations, 0);
}

struct AlwaysSuccessOwned;

impl HostOwnedFunction for AlwaysSuccessOwned {
    fn call(&mut self, _call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
    }
}

struct MismatchedBoundOwned {
    bound: Arc<BoundHostProgram>,
}

impl HostOwnedFunction for MismatchedBoundOwned {
    fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
        let rollback = call
            .arg(0)
            .cloned()
            .ok_or_else(|| VmError::HostError("missing callback".to_string()))?;
        let callback = call.take_arg(0)?;
        let result = call.spawn_owned_callable_vm_with_bound_program(
            Arc::clone(&self.bound),
            &callback,
            |_| Ok(()),
        );
        match result {
            Ok(_) => {
                call.restore_arg(0, rollback)?;
                Err(VmError::HostError(
                    "mismatched bound artifact was accepted".to_string(),
                ))
            }
            Err(error) => {
                call.restore_arg(0, rollback)?;
                Err(error)
            }
        }
    }
}

fn owned_catalog() -> (Arc<HostApiCatalog>, HostFunctionSchema) {
    let callback = HostTypeSchema::Callable {
        params: vec![HostTypeSchema::Bool],
        result: Box::new(HostTypeSchema::Unknown),
    };
    let function = HostFunctionSchema::with_return(
        "owned::register",
        vec![HostParamSchema::with_passing(
            "callback",
            callback,
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Bool,
    );
    let mut builder = HostApiBuilder::new();
    builder.function(function.clone());
    (Arc::new(builder.build().expect("owned catalog")), function)
}

#[test]
fn supplied_bound_artifact_mismatch_is_rejected_without_registry_fallback() {
    let (catalog, function) = owned_catalog();
    let compiled = compile_source_with_flavor_and_options(
        "use owned; owned::register(|premature| null);",
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("owned source must compile");
    let program = Arc::new(compiled.program);
    let schema = vm::HostImportSchema::from_function(&catalog, &function);

    let mut wrong_registry = HostFunctionRegistry::empty();
    wrong_registry
        .register_exact_owned("owned::register", 1, schema.clone(), |_context| {
            Box::new(AlwaysSuccessOwned)
        })
        .expect("wrong registry owned entry");
    let wrong_bound = wrong_registry
        .bind_program_once(Arc::clone(&program))
        .expect("wrong bound artifact");

    let mut root_registry = HostFunctionRegistry::empty();
    let mismatched = Arc::clone(&wrong_bound);
    root_registry
        .register_exact_owned("owned::register", 1, schema, move |_context| {
            Box::new(MismatchedBoundOwned {
                bound: Arc::clone(&mismatched),
            })
        })
        .expect("root registry owned entry");
    let root_bound = root_registry
        .bind_program_once(Arc::clone(&program))
        .expect("root bound artifact");

    let scope = BindModeTestScope::enter();
    let mut vm = Vm::new_bound(root_bound).expect("root VM");
    let error = vm.run().expect_err("mismatched bound artifact must fail");
    assert!(error.to_string().contains("does not match"));
    let snapshot = scope.snapshot();
    assert_eq!(snapshot.full_bind_installs, 0);
    assert_eq!(snapshot.bound_vm_instantiations, 1);
}
