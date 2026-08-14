use super::async_host::WaitingHostOp;
use super::*;
use crate::builtins::BuiltinFunction;
use crate::bytecode::TypeMap;
#[cfg(feature = "sqlite")]
use crate::{SqliteHostExt, SqlitePolicy};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

fn native_cache_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn failed_dynamic_builtin_override_preserves_runtime_owned_pending_binding() {
    struct Dummy;

    impl HostFunction for Dummy {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            unreachable!("rejected override must never be installed")
        }
    }

    let compiled = crate::compile_source("use runtime; runtime::sleep(0);")
        .expect("runtime sleep program should compile");
    let mut vm = Vm::new(compiled.program);
    vm.ensure_call_bindings()
        .expect("default fallback should bind runtime sleep");
    let slot = vm.host.host_function_symbols["runtime::sleep"];
    vm.host.runtime_owned_pending_host_slots.insert(slot);
    assert!(vm.host.runtime_owned_pending_host_slots.contains(&slot));

    vm.bind_builtin_override("runtime::sleep", Box::new(Dummy))
        .expect_err("runtime sleep is a host import, not a builtin override");

    assert!(vm.host.runtime_owned_pending_host_slots.contains(&slot));
}

#[test]
fn failed_static_builtin_override_preserves_runtime_owned_pending_binding() {
    fn dummy(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        unreachable!("rejected override must never be installed")
    }

    let compiled = crate::compile_source("use runtime; runtime::sleep(0);")
        .expect("runtime sleep program should compile");
    let mut vm = Vm::new(compiled.program);
    vm.ensure_call_bindings()
        .expect("default fallback should bind runtime sleep");
    let slot = vm.host.host_function_symbols["runtime::sleep"];
    vm.host.runtime_owned_pending_host_slots.insert(slot);
    assert!(vm.host.runtime_owned_pending_host_slots.contains(&slot));

    vm.bind_builtin_static_override("runtime::sleep", dummy)
        .expect_err("runtime sleep is a host import, not a builtin override");

    assert!(vm.host.runtime_owned_pending_host_slots.contains(&slot));
}

#[test]
fn root_ret_completes_explicit_halt_frame() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    assert_eq!(vm.instance.execution_frames.len(), 1);
    assert_eq!(
        vm.instance.execution_frames[0].continuation,
        FrameContinuation::Halt
    );

    assert_eq!(vm.run().expect("root ret should run"), VmStatus::Halted);
    assert!(vm.instance.execution_frames.is_empty());
    assert!(vm.stack().is_empty());

    vm.reset_for_reuse();
    assert_eq!(vm.instance.execution_frames.len(), 1);
    assert_eq!(vm.stack(), &[]);
}

#[test]
fn reset_for_reuse_keeps_host_operation_ids_monotonic() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    assert_eq!(vm.allocate_host_op_id(), 1);
    vm.reset_for_reuse();
    assert_eq!(vm.allocate_host_op_id(), 2);
}

#[test]
fn async_host_future_is_submitted_to_the_host_bridge() {
    use std::sync::{Arc, Mutex};

    struct RecordingBridge {
        submitted: Arc<Mutex<Vec<HostOpId>>>,
        future: Arc<Mutex<Option<HostFuture>>>,
    }

    impl HostAsyncBridge for RecordingBridge {
        fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
            self.submitted.lock().expect("submitted lock").push(op_id);
            *self.future.lock().expect("future lock") = Some(future);
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }
    }

    let submitted = Arc::new(Mutex::new(Vec::new()));
    let future = Arc::new(Mutex::new(None));
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    vm.set_async_bridge(Box::new(RecordingBridge {
        submitted: Arc::clone(&submitted),
        future: Arc::clone(&future),
    }));

    let outcome = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::one(Value::Int(42))))
        }))
        .expect("host bridge should accept future");
    let CallOutcome::Pending(op_id) = outcome else {
        panic!("async host submission should suspend");
    };

    assert_eq!(*submitted.lock().expect("submitted lock"), vec![op_id]);
    assert!(future.lock().expect("future lock").is_some());
    assert_eq!(vm.host.runtime_operations.active_count(), 0);
}

#[test]
fn async_host_submission_without_driver_fails_and_retires_the_id() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let error = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect_err("missing host async driver should fail");

    assert!(
        error
            .to_string()
            .contains("async host function requires a host async bridge")
    );
    assert_eq!(vm.allocate_host_op_id(), 2);
    assert_eq!(vm.host.runtime_operations.active_count(), 0);
}

#[test]
fn completing_a_submitted_host_op_cancels_the_driver_future() {
    use std::sync::{Arc, Mutex};

    struct CancelRecordingBridge(Arc<Mutex<Vec<HostOpId>>>);

    impl HostAsyncBridge for CancelRecordingBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }

        fn cancel_op(&mut self, op_id: HostOpId) {
            self.0.lock().expect("cancel lock").push(op_id);
        }
    }

    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    vm.set_async_bridge(Box::new(CancelRecordingBridge(Arc::clone(&cancelled))));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect("future should submit")
    else {
        panic!("submission should return pending");
    };
    vm.set_waiting_host_op(op_id)
        .expect("submitted op should register");

    vm.complete_host_op(op_id, CallReturn::none())
        .expect("manual completion should succeed");

    assert_eq!(*cancelled.lock().expect("cancel lock"), vec![op_id]);
    assert_eq!(vm.waiting_host_op_id(), None);
    assert_eq!(vm.host.runtime_operations.active_count(), 0);
}

#[test]
fn failed_submitted_host_completion_clears_waiting_state() {
    struct FailingCompletionBridge;

    impl HostAsyncBridge for FailingCompletionBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }

        fn poll_submitted_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<HostFutureOutput>> {
            std::task::Poll::Ready(Ok(HostFutureOutput::complete(|_| {
                Err(VmError::HostError("completion failed".to_string()))
            })))
        }
    }

    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    vm.set_async_bridge(Box::new(FailingCompletionBridge));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect("future should submit")
    else {
        panic!("submission should return pending");
    };
    vm.set_waiting_host_op(op_id)
        .expect("submitted op should register");
    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);

    let result = vm.poll_waiting_host_op(&mut context);

    assert!(matches!(
        result,
        std::task::Poll::Ready(Err(VmError::HostError(message)))
            if message == "completion failed"
    ));
    assert_eq!(vm.waiting_host_op_id(), None);
    assert_eq!(vm.host.runtime_operations.active_count(), 0);
}

#[test]
fn unused_host_operation_ids_do_not_consume_registry_capacity() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    for _ in 0..128 {
        vm.allocate_host_op_id();
    }
    assert_eq!(vm.host.runtime_operations.active_count(), 0);
}

#[test]
fn external_host_operations_join_the_shared_registry_without_id_collisions() {
    use crate::builtins::runtime::cancellation::{OperationId, OperationOwner};

    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let runtime_operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Io,
            Some(&vm.run_ctx.cancellation),
            None,
            None,
        )
        .expect("runtime operation should start");

    let collision = vm
        .set_waiting_host_op(runtime_operation.id().raw())
        .expect_err("host operation must not reuse a runtime-owned id");
    assert!(collision.to_string().contains("collides"));
    assert!(
        vm.host
            .runtime_operations
            .get(runtime_operation.id())
            .is_ok()
    );
    vm.host
        .runtime_operations
        .complete(runtime_operation.id())
        .expect("runtime operation should complete");
    vm.set_waiting_host_op(runtime_operation.id().raw())
        .expect_err("colliding external operation id must remain retired");

    vm.set_waiting_host_op(99)
        .expect("external host operation should register");
    let external = vm
        .host
        .runtime_operations
        .get(OperationId::from_raw(99).expect("operation id should be valid"))
        .expect("external operation should be registered");
    assert_eq!(external.owner(), OperationOwner::HostBridge);
}

#[test]
fn invalid_host_completion_preserves_the_registered_operation() {
    use crate::builtins::runtime::cancellation::{OperationId, OperationOwner};

    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    vm.set_waiting_host_op(101)
        .expect("external host operation should register");

    vm.complete_host_op(102, CallReturn::none())
        .expect_err("completion for a different operation should fail");
    let operation_id = OperationId::from_raw(101).expect("operation id should be valid");
    assert_eq!(
        vm.host
            .runtime_operations
            .get(operation_id)
            .expect("waiting operation should remain registered")
            .owner(),
        OperationOwner::HostBridge
    );
    assert_eq!(vm.waiting_host_op_id(), Some(101));
}

#[test]
fn reset_and_drop_cleanup_real_host_resources_exactly_once() {
    use crate::builtins::runtime::cancellation::CancellationReason;
    use crate::builtins::runtime::resource::ResourceTypeId;

    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let reasons = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let cleanup_count_for_resource = Arc::clone(&cleanup_count);
    let reasons_for_resource = Arc::clone(&reasons);
    vm.host
        .runtime_resources
        .insert_with_cleanup(ResourceTypeId::IO_FILE, (), move |(), reason| {
            cleanup_count_for_resource.fetch_add(1, Ordering::SeqCst);
            reasons_for_resource
                .lock()
                .expect("reason lock")
                .push(reason);
            Ok(())
        })
        .expect("test resource should be inserted");

    vm.reset_for_reuse();
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        reasons.lock().expect("reason lock").as_slice(),
        &[CancellationReason::VmReset]
    );

    drop(vm);
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
}

#[test]
fn drop_cleans_real_host_resources_without_prior_reset() {
    use crate::builtins::runtime::cancellation::CancellationReason;
    use crate::builtins::runtime::resource::ResourceTypeId;

    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let cleanup_reason = Arc::new(Mutex::new(None));
    {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
        let cleanup_count_for_resource = Arc::clone(&cleanup_count);
        let cleanup_reason_for_resource = Arc::clone(&cleanup_reason);
        vm.host
            .runtime_resources
            .insert_with_cleanup(ResourceTypeId::IO_FILE, (), move |(), reason| {
                cleanup_count_for_resource.fetch_add(1, Ordering::SeqCst);
                *cleanup_reason_for_resource.lock().expect("reason lock") = Some(reason);
                Ok(())
            })
            .expect("test resource should be inserted");
    }

    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        *cleanup_reason.lock().expect("reason lock"),
        Some(CancellationReason::VmReset)
    );
}

#[test]
fn reset_propagates_to_real_host_operation_cleanup() {
    use crate::builtins::runtime::cancellation::{
        CancellationReason, OperationEnd, OperationOwner, OperationStatus,
    };

    let cleanup_end = Arc::new(Mutex::new(None));
    let cleanup_end_for_operation = Arc::clone(&cleanup_end);
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Io,
            Some(&vm.run_ctx.cancellation),
            None,
            Some(Box::new(move |end| {
                *cleanup_end_for_operation.lock().expect("cleanup lock") = Some(end);
                Ok(())
            })),
        )
        .expect("test operation should start");
    vm.instance.waiting_host_op = Some(WaitingHostOp {
        op_id: operation.id().raw(),
    });

    vm.reset_for_reuse();
    assert_eq!(
        operation.status(),
        OperationStatus::Cancelled(CancellationReason::VmReset)
    );
    assert_eq!(
        *cleanup_end.lock().expect("cleanup lock"),
        Some(OperationEnd::Cancelled(CancellationReason::VmReset))
    );
}

#[test]
fn deadline_cancellation_closes_operation_payload_before_registry_removal() {
    use crate::builtins::runtime::cancellation::{CancellationReason, OperationOwner};
    use crate::builtins::runtime::resource::ResourceTypeId;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    let cleanup_reason = Arc::new(Mutex::new(None));
    let cleanup_reason_for_payload = Arc::clone(&cleanup_reason);
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Io,
            Some(&vm.run_ctx.cancellation),
            Some(Instant::now() - Duration::from_millis(1)),
            None,
        )
        .expect("deadline operation should start");
    let payload = vm
        .host
        .runtime_resources
        .insert_with_cleanup(ResourceTypeId::CALLBACK, (), move |(), reason| {
            *cleanup_reason_for_payload.lock().expect("cleanup lock") = Some(reason);
            Ok(())
        })
        .expect("payload should be inserted");
    operation.set_payload(payload);

    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    let result =
        crate::builtins::runtime::poll_builtin_io_op(&mut vm, operation.id().raw(), &mut context);

    assert!(matches!(result, Poll::Ready(Err(_))));
    assert_eq!(
        *cleanup_reason.lock().expect("cleanup lock"),
        Some(CancellationReason::Deadline)
    );
    assert!(vm.host.runtime_operations.get(operation.id()).is_err());
    assert!(
        vm.host
            .runtime_resources
            .get::<()>(payload, ResourceTypeId::CALLBACK)
            .is_err()
    );
}

#[test]
fn worker_observed_deadline_retains_payload_until_vm_consumes_operation() {
    use crate::builtins::runtime::cancellation::{CancellationReason, OperationOwner};
    use crate::builtins::runtime::error::{RuntimeError, RuntimeErrorCode};
    use crate::builtins::runtime::resource::ResourceTypeId;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    let cleanup_reason = Arc::new(Mutex::new(None));
    let cleanup_reason_for_payload = Arc::clone(&cleanup_reason);
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Io,
            Some(&vm.run_ctx.cancellation),
            Some(Instant::now() - Duration::from_millis(1)),
            None,
        )
        .expect("deadline operation should start");
    let payload = vm
        .host
        .runtime_resources
        .insert_with_cleanup(ResourceTypeId::CALLBACK, (), move |(), reason| {
            *cleanup_reason_for_payload.lock().expect("cleanup lock") = Some(reason);
            Ok(())
        })
        .expect("payload should be inserted");
    operation.set_payload(payload);

    assert!(
        operation
            .fail(RuntimeError::new(
                RuntimeErrorCode::OperationFailed,
                "test::worker",
                "worker failure",
            ))
            .expect("worker terminal transition should succeed")
    );
    assert!(vm.host.runtime_operations.get(operation.id()).is_ok());

    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    let result =
        crate::builtins::runtime::poll_builtin_io_op(&mut vm, operation.id().raw(), &mut context);

    assert!(matches!(result, Poll::Ready(Err(_))));
    assert_eq!(
        *cleanup_reason.lock().expect("cleanup lock"),
        Some(CancellationReason::Deadline)
    );
    assert!(vm.host.runtime_operations.get(operation.id()).is_err());
    assert!(
        vm.host
            .runtime_resources
            .get::<()>(payload, ResourceTypeId::CALLBACK)
            .is_err()
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_reconfiguration_only_closes_sqlite_owned_state() {
    use crate::builtins::runtime::cancellation::OperationOwner;
    use crate::builtins::runtime::resource::ResourceTypeId;

    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
    let io_resource = vm
        .host
        .runtime_resources
        .insert(ResourceTypeId::IO_FILE, 11_i64)
        .expect("IO resource should be inserted");
    let sqlite_resource = vm
        .host
        .runtime_resources
        .insert(ResourceTypeId::SQLITE_CONNECTION, 22_i64)
        .expect("SQLite resource should be inserted");
    let io_operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Io,
            Some(&vm.run_ctx.cancellation),
            None,
            None,
        )
        .expect("IO operation should start");
    io_operation.set_resource(io_resource);
    let sqlite_operation = vm
        .host
        .runtime_operations
        .start_owned(
            OperationOwner::Sqlite,
            Some(&vm.run_ctx.cancellation),
            None,
            None,
        )
        .expect("SQLite operation should start");
    sqlite_operation.set_resource(sqlite_resource);

    vm.configure_sqlite(SqlitePolicy::default());

    assert!(
        vm.host
            .runtime_resources
            .get::<i64>(io_resource, ResourceTypeId::IO_FILE)
            .is_ok()
    );
    assert!(vm.host.runtime_operations.get(io_operation.id()).is_ok());
    assert!(
        vm.host
            .runtime_resources
            .get::<i64>(sqlite_resource, ResourceTypeId::SQLITE_CONNECTION)
            .is_err()
    );
    assert!(
        vm.host
            .runtime_operations
            .get(sqlite_operation.id())
            .is_err()
    );
}

#[test]
fn shared_capture_cell_rejects_callable_ownership_cycle() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]).with_local_count(1));
    let cell = Arc::new(Mutex::new(Value::Null));
    vm.instance.capture_cells.insert(0, Arc::clone(&cell));
    let environment = Arc::new(crate::CallableEnvironment {
        cells: Mutex::new(vec![cell]),
    });
    let callable = Value::Callable(Arc::new(crate::CallableValue {
        prototype_id: 0,
        kind: crate::CallableKind::Closure,
        env: Some(environment),
    }));
    assert!(matches!(
        vm.store_local_with_drop_contract(0, callable),
        Err(VmError::InvalidFrameState(
            "callable capture ownership cycle is unsupported"
        ))
    ));
    assert_eq!(vm.locals()[0], Value::Null);
}

#[test]
fn inline_callable_identity_requires_capture_free_function_item_state() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]).with_local_count(1));
    let malformed_environment = Arc::new(crate::CallableEnvironment {
        cells: Mutex::new(vec![Arc::new(Mutex::new(Value::Int(7)))]),
    });
    vm.set_local(
        0,
        Value::Callable(Arc::new(crate::CallableValue {
            prototype_id: 42,
            kind: crate::CallableKind::FunctionItem,
            env: Some(malformed_environment),
        })),
    )
    .expect("install malformed callable");
    assert_eq!(vm.active_local_callable_prototypes(), Some(vec![None]));

    vm.set_local(
        0,
        Value::Callable(Arc::new(crate::CallableValue {
            prototype_id: 42,
            kind: crate::CallableKind::Closure,
            env: None,
        })),
    )
    .expect("install closure-shaped callable");
    assert_eq!(vm.active_local_callable_prototypes(), Some(vec![None]));

    vm.set_local(
        0,
        Value::Callable(Arc::new(crate::CallableValue {
            prototype_id: 42,
            kind: crate::CallableKind::FunctionItem,
            env: None,
        })),
    )
    .expect("install inline-compatible callable");
    assert_eq!(vm.active_local_callable_prototypes(), Some(vec![Some(42)]));
}

#[test]
fn callable_operand_type_hint_roundtrips() {
    let packed = pack_operand_types(ValueType::Callable, ValueType::Callable);
    assert_eq!(
        unpack_operand_types(packed),
        (ValueType::Callable, ValueType::Callable)
    );
}

#[test]
fn callvalue_decodes_its_arity_before_callable_validation() {
    let mut vm = Vm::new(Program::new(
        Vec::new(),
        vec![OpCode::CallValue as u8, 0, OpCode::Ret as u8],
    ));
    vm.instance.stack.push(Value::Null);
    assert!(matches!(vm.run(), Err(VmError::InvalidCallable)));
    assert_eq!(vm.ip(), 2);
}

#[test]
fn callvalue_enters_script_frame_and_resumes_caller() {
    let mut bc = crate::BytecodeBuilder::new();
    bc.ldloc(0);
    bc.ldc(0);
    bc.call_value(1);
    bc.ret();
    let function_entry = bc.position();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.ret();
    let function_end = bc.position();

    let program = Program::new(vec![Value::Int(41), Value::Int(1)], bc.finish())
        .with_local_count(1)
        .with_callable_metadata(
            vec![crate::ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![crate::CallablePrototype {
                kind: crate::CallableKind::FunctionItem,
                target: crate::CallableTarget::ScriptFunction(0),
                arity: 1,
                frame_local_count: 1,
                parameter_slots: vec![0],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![
                crate::FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                crate::FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![crate::RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        );
    let mut vm = Vm::new(program);

    assert_eq!(vm.run().expect("script call should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert_eq!(vm.call_depth(), 0);
}

#[test]
fn script_call_depth_limit_is_configurable() {
    let compiled = crate::compile_source_for_repl(
        "fn recurse(value: int) -> int { recurse(value) } recurse(1);",
    )
    .expect("recursive callable should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));

    assert_eq!(vm.max_script_call_depth(), 1024);
    assert!(matches!(
        vm.set_max_script_call_depth(0),
        Err(VmError::InvalidCallStackLimit(0))
    ));
    vm.set_max_script_call_depth(3)
        .expect("positive call depth should be accepted");
    assert_eq!(vm.max_script_call_depth(), 3);
    assert!(matches!(
        vm.run(),
        Err(VmError::CallStackOverflow { limit: 3 })
    ));
}

#[test]
fn host_can_invoke_exported_callable_and_reset_rebinds_program_owned_value() {
    let mut bc = crate::BytecodeBuilder::new();
    bc.ret();
    let entry = bc.position();
    bc.ldloc(0);
    bc.ldc(0);
    bc.add();
    bc.ret();
    let end = bc.position();
    let program = Program::new(vec![Value::Int(1)], bc.finish())
        .with_local_count(1)
        .with_callable_metadata(
            vec![crate::ScriptFunction {
                entry_ip: entry,
                end_ip: end,
            }],
            vec![crate::CallablePrototype {
                kind: crate::CallableKind::FunctionItem,
                target: crate::CallableTarget::ScriptFunction(0),
                arity: 1,
                frame_local_count: 1,
                parameter_slots: vec![0],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![
                crate::FunctionRegion {
                    start_ip: 0,
                    end_ip: entry,
                    prototype_id: None,
                },
                crate::FunctionRegion {
                    start_ip: entry,
                    end_ip: end,
                    prototype_id: Some(0),
                },
            ],
            vec![crate::RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        );
    let mut vm = Vm::new(program);
    let callable = vm.locals()[0].clone();
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    assert_eq!(
        vm.invoke_callable(callable.clone(), &[Value::Int(41)])
            .expect("host invocation should return"),
        Value::Int(42)
    );
    vm.queue_callable(callable.clone(), vec![Value::Int(1)])
        .expect("queue first callback");
    vm.queue_callable(callable.clone(), vec![Value::Int(2)])
        .expect("queue second callback");
    assert_eq!(vm.queued_callable_count(), 2);
    assert_eq!(
        vm.drain_callable_queue().expect("drain callbacks"),
        vec![Value::Int(2), Value::Int(3)]
    );
    vm.queue_callable(callable.clone(), vec![Value::Int(3)])
        .expect("queue callback before shutdown");
    vm.shutdown();
    assert_eq!(vm.queued_callable_count(), 0);
    assert!(matches!(
        vm.invoke_callable(callable.clone(), &[Value::Int(1)]),
        Err(VmError::InvalidFrameState("vm is shut down"))
    ));

    vm.reset_for_reuse();
    assert_eq!(vm.run().expect("reset root should halt"), VmStatus::Halted);
    let rebound = vm.locals()[0].clone();
    assert_eq!(
        vm.invoke_callable(rebound, &[Value::Int(1)])
            .expect("reset should rebind the Program-owned function item"),
        Value::Int(2)
    );
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_move_detach_without_stack_contract_mismatch() {
    let compiled = crate::compile_source_for_repl(
        r#"
            let source = "x";
            let moved = source;
            moved;
        "#,
    )
    .expect("move source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::String(Arc::new("x".to_string()))]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_script_callable_frames_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            let f = add_one;
            add_one(41);
        "#,
    )
    .expect("script frame source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(vm.aot_exec_count() >= 3);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_typed_script_callable_parameter_equality_without_interpreter_boundary() {
    let compiled = crate::compile_source(
        r#"
            fn is_zero(value: int) -> bool { value == 0 }
            let f = is_zero;
            is_zero(0);
        "#,
    )
    .expect("typed equality source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_script_callable_bool_return_in_branch_without_interpreter_boundary() {
    let compiled = crate::compile_source(
        r#"
            fn is_zero(value: int) -> bool { value == 0 }
            let f = is_zero;
            let selected = if is_zero(0) => { 1 } else => { 2 };
            selected;
        "#,
    )
    .expect("typed branch source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(1)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_capturing_closure_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            let answer = 42;
            let get_answer = || answer;
            get_answer();
        "#,
    )
    .expect("closure source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(vm.aot_exec_count() >= 3);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_builtin_callable_values_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            let function = len;
            function("abc");
        "#,
    )
    .expect("builtin callable source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(3)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_callable_call_resumes_after_fuel_yield_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            let f = add_one;
            add_one(41);
        "#,
    )
    .expect("callable source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    vm.set_fuel(0);
    assert_eq!(
        vm.run().expect("fuel exhaustion should yield"),
        VmStatus::Yielded
    );
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Fuel));
    vm.set_fuel(100);
    assert_eq!(
        vm.resume().expect("aot callable should resume"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_nested_script_callables_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn inc(value: int) -> int { value + 1 }
            fn twice(value: int) -> int { inc(inc(value)) }
            let f = inc;
            let g = twice;
            twice(40);
        "#,
    )
    .expect("nested callable source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("nested aot call should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_recursive_script_callable_reports_depth_limit_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn recurse(value: int) -> int { recurse(value) }
            let f = recurse;
            recurse(1);
        "#,
    )
    .expect("recursive callable source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.compile_aot().expect("aot compilation should succeed");
    assert!(matches!(
        vm.run(),
        Err(VmError::CallStackOverflow { limit: 1024 })
    ));
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_host_callable_value_waits_and_resumes_without_interpreter_boundary() {
    struct PendingAotHost;

    impl HostFunction for PendingAotHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Pending(812))
        }
    }

    let compiled = crate::compile_source_for_repl(
        r#"
            fn action(value: int) -> int;
            let function = action;
            function(41);
        "#,
    )
    .expect("host callable source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.register_function(Box::new(PendingAotHost));
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("pending host callable should wait"),
        VmStatus::Waiting(812)
    );
    assert!(!vm.engine.aot_interpreter_boundary_hit);
    vm.complete_host_op(812, vec![Value::Int(42)])
        .expect("host operation should complete");
    assert_eq!(
        vm.resume().expect("aot host callable should resume"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[test]
fn typed_script_callbacks_invoke_queue_unsubscribe_and_invalidate() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable.clone())
        .expect("typed callback should bind");

    assert_eq!(callback.call(&mut store, (41,)).expect("direct call"), 42);
    let queued = callback.prepare((40,)).expect("queued call should prepare");
    let queued = std::thread::spawn(move || queued)
        .join()
        .expect("queued invocation should cross threads");
    store
        .enqueue_callback(queued)
        .expect("queued call should bind to its store");
    assert_eq!(
        store.drain_callbacks().expect("queue should drain"),
        vec![Value::Int(41)]
    );

    let alias = callback.clone();
    assert!(matches!(
        store.script_callback::<(bool,), i64>(callable.clone()),
        Err(VmError::TypeMismatch("script callback argument schema"))
    ));
    assert!(matches!(
        store.script_callback::<(i64,), bool>(callable.clone()),
        Err(VmError::TypeMismatch("script callback result schema"))
    ));

    callback.unsubscribe();
    assert!(!alias.is_subscribed());
    assert!(matches!(
        alias.prepare((1,)),
        Err(VmError::InvalidFrameState(
            "script callback is unsubscribed"
        ))
    ));

    let independently_subscribed: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("second callback should bind");
    let queued_before_unsubscribe = independently_subscribed
        .prepare((1,))
        .expect("active callback should prepare");
    independently_subscribed.unsubscribe();
    assert!(matches!(
        store.enqueue_callback(queued_before_unsubscribe),
        Err(VmError::InvalidFrameState(
            "script callback is unsubscribed"
        ))
    ));
}

#[test]
fn callback_unsubscribe_cancels_already_enqueued_work() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("callback should bind");
    let queued = callback.prepare((41,)).expect("callback should prepare");
    store
        .enqueue_callback(queued)
        .expect("callback should enqueue");
    callback.unsubscribe();
    assert_eq!(
        store
            .drain_callbacks()
            .expect("canceled queue should drain"),
        Vec::<Value>::new()
    );
}

#[test]
fn store_reset_and_replacement_invalidate_callback_registries() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one;
        "#,
    )
    .expect("first callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("first root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("first callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("first callback should bind");
    let prepared = callback.prepare((1,)).expect("callback should prepare");

    store.reset_for_reuse();
    assert!(!callback.is_subscribed());
    assert!(matches!(
        store.enqueue_callback(prepared),
        Err(VmError::InvalidFrameState(
            "script callback belongs to another store"
        ))
    ));

    let replacement = crate::compile_source_for_repl(
        r#"
            fn double(value: int) -> int { value * 2 }
            double;
        "#,
    )
    .expect("replacement callback source should compile");
    let mut replacement_vm = Vm::new(replacement.program.with_local_count(replacement.locals));
    assert_eq!(
        replacement_vm.run().expect("replacement root should halt"),
        VmStatus::Halted
    );
    let replacement_callable = replacement_vm
        .stack()
        .last()
        .cloned()
        .expect("replacement callable result");
    store.replace_vm(replacement_vm);
    let replacement_callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(replacement_callable)
        .expect("replacement callback should bind");
    assert_eq!(
        replacement_callback
            .call(&mut store, (21,))
            .expect("replacement callback should run"),
        42
    );
}

#[test]
fn synchronous_callback_error_unwinds_before_next_invocation() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn fail() -> int { 1 / 0 }
            fn answer() -> int { 42 }
            fail;
            answer;
        "#,
    )
    .expect("callback error source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let fail_callable = vm.stack()[0].clone();
    let answer_callable = vm.stack()[1].clone();
    let mut store = crate::Store::from_vm(vm);
    let fail: crate::ScriptCallback<(), i64> = store
        .script_callback(fail_callable)
        .expect("failing callback should bind");
    let answer: crate::ScriptCallback<(), i64> = store
        .script_callback(answer_callable)
        .expect("answer callback should bind");

    assert!(matches!(
        fail.call(&mut store, ()),
        Err(VmError::DivisionByZero)
    ));
    assert_eq!(store.vm().call_depth(), 0);
    assert!(store.vm().execution_frames().is_empty());
    assert_eq!(
        answer
            .call(&mut store, ())
            .expect("next callback should run without reset"),
        42
    );
}

#[test]
fn final_script_callback_releases_capture_environment_once() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn make_callback() {
                let captured = 42;
                || captured
            }
            make_callback();
        "#,
    )
    .expect("capturing callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("capturing callback");
    let Value::Callable(callable_value) = &callable else {
        panic!("expected callable value");
    };
    let environment = callable_value
        .env
        .as_ref()
        .expect("capturing callback should own an environment");
    let weak_environment = Arc::downgrade(environment);
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(), i64> = store
        .script_callback(callable.clone())
        .expect("capturing callback should bind");

    store.vm_mut().shutdown();
    drop(callable);
    drop(store);
    assert!(weak_environment.upgrade().is_some());
    drop(callback);
    assert!(weak_environment.upgrade().is_none());
}

#[test]
fn store_resolves_only_exported_script_functions_by_name() {
    let compiled = crate::compile_source_for_repl(
        r#"
            pub fn add_one(value: int) -> int { value + 1 }
            fn private_double(value: int) -> int { value * 2 }
        "#,
    )
    .expect("exported callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback_by_name("add_one")
        .expect("exported callback should resolve");
    assert_eq!(callback.call(&mut store, (41,)).expect("export call"), 42);
    assert!(matches!(
        store.resolve_exported_callable("private_double"),
        Err(VmError::HostError(_))
    ));
}

#[test]
fn store_rejects_callable_values_from_another_store() {
    let first =
        crate::compile_source_for_repl("pub fn value() -> int { 11 }").expect("first store source");
    let mut first_store =
        crate::Store::from_vm(Vm::new(first.program.with_local_count(first.locals)));
    assert_eq!(first_store.run().expect("first root"), VmStatus::Halted);
    let foreign = first_store
        .resolve_exported_callable("value")
        .expect("first export");

    let second = crate::compile_source_for_repl("pub fn value() -> int { 22 }")
        .expect("second store source");
    let mut second_store =
        crate::Store::from_vm(Vm::new(second.program.with_local_count(second.locals)));
    assert_eq!(second_store.run().expect("second root"), VmStatus::Halted);
    let injected_slot = u8::try_from(second_store.vm().program().exported_callables[0].local_slot)
        .expect("test slot fits u8");
    second_store
        .vm_mut()
        .set_local(injected_slot, foreign.clone())
        .expect("foreign value can be injected into raw VM state");
    assert!(matches!(
        second_store.script_callback::<(), i64>(foreign),
        Err(VmError::InvalidFrameState(
            "script callable does not belong to this store"
        ))
    ));
}

#[test]
fn callback_queue_preserves_completed_results_and_remaining_events_after_error() {
    let compiled = crate::compile_source_for_repl(
        r#"
            pub fn first() -> int { 1 }
            pub fn fail() -> int { 1 / 0 }
            pub fn third() -> int { 3 }
        "#,
    )
    .expect("queue source");
    let mut store =
        crate::Store::from_vm(Vm::new(compiled.program.with_local_count(compiled.locals)));
    assert_eq!(store.run().expect("queue root"), VmStatus::Halted);
    let first: crate::ScriptCallback<(), i64> = store.script_callback_by_name("first").unwrap();
    let fail: crate::ScriptCallback<(), i64> = store.script_callback_by_name("fail").unwrap();
    let third: crate::ScriptCallback<(), i64> = store.script_callback_by_name("third").unwrap();
    store.enqueue_callback(first.prepare(()).unwrap()).unwrap();
    store.enqueue_callback(fail.prepare(()).unwrap()).unwrap();
    store.enqueue_callback(third.prepare(()).unwrap()).unwrap();

    assert!(matches!(
        store.drain_callbacks(),
        Err(VmError::DivisionByZero)
    ));
    assert_eq!(store.take_callback_result::<i64>().unwrap(), Some(1));
    assert_eq!(store.vm().queued_callable_count(), 1);
    assert_eq!(store.drain_callbacks().unwrap(), vec![Value::Int(3)]);
}

#[test]
fn typed_script_callback_can_wait_resume_and_return_to_host() {
    struct PendingCallbackHost;

    impl HostFunction for PendingCallbackHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Pending(811))
        }
    }

    let compiled = crate::compile_source_for_repl(
        r#"
            fn wait();
            fn callback() -> int {
                wait();
                42;
            }
            callback;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.register_function(Box::new(PendingCallbackHost));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(), i64> = store
        .script_callback(callable)
        .expect("typed callback should bind");

    assert_eq!(
        callback
            .start(&mut store, ())
            .expect("callback should start"),
        VmStatus::Waiting(811)
    );
    assert_eq!(store.vm().call_depth(), 1);
    store
        .vm_mut()
        .complete_host_op(811, Vec::new())
        .expect("host completion should succeed");
    assert_eq!(
        store.resume().expect("callback should resume"),
        VmStatus::Halted
    );
    assert_eq!(store.vm().call_depth(), 0);
    assert_eq!(
        store
            .take_callback_result::<i64>()
            .expect("typed callback result")
            .expect("callback should produce a result"),
        42
    );
}

#[test]
fn typed_script_callback_can_yield_resume_and_return_to_host() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn sum_to(limit: int) -> int {
                let mut index = 0;
                let mut total = 0;
                while index < limit {
                    total = total + index;
                    index = index + 1;
                }
                total;
            }
            sum_to;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("typed callback should bind");

    store.set_fuel(4);
    let mut status = callback
        .start(&mut store, (100,))
        .expect("callback should start");
    assert_eq!(store.vm().call_depth(), 1);
    let mut yields = 0usize;
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yields += 1;
                assert!(yields < 1_000, "callback should make progress");
                store.recharge(4).expect("fuel recharge should succeed");
                status = store.resume().expect("callback should resume");
            }
            VmStatus::Waiting(_) => panic!("unexpected waiting callback"),
        }
    }
    assert!(yields > 0);
    assert_eq!(store.vm().call_depth(), 0);
    assert_eq!(
        store
            .take_callback_result::<i64>()
            .expect("typed callback result")
            .expect("callback should produce a result"),
        4_950
    );
}

#[test]
fn vm_instances_share_decoded_instruction_metadata_across_program_clones() {
    let compiled = crate::compile_source(
        r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 16 {
            let a = i + 7;
            let b = a - 3;
            sum = sum + b;
            i = i + 1;
        }
        sum;
    "#,
    )
    .expect("source should compile");

    let base_program = compiled.program.with_local_count(compiled.locals.max(8));
    let vm_one = Vm::new(
        base_program
            .clone()
            .with_local_count(base_program.local_count + 8),
    );
    let vm_two = Vm::new(base_program.with_local_count(compiled.locals.max(8) + 16));

    assert!(
        Arc::ptr_eq(
            &vm_one.engine.decoded_instruction_data,
            &vm_two.engine.decoded_instruction_data
        ),
        "program clones should share decoded instruction metadata"
    );
}

#[test]
fn borrowed_map_iterator_state_is_released_after_break() {
    let compiled = crate::compile_source_with_flavor(
        r#"
        let values: map<int> = {a: 1, b: 2};
        for (key: string, value: int) in &values {
            key;
            value;
            break;
        }
        values;
        "#,
        crate::SourceFlavor::RustScript,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert!(
        vm.instance
            .map_iterators
            .iter()
            .flatten()
            .all(Option::is_none),
        "break must release every iterator owned by the exited loop"
    );
}

#[test]
fn borrowed_map_iterator_state_is_released_after_runtime_error() {
    let compiled = crate::compile_source_with_flavor(
        r#"
        let values: map<int> = {a: 1};
        let zero: int = 0;
        for (key: string, value: int) in &values {
            let failure: int = 1 / zero;
        }
        "#,
        crate::SourceFlavor::RustScript,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    vm.run().expect_err("program should fail at runtime");
    assert!(
        vm.instance
            .map_iterators
            .iter()
            .flatten()
            .all(Option::is_none),
        "runtime errors must release active map iterators"
    );
}

#[test]
fn map_iterator_ids_are_isolated_by_call_depth() {
    let program = Program::new(Vec::new(), vec![OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    let Value::Map(outer) = Value::map(vec![(Value::string("outer"), Value::Int(1))]) else {
        unreachable!();
    };
    let Value::Map(inner) = Value::map(vec![(Value::string("inner"), Value::Int(2))]) else {
        unreachable!();
    };

    vm.init_map_iterator(7, outer).expect("outer init");
    vm.instance.call_depth = 1;
    vm.init_map_iterator(7, inner).expect("inner init");
    assert!(vm.advance_map_iterator(7).expect("inner advance"));
    assert_eq!(
        vm.take_map_iterator_key(7).expect("inner key"),
        Value::string("inner")
    );
    vm.close_map_iterator(7).expect("inner close");

    vm.instance.call_depth = 0;
    assert!(vm.advance_map_iterator(7).expect("outer advance"));
    assert_eq!(
        vm.take_map_iterator_key(7).expect("outer key"),
        Value::string("outer")
    );
}

#[test]
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_resets_when_program_changes() {
    let _guard = native_cache_test_lock()
        .lock()
        .expect("native cache test lock should succeed");
    jit::runtime::clear_native_trace_cache_for_tests();

    let source_one = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 8 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;
    let source_two = r#"
        let mut k = 0;
        let mut total = 0;
        while k < 9 {
            total = total + k;
            k = k + 1;
        }
        total;
    "#;

    let compiled_one = crate::compile_source(source_one).expect("source one should compile");
    let compiled_two = crate::compile_source(source_two).expect("source two should compile");

    let mut vm_one = Vm::new(compiled_one.program);
    vm_one.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status_one = vm_one.run().expect("first vm should run");
    assert_eq!(status_one, VmStatus::Halted);
    let vm_one_trace_count = vm_one.jit_native_trace_count();
    assert!(
        vm_one_trace_count > 0,
        "first vm should produce native traces"
    );

    let (cache_program_after_one, cache_entries_after_one) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_one,
        Some(vm_one.engine.program_cache_key),
        "cache should be keyed to first program after first run"
    );
    assert_eq!(
        cache_entries_after_one, vm_one_trace_count,
        "cache entry count should match first program traces"
    );

    let mut vm_two = Vm::new(compiled_two.program);
    vm_two.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_ne!(
        vm_one.engine.program_cache_key, vm_two.engine.program_cache_key,
        "test programs should have different cache keys"
    );
    let status_two = vm_two.run().expect("second vm should run");
    assert_eq!(status_two, VmStatus::Halted);
    let vm_two_trace_count = vm_two.jit_native_trace_count();
    assert!(
        vm_two_trace_count > 0,
        "second vm should produce native traces"
    );

    let (cache_program_after_two, cache_entries_after_two) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_two,
        Some(vm_two.engine.program_cache_key),
        "cache should switch to second program key"
    );
    assert_eq!(
        cache_entries_after_two, vm_two_trace_count,
        "cache should only contain traces from the active program"
    );
}

#[test]
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_reuses_entries_for_same_program() {
    let _guard = native_cache_test_lock()
        .lock()
        .expect("native cache test lock should succeed");
    jit::runtime::clear_native_trace_cache_for_tests();

    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 10 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;
    let compiled = crate::compile_source(source).expect("source should compile");

    let mut vm_one = Vm::new(compiled.program.clone());
    vm_one.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status_one = vm_one.run().expect("first vm should run");
    assert_eq!(status_one, VmStatus::Halted);
    let vm_one_trace_count = vm_one.jit_native_trace_count();
    assert!(
        vm_one_trace_count > 0,
        "first vm should produce native traces"
    );

    let (cache_program_after_one, cache_entries_after_one) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_one,
        Some(vm_one.engine.program_cache_key),
        "cache should be keyed to the first program"
    );
    assert_eq!(
        cache_entries_after_one, vm_one_trace_count,
        "cache entry count should match first vm traces"
    );

    let mut vm_two = Vm::new(compiled.program);
    vm_two.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm_two.engine.program_cache_key, vm_one.engine.program_cache_key,
        "same program should use identical cache key"
    );

    let status_two = vm_two.run().expect("second vm should run");
    assert_eq!(status_two, VmStatus::Halted);
    let vm_two_trace_count = vm_two.jit_native_trace_count();
    assert_eq!(
        vm_two_trace_count, vm_one_trace_count,
        "same program should compile same native trace count"
    );

    let (cache_program_after_two, cache_entries_after_two) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_two,
        Some(vm_two.engine.program_cache_key),
        "cache key should remain the same for identical program"
    );
    assert_eq!(
        cache_entries_after_two, cache_entries_after_one,
        "cache entries should be reused instead of duplicated"
    );
}

fn step_once(vm: &mut Vm) -> VmResult<ExecOutcome> {
    let opcode = vm.read_u8()?;
    vm.execute_interpreter_instruction(opcode, true)
}

fn assert_shared_heap_backing(lhs: &Value, rhs: &Value) {
    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared string backing");
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared array backing");
        }
        (Value::Map(lhs), Value::Map(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared map backing");
        }
        _ => panic!("expected matching heap values, got lhs={lhs:?} rhs={rhs:?}"),
    }
}

#[test]
fn interpreter_metrics_track_operand_hint_hits_for_typed_add() {
    let mut operand_types = HashMap::new();
    operand_types.insert(4usize, (ValueType::Int, ValueType::Int));
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldloc as u8,
            1,
            OpCode::Add as u8,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2)
    .with_type_map(TypeMap {
        strict_types: true,
        local_types: vec![ValueType::Int, ValueType::Int],
        local_schemas: vec![None, None],
        callable_slots: vec![false, false],
        optional_slots: vec![false, false],
        operand_types,
    });
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Int(7))
        .expect("setting first local should succeed");
    vm.set_local(1, Value::Int(5))
        .expect("setting second local should succeed");

    let status = vm.run().expect("typed add program should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12)]);
    let metrics = vm.interpreter_metrics_snapshot();
    assert_eq!(metrics.operand_hint_hit_count, 1);
    assert_eq!(metrics.operand_hint_miss_count, 0);
}

#[test]
fn interpreter_uses_typed_builtin_fast_path_for_slice_calls() {
    let [call_lo, call_hi] = BuiltinFunction::Slice.call_index().to_le_bytes();
    let mut operand_types = HashMap::new();
    operand_types.insert(15usize, (ValueType::String, ValueType::Int));
    let program = Program::new(
        vec![Value::string("abcd"), Value::Int(1), Value::Int(2)],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            2,
            0,
            0,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            3,
            OpCode::Ret as u8,
        ],
    )
    .with_type_map(TypeMap {
        strict_types: true,
        local_types: Vec::new(),
        local_schemas: Vec::new(),
        callable_slots: Vec::new(),
        optional_slots: Vec::new(),
        operand_types,
    });
    let mut vm = Vm::new(program);

    let status = vm.run().expect("typed slice builtin should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("bc")]);
    let metrics = vm.interpreter_metrics_snapshot();
    assert_eq!(metrics.typed_builtin_fast_path_count, 1);
    assert_eq!(metrics.projection_fast_path_count, 0);
    assert_eq!(metrics.generic_builtin_call_count, 0);
}

#[test]
fn interpreter_superinstructions_use_local_type_hints() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1)
    .with_type_map(TypeMap {
        strict_types: true,
        local_types: vec![ValueType::Int],
        local_schemas: vec![None],
        callable_slots: vec![false],
        optional_slots: vec![false],
        operand_types: HashMap::new(),
    });
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Int(9))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("ldloc should fuse scalar sequence");

    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.locals[0], Value::Int(10));
    let metrics = vm.interpreter_metrics_snapshot();
    assert_eq!(metrics.scalar_superinstruction_count, 1);
    assert!(
        metrics.local_type_hint_hit_count >= 1,
        "expected local type hints to seed superinstruction execution"
    );
}

#[test]
fn interpreter_ldc_shares_string_constant_backing() {
    let program = Program::new(
        vec![Value::string("shared")],
        vec![OpCode::Ldc as u8, 0, 0, 0, 0, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);

    let outcome = step_once(&mut vm).expect("ldc should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    let constant = vm
        .program
        .constants
        .first()
        .expect("program should keep a constant");
    assert_shared_heap_backing(constant, &vm.stack()[0]);
}

#[test]
fn interpreter_dup_shares_array_backing() {
    let program = Program::new(vec![], vec![OpCode::Dup as u8, OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    vm.instance
        .stack
        .push(Value::array(vec![Value::Int(1), Value::Int(2)]));

    let outcome = step_once(&mut vm).expect("dup should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.stack().len(), 2);
    assert_shared_heap_backing(&vm.stack()[0], &vm.stack()[1]);
}

#[test]
fn shared_string_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::string("alive"))
        .expect("setting local should succeed");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(5)]);
}

#[test]
fn shared_array_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::array(vec![Value::Int(1), Value::Int(2)]))
        .expect("setting local should succeed");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn shared_map_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Count.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::map(vec![(Value::string("k"), Value::Int(9))]))
        .expect("setting local should succeed");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(1)]);
}

#[test]
fn interpreter_ldloc_preserves_local_slot() {
    let program =
        Program::new(vec![], vec![OpCode::Ldloc as u8, 0, OpCode::Ret as u8]).with_local_count(1);
    let mut vm = Vm::new(program);
    let map_value = Value::map(vec![(Value::string("k"), Value::Int(9))]);
    vm.set_local(0, map_value.clone())
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 2);
    assert_eq!(
        vm.instance.locals[0], map_value,
        "ldloc should leave local intact"
    );
    assert_eq!(
        vm.stack(),
        &[map_value],
        "stack should receive copied value"
    );
    assert_shared_heap_backing(&vm.instance.locals[0], &vm.stack()[0]);
    assert_eq!(vm.drop_contract_event_count(), 0);
}

#[test]
fn interpreter_explicit_move_sequence_clears_local_slot() {
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    let map_value = Value::map(vec![(Value::string("k"), Value::Int(9))]);
    vm.set_local(0, map_value.clone())
        .expect("setting local should succeed");

    let ldloc = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(ldloc, ExecOutcome::Continue));
    assert_eq!(vm.instance.locals[0], map_value);
    assert_eq!(vm.stack(), std::slice::from_ref(&map_value));
    assert_shared_heap_backing(&vm.instance.locals[0], &vm.stack()[0]);

    let ldc = step_once(&mut vm).expect("ldc should execute");
    assert!(matches!(ldc, ExecOutcome::Continue));
    assert_eq!(vm.stack(), &[map_value.clone(), Value::Null]);

    let stloc = step_once(&mut vm).expect("stloc should execute");
    assert!(matches!(stloc, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 9);
    assert_eq!(vm.instance.locals[0], Value::Null);
    assert_eq!(vm.stack(), &[map_value]);
}

#[test]
fn interpreter_fuses_ldloc_ldc_add_stloc_without_touching_stack() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Int(41))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("fused sequence should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 10, "fusion should consume ldc/add/stloc");
    assert_eq!(vm.instance.locals[0], Value::Int(41));
    assert_eq!(vm.instance.locals[1], Value::Int(42));
    assert!(
        vm.stack().is_empty(),
        "fusion should avoid transient stack traffic"
    );
}

#[test]
fn interpreter_fuses_ldloc_ldc_compare_brfalse() {
    let program = Program::new(
        vec![Value::Int(10), Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Clt as u8,
            OpCode::Brfalse as u8,
            15,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Int(42))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("fused compare should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(
        vm.instance.ip, 15,
        "fusion should jump directly to branch target"
    );
    assert!(
        vm.stack().is_empty(),
        "fusion should avoid bool stack traffic"
    );
}

#[test]
fn interpreter_fuses_generic_scalar_update_chain() {
    let program = Program::new(
        vec![Value::Int(3), Value::Int(7)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldloc as u8,
            1,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Mul as u8,
            OpCode::Add as u8,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Int(10))
        .expect("setting local should succeed");
    vm.set_local(1, Value::Int(4))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("generic chain should fuse");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 19);
    assert_eq!(vm.instance.locals[0], Value::Int(29));
    assert_eq!(vm.instance.locals[1], Value::Int(4));
    assert!(vm.stack().is_empty());
}

#[test]
fn interpreter_fuses_float_scalar_sequences() {
    let program = Program::new(
        vec![Value::Float(1.5), Value::Float(2.0)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Cgt as u8,
            OpCode::Brfalse as u8,
            24,
            0,
            0,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Float(1.0))
        .expect("setting local should succeed");

    let first = step_once(&mut vm).expect("float update should fuse");
    assert!(matches!(first, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 10);
    assert_eq!(vm.instance.locals[0], Value::Float(2.5));
    assert!(vm.stack().is_empty());

    let second = step_once(&mut vm).expect("float compare should fuse");
    assert!(matches!(second, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 23);
    assert!(vm.stack().is_empty());
}

#[test]
fn interpreter_does_not_fuse_ldloc_sequences_when_fuel_is_enabled() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::Int(41))
        .expect("setting local should succeed");
    vm.set_fuel(32);

    let opcode = vm.read_u8().expect("ldloc opcode should decode");
    let outcome = vm
        .execute_interpreter_instruction(opcode, false)
        .expect("ldloc should execute without fusion");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(
        vm.instance.ip, 2,
        "ldloc should advance only past its operand"
    );
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    assert_eq!(vm.instance.locals[0], Value::Int(41));
}

#[test]
fn interpreter_copy_like_ldloc_dup_stloc_shares_map_backing_with_fuel() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_local(0, Value::map(vec![(Value::string("k"), Value::Int(9))]))
        .expect("setting local should succeed");
    vm.set_fuel(32);

    let _ = step_once(&mut vm).expect("ldloc should execute");
    let _ = step_once(&mut vm).expect("dup should execute");
    let _ = step_once(&mut vm).expect("stloc should execute");

    assert_eq!(vm.stack().len(), 1);
    assert_shared_heap_backing(&vm.instance.locals[0], &vm.stack()[0]);
}

#[test]
fn interpreter_fuses_call_ret_without_fuel() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.instance.stack.push(Value::string("tail"));

    let outcome = step_once(&mut vm).expect("call should execute");
    assert!(matches!(outcome, ExecOutcome::Halted));
    assert_eq!(
        vm.instance.ip, 5,
        "tail-call fusion should consume trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn interpreter_fuses_call_ret_when_fuel_enabled_if_tail_tick_available() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(1);
    vm.instance.stack.push(Value::string("tail"));

    // `step_once` bypasses the outer run-loop pre-tick, so this fuel only covers fused `ret`.
    let call = step_once(&mut vm).expect("call should execute");
    assert!(matches!(call, ExecOutcome::Halted));
    assert_eq!(
        vm.instance.ip, 5,
        "tail-call fusion should consume trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(vm.get_fuel(), Some(0));
}

#[test]
fn interpreter_call_ret_fusion_preserves_ip_when_tail_tick_exhausted() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(0);
    vm.instance.stack.push(Value::string("tail"));

    let err = match step_once(&mut vm) {
        Ok(_) => panic!("tail tick should fail with out-of-fuel"),
        Err(err) => err,
    };
    assert!(matches!(err, VmError::OutOfFuel { .. }));
    assert_eq!(
        vm.instance.ip, 4,
        "ret must remain pending when tail tick cannot be charged"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn interpreter_call_ret_fusion_preserves_ip_when_epoch_deadline_is_reached() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");
    vm.instance.stack.push(Value::string("tail"));

    let err = match step_once(&mut vm) {
        Ok(_) => panic!("tail tick should fail with epoch deadline reached"),
        Err(err) => err,
    };
    assert!(matches!(err, VmError::EpochDeadlineReached { .. }));
    assert_eq!(
        vm.instance.ip, 4,
        "ret must remain pending when the epoch check trips during fused tail execution"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn run_consumes_two_ticks_for_call_ret_when_fuel_enabled() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(2);
    vm.instance.stack.push(Value::string("tail"));

    let status = vm.run().expect("run should complete");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.instance.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(
        vm.get_fuel(),
        Some(0),
        "call+ret should spend two ticks with fuel metering enabled"
    );
}

#[test]
fn run_yields_before_ret_in_call_ret_sequence_when_out_of_fuel() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(1);
    vm.instance.stack.push(Value::string("tail"));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.instance.ip, 4,
        "fuel exhaustion should happen before trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(vm.get_fuel(), Some(0));

    vm.add_fuel(1).expect("recharging fuel should succeed");
    let resumed = vm.resume().expect("resume should execute trailing ret");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.instance.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn run_yields_before_ret_in_call_ret_sequence_when_epoch_deadline_is_reached() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_epoch_check_interval(2)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    assert_eq!(vm.increment_epoch(), 1);
    vm.instance.stack.push(Value::string("tail"));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.instance.ip, 4,
        "epoch interruption should happen before trailing ret"
    );
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Epoch));
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let resumed = vm
        .resume()
        .expect("resume should auto re-arm the epoch deadline and execute trailing ret");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.instance.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn dropping_pre_cancelled_invocation_consumes_cancellation_at_the_boundary() {
    // Dropping an invocation with a pending typed cancellation retires that
    // invocation without manufacturing an unobservable terminal item. The VM
    // cancellation root is refreshed for immediate reuse.
    let compiled = crate::compile_source(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    )
    .expect("invocation source should compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().expect("root frame should halt"), VmStatus::Halted);

    vm.run_ctx
        .cancel(CancellationReason::Requested)
        .expect("pre-cancellation should be accepted");
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    {
        let _invocation = vm
            .start_invocation(callable.clone(), vec![])
            .expect("invocation should start");
    }
    assert!(
        vm.run_ctx.cancellation.reason().is_none(),
        "dropping the invocation must consume its pending cancellation"
    );

    let mut replacement = vm
        .start_invocation(callable, vec![])
        .expect("the vm should be reusable after the dropped invocation");
    assert!(matches!(
        replacement.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
}

#[test]
fn pre_cancelled_invocation_delivers_one_typed_error_then_fused_end() {
    // Functional contract of the pre-cancelled path: exactly one typed
    // Cancelled item, a fused end, and the cancellation consumed at the
    // invocation boundary (a later invocation runs normally).
    let compiled = crate::compile_source(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    )
    .expect("invocation source should compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().expect("root frame should halt"), VmStatus::Halted);

    vm.run_ctx
        .cancel(CancellationReason::Requested)
        .expect("pre-cancellation should be accepted");
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    {
        let mut invocation = vm
            .start_invocation(callable.clone(), vec![])
            .expect("invocation should start");

        match invocation.poll_next().expect("poll should succeed") {
            InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
                CancellationReason::Requested,
            )))) => {}
            other => panic!("expected a typed cancellation item, got {other:?}"),
        }
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ));
    }

    // A new invocation on the same VM must run to completion instead of
    // being cancelled on arrival.
    let mut second = vm
        .start_invocation(callable, vec![])
        .expect("a new invocation may start after fusion");
    match second.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42))))) => {}
        other => panic!("the second invocation must complete normally, got {other:?}"),
    }
    assert!(matches!(
        second.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn call_ret_fusion_pattern_requires_immediate_ret() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let with_ret = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm_with_ret = Vm::new(with_ret);
    vm_with_ret.instance.ip = 4;
    assert!(vm_with_ret.can_fuse_call_ret_pattern());

    let wrong_next = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Nop as u8],
    );
    let mut vm_wrong_next = Vm::new(wrong_next);
    vm_wrong_next.instance.ip = 4;
    assert!(!vm_wrong_next.can_fuse_call_ret_pattern());

    let no_next = Program::new(vec![], vec![OpCode::Call as u8, call_lo, call_hi, 1]);
    let mut vm_no_next = Vm::new(no_next);
    vm_no_next.instance.ip = 4;
    assert!(!vm_no_next.can_fuse_call_ret_pattern());
}

#[test]
fn program_cache_key_distinguishes_call_script_from_call_value() {
    // A direct-only call lowers to `CallScript`; the same call through a
    // materialized callable lowers to `CallValue`. The static cache identity
    // must treat the two programs as different even when their metadata
    // otherwise matches, because the native call boundary differs.
    let direct = crate::compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let materialized =
        crate::compile_source("fn add2(value: int) -> int { value + 2 } let f = add2; f(40);")
            .expect("materialized call source should compile");

    let mut direct_vm = Vm::new(direct.program);
    let mut materialized_vm = Vm::new(materialized.program);
    let direct_key = direct_vm.ensure_program_cache_key();
    let materialized_key = materialized_vm.ensure_program_cache_key();
    assert_ne!(
        direct_key, materialized_key,
        "CallScript and CallValue programs must not share cache identity"
    );

    // The same direct program reproduces the same key across VMs.
    let direct_repeat = crate::compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let mut repeat_vm = Vm::new(direct_repeat.program);
    assert_eq!(
        repeat_vm.ensure_program_cache_key(),
        direct_key,
        "identical programs must share cache identity"
    );
}

#[test]
fn native_callable_abi_version_covers_direct_script_calls() {
    // `CallScript` adds a new native boundary helper and exit contract, and
    // the JIT inline ownership bridge adds the root-callable materialization
    // helper; the native callable ABI revision must reflect both so every
    // directly coupled program/native cache is invalidated exactly once.
    assert_eq!(
        super::native::NATIVE_CALLABLE_ABI_VERSION,
        7,
        "native callable ABI revision must cover direct script call and root-callable materialization semantics"
    );
    let direct = crate::compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let mut vm = Vm::new(direct.program);
    let key = vm.ensure_program_cache_key();
    assert_ne!(key, 0, "cache key must be non-trivial");
}
