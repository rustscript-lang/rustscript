use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use vm::{
    CallOutcome, CallReturn, CancellationReason, HostAsyncBridge, HostFunction, HostFuture,
    HostFutureOutput, HostOpId, HostStreamAction, HostStreamDriver, HostStreamPoll,
    InvocationError, InvocationPoll, JitConfig, Value, Vm, VmError, VmMap, VmResult, VmStatus,
    compile_source,
};

fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Map(Arc::new(VmMap::from_entries(
        entries
            .into_iter()
            .map(|(key, value)| (string(key), value))
            .collect(),
    )))
}

fn string(value: &str) -> Value {
    Value::String(Arc::new(value.to_string()))
}

fn map_field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Map(entries) = value else {
        return None;
    };
    entries.get(&string(name))
}

#[derive(Default)]
struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn context() -> Context<'static> {
    let waker = Waker::from(Arc::new(NoopWake));
    Context::from_waker(Box::leak(Box::new(waker)))
}

struct SyntheticDriver {
    items: VecDeque<Value>,
    polls: Arc<AtomicUsize>,
    applied: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}

impl Drop for SyntheticDriver {
    fn drop(&mut self) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }
}

impl HostStreamDriver for SyntheticDriver {
    fn poll_next(&mut self, _cx: &mut Context<'_>) -> Poll<Result<HostStreamPoll, VmError>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        match self.items.pop_front() {
            Some(item) => Poll::Ready(Ok(HostStreamPoll::Item(item))),
            None => Poll::Ready(Ok(HostStreamPoll::Complete(map([
                ("outcome", string("eof")),
                (
                    "items",
                    Value::Int(self.applied.load(Ordering::SeqCst) as i64),
                ),
            ])))),
        }
    }

    fn apply_action(&mut self, action: Value) -> Result<HostStreamAction, VmError> {
        let Some(Value::String(action)) = map_field(&action, "action") else {
            return Err(VmError::HostError(
                "stream callback action must be a map with string 'action'".to_string(),
            ));
        };
        self.applied.fetch_add(1, Ordering::SeqCst);
        match action.as_str() {
            "continue" => Ok(HostStreamAction::Continue),
            "stop" => Ok(HostStreamAction::Complete(map([
                ("outcome", string("stopped")),
                (
                    "items",
                    Value::Int(self.applied.load(Ordering::SeqCst) as i64),
                ),
            ]))),
            other => Err(VmError::HostError(format!(
                "invalid synthetic stream action '{other}'"
            ))),
        }
    }
}

struct SyntheticStreamHost {
    polls: Arc<AtomicUsize>,
    applied: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
    invalid_first: bool,
}

struct YieldOnceHost(bool);

impl HostFunction for YieldOnceHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, VmError> {
        if !self.0 {
            self.0 = true;
            Ok(CallOutcome::Yield)
        } else {
            Ok(CallOutcome::Return(vec![Value::Null].into()))
        }
    }
}

struct WaitHost;

impl HostFunction for WaitHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, VmError> {
        vm.submit_host_future(Box::pin(std::future::pending()))
    }
}

#[derive(Default)]
struct PendingBridge {
    futures: HashMap<HostOpId, HostFuture>,
}

impl HostAsyncBridge for PendingBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        self.futures.insert(op_id, future);
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        Poll::Ready(Err(VmError::HostError(format!(
            "unknown host operation {op_id}"
        ))))
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        self.futures.get_mut(&op_id).map_or(
            Poll::Ready(Err(VmError::HostError(format!(
                "unknown submitted host operation {op_id}"
            )))),
            |future| future.as_mut().poll(cx),
        )
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.futures.remove(&op_id);
    }
}

struct ErrorAfterYieldHost(bool);

impl HostFunction for ErrorAfterYieldHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, VmError> {
        if !self.0 {
            self.0 = true;
            Ok(CallOutcome::Yield)
        } else {
            Err(VmError::HostError(
                "callback resumed into failure".to_string(),
            ))
        }
    }
}

impl HostFunction for SyntheticStreamHost {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let [callback] = args else {
            return Err(VmError::HostError("expected one callback".to_string()));
        };
        let driver = SyntheticDriver {
            items: (1..=4)
                .map(|number| {
                    map([
                        ("kind", string("item")),
                        ("n", Value::Int(number)),
                        (
                            "action",
                            string(if self.invalid_first && number == 1 {
                                "invalid"
                            } else if number == 3 {
                                "stop"
                            } else {
                                "continue"
                            }),
                        ),
                    ])
                })
                .collect(),
            polls: Arc::clone(&self.polls),
            applied: Arc::clone(&self.applied),
            stopped: Arc::clone(&self.stopped),
        };
        let outcome = vm.submit_callable_stream(callback.clone(), driver)?;
        Ok(outcome)
    }
}

fn setup(source: &str) -> (Vm, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let compiled = compile_source(source).expect("stream source should compile");
    let polls = Arc::new(AtomicUsize::new(0));
    let applied = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(compiled.program);
    vm.set_async_bridge(Box::new(PendingBridge::default()));
    for function in compiled.functions {
        match function.name.as_str() {
            "synthetic_stream" | "synthetic_invalid" => {
                let invalid_first = function.name == "synthetic_invalid";
                vm.register_function(Box::new(SyntheticStreamHost {
                    polls: Arc::clone(&polls),
                    applied: Arc::clone(&applied),
                    stopped: Arc::clone(&stopped),
                    invalid_first,
                }));
            }
            "yield_once" => {
                vm.register_function(Box::new(YieldOnceHost(false)));
            }
            "wait_once" => {
                vm.register_function(Box::new(WaitHost));
            }
            "error_after_yield" => {
                vm.register_function(Box::new(ErrorAfterYieldHost(false)));
            }
            other => panic!("unexpected host import {other}"),
        }
    }
    (vm, polls, applied, stopped)
}

fn poll_once(vm: &mut Vm) -> Poll<Result<(), VmError>> {
    vm.poll_waiting_host_op(&mut context())
}

#[test]
fn delivers_three_maps_to_a_closure_in_order_and_returns_summary() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        synthetic_stream(|item| item);
    "#;
    let (mut vm, polls, applied, stopped) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    for expected in 1..=3 {
        assert!(matches!(poll_once(&mut vm), Poll::Pending));
        assert_eq!(polls.load(Ordering::SeqCst), expected);
        assert_eq!(applied.load(Ordering::SeqCst), expected);
    }
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(
        map_field(&vm.stack()[0], "outcome"),
        Some(&string("stopped"))
    );
    assert_eq!(map_field(&vm.stack()[0], "items"), Some(&Value::Int(3)));
}

#[test]
fn producer_is_not_polled_until_callback_action_is_applied() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        synthetic_stream(|item| item);
    "#;
    let (mut vm, polls, applied, _) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    assert!(matches!(poll_once(&mut vm), Poll::Pending));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(applied.load(Ordering::SeqCst), 1);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    vm.reset_for_reuse();
}

#[test]
fn invalid_action_aborts_before_a_second_producer_poll() {
    let source = r#"
        fn synthetic_invalid(callback: fn(map) -> map) -> map;
        synthetic_invalid(|item| item);
    "#;
    let (mut vm, polls, _, _) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    let Poll::Ready(Err(VmError::HostError(message))) = poll_once(&mut vm) else {
        panic!("invalid action should fail immediately")
    };
    assert!(message.contains("invalid synthetic stream action"));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(vm.waiting_host_op_id().is_none());
}

#[test]
fn yielded_callback_resumes_before_the_producer_is_polled_again() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        fn yield_once();
        fn callback(item: map) -> map { yield_once(); item }
        synthetic_stream(callback);
    "#;
    let (mut vm, polls, applied, _) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    assert!(matches!(poll_once(&mut vm), Poll::Ready(Ok(()))));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(applied.load(Ordering::SeqCst), 0);
    assert_eq!(
        vm.resume().unwrap(),
        VmStatus::Waiting(vm.waiting_host_op_id().unwrap())
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(applied.load(Ordering::SeqCst), 1);
}

#[test]
fn waiting_callback_resumes_to_the_outer_stream_continuation() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        fn wait_once();
        fn callback(item: map) -> map { wait_once(); item }
        synthetic_stream(callback);
    "#;
    let (mut vm, polls, applied, _) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    assert!(matches!(poll_once(&mut vm), Poll::Ready(Ok(()))));
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    let inner_id = vm.waiting_host_op_id().unwrap();
    assert_ne!(inner_id, 0);
    vm.complete_host_op(inner_id, vec![Value::Null]).unwrap();
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(applied.load(Ordering::SeqCst), 1);
}

#[test]
fn resumed_callback_error_releases_the_stream_driver() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        fn error_after_yield();
        fn callback(item: map) -> map { error_after_yield(); item }
        synthetic_stream(callback);
    "#;
    let (mut vm, polls, applied, stopped) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    assert!(matches!(poll_once(&mut vm), Poll::Ready(Ok(()))));
    assert!(
        matches!(vm.resume(), Err(VmError::HostError(message)) if message == "callback resumed into failure")
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(applied.load(Ordering::SeqCst), 0);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(vm.waiting_host_op_id().is_none());
}

#[test]
fn invocation_cancellation_during_callback_wait_releases_the_stream_driver() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        fn wait_once();
        fn callback(item: map) -> map { wait_once(); item }
        pub fn run() -> map { synthetic_stream(callback) }
    "#;
    let (mut vm, polls, applied, stopped) = setup(source);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    let callable = vm.resolve_exported_callable("run").unwrap();
    {
        let mut invocation = vm.start_invocation(callable, vec![]).unwrap();
        assert!(matches!(
            invocation.poll_next().unwrap(),
            InvocationPoll::Pending
        ));
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        invocation.cancel(CancellationReason::Requested).unwrap();
        assert!(matches!(
            invocation.poll_next().unwrap(),
            InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
                CancellationReason::Requested
            ))))
        ));
    }
    assert_eq!(applied.load(Ordering::SeqCst), 0);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(vm.waiting_host_op_id().is_none());
}

#[test]
fn reset_and_shutdown_release_a_waiting_stream_driver() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        synthetic_stream(|item| item);
    "#;
    let (mut reset_vm, ..) = setup(source);
    assert!(matches!(reset_vm.run().unwrap(), VmStatus::Waiting(_)));
    reset_vm.reset_for_reuse();
    assert!(reset_vm.waiting_host_op_id().is_none());

    let (mut shutdown_vm, ..) = setup(source);
    assert!(matches!(shutdown_vm.run().unwrap(), VmStatus::Waiting(_)));
    shutdown_vm.shutdown();
    assert!(shutdown_vm.waiting_host_op_id().is_none());
}

#[test]
fn callback_schema_accepts_closures_and_named_generic_functions() {
    for source in [
        r#"fn synthetic_stream(callback: fn(map) -> map) -> map; synthetic_stream(|value| value);"#,
        r#"
            fn synthetic_stream(callback: fn(map) -> map) -> map;
            fn identity<T>(value: T) -> T { value }
            synthetic_stream(identity);
        "#,
    ] {
        compile_source(source).expect("typed callable should compile");
    }
}

#[test]
fn callback_schema_mismatches_are_rejected_at_compile_time() {
    for source in [
        r#"fn synthetic_stream(callback: fn(map) -> map) -> map; synthetic_stream(|value, extra| {action: "stop"});"#,
        r#"fn synthetic_stream(callback: fn(map) -> map) -> map; synthetic_stream(|value: int| {action: "stop"});"#,
        r#"fn synthetic_stream(callback: fn(map) -> map) -> map; synthetic_stream(|value| 1);"#,
    ] {
        assert!(
            compile_source(source).is_err(),
            "source unexpectedly compiled: {source}"
        );
    }
}

#[test]
fn dropping_vm_releases_a_waiting_stream_driver_once() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        synthetic_stream(|item| item);
    "#;
    let (mut vm, _polls, _applied, stopped) = setup(source);
    assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
    drop(vm);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
}

#[test]
fn interpreter_jit_and_aot_use_the_same_host_stream_continuation() {
    let source = r#"
        fn synthetic_stream(callback: fn(map) -> map) -> map;
        synthetic_stream(|item| item);
    "#;
    for backend in ["interpreter", "jit", "aot"] {
        let (mut vm, polls, applied, _stopped) = setup(source);
        vm.set_jit_config(JitConfig {
            enabled: backend == "jit",
            hot_loop_threshold: 1,
            max_trace_len: 128,
        });
        if backend == "aot" {
            vm.compile_aot().expect("aot compile should succeed");
        }
        assert!(matches!(vm.run().unwrap(), VmStatus::Waiting(_)));
        for _ in 0..3 {
            assert!(matches!(poll_once(&mut vm), Poll::Pending));
        }
        assert_eq!(vm.run().unwrap(), VmStatus::Halted, "{backend}");
        assert_eq!(polls.load(Ordering::SeqCst), 3, "{backend}");
        assert_eq!(applied.load(Ordering::SeqCst), 3, "{backend}");
    }
}
