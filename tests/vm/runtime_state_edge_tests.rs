#[path = "../common/mod.rs"]
mod common;
use common::*;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::task::{Context, Poll};

/// A dynamic host that returns `Pending(op_id)` for a real scope-registered
/// operation started on the first call. Tests complete it through
/// `complete_host_op`.
struct PendingOnce {
    call_count: Arc<AtomicUsize>,
}

impl HostFunction for PendingOnce {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let op_id = vm
            .host_context()
            .start_operation(vm::operation::OperationSpec::new(PendingOperationDriver))
            .expect("start pending scope operation");
        Ok(CallOutcome::Pending(op_id.raw()))
    }
}

const FABRICATED_PENDING_ID: vm::HostOpId = 321;

struct FabricatedDynamicPending;

impl HostFunction for FabricatedDynamicPending {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Pending(FABRICATED_PENDING_ID))
    }
}

fn fabricated_static_pending(_vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    Ok(CallOutcome::Pending(FABRICATED_PENDING_ID))
}

struct FabricatedStackPending;

impl vm::HostStackFunction for FabricatedStackPending {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Pending(FABRICATED_PENDING_ID))
    }
}

struct FabricatedArgsPending;

impl HostArgsFunction for FabricatedArgsPending {
    fn call(&mut self, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Pending(FABRICATED_PENDING_ID))
    }
}

struct PendingById {
    op_id: vm::HostOpId,
}

impl HostFunction for PendingById {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Pending(self.op_id))
    }
}

struct RecordingPendingOperation {
    cancellations: Arc<Mutex<Vec<vm::operation::OperationCancelReason>>>,
}

impl vm::operation::HostOperation for RecordingPendingOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<vm::operation::OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(
        &mut self,
        reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        self.cancellations.lock().unwrap().push(reason);
        Ok(())
    }
}

struct RecordingPendingHost {
    cancellations: Arc<Mutex<Vec<vm::operation::OperationCancelReason>>>,
}

impl HostFunction for RecordingPendingHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        let op_id = vm
            .host_context()
            .start_operation(vm::operation::OperationSpec::new(
                RecordingPendingOperation {
                    cancellations: Arc::clone(&self.cancellations),
                },
            ))
            .expect("start recording pending operation");
        Ok(CallOutcome::Pending(op_id.raw()))
    }
}

struct ReadyOperation;

impl vm::operation::HostOperation for ReadyOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<vm::operation::OperationResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn cancel(
        &mut self,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        Ok(())
    }
}

struct ReadyPendingHost {
    op_id: Arc<AtomicU64>,
}

impl HostFunction for ReadyPendingHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        let op_id = vm
            .host_context()
            .start_operation(vm::operation::OperationSpec::new(ReadyOperation))
            .expect("start ready operation")
            .raw();
        self.op_id.store(op_id, Ordering::SeqCst);
        Ok(CallOutcome::Pending(op_id))
    }
}

struct StoredPendingHost {
    op_id: Arc<AtomicU64>,
}

impl HostFunction for StoredPendingHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Pending(self.op_id.load(Ordering::SeqCst)))
    }
}

fn pending_call_program() -> Program {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();
    Program::new(Vec::new(), bc.finish())
}

fn assert_fabricated_pending_rejected(bind: impl FnOnce(&mut Vm)) {
    let mut vm = new_runtime_state_vm(pending_call_program());
    bind(&mut vm);

    let error = vm
        .run()
        .expect_err("fabricated pending id must be rejected");
    assert!(
        matches!(
            error,
            vm::VmError::Operation(vm::operation::OperationError { .. })
        ),
        "expected a typed operation error, got {error:?}"
    );
    assert!(
        error.to_string().contains("321"),
        "error should carry the fabricated id: {error}"
    );
    assert_eq!(
        vm.waiting_host_op_id(),
        None,
        "VM must not enter Waiting on a fabricated id"
    );
}

fn new_runtime_state_vm(program: Program) -> Vm {
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_drop_contract_events_enabled(true);
    vm
}

#[test]
fn run_while_waiting_does_not_replay_pending_host_call() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();
    let program = Program::new(Vec::new(), bc.finish());

    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = new_runtime_state_vm(program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
    }));

    let first = vm.run().expect("first run should wait");
    let VmStatus::Waiting(op_id) = first else {
        panic!("expected waiting status, got {first:?}");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second = vm.run().expect("second run should stay waiting");
    assert_eq!(second, VmStatus::Waiting(op_id));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "host call should not be replayed while pending"
    );

    vm.complete_host_op(op_id, vec![Value::Int(9)])
        .expect("host op completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(9)]);
}

#[test]
fn complete_host_op_rejects_wrong_and_missing_ids() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();
    let program = Program::new(Vec::new(), bc.finish());

    let mut vm = new_runtime_state_vm(program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::new(AtomicUsize::new(0)),
    }));

    let status = vm.run().expect("first run should wait");
    let VmStatus::Waiting(op_id) = status else {
        panic!("expected waiting status, got {status:?}");
    };

    let wrong_err = vm
        .complete_host_op(77, vec![Value::Int(1)])
        .expect_err("wrong op id should fail");
    assert!(
        wrong_err
            .to_string()
            .contains("host op 77 completed while vm waits"),
        "unexpected error: {wrong_err}"
    );
    assert_eq!(vm.waiting_host_op_id(), Some(op_id));

    vm.complete_host_op(op_id, vec![Value::Int(4)])
        .expect("matching op id should complete");
    assert_eq!(vm.waiting_host_op_id(), None);

    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let missing_err = vm
        .complete_host_op(op_id, vec![Value::Int(2)])
        .expect_err("completing when not waiting should fail");
    assert!(
        missing_err.to_string().contains("not waiting"),
        "unexpected error: {missing_err}"
    );
}

/// A dynamic bound host returning a fabricated Pending id is rejected before
/// the VM enters Waiting.
#[test]
fn fabricated_dynamic_pending_id_is_rejected_before_waiting() {
    assert_fabricated_pending_rejected(|vm| {
        vm.register_function(Box::new(FabricatedDynamicPending));
    });
}

/// A static VM-aware bound host follows the same scope-membership validation.
#[test]
fn fabricated_static_pending_id_is_rejected_before_waiting() {
    assert_fabricated_pending_rejected(|vm| {
        vm.register_static_function(fabricated_static_pending);
    });
}

/// A borrowed-stack bound host follows the same scope-membership validation.
#[test]
fn fabricated_stack_pending_id_is_rejected_before_waiting() {
    assert_fabricated_pending_rejected(|vm| {
        vm.register_stack_function(Box::new(FabricatedStackPending));
    });
}

/// An args-only bound host follows the same scope-membership validation.
#[test]
fn fabricated_args_pending_id_is_rejected_before_waiting() {
    assert_fabricated_pending_rejected(|vm| {
        vm.register_args_function(Box::new(FabricatedArgsPending));
    });
}

#[test]
fn stale_pending_id_is_rejected_before_waiting_or_cancelling_current_op() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.call(1, 0);
    bc.call(2, 0);
    bc.ret();

    let stale_id = Arc::new(AtomicU64::new(0));
    let current_cancellations = Arc::new(Mutex::new(Vec::new()));
    let mut vm = new_runtime_state_vm(Program::new(Vec::new(), bc.finish()));
    vm.register_function(Box::new(ReadyPendingHost {
        op_id: Arc::clone(&stale_id),
    }));
    vm.register_function(Box::new(RecordingPendingHost {
        cancellations: Arc::clone(&current_cancellations),
    }));
    vm.register_function(Box::new(StoredPendingHost {
        op_id: Arc::clone(&stale_id),
    }));

    let first = vm.run().expect("ready operation should enter Waiting");
    let VmStatus::Waiting(first_id) = first else {
        panic!("expected first waiting status, got {first:?}");
    };
    assert_eq!(first_id, stale_id.load(Ordering::SeqCst));
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let poll_error = match vm.poll_waiting_host_op(&mut cx) {
        Poll::Ready(Err(error)) => error,
        other => panic!("ready operation should fail without a result adapter, got {other:?}"),
    };
    assert!(poll_error.to_string().contains("without a result"));
    assert_eq!(vm.waiting_host_op_id(), None);

    let second = vm.resume().expect("second operation should enter Waiting");
    let VmStatus::Waiting(current_id) = second else {
        panic!("expected current waiting status, got {second:?}");
    };
    let completion_error = vm
        .complete_host_op(first_id, Vec::new())
        .expect_err("stale id must not complete the current operation");
    assert!(completion_error.to_string().contains("while vm waits"));
    assert_eq!(vm.waiting_host_op_id(), Some(current_id));
    assert!(current_cancellations.lock().unwrap().is_empty());

    vm.complete_host_op(current_id, Vec::new())
        .expect("current operation should complete");
    assert_eq!(
        current_cancellations.lock().unwrap().as_slice(),
        &[vm::operation::OperationCancelReason::Requested]
    );

    let stale_error = vm
        .resume()
        .expect_err("a bound host must reject the consumed operation id");
    let vm::VmError::Operation(stale_error) = stale_error else {
        panic!("expected stale operation error, got {stale_error:?}");
    };
    assert_eq!(
        stale_error.code(),
        vm::operation::OperationErrorCode::OperationStale
    );
    assert_eq!(vm.waiting_host_op_id(), None);
}

#[test]
fn foreign_pending_id_is_rejected_without_cancelling_either_operation() {
    let foreign_cancellations = Arc::new(Mutex::new(Vec::new()));
    let mut foreign_vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("foreign VM construction must succeed");
    let foreign_id = foreign_vm
        .host_context()
        .start_operation(vm::operation::OperationSpec::new(
            RecordingPendingOperation {
                cancellations: Arc::clone(&foreign_cancellations),
            },
        ))
        .expect("start foreign operation")
        .raw();

    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.call(1, 0);
    bc.ret();
    let current_cancellations = Arc::new(Mutex::new(Vec::new()));
    let mut vm = new_runtime_state_vm(Program::new(Vec::new(), bc.finish()));
    vm.register_function(Box::new(RecordingPendingHost {
        cancellations: Arc::clone(&current_cancellations),
    }));
    vm.register_function(Box::new(PendingById { op_id: foreign_id }));

    let first = vm.run().expect("current operation should enter Waiting");
    let VmStatus::Waiting(current_id) = first else {
        panic!("expected current waiting status, got {first:?}");
    };
    let completion_error = vm
        .complete_host_op(foreign_id, Vec::new())
        .expect_err("foreign id must not complete the current operation");
    assert!(completion_error.to_string().contains("while vm waits"));
    assert_eq!(vm.waiting_host_op_id(), Some(current_id));
    assert!(current_cancellations.lock().unwrap().is_empty());
    assert!(foreign_cancellations.lock().unwrap().is_empty());

    vm.complete_host_op(current_id, Vec::new())
        .expect("current operation should complete");
    let foreign_error = vm
        .resume()
        .expect_err("a bound host must reject a foreign operation id");
    let vm::VmError::Operation(foreign_error) = foreign_error else {
        panic!("expected foreign operation error, got {foreign_error:?}");
    };
    assert_eq!(
        foreign_error.code(),
        vm::operation::OperationErrorCode::OperationWrongRegistry
    );
    assert_eq!(vm.waiting_host_op_id(), None);
    assert!(foreign_cancellations.lock().unwrap().is_empty());
}

#[test]
fn wrong_live_completion_does_not_cancel_unrelated_operation() {
    let waiting_cancellations = Arc::new(Mutex::new(Vec::new()));
    let mut vm = new_runtime_state_vm(pending_call_program());
    vm.register_function(Box::new(RecordingPendingHost {
        cancellations: Arc::clone(&waiting_cancellations),
    }));
    let first = vm.run().expect("bound operation should enter Waiting");
    let VmStatus::Waiting(waiting_id) = first else {
        panic!("expected waiting status, got {first:?}");
    };

    let unrelated_cancellations = Arc::new(Mutex::new(Vec::new()));
    let unrelated_id = vm
        .host_context()
        .start_operation(vm::operation::OperationSpec::new(
            RecordingPendingOperation {
                cancellations: Arc::clone(&unrelated_cancellations),
            },
        ))
        .expect("start unrelated current-scope operation")
        .raw();
    let error = vm
        .complete_host_op(unrelated_id, Vec::new())
        .expect_err("wrong live id must not complete the waiting operation");
    assert!(error.to_string().contains("while vm waits"));
    assert_eq!(vm.waiting_host_op_id(), Some(waiting_id));
    assert!(waiting_cancellations.lock().unwrap().is_empty());
    assert!(unrelated_cancellations.lock().unwrap().is_empty());

    vm.complete_host_op(waiting_id, Vec::new())
        .expect("matching operation should complete");
    assert_eq!(
        waiting_cancellations.lock().unwrap().as_slice(),
        &[vm::operation::OperationCancelReason::Requested]
    );
    assert!(unrelated_cancellations.lock().unwrap().is_empty());
    assert_eq!(vm.resume().expect("program should halt"), VmStatus::Halted);

    vm.reset_for_reuse();
    assert_eq!(
        unrelated_cancellations.lock().unwrap().as_slice(),
        &[vm::operation::OperationCancelReason::VmReset],
        "reset must cancel the unrelated operation through its own driver"
    );
}

#[test]
fn dropping_vm_cancels_bound_custom_operation_with_vm_drop_reason() {
    let cancellations = Arc::new(Mutex::new(Vec::new()));
    let mut vm = new_runtime_state_vm(pending_call_program());
    vm.register_function(Box::new(RecordingPendingHost {
        cancellations: Arc::clone(&cancellations),
    }));
    assert!(matches!(
        vm.run().expect("bound operation should enter Waiting"),
        VmStatus::Waiting(_)
    ));

    drop(vm);
    assert_eq!(
        cancellations.lock().unwrap().as_slice(),
        &[vm::operation::OperationCancelReason::VmDrop]
    );
}

#[test]
fn waiting_host_op_preserves_single_drop_state_for_moved_locals() {
    let source = r#"
        fn wait();

        let a = "payload";
        let b = a;
        wait();
        b;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let a_index = debug.local_index("a").expect("a binding should exist");
    let b_index = debug.local_index("b").expect("b binding should exist");

    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = new_runtime_state_vm(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
    }));

    let first = vm.run().expect("first run should wait");
    let VmStatus::Waiting(op_id) = first else {
        panic!("expected waiting status, got {first:?}");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(vm.locals()[a_index as usize], Value::Null);
    assert_eq!(vm.locals()[b_index as usize], Value::string("payload"));

    let second = vm.run().expect("second run should still wait");
    assert_eq!(second, VmStatus::Waiting(op_id));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "host call should not be replayed while pending"
    );
    assert_eq!(
        vm.locals()[a_index as usize],
        Value::Null,
        "source local should stay dropped exactly once while waiting"
    );
    assert_eq!(
        vm.locals()[b_index as usize],
        Value::string("payload"),
        "moved target local should stay intact while waiting"
    );

    vm.complete_host_op(op_id, Vec::new())
        .expect("host completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("payload")]);
    assert_eq!(vm.locals()[a_index as usize], Value::Null);
}

#[test]
fn waiting_host_op_preserves_interprocedural_closure_state_then_clears_on_resume() {
    let source = r#"
        fn wait();
        fn apply_after_wait(func, value) {
            wait();
            func(value);
        }

        let seed = "!";
        let closure = |x| x + seed;
        apply_after_wait(closure, "a");
        0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = new_runtime_state_vm(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
    }));

    let first = vm.run().expect("first run should wait");
    let VmStatus::Waiting(op_id) = first else {
        panic!("expected waiting status, got {first:?}");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let waiting_locals = vm.locals().to_vec();

    let second = vm.run().expect("second run should still wait");
    assert_eq!(second, VmStatus::Waiting(op_id));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "host call should not be replayed while pending"
    );
    assert_eq!(
        vm.locals(),
        waiting_locals.as_slice(),
        "waiting runs should not mutate closure/call-frame state"
    );

    vm.complete_host_op(op_id, Vec::new())
        .expect("host completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(0)));
    assert!(
        vm.locals().iter().all(|value| {
            matches!(value, Value::Null)
                || matches!(
                    value,
                    Value::Callable(callable)
                        if callable.kind == vm::CallableKind::FunctionItem
                            && callable.env.is_none()
                )
        }),
        "expected closure and transient call-frame state to clear after resume, got {:?}",
        vm.locals()
    );
}

#[test]
fn drop_contract_counts_overwrites_and_reset_clears_counter() {
    let source = r#"
        let mut value = { payload: [1, 2, 3], name: "a" };
        value = { payload: [4], name: "b" };
        null;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = new_runtime_state_vm(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    let after_run = vm.drop_contract_event_count();
    assert!(
        after_run > 0,
        "expected drop contract to observe overwrite cleanup, got {after_run}"
    );

    vm.reset_for_reuse();
    let after_reset = vm.drop_contract_event_count();
    assert_eq!(
        after_reset, 0,
        "reset_for_reuse should clear drop accounting"
    );
}

#[test]
fn reset_for_reuse_counts_cleanup_drops_from_live_state() {
    let live_map = Value::map(vec![(
        Value::string("k"),
        Value::array(vec![Value::Int(1)]),
    )]);
    let live_stack_value = Value::string("live");
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.ret();

    let program = Program::new(vec![live_map, live_stack_value], bc.finish());
    let mut vm = new_runtime_state_vm(program);
    vm.set_fuel(3);

    let status = vm.run().expect("run should yield before cleanup");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.drop_contract_event_count(),
        0,
        "cleanup should not have run before reset"
    );

    vm.reset_for_reuse();
    assert_eq!(
        vm.drop_contract_event_count(),
        5,
        "reset should count drops fired while clearing live locals and stack"
    );
    assert!(
        vm.stack().is_empty(),
        "reset should clear live stack values, got {:?}",
        vm.stack()
    );
    assert_eq!(
        vm.locals(),
        &[Value::Null],
        "reset should clear live locals, got {:?}",
        vm.locals()
    );
}

#[test]
fn waiting_run_does_not_replay_drop_contract_events() {
    let source = r#"
        fn wait();
        let mut value = { payload: [1, 2], name: "x" };
        wait();
        value = { payload: [3], name: "y" };
        0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = new_runtime_state_vm(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
    }));

    let first = vm.run().expect("first run should wait");
    let VmStatus::Waiting(op_id) = first else {
        panic!("expected waiting status, got {first:?}");
    };
    let after_first = vm.drop_contract_event_count();

    let second = vm.run().expect("second run should stay waiting");
    assert_eq!(second, VmStatus::Waiting(op_id));
    assert_eq!(
        vm.drop_contract_event_count(),
        after_first,
        "while waiting, VM should not replay drop-side effects"
    );

    vm.complete_host_op(op_id, Vec::new())
        .expect("host completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert!(
        vm.drop_contract_event_count() >= after_first,
        "resume may advance drop state, but must not regress"
    );
}
