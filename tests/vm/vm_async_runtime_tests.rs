use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use vm::{
    BytecodeBuilder, CallOutcome, CancellationReason, HostAsyncBridge, HostFunction, HostFuture,
    HostImport, HostOpId, Program, Value, ValueType, Vm, VmError, VmResult, VmStatus,
};

type AsyncHostResult = Result<vm::CallReturn, VmError>;
type SharedAsyncOps = Arc<Mutex<TestAsyncOps>>;

#[derive(Default)]
struct TestAsyncOps {
    pending: HashMap<HostOpId, HostFuture>,
    cancellations: Vec<(HostOpId, CancellationReason)>,
}

impl TestAsyncOps {
    fn poll_submitted(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<AsyncHostResult> {
        let Some(future) = self.pending.get_mut(&op_id) else {
            return Poll::Ready(Err(VmError::HostError(format!(
                "unknown async host op {op_id}",
            ))));
        };
        match Pin::new(future).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending.remove(&op_id);
                Poll::Ready(match result {
                    Ok(output) => match output {
                        vm::HostFutureOutput::Return(value) => Ok(value),
                        vm::HostFutureOutput::VmCompletion(completion) => {
                            // A submitted-future completion runs later through
                            // the VM; for these tests all futures return a
                            // plain value, so this arm is only reached if an
                            // embedder submits a VmCompletion (unsupported here).
                            let _ = completion;
                            Ok(vm::CallReturn::none())
                        }
                    },
                    Err(error) => Err(error),
                })
            }
        }
    }
}

struct TestAsyncBridge {
    ops: SharedAsyncOps,
}

impl TestAsyncBridge {
    fn new(ops: SharedAsyncOps) -> Self {
        Self { ops }
    }
}

impl HostAsyncBridge for TestAsyncBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        let Ok(mut ops) = self.ops.lock() else {
            return Err(VmError::HostError(
                "test async ops lock poisoned".to_string(),
            ));
        };
        ops.pending.insert(op_id, future);
        Ok(())
    }

    fn poll_op(
        &mut self,
        op_id: HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<vm::CallReturn, VmError>> {
        let ops = self.ops.lock().expect("test async ops lock poisoned");
        if ops.pending.contains_key(&op_id) {
            Poll::Pending
        } else {
            Poll::Ready(Err(VmError::HostError(format!(
                "unknown async host op {op_id}",
            ))))
        }
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<Result<vm::HostFutureOutput, VmError>> {
        let polled = self
            .ops
            .lock()
            .expect("test async ops lock poisoned")
            .poll_submitted(op_id, cx);
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => Poll::Ready(result.map(vm::HostFutureOutput::Return)),
        }
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        let mut ops = self.ops.lock().expect("test async ops lock poisoned");
        ops.pending.remove(&op_id);
    }

    fn cancel_op_with_reason(&mut self, op_id: HostOpId, reason: CancellationReason) {
        let mut ops = self.ops.lock().expect("test async ops lock poisoned");
        ops.pending.remove(&op_id);
        ops.cancellations.push((op_id, reason));
    }
}

struct AsyncAddOneFunction {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl AsyncAddOneFunction {
    fn new(calls: Arc<AtomicUsize>, delay: Duration) -> Self {
        Self { calls, delay }
    }
}

impl HostFunction for AsyncAddOneFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = match args {
            [Value::Int(value)] => *value,
            _ => return Err(VmError::TypeMismatch("int")),
        };

        let previous = self.calls.fetch_add(1, Ordering::SeqCst);
        if previous != 0 {
            return Err(VmError::HostError(
                "async host call should not be replayed after pending".to_string(),
            ));
        }

        let delay = self.delay;
        // Submit a real HostFuture through the modern scope-operation path:
        // `submit_host_future` registers a HostFutureOperation in the current
        // ExecutionScope and returns its packed scope id. The future waits on
        // the tokio timer, then resolves to the incremented value.
        let future = async move {
            tokio::time::sleep(delay).await;
            Ok(vm::HostFutureOutput::returning(
                vec![Value::Int(value + 1)].into(),
            ))
        };
        vm.submit_host_future(Box::pin(future))
    }
}

fn build_async_import_program(input: i64) -> Program {
    let constants = vec![Value::Int(input)];
    let imports = vec![HostImport {
        name: "edge::async_add_one".to_string(),
        arity: 1,
        return_type: ValueType::Int,
        schema: None,
    }];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();
    Program::with_imports_and_debug(constants, bc.finish(), imports, None)
}

async fn drive_vm_to_halt(vm: &mut Vm) -> Result<(), VmError> {
    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(()),
            VmStatus::Yielded => {
                status = vm.resume()?;
            }
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op().await?;
                status = vm.resume()?;
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn async_host_call_waits_and_resumes_via_tokio_runtime() {
    let ops = Arc::new(Mutex::new(TestAsyncOps::default()));
    let calls = Arc::new(AtomicUsize::new(0));

    let mut vm =
        Vm::try_new(build_async_import_program(41)).expect("test VM construction must not fail");
    vm.bind_function(
        "edge::async_add_one",
        Box::new(AsyncAddOneFunction::new(
            calls.clone(),
            Duration::from_millis(25),
        )),
    );
    vm.set_async_bridge(Box::new(TestAsyncBridge::new(ops)));

    let status = vm.run().expect("vm should wait for async host operation");
    let op_id = match status {
        VmStatus::Waiting(op_id) => op_id,
        other => panic!("expected waiting status, got {other:?}"),
    };
    // Every pending host operation is a packed execution-scope operation id.
    assert!(
        vm::operation::OperationId::from_raw(op_id).is_ok(),
        "waiting op id must be a packed scope id, got {op_id}"
    );
    assert_eq!(vm.host_context().operation_count(), 1);
    assert!(
        op_id > u16::MAX as u64,
        "packed scope ids are far larger than the retired small external ids"
    );

    tokio::time::timeout(Duration::from_secs(1), vm.await_waiting_host_op())
        .await
        .expect("awaiting host operation timed out")
        .expect("host operation should complete");

    assert!(
        vm.waiting_host_op_id().is_none(),
        "vm should clear waiting state once op completes"
    );

    let status = vm
        .resume()
        .expect("vm should resume after host op completion");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[tokio::test(flavor = "current_thread")]
async fn reset_cancels_pending_host_bridge_operation() {
    let ops = Arc::new(Mutex::new(TestAsyncOps::default()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm =
        Vm::try_new(build_async_import_program(41)).expect("test VM construction must not fail");
    vm.bind_function(
        "edge::async_add_one",
        Box::new(AsyncAddOneFunction::new(calls, Duration::from_secs(60))),
    );
    vm.set_async_bridge(Box::new(TestAsyncBridge::new(ops.clone())));

    assert!(matches!(
        vm.run().expect("pending call"),
        VmStatus::Waiting(_)
    ));
    assert_eq!(ops.lock().unwrap().pending.len(), 1);
    vm.reset_for_reuse();
    let ops = ops.lock().unwrap();
    assert_eq!(ops.pending.len(), 0);
    assert_eq!(ops.cancellations.len(), 1);
    assert_eq!(ops.cancellations[0].1, CancellationReason::VmReset);
    drop(ops);
    assert_eq!(vm.waiting_host_op_id(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn user_cancellation_reaches_host_bridge_and_clears_waiting_state() {
    let ops = Arc::new(Mutex::new(TestAsyncOps::default()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm =
        Vm::try_new(build_async_import_program(41)).expect("test VM construction must not fail");
    vm.bind_function(
        "edge::async_add_one",
        Box::new(AsyncAddOneFunction::new(calls, Duration::from_secs(60))),
    );
    vm.set_async_bridge(Box::new(TestAsyncBridge::new(ops.clone())));

    let op_id = match vm.run().expect("pending call") {
        VmStatus::Waiting(op_id) => op_id,
        status => panic!("expected waiting status, got {status:?}"),
    };
    let error = vm
        .wait_for_host_op_blocking_with_cancel(|| true)
        .expect_err("user cancellation should stop the wait");
    assert!(error.to_string().contains("cancelled"));
    let ops = ops.lock().unwrap();
    assert_eq!(ops.pending.len(), 0);
    assert_eq!(
        ops.cancellations,
        vec![(op_id, CancellationReason::Requested)]
    );
    assert_eq!(vm.waiting_host_op_id(), None);
    assert!(op_id > 0);
}

#[tokio::test(flavor = "current_thread")]
async fn vm_waiting_on_async_host_op_does_not_block_tokio_tasks() {
    let ops = Arc::new(Mutex::new(TestAsyncOps::default()));
    let calls = Arc::new(AtomicUsize::new(0));

    let mut vm =
        Vm::try_new(build_async_import_program(5)).expect("test VM construction must not fail");
    vm.bind_function(
        "edge::async_add_one",
        Box::new(AsyncAddOneFunction::new(
            calls.clone(),
            Duration::from_millis(40),
        )),
    );
    vm.set_async_bridge(Box::new(TestAsyncBridge::new(ops)));

    let ticks = Arc::new(AtomicUsize::new(0));
    let stop_ticker = Arc::new(AtomicBool::new(false));
    let ticker_ticks = ticks.clone();
    let ticker_stop = stop_ticker.clone();
    let ticker = tokio::spawn(async move {
        while !ticker_stop.load(Ordering::Relaxed) {
            ticker_ticks.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    tokio::time::timeout(Duration::from_secs(1), drive_vm_to_halt(&mut vm))
        .await
        .expect("driving vm to completion timed out")
        .expect("vm should run to completion");

    let observed_ticks = ticks.load(Ordering::Relaxed);
    stop_ticker.store(true, Ordering::Relaxed);
    ticker.await.expect("ticker task should exit cleanly");

    assert!(
        observed_ticks > 0,
        "expected tokio task to make progress while vm was waiting on async host op"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
}
