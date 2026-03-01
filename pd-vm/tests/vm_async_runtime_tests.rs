#![cfg(feature = "runtime")]

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use tokio::sync::oneshot;
use vm::{
    BytecodeBuilder, CallOutcome, HostAsyncBridge, HostFunction, HostImport, HostOpId, Program,
    Value, Vm, VmError, VmStatus,
};

type AsyncHostResult = Result<Vec<Value>, VmError>;
type SharedAsyncOps = Arc<Mutex<TestAsyncOps>>;

#[derive(Default)]
struct TestAsyncOps {
    pending: HashMap<HostOpId, oneshot::Receiver<AsyncHostResult>>,
}

impl TestAsyncOps {
    fn schedule_future<F>(&mut self, vm: &mut Vm, future: F) -> Result<HostOpId, VmError>
    where
        F: Future<Output = AsyncHostResult> + Send + 'static,
    {
        let op_id = vm.allocate_host_op_id();
        let (sender, receiver) = oneshot::channel();
        self.pending.insert(op_id, receiver);
        tokio::spawn(async move {
            let _ = sender.send(future.await);
        });
        Ok(op_id)
    }

    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<AsyncHostResult> {
        let poll_state = {
            let receiver = match self.pending.get_mut(&op_id) {
                Some(receiver) => receiver,
                None => {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "unknown async host op {op_id}",
                    ))));
                }
            };
            Pin::new(receiver).poll(cx)
        };

        match poll_state {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.pending.remove(&op_id);
                Poll::Ready(result)
            }
            Poll::Ready(Err(_)) => {
                self.pending.remove(&op_id);
                Poll::Ready(Err(VmError::HostError(format!(
                    "async host op {op_id} was cancelled",
                ))))
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
    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<Result<Vec<Value>, VmError>> {
        self.ops
            .lock()
            .expect("test async ops lock poisoned")
            .poll_op(op_id, cx)
    }
}

struct AsyncAddOneFunction {
    ops: SharedAsyncOps,
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl AsyncAddOneFunction {
    fn new(ops: SharedAsyncOps, calls: Arc<AtomicUsize>, delay: Duration) -> Self {
        Self { ops, calls, delay }
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
        let mut ops = self.ops.lock().expect("test async ops lock poisoned");
        let op_id = ops.schedule_future(vm, async move {
            tokio::time::sleep(delay).await;
            Ok(vec![Value::Int(value + 1)])
        })?;
        Ok(CallOutcome::Pending(op_id))
    }
}

fn build_async_import_program(input: i64) -> Program {
    let constants = vec![Value::Int(input)];
    let imports = vec![HostImport {
        name: "edge::async_add_one".to_string(),
        arity: 1,
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

    let mut vm = Vm::new(build_async_import_program(41));
    vm.bind_function(
        "edge::async_add_one",
        Box::new(AsyncAddOneFunction::new(
            ops.clone(),
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
    assert_eq!(op_id, 1);

    tokio::time::timeout(Duration::from_secs(1), vm.await_waiting_host_op())
        .await
        .expect("awaiting host operation timed out")
        .expect("host operation should complete");

    assert!(
        vm.waiting_host_op_id().is_none(),
        "vm should clear waiting state once op completes"
    );

    let status = vm.resume().expect("vm should resume after host op completion");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[tokio::test(flavor = "current_thread")]
async fn vm_waiting_on_async_host_op_does_not_block_tokio_tasks() {
    let ops = Arc::new(Mutex::new(TestAsyncOps::default()));
    let calls = Arc::new(AtomicUsize::new(0));

    let mut vm = Vm::new(build_async_import_program(5));
    vm.bind_function(
        "edge::async_add_one",
        Box::new(AsyncAddOneFunction::new(
            ops.clone(),
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
