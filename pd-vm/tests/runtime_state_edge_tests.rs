#![cfg(feature = "runtime")]
mod common;
use common::*;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};

struct PendingOnce {
    call_count: Arc<AtomicUsize>,
    op_id: u64,
}

impl HostFunction for PendingOnce {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Pending(self.op_id))
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

#[test]
fn run_while_waiting_does_not_replay_pending_host_call() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();
    let program = Program::new(Vec::new(), bc.finish());

    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
        op_id: 55,
    }));

    let first = vm.run().expect("first run should wait");
    assert_eq!(first, VmStatus::Waiting(55));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second = vm.run().expect("second run should stay waiting");
    assert_eq!(second, VmStatus::Waiting(55));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "host call should not be replayed while pending"
    );

    vm.complete_host_op(55, vec![Value::Int(9)])
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

    let mut vm = Vm::new(program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::new(AtomicUsize::new(0)),
        op_id: 99,
    }));

    let status = vm.run().expect("first run should wait");
    assert_eq!(status, VmStatus::Waiting(99));

    let wrong_err = vm
        .complete_host_op(77, vec![Value::Int(1)])
        .expect_err("wrong op id should fail");
    assert!(
        wrong_err
            .to_string()
            .contains("host op 77 completed while vm waits on 99"),
        "unexpected error: {wrong_err}"
    );
    assert_eq!(vm.waiting_host_op_id(), Some(99));

    vm.complete_host_op(99, vec![Value::Int(4)])
        .expect("matching op id should complete");
    assert_eq!(vm.waiting_host_op_id(), None);

    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let missing_err = vm
        .complete_host_op(99, vec![Value::Int(2)])
        .expect_err("completing when not waiting should fail");
    assert!(
        missing_err
            .to_string()
            .contains("host op 99 completed but vm is not waiting on any op"),
        "unexpected error: {missing_err}"
    );
}

#[test]
fn poll_waiting_host_op_reports_missing_async_bridge() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();
    let program = Program::new(Vec::new(), bc.finish());

    let mut vm = Vm::new(program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::new(AtomicUsize::new(0)),
        op_id: 321,
    }));

    let status = vm.run().expect("run should wait");
    assert_eq!(status, VmStatus::Waiting(321));

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match vm.poll_waiting_host_op(&mut cx) {
        Poll::Ready(Err(err)) => {
            assert!(
                err.to_string()
                    .contains("vm waiting on host op 321 without an async bridge"),
                "unexpected error: {err}"
            );
        }
        other => panic!("expected missing bridge error, got {other:?}"),
    }
    assert_eq!(
        vm.waiting_host_op_id(),
        Some(321),
        "missing bridge poll should keep waiting state intact"
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
    let mut vm = Vm::new(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
        op_id: 700,
    }));

    let first = vm.run().expect("first run should wait");
    assert_eq!(first, VmStatus::Waiting(700));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(vm.locals()[a_index as usize], Value::Null);
    assert_eq!(
        vm.locals()[b_index as usize],
        Value::String("payload".to_string())
    );

    let second = vm.run().expect("second run should still wait");
    assert_eq!(second, VmStatus::Waiting(700));
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
        Value::String("payload".to_string()),
        "moved target local should stay intact while waiting"
    );

    vm.complete_host_op(700, Vec::new())
        .expect("host completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("payload".to_string())]);
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
    let mut vm = Vm::new(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
        op_id: 701,
    }));

    let first = vm.run().expect("first run should wait");
    assert_eq!(first, VmStatus::Waiting(701));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let waiting_locals = vm.locals().to_vec();

    let second = vm.run().expect("second run should still wait");
    assert_eq!(second, VmStatus::Waiting(701));
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

    vm.complete_host_op(701, Vec::new())
        .expect("host completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(0)));
    assert!(
        vm.locals().iter().all(|value| matches!(value, Value::Null)),
        "expected closure and inline call frames to clear after resume, got {:?}",
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
    let mut vm = Vm::new(compiled.program);
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
    let live_map = Value::Map(vec![(
        Value::String("k".to_string()),
        Value::Array(vec![Value::Int(1)]),
    )]);
    let live_stack_value = Value::String("live".to_string());
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.ret();

    let program = Program::new(vec![live_map, live_stack_value], bc.finish());
    let mut vm = Vm::new(program);
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
    let mut vm = Vm::new(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
        op_id: 702,
    }));

    let first = vm.run().expect("first run should wait");
    assert_eq!(first, VmStatus::Waiting(702));
    let after_first = vm.drop_contract_event_count();

    let second = vm.run().expect("second run should stay waiting");
    assert_eq!(second, VmStatus::Waiting(702));
    assert_eq!(
        vm.drop_contract_event_count(),
        after_first,
        "while waiting, VM should not replay drop-side effects"
    );

    vm.complete_host_op(702, Vec::new())
        .expect("host completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert!(
        vm.drop_contract_event_count() >= after_first,
        "resume may advance drop state, but must not regress"
    );
}
