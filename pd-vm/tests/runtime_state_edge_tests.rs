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
