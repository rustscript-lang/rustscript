#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::{
    HostBindingKind, HostExecutionKind, callable_param_expr, classify_host_binding,
    infer_host_execution,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use syn::parse_quote;
use vm::{
    BuiltinFunction, BytecodeBuilder, CallOutcome, CallReturn, CapabilityProfile, HostApiBuilder,
    HostArgsFunction, HostAsyncBridge, HostAsyncOpTerminal, HostFunction, HostFunctionRegistry,
    HostFunctionSchema, HostImport, HostImportSchema, HostParamSchema, HostStackFunction,
    HostTypeSchema, JitConfig, JitTraceTerminal, Program, StandardSurfaceComposition, Value, Vm,
    VmError, VmStatus, compile_source,
};

fn schema_for(function: &HostFunctionSchema) -> (HostImport, HostImportSchema) {
    let mut builder = HostApiBuilder::new();
    builder.function(function.clone());
    let catalog = builder.build().expect("test catalog");
    let schema = HostImportSchema::from_function(&catalog, function);
    let import = HostImport {
        name: function.name.clone(),
        arity: function.params.len() as u8,
        return_type: match function.return_type {
            HostTypeSchema::Null => vm::ValueType::Null,
            HostTypeSchema::Int => vm::ValueType::Int,
            HostTypeSchema::Float => vm::ValueType::Float,
            HostTypeSchema::Number => vm::ValueType::Float,
            HostTypeSchema::Bool => vm::ValueType::Bool,
            HostTypeSchema::String => vm::ValueType::String,
            HostTypeSchema::Bytes => vm::ValueType::Bytes,
            HostTypeSchema::Array(_) => vm::ValueType::Array,
            HostTypeSchema::Map(_) | HostTypeSchema::Named { .. } => vm::ValueType::Map,
            HostTypeSchema::Callable { .. } => vm::ValueType::Callable,
            HostTypeSchema::Resource(_) | HostTypeSchema::Optional(_) | HostTypeSchema::Unknown => {
                vm::ValueType::Unknown
            }
        },
    };
    (import, schema)
}

fn program_with_imports(
    imports: Vec<HostImport>,
    schemas: Vec<HostImportSchema>,
    code: Vec<u8>,
) -> Arc<Program> {
    Arc::new(
        Program::with_imports_and_debug(Vec::new(), code, imports, None)
            .with_host_import_schemas(schemas)
            .expect("aligned host schemas"),
    )
}

fn return_int(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(7))))
}

fn return_int_99(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(99))))
}

struct FailingComposition;

impl StandardSurfaceComposition for FailingComposition {
    fn import_in_standard(&self, _import: &HostImport) -> bool {
        false
    }

    fn ensure_surfaces(
        &self,
        _imports: &[HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> vm::VmResult<bool> {
        registry.register_static("composition::probe", 0, return_int);
        Err(vm::VmError::HostError(
            "intentional composition probe failure".to_string(),
        ))
    }

    fn build_default_registry(&self) -> vm::VmResult<HostFunctionRegistry> {
        Ok(HostFunctionRegistry::empty())
    }

    fn bind_default_name(&self, _vm: &mut Vm, _name: &str) -> bool {
        false
    }
}

struct SuccessfulComposition;

impl StandardSurfaceComposition for SuccessfulComposition {
    fn import_in_standard(&self, import: &HostImport) -> bool {
        import.name == "composition::staged"
    }

    fn ensure_surfaces(
        &self,
        _imports: &[HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> vm::VmResult<bool> {
        registry.register_static("composition::staged", 0, return_int);
        Ok(true)
    }

    fn build_default_registry(&self) -> vm::VmResult<HostFunctionRegistry> {
        Ok(HostFunctionRegistry::empty())
    }

    fn bind_default_name(&self, _vm: &mut Vm, _name: &str) -> bool {
        false
    }
}

struct MatrixResource;

impl vm::resource::HostResource for MatrixResource {}

struct MatrixModule(u64);

struct MatrixAsyncBridge;

impl HostAsyncBridge for MatrixAsyncBridge {
    fn poll_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<vm::CallReturn>> {
        Poll::Pending
    }

    fn poll_submitted_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<vm::HostFutureOutput>> {
        Poll::Pending
    }

    fn cleanup_op(
        &mut self,
        _op_id: vm::HostOpId,
        _terminal: HostAsyncOpTerminal,
    ) -> vm::VmResult<()> {
        Ok(())
    }
}

struct PendingResetBridge {
    submitted: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
    futures: std::collections::HashMap<vm::HostOpId, vm::HostFuture>,
}

impl HostAsyncBridge for PendingResetBridge {
    fn submit_op(&mut self, op_id: vm::HostOpId, future: vm::HostFuture) -> vm::VmResult<()> {
        if self.futures.insert(op_id, future).is_some() {
            return Err(vm::VmError::HostError(format!(
                "duplicate pending operation {op_id}"
            )));
        }
        self.submitted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn poll_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<vm::CallReturn>> {
        Poll::Pending
    }

    fn poll_submitted_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<vm::HostFutureOutput>> {
        Poll::Pending
    }

    fn request_cancel_op(
        &mut self,
        _op_id: vm::HostOpId,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::VmResult<()> {
        self.cancellations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn poll_cancel_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn cleanup_op(
        &mut self,
        op_id: vm::HostOpId,
        _terminal: HostAsyncOpTerminal,
    ) -> vm::VmResult<()> {
        self.futures.remove(&op_id);
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct PendingStackHost;

impl HostStackFunction for PendingStackHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        vm.submit_host_future(Box::pin(std::future::pending::<
            vm::VmResult<vm::HostFutureOutput>,
        >()))
    }
}

struct MatrixFactoryHost(i64);

impl HostFunction for MatrixFactoryHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(self.0))))
    }
}

impl HostStackFunction for MatrixFactoryHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(self.0))))
    }
}

impl HostArgsFunction for MatrixFactoryHost {
    fn call(&mut self, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(self.0))))
    }
}

struct ReentrantStackHost {
    attempted_reentry: bool,
}

impl HostStackFunction for ReentrantStackHost {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        if !self.attempted_reentry {
            self.attempted_reentry = true;
            vm.reset_for_reuse()
                .expect("stack mutation reset should be accepted");
            let error = vm
                .run()
                .expect_err("same stack host slot must reject re-entry safely");
            assert!(error.to_string().contains("already executing"));
        }
        assert_eq!(args, &[Value::Int(41)]);
        Ok(CallOutcome::Return(CallReturn::one(args[0].clone())))
    }
}

struct ErrorThenSuccessStackHost {
    calls: Arc<AtomicUsize>,
}

impl HostStackFunction for ErrorThenSuccessStackHost {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(VmError::HostError("stack callback error".to_string()));
        }
        Ok(CallOutcome::Return(CallReturn::one(
            args.first().cloned().unwrap_or(Value::Null),
        )))
    }
}

struct PanicThenSuccessStackHost {
    calls: Arc<AtomicUsize>,
}

impl HostStackFunction for PanicThenSuccessStackHost {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("stack callback panic");
        }
        Ok(CallOutcome::Return(CallReturn::one(
            args.first().cloned().unwrap_or(Value::Null),
        )))
    }
}

struct CountingStackHost {
    calls: Arc<AtomicUsize>,
}

impl HostStackFunction for CountingStackHost {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(
            args.first().cloned().unwrap_or(Value::Null),
        )))
    }
}

struct DifferentSlotRecursingStackHost {
    nested_runs: Arc<AtomicUsize>,
}

impl HostStackFunction for DifferentSlotRecursingStackHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.nested_runs.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            vm.run()
                .expect("different stack-host slot should be reentrant"),
            VmStatus::Halted
        );
        Ok(CallOutcome::Halt)
    }
}

struct MatrixOwnedHost;

impl vm::HostOwnedFunction for MatrixOwnedHost {
    fn call(&mut self, _call: &mut vm::OwnedHostCall<'_>) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(4))))
    }
}

struct IsolatedCounterHost {
    calls: usize,
}

impl HostFunction for IsolatedCounterHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.calls += 1;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            self.calls as i64,
        ))))
    }
}

fn unary_stack_program(name: &str) -> Arc<Program> {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ret();
    Arc::new(Program::with_imports_and_debug(
        vec![Value::Int(41)],
        bytecode.finish(),
        vec![HostImport {
            name: name.to_string(),
            arity: 1,
            return_type: vm::ValueType::Int,
        }],
        None,
    ))
}

#[test]
fn one_bound_program_constructs_ten_thousand_vms() {
    let function = HostFunctionSchema::with_return("bound::value", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());

    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("static binding");
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("prepare bound program");

    for _ in 0..10_000 {
        let vm = Vm::new_bound(Arc::clone(&bound)).expect("construct bound vm");
        assert_eq!(vm.bound_function_count(), 1);
    }
}

#[test]
fn bound_program_accepts_sparse_registry_slots() {
    let function =
        HostFunctionSchema::with_return("bound::sparse", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());

    let mut registry = HostFunctionRegistry::empty();
    registry.register_static("bound::unused", 0, return_int_99);
    registry
        .register_catalog_static(schema, return_int)
        .expect("sparse catalog binding");
    let bound = registry
        .bind_program_once(program)
        .expect("sparse bound program");
    let mut vm = Vm::new_bound(bound).expect("sparse bound VM");

    assert_eq!(
        vm.run().expect("sparse bound VM should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

#[test]
fn stack_host_reentry_and_reset_preserve_owned_argument_semantics() {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        vec![Value::Int(41)],
        bytecode.finish(),
        vec![HostImport {
            name: "bound::reentrant_stack".to_string(),
            arity: 1,
            return_type: vm::ValueType::Int,
        }],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.register_stack("bound::reentrant_stack", 1, || {
        Box::new(ReentrantStackHost {
            attempted_reentry: false,
        })
    });
    let bound = registry
        .bind_program_once(program)
        .expect("reentrant stack binding");
    let mut vm = Vm::new_bound(bound).expect("reentrant stack VM");

    assert_eq!(
        vm.run().expect("reentrant stack VM should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    vm.reset_for_reuse()
        .expect("same-slot host must remain resettable");
    assert_eq!(
        vm.run()
            .expect("same-slot host must remain callable after reset"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(41)]);
}

#[test]
fn stack_host_error_restores_slot_after_reset_and_reuse() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::clone(&calls);
    let mut registry = HostFunctionRegistry::empty();
    registry.register_stack("bound::error_stack", 1, move || {
        Box::new(ErrorThenSuccessStackHost {
            calls: Arc::clone(&factory_calls),
        })
    });
    let mut vm = Vm::new_bound(
        registry
            .bind_program_once(unary_stack_program("bound::error_stack"))
            .expect("error stack binding"),
    )
    .expect("error stack VM");

    let error = vm
        .run()
        .expect_err("first stack callback must return an error");
    assert!(error.to_string().contains("stack callback error"));
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    vm.reset_for_reuse()
        .expect("error callback VM must reset for reuse");
    assert_eq!(
        vm.run().expect("restored error slot must run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn stack_host_panic_restores_slot_after_catch_reset_and_reuse() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::clone(&calls);
    let mut registry = HostFunctionRegistry::empty();
    registry.register_stack("bound::panic_stack", 1, move || {
        Box::new(PanicThenSuccessStackHost {
            calls: Arc::clone(&factory_calls),
        })
    });
    let mut vm = Vm::new_bound(
        registry
            .bind_program_once(unary_stack_program("bound::panic_stack"))
            .expect("panic stack binding"),
    )
    .expect("panic stack VM");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run()));
    assert!(result.is_err(), "first stack callback must panic");
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    vm.reset_for_reuse()
        .expect("panic callback VM must reset for reuse");
    assert_eq!(
        vm.run().expect("restored panic slot must run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn stack_host_allows_recursion_through_a_different_slot_and_restores_both_slots() {
    let outer_runs = Arc::new(AtomicUsize::new(0));
    let inner_runs = Arc::new(AtomicUsize::new(0));
    let outer_counter = Arc::clone(&outer_runs);
    let inner_counter = Arc::clone(&inner_runs);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ldc(1);
    bytecode.call(1, 1);
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        vec![Value::Int(41), Value::Int(42)],
        bytecode.finish(),
        vec![
            HostImport {
                name: "bound::outer_stack".to_string(),
                arity: 1,
                return_type: vm::ValueType::Int,
            },
            HostImport {
                name: "bound::inner_stack".to_string(),
                arity: 1,
                return_type: vm::ValueType::Int,
            },
        ],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.register_stack("bound::outer_stack", 1, move || {
        Box::new(DifferentSlotRecursingStackHost {
            nested_runs: Arc::clone(&outer_counter),
        })
    });
    registry.register_stack("bound::inner_stack", 1, move || {
        Box::new(CountingStackHost {
            calls: Arc::clone(&inner_counter),
        })
    });
    let bound = registry
        .bind_program_once(program)
        .expect("different-slot stack binding");
    let mut vm = Vm::new_bound(bound).expect("different-slot stack VM");

    assert_eq!(
        vm.run().expect("different-slot recursion must run"),
        VmStatus::Halted
    );
    assert_eq!(outer_runs.load(Ordering::SeqCst), 1);
    assert_eq!(inner_runs.load(Ordering::SeqCst), 1);
    vm.reset_for_reuse()
        .expect("different-slot stack VM must reset for reuse");
    assert_eq!(
        vm.run()
            .expect("both stack slots must remain callable after reset"),
        VmStatus::Halted
    );
    assert_eq!(outer_runs.load(Ordering::SeqCst), 2);
    assert_eq!(inner_runs.load(Ordering::SeqCst), 2);
}

#[test]
fn bound_program_keeps_mutable_host_state_per_vm() {
    let function =
        HostFunctionSchema::with_return("bound::counter", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());

    let factory_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog(schema, {
            let factory_count = Arc::clone(&factory_count);
            move || {
                factory_count.fetch_add(1, Ordering::SeqCst);
                Box::new(IsolatedCounterHost { calls: 0 })
            }
        })
        .expect("counter binding");
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("prepare counter binding");
    assert_eq!(factory_count.load(Ordering::SeqCst), 0);

    let mut first = Vm::new_bound(Arc::clone(&bound)).expect("first bound vm");
    let mut second = Vm::new_bound(bound).expect("second bound vm");
    assert_eq!(factory_count.load(Ordering::SeqCst), 2);
    assert_eq!(first.run().expect("first run"), VmStatus::Halted);
    assert_eq!(second.run().expect("second run"), VmStatus::Halted);
    assert_eq!(first.stack(), &[Value::Int(1)]);
    assert_eq!(second.stack(), &[Value::Int(1)]);
}

#[test]
fn bound_vm_lifecycle_matrix_isolates_module_resource_and_async_state() {
    let program = Arc::new(Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]));
    let bound = HostFunctionRegistry::empty()
        .bind_program_once(program)
        .expect("empty bound program");
    let mut first = Vm::new_bound(Arc::clone(&bound)).expect("first lifecycle VM");
    let mut second = Vm::new_bound(bound).expect("second lifecycle VM");

    assert_eq!(first.allocate_host_op_id(), 1);
    assert_eq!(second.allocate_host_op_id(), 1);
    first
        .set_async_bridge(Box::new(MatrixAsyncBridge))
        .expect("first async bridge");
    second
        .set_async_bridge(Box::new(MatrixAsyncBridge))
        .expect("second async bridge");
    first.clear_async_bridge().expect("first bridge clear");
    second.clear_async_bridge().expect("second bridge clear");

    first.host_context().set_module_state(MatrixModule(7));
    assert_eq!(
        first
            .host_context()
            .module_state::<MatrixModule>()
            .map(|state| state.0),
        Some(7)
    );
    assert!(
        second
            .host_context()
            .module_state::<MatrixModule>()
            .is_none()
    );

    let _first_resource = first
        .host_context()
        .push_resource(MatrixResource)
        .expect("first resource");
    assert_eq!(first.host_context().resource_count(), 1);
    assert_eq!(second.host_context().resource_count(), 0);
}

#[test]
fn bound_vm_reset_cancels_active_submitted_async_operation() {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: "bound::pending_stack".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        }],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.register_stack("bound::pending_stack", 0, || Box::new(PendingStackHost));
    let bound = registry
        .bind_program_once(program)
        .expect("pending stack binding");
    let mut vm = Vm::new_bound(bound).expect("pending stack VM");
    let submitted = Arc::new(AtomicUsize::new(0));
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cleanups = Arc::new(AtomicUsize::new(0));
    vm.set_async_bridge(Box::new(PendingResetBridge {
        submitted: Arc::clone(&submitted),
        cancellations: Arc::clone(&cancellations),
        cleanups: Arc::clone(&cleanups),
        futures: std::collections::HashMap::new(),
    }))
    .expect("pending bridge");

    assert_eq!(
        vm.run().expect("pending call should run"),
        VmStatus::Waiting(1)
    );
    assert_eq!(submitted.load(Ordering::SeqCst), 1);
    vm.reset_for_reuse()
        .expect("reset should cancel pending call");
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert!(vm.is_reusable());
    vm.clear_async_bridge().expect("cleared bridge after reset");
}

#[test]
fn bound_program_instantiates_each_dynamic_factory_kind_per_vm() {
    let imports = vec![
        HostImport {
            name: "matrix::dynamic".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        },
        HostImport {
            name: "matrix::stack".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        },
        HostImport {
            name: "matrix::args".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        },
    ];
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.call(1, 0);
    bytecode.call(2, 0);
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        imports,
        None,
    ));
    let dynamic_count = Arc::new(AtomicUsize::new(0));
    let stack_count = Arc::new(AtomicUsize::new(0));
    let args_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::empty();
    registry.register("matrix::dynamic", 0, {
        let count = Arc::clone(&dynamic_count);
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(MatrixFactoryHost(1))
        }
    });
    registry.register_stack("matrix::stack", 0, {
        let count = Arc::clone(&stack_count);
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(MatrixFactoryHost(2))
        }
    });
    registry.register_args("matrix::args", 0, {
        let count = Arc::clone(&args_count);
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(MatrixFactoryHost(3))
        }
    });
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("dynamic factory matrix binding");
    let mut first = Vm::new_bound(Arc::clone(&bound)).expect("first factory matrix VM");
    let mut second = Vm::new_bound(bound).expect("second factory matrix VM");
    assert_eq!(dynamic_count.load(Ordering::SeqCst), 2);
    assert_eq!(stack_count.load(Ordering::SeqCst), 2);
    assert_eq!(args_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        first.run().expect("first factory matrix run"),
        VmStatus::Halted
    );
    assert_eq!(
        second.run().expect("second factory matrix run"),
        VmStatus::Halted
    );
    assert_eq!(
        first.stack(),
        &[Value::Int(1), Value::Int(2), Value::Int(3)]
    );
    assert_eq!(
        second.stack(),
        &[Value::Int(1), Value::Int(2), Value::Int(3)]
    );

    let owned_schema =
        HostFunctionSchema::with_return("matrix::owned", Vec::new(), HostTypeSchema::Int);
    let (owned_import, owned_schema) = schema_for(&owned_schema);
    let mut owned_code = BytecodeBuilder::new();
    owned_code.call(0, 0);
    owned_code.ret();
    let owned_program = program_with_imports(
        vec![owned_import],
        vec![owned_schema.clone()],
        owned_code.finish(),
    );
    let owned_count = Arc::new(AtomicUsize::new(0));
    let mut owned_registry = HostFunctionRegistry::empty();
    owned_registry
        .register_exact_owned("matrix::owned", 0, owned_schema, {
            let count = Arc::clone(&owned_count);
            move |_context| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::new(MatrixOwnedHost)
            }
        })
        .expect("owned factory registration");
    let owned_bound = owned_registry
        .bind_program_once(owned_program)
        .expect("owned factory binding");
    let mut owned_first = Vm::new_bound(Arc::clone(&owned_bound)).expect("first owned VM");
    let mut owned_second = Vm::new_bound(owned_bound).expect("second owned VM");
    assert_eq!(owned_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        owned_first.run().expect("first owned run"),
        VmStatus::Halted
    );
    assert_eq!(
        owned_second.run().expect("second owned run"),
        VmStatus::Halted
    );
    assert_eq!(owned_first.stack(), &[Value::Int(4)]);
    assert_eq!(owned_second.stack(), &[Value::Int(4)]);
}

#[test]
fn bound_program_preserves_full_schema_overloads() {
    let int_function = HostFunctionSchema::with_return(
        "bound::overloaded",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    );
    let string_function = HostFunctionSchema::with_return(
        "bound::overloaded",
        vec![HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::String,
    );
    let mut catalog_builder = HostApiBuilder::new();
    catalog_builder.function(int_function.clone());
    catalog_builder.function(string_function.clone());
    let catalog = catalog_builder.build().expect("overload catalog");
    let int_schema = HostImportSchema::from_function(&catalog, &int_function);
    let string_schema = HostImportSchema::from_function(&catalog, &string_function);

    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ldc(1);
    bytecode.call(1, 1);
    bytecode.ret();
    let program = Arc::new(
        Program::with_imports_and_debug(
            vec![Value::Int(1), Value::string("x")],
            bytecode.finish(),
            vec![
                HostImport {
                    name: "bound::overloaded".to_string(),
                    arity: 1,
                    return_type: vm::ValueType::Int,
                },
                HostImport {
                    name: "bound::overloaded".to_string(),
                    arity: 1,
                    return_type: vm::ValueType::String,
                },
            ],
            None,
        )
        .with_host_import_schemas(vec![int_schema.clone(), string_schema.clone()])
        .expect("aligned overload schemas"),
    );
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(int_schema, return_int)
        .expect("integer overload");
    registry
        .register_catalog_static(string_schema, |_vm, _args| {
            Ok(CallOutcome::Return(CallReturn::one(Value::string("text"))))
        })
        .expect("string overload");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare overload binding");
    let mut vm = Vm::new_bound(bound).expect("construct overload vm");
    assert_eq!(vm.run().expect("run overload vm"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7), Value::string("text")]);
}

#[test]
fn bound_program_rejects_registry_generation_changes() {
    let function =
        HostFunctionSchema::with_return("bound::generation", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("generation binding");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare generation binding");

    registry.register_static("bound::after", 0, return_int);
    let error = match Vm::new_bound(bound) {
        Ok(_) => panic!("stale bound program must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stale"));
}

#[test]
fn bound_program_survives_a_failed_registry_transaction() {
    let function =
        HostFunctionSchema::with_return("bound::transaction", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("transaction binding");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare transaction binding");

    let result: vm::VmResult<()> = registry.transactionally(|staged| {
        staged.register_static("bound::temporary", 0, return_int);
        Err(vm::VmError::HostError("abort transaction".to_string()))
    });
    assert!(result.is_err());
    Vm::new_bound(bound).expect("failed transaction must preserve the bound program");
}

#[test]
fn committed_registry_transaction_invalidates_existing_bound_programs() {
    let function =
        HostFunctionSchema::with_return("bound::commit", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("commit binding");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare commit binding");

    registry
        .transactionally(|staged| {
            staged.register_static("bound::committed", 0, return_int);
            Ok(())
        })
        .expect("commit transaction");
    let error = match Vm::new_bound(bound) {
        Ok(_) => panic!("commit must invalidate old artifact"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stale"));
}

#[test]
fn sibling_registry_mutation_invalidates_old_plans_but_allows_new_untouched_snapshot_plans() {
    let function =
        HostFunctionSchema::with_return("bound::sibling", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = program_with_imports(
        vec![import.clone()],
        vec![schema.clone()],
        bytecode.finish(),
    );
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema.clone(), return_int)
        .expect("sibling binding");
    let plan = registry
        .prepare_plan_with_schemas(std::slice::from_ref(&import), &[Some(schema.clone())])
        .expect("prepare sibling plan");
    let mut sibling = registry.clone();
    sibling.register_static("bound::sibling_only", 0, return_int);

    let mut stale_vm = Vm::new_shared(Arc::clone(&program));
    let error = registry
        .bind_vm_with_plan(&mut stale_vm, &plan)
        .expect_err("sibling mutation must invalidate the old plan");
    assert!(error.to_string().contains("stale"));

    let fresh_plan = registry
        .prepare_plan_with_schemas(&[import], &[Some(schema)])
        .expect("untouched sibling snapshot should prepare a fresh plan");
    let mut fresh_vm = Vm::new_shared(program);
    registry
        .bind_vm_with_plan(&mut fresh_vm, &fresh_plan)
        .expect("fresh plan for untouched snapshot should bind");
    assert_eq!(
        fresh_vm.run().expect("fresh sibling vm should run"),
        VmStatus::Halted
    );
}

#[test]
fn failed_composition_probe_does_not_mutate_source_registry_or_plan_witness() {
    let function = HostFunctionSchema::with_return(
        "bound::composition_source",
        Vec::new(),
        HostTypeSchema::Int,
    );
    let (import, _schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![import.clone()],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.register_static("bound::composition_source", 0, return_int);
    registry.set_standard_composition(Arc::new(FailingComposition));
    let plan = registry.prepare_plan(&[import]).expect("source plan");

    let error = match registry.bind_program_once(Arc::clone(&program)) {
        Ok(_) => panic!("composition probe should fail"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("intentional composition probe failure")
    );
    assert!(!registry.contains_name("composition::probe"));

    let mut vm = Vm::new_shared(program);
    registry
        .bind_vm_with_plan(&mut vm, &plan)
        .expect("failed probe must not stale the source plan");
    assert_eq!(vm.run().expect("source plan should run"), VmStatus::Halted);
}

#[test]
fn successful_composition_probe_stages_only_bound_registry() {
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: "composition::staged".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        }],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.set_standard_composition(Arc::new(SuccessfulComposition));
    assert!(!registry.contains_name("composition::staged"));

    let bound = registry
        .bind_program_once(program)
        .expect("successful composition probe");
    assert!(!registry.contains_name("composition::staged"));
    let mut vm = Vm::new_bound(bound).expect("composed bound VM");
    assert_eq!(
        vm.run().expect("composed bound VM should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

#[test]
fn new_bound_preserves_default_composition_for_builtin_and_regex_paths() {
    let compiled = compile_source(
        r#"
            use re;
            let mut i = 0;
            while i < 4 {
                let _ = string_contains("rustscript", "script");
                let _ = re::match("^rust", "rustscript");
                i = i + 1;
            }
            i;
        "#,
    )
    .expect("builtin and regex program should compile");
    let bound = HostFunctionRegistry::new()
        .bind_program_once(Arc::new(compiled.program))
        .expect("default registry should prepare builtin and regex program");
    let mut vm = Vm::new_bound(bound).expect("bound VM should retain default composition");
    assert!(vm.standard_composition().is_some());
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm.run().expect("builtin and regex VM should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    if native_jit_supported() {
        assert!(vm.jit_native_exec_count() > 0);
    }
}

#[test]
fn bound_vm_rejects_legacy_registration_without_positional_remap() {
    let function =
        HostFunctionSchema::with_return("bound::duplicate", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(1, 0);
    bytecode.ret();
    let program = program_with_imports(
        vec![import.clone(), import],
        vec![schema.clone(), schema.clone()],
        bytecode.finish(),
    );
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("duplicate import binding");
    let bound = registry
        .bind_program_once(program)
        .expect("duplicate import artifact");
    let mut vm = Vm::new_bound(bound).expect("duplicate import VM");
    assert_eq!(vm.register_static_function(return_int_99), u16::MAX);
    let error = vm
        .run()
        .expect_err("post-bind legacy registration must fail closed");
    assert!(error.to_string().contains("after host binding"));
}

#[test]
fn bound_program_rejects_capability_profile_changes() {
    let function =
        HostFunctionSchema::with_return("bound::capability", Vec::new(), HostTypeSchema::Int);
    let (import, _) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![import],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.register_static("bound::capability", 0, return_int);
    let bound = registry
        .bind_program_once(program)
        .expect("prepare capability binding");

    registry
        .allow_builtin("bound::capability")
        .expect("registered host capability should be known");
    let error = match Vm::new_bound(bound) {
        Ok(_) => panic!("changed capability profile must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stale"));
}

#[test]
fn bound_program_rejects_catalog_schema_mismatch_during_prepare() {
    let registered = HostFunctionSchema::with_return(
        "bound::schema",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    );
    let requested = HostFunctionSchema::with_return(
        "bound::schema",
        vec![HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::Int,
    );
    let (import, registered_schema) = schema_for(&registered);
    let (_, requested_schema) = schema_for(&requested);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![requested_schema], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(registered_schema, return_int)
        .expect("registered schema");

    let error = match registry.bind_program_once(program) {
        Ok(_) => panic!("catalog schema mismatch must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("bound::schema"));
}

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[test]
fn build_scanner_uses_the_shared_host_type_parser() {
    let function: syn::ItemFn = parse_quote! {
        fn inspect(
            #[pd_host_resource(passing = "take_owned")]
            resource: DemoResource,
            optional: Option<i64>,
        ) -> VmResult<Option<String>> {
            unimplemented!()
        }
    };
    let params = build_script::parse_callable_params(&function);
    assert_eq!(params.len(), 2);
    assert_eq!(params[0].ty_label, "resource");
    assert!(!params[0].optional);
    assert_eq!(params[1].ty_label, "int | null");
    assert!(params[1].optional);
    assert_eq!(
        pd_host_schema::type_label(&parse_quote!(VmResult<Option<String>>)).unwrap(),
        "string | null"
    );
}

#[test]
fn preserves_typed_callable_host_parameter_schema() {
    let ty: syn::Type = parse_quote!(VmCallable<fn(VmMap) -> VmMap>);
    assert_eq!(
        pd_host_schema::type_label(&ty).expect("callable type should parse"),
        "fn(map) -> map"
    );
    assert_eq!(
        callable_param_expr("fn(map) -> map"),
        "CallableParamType::Callable(CallableType { params: &[CallableParamType::Map], return_type: &CallableParamType::Map })"
    );

    let float_ty: syn::Type = parse_quote!(VmCallable<fn(f64) -> f64>);
    assert_eq!(
        pd_host_schema::type_label(&float_ty).expect("callable type should parse"),
        "fn(float) -> float"
    );
    assert_eq!(
        callable_param_expr("fn(float) -> float"),
        "CallableParamType::Callable(CallableType { params: &[CallableParamType::Float], return_type: &CallableParamType::Float })"
    );
}

#[test]
fn classifies_best_effort_host_bindings_from_signatures() {
    for function in [
        parse_quote!(
            fn host(vm: &mut Vm, value: i64) -> i64 {}
        ),
        parse_quote!(
            fn host(value: i64, vm: &mut (crate::vm::Vm)) -> VmResult<i64> {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticStack
        );
    }

    for function in [
        parse_quote!(
            fn host() -> CallOutcome {}
        ),
        parse_quote!(
            fn host() -> VmResult<CallOutcome> {}
        ),
        parse_quote!(
            fn host() -> (VmResult<&CallOutcome>) {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticArgs
        );
    }

    for function in [
        parse_quote!(
            fn host() {}
        ),
        parse_quote!(
            fn host() -> () {}
        ),
        parse_quote!(
            fn host() -> Option<i64> {}
        ),
        parse_quote!(
            fn host() -> bool {}
        ),
        parse_quote!(
            fn host() -> VmResult<bool> {}
        ),
        parse_quote!(
            fn host() -> Value {}
        ),
        parse_quote!(
            fn host() -> Vec<Value> {}
        ),
        parse_quote!(
            fn host() -> Vec<(Value, Value)> {}
        ),
        parse_quote!(
            fn host() -> SharedArray {}
        ),
        parse_quote!(
            fn host() -> NumberValue {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticNonYieldingArgs
        );
    }

    for unsupported in [
        parse_quote!(
            fn host() -> impl IntoVmValue {}
        ),
        parse_quote!(
            fn host() -> VmResult {}
        ),
        parse_quote!(
            fn host() -> Result<bool, HostError> {}
        ),
        parse_quote!(
            fn host() -> CustomReturn {}
        ),
        parse_quote!(
            fn host() -> Vec<bool> {}
        ),
        parse_quote!(
            fn host() -> VmResult<Result<bool, HostError>> {}
        ),
        parse_quote!(
            fn host() -> Option<bool, i64> {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&unsupported),
            HostBindingKind::StaticArgs
        );
    }
}

#[test]
fn infers_host_suspension_from_the_return_signature() {
    for function in [
        parse_quote!(
            fn host() -> HostCallResult<Value> {}
        ),
        parse_quote!(
            fn host() -> VmResult<HostCallResult<Value>> {}
        ),
    ] {
        assert_eq!(
            infer_host_execution(&function),
            HostExecutionKind::MaySuspend
        );
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticArgs
        );
    }

    let synchronous = parse_quote!(
        fn host() -> VmResult<Value> {}
    );
    assert_eq!(infer_host_execution(&synchronous), HostExecutionKind::Sync);

    let asynchronous = parse_quote!(
        async fn host(value: String) -> VmResult<String> {}
    );
    assert_eq!(
        infer_host_execution(&asynchronous),
        HostExecutionKind::MaySuspend
    );
    assert_eq!(
        classify_host_binding(&asynchronous),
        HostBindingKind::StaticStack
    );
}

fn assert_runtime_sleep_loop_uses_native_host_call(bind_cached_registry: bool) {
    let compiled = compile_source(
        r#"
            use runtime;
            let mut i = 0;
            while i < 100 {
                let _ = runtime::sleep(0);
                i = i + 1;
            }
            i;
        "#,
    )
    .expect("runtime::sleep loop should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    if bind_cached_registry {
        HostFunctionRegistry::new()
            .bind_vm_cached(&mut vm)
            .expect("cached registry should bind runtime::sleep");
    }

    let status = vm.run();
    assert!(
        status.is_ok(),
        "runtime::sleep loop should run: {status:?}\n{}",
        vm.dump_jit_info()
    );
    assert_eq!(status.unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.iter().any(|trace| {
                trace.terminal == JitTraceTerminal::LoopBack
                    && trace.op_names().iter().any(|op| op == "host_call")
                    && trace.ssa_text().contains("host_call")
            }),
            "runtime::sleep should remain in a loop-back trace, cached={bind_cached_registry}, dump:\n{}",
            vm.dump_jit_info()
        );
        assert!(
            vm.jit_native_exec_count() > 0,
            "runtime::sleep loop should execute natively, cached={bind_cached_registry}, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn runtime_sleep_default_bindings_remain_inside_jit_loop_traces() {
    assert_runtime_sleep_loop_uses_native_host_call(false);
    assert_runtime_sleep_loop_uses_native_host_call(true);
}

#[test]
fn runtime_exit_still_halts_for_direct_and_cached_default_bindings() {
    for bind_cached_registry in [false, true] {
        let compiled = compile_source(
            r#"
                use runtime;
                runtime::exit();
                99;
            "#,
        )
        .expect("runtime::exit program should compile");
        let mut vm = Vm::new(compiled.program);
        if bind_cached_registry {
            HostFunctionRegistry::new()
                .bind_vm_cached(&mut vm)
                .expect("cached registry should bind runtime::exit");
        }

        assert_eq!(
            vm.run().expect("runtime::exit should run"),
            VmStatus::Halted
        );
        assert!(vm.stack().is_empty());
    }
}

#[test]
fn restricted_capabilities_disable_trace_jit_for_host_imports_and_builtins() {
    for source in [
        r#"
            use runtime;
            let mut i = 0;
            while i < 4 {
                let _ = runtime::sleep(0);
                i = i + 1;
            }
            i;
        "#,
        r#"
            use re;
            let mut i = 0;
            while i < 4 {
                let _ = re::match("a", "a");
                i = i + 1;
            }
            i;
        "#,
    ] {
        let compiled = compile_source(source).expect("restricted loop should compile");
        let mut vm = Vm::new(compiled.program);
        vm.set_jit_config(JitConfig {
            enabled: native_jit_supported(),
            hot_loop_threshold: 1,
            max_trace_len: 512,
        });
        let error = HostFunctionRegistry::restricted()
            .bind_vm_cached(&mut vm)
            .expect_err("restricted registry should reject ungranted capability during preflight");

        assert!(
            error
                .to_string()
                .contains("capability profile does not allow")
        );
        assert_eq!(vm.jit_native_exec_count(), 0);
    }
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn generated_http_imports_are_unique_typed_and_independently_capability_gated() {
    const IMPORTS: [&str; 2] = ["http::client::request", "http::client::sse"];
    let callables = vm::default_host_callables();
    for name in IMPORTS {
        let discovered = callables
            .iter()
            .filter(|callable| callable.name == name)
            .collect::<Vec<_>>();
        assert_eq!(discovered.len(), 1, "{name} discovery count");
        let callable = discovered[0];
        assert_eq!(callable.signature.return_type, "map");
        if name == "http::client::request" {
            assert_eq!(callable.signature.params.len(), 1);
            assert_eq!(callable.signature.params[0].ty.display_label(), "map");
        } else {
            assert_eq!(callable.signature.params.len(), 2);
            assert_eq!(callable.signature.params[0].ty.display_label(), "map");
            assert_eq!(
                callable.signature.params[1].ty.display_label(),
                "fn(map) -> map"
            );
            assert_eq!(callable.host_execution, vm::HostExecution::MaySuspend);
        }
    }

    for mask in 0_u8..4 {
        let mut builder = CapabilityProfile::builder();
        for (index, name) in IMPORTS.iter().enumerate() {
            if mask & (1 << index) != 0 {
                builder = builder.allow_host_import(*name);
            }
        }
        let profile = builder.build();
        for (index, name) in IMPORTS.iter().enumerate() {
            assert_eq!(
                profile.allows_host_import(name),
                mask & (1 << index) != 0,
                "mask {mask:02b}, import {name}"
            );
        }
    }
}

#[test]
fn capability_profile_fingerprint_uses_stable_callable_identities() {
    let first = CapabilityProfile::builder()
        .allow_builtin(BuiltinFunction::JsonEncode)
        .allow_host_import("custom::echo")
        .build();
    let reordered = CapabilityProfile::builder()
        .allow_host_import("custom::echo")
        .allow_builtin(BuiltinFunction::JsonEncode)
        .build();

    assert_eq!(first, reordered);
    assert_eq!(first.fingerprint(), reordered.fingerprint());
    assert!(first.allows_builtin(BuiltinFunction::JsonEncode));
    assert!(first.allows_host_import("custom::echo"));
    assert!(!first.allows_host_import("custom::other"));
    assert_ne!(
        first.fingerprint(),
        CapabilityProfile::deny_all().fingerprint()
    );
    assert_ne!(
        CapabilityProfile::allow_all().fingerprint(),
        CapabilityProfile::deny_all().fingerprint()
    );
}

#[test]
fn vm_host_core_does_not_name_builtin_subsystem_policies() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let host_runtime = std::fs::read_to_string(manifest.join("src/vm/host_runtime.rs"))
        .expect("host runtime source");
    let capability =
        std::fs::read_to_string(manifest.join("src/vm/capability.rs")).expect("capability source");
    let host = std::fs::read_to_string(manifest.join("src/vm/host.rs")).expect("host source");

    for forbidden in [
        "HttpState",
        "IoPolicy",
        "SqlitePolicy",
        "http_state",
        "io_policy",
        "sqlite_policy",
    ] {
        assert!(
            !host_runtime.contains(forbidden),
            "HostRuntime leaked {forbidden}"
        );
        assert!(
            !capability.contains(forbidden),
            "capability.rs leaked {forbidden}"
        );
        assert!(!host.contains(forbidden), "host.rs leaked {forbidden}");
    }
}
