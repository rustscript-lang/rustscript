#![cfg(feature = "runtime")]

//! Invocation item stream contract tests.
//!
//! An invocation behaves like `Stream<Item = Result<InvocationItem, InvocationError>>`:
//! zero or more `Event` items, then exactly one `Complete` item or one typed error,
//! then a fused end of stream. Input enters through ordinary callable arguments and
//! polling drives execution (backpressure).

#[cfg(feature = "async")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use vm::{
    HostAsyncBridge, HostAsyncOpTerminal, HostFunctionRegistry, InvocationError, InvocationItem,
    InvocationPoll, Store, Value, Vm, VmError, compile_source, compile_source_for_repl_with_locals,
    operation::{
        HostOperation, OperationCancelReason, OperationError, OperationErrorCode, OperationResult,
        OperationSpec,
    },
};

/// Compiles a source, binds the default runtime host registry, and completes the
/// root frame so exported callables can be started.
fn compiled_vm(source: &str) -> Vm {
    let program = compile_source(source)
        .expect("invocation source should compile")
        .program;
    let mut vm = Vm::new(program);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default runtime host registry should bind");
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );
    vm
}

/// Drives one exported `run` callable to the end of its invocation stream.
fn collect_items(vm: &mut Vm, args: Vec<Value>) -> Vec<Result<InvocationItem, InvocationError>> {
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, args)
        .expect("invocation should start");
    let mut items = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "invocation drive loop must terminate"
        );
        match invocation
            .poll_next()
            .expect("invocation poll should not fail")
        {
            InvocationPoll::Ready(Some(item)) => items.push(item),
            InvocationPoll::Ready(None) => break,
            InvocationPoll::Pending => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    items
}

#[test]
fn invocation_input_arrives_as_ordinary_callable_arguments() {
    let mut vm = compiled_vm(
        r#"
        pub fn run(input: map<string>) -> map<string> {
            input;
        }
        "#,
    );
    let input = Value::map(vec![(Value::string("kind"), Value::string("message"))]);
    let items = collect_items(&mut vm, vec![input.clone()]);
    assert_eq!(items.len(), 1, "expected exactly one stream item");
    assert!(
        matches!(&items[0], Ok(InvocationItem::Complete(value)) if *value == input),
        "the exact structured argument must be the callable input, got {:?}",
        items
    );
}

#[test]
fn invocation_without_events_yields_complete_then_fused_end() {
    let mut vm = compiled_vm(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable.clone(), vec![])
        .expect("invocation should start");

    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
    assert!(
        matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ),
        "the stream must stay fused after Complete"
    );
    drop(invocation);

    // Once the first invocation has fused, a new invocation may start on the
    // same VM.
    let mut second = vm
        .start_invocation(callable, vec![])
        .expect("a new invocation may start after fusion");
    assert!(matches!(
        second.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
    assert!(matches!(
        second.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn dropping_an_unpolled_invocation_allows_a_second_invocation() {
    let mut vm = compiled_vm(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    {
        // Dropping the handle retires even a CompletePending invocation whose
        // terminal item was never observed.
        let _invocation = vm
            .start_invocation(callable.clone(), vec![])
            .expect("first invocation should start");
    }
    let mut second = vm
        .start_invocation(callable, vec![])
        .expect("dropping the first handle must release the vm immediately");
    assert!(matches!(
        second.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
}

#[test]
fn invocation_failures_are_typed_items_without_stack_or_string_inspection() {
    let mut vm = compiled_vm(
        r#"
        pub fn run(input: int) -> int {
            100 / input;
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![Value::Int(0)])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Vm(VmError::DivisionByZero)))) => {}
        other => panic!("expected a typed division-by-zero item, got {other:?}"),
    }
    assert!(
        matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ),
        "the stream must fuse after the error item"
    );
}

/// Records one script-visible progress note per call.
struct ProgressNote(Arc<Mutex<Vec<Value>>>);

impl vm::HostArgsFunction for ProgressNote {
    fn call(&mut self, args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        if let Some(value) = args.first() {
            self.0
                .lock()
                .expect("progress note lock should not be poisoned")
                .push(value.clone());
        }
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(
            args.first().cloned().unwrap_or(Value::Null),
        )))
    }
}

#[test]
fn invocation_emits_events_then_complete_in_order() {
    let mut vm = compiled_vm(
        r#"
        use stream;
        pub fn run() -> string {
            stream::emit("first");
            stream::emit("second");
            "done";
        }
        "#,
    );
    let items = collect_items(&mut vm, vec![]);
    assert_eq!(
        items.len(),
        3,
        "expected event, event, complete; got {items:?}"
    );
    assert!(
        matches!(&items[0], Ok(InvocationItem::Event(value)) if *value == Value::string("first"))
    );
    assert!(
        matches!(&items[1], Ok(InvocationItem::Event(value)) if *value == Value::string("second"))
    );
    assert!(
        matches!(&items[2], Ok(InvocationItem::Complete(value)) if *value == Value::string("done"))
    );
}

#[test]
fn invocation_event_values_never_replace_the_callable_return_value() {
    let mut vm = compiled_vm(
        r#"
        use stream;
        pub fn run() -> int {
            stream::emit("payload");
            42;
        }
        "#,
    );
    let items = collect_items(&mut vm, vec![]);
    assert_eq!(
        items.len(),
        2,
        "expected event then complete; got {items:?}"
    );
    assert!(
        matches!(&items[0], Ok(InvocationItem::Event(value)) if *value == Value::string("payload"))
    );
    assert!(matches!(
        &items[1],
        Ok(InvocationItem::Complete(Value::Int(42)))
    ));
}

#[test]
fn invocation_polling_pauses_execution_and_exposes_one_event_at_a_time() {
    let program = compile_source(
        r#"
        use stream;
        fn note_progress(value: string) -> string;
        pub fn run() -> string {
            stream::emit("a");
            note_progress("after-a");
            stream::emit("b");
            note_progress("after-b");
            "done";
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    let notes = Arc::new(Mutex::new(Vec::<Value>::new()));
    vm.bind_args_function("note_progress", Box::new(ProgressNote(Arc::clone(&notes))));
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    // First poll: the script paused at the first emit; nothing after it ran.
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("a")
    ));
    assert!(
        notes.lock().expect("notes lock").is_empty(),
        "execution must not advance while polling is paused"
    );

    // Second poll: resume past emit(a), run note_progress("after-a"), pause at
    // emit(b). Exactly one progress note may exist.
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("b")
    ));
    assert_eq!(
        notes.lock().expect("notes lock").len(),
        1,
        "exactly one progress note between polls"
    );

    // Third poll: resume past emit(b), run note_progress("after-b"), complete.
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value)))) if value == Value::string("done")
    ));
    assert_eq!(notes.lock().expect("notes lock").len(), 2);
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_cancellation_produces_one_typed_error_item_then_fused_end() {
    let mut vm = compiled_vm(
        r#"
        use stream;
        pub fn run() -> string {
            stream::emit("before");
            while true {
                1;
            }
            "unreachable";
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("before")
    ));

    invocation
        .cancel(OperationCancelReason::Requested)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Requested,
        )))) => {}
        other => panic!("expected a typed cancellation item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_repeated_cancellation_preserves_the_first_reason() {
    let mut vm = compiled_vm(
        r#"
        use stream;
        pub fn run() -> string {
            stream::emit("before");
            while true {
                1;
            }
            "unreachable";
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("before")
    ));

    invocation
        .cancel(OperationCancelReason::Requested)
        .expect("first cancellation should be accepted");
    invocation
        .cancel(OperationCancelReason::Deadline)
        .expect("repeat cancellation should be idempotent");

    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Requested,
        ))))
    ));
}

#[test]
fn invocation_fuel_exhaustion_produces_one_typed_error_item() {
    let mut vm = compiled_vm(
        r#"
        pub fn run() -> int {
            while true {
                1;
            }
            42;
        }
        "#,
    );
    vm.set_fuel(8);
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::OutOfFuel {
            needed: _,
            remaining: 0,
        }))) => {}
        other => panic!("expected a typed out-of-fuel item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_deadline_expiry_produces_one_typed_error_item() {
    let mut vm = compiled_vm(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    );
    vm.set_epoch_deadline(0)
        .expect("epoch deadline should be configured");
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::DeadlineReached {
            current: 0,
            deadline: 0,
        }))) => {}
        other => panic!("expected a typed deadline item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_host_failure_produces_one_typed_error_item() {
    let program = compile_source(
        r#"
        fn fail_host() -> int;
        pub fn run() -> int {
            fail_host();
            42;
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.bind_stack_function("fail_host", Box::new(FailingHost));
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Host { message }))) => {
            assert_eq!(message, "boom");
        }
        other => panic!("expected a typed host failure item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_event_bound_violations_are_typed_capability_errors() {
    let mut vm = compiled_vm(
        r#"
        use stream;
        pub fn run(input: string) -> int {
            stream::emit(input);
            42;
        }
        "#,
    );
    let oversized = "x".repeat(70 * 1024);
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![Value::string(oversized)])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Capability(error)))) => {
            assert_eq!(error.code(), vm::RuntimeErrorCode::EventPayloadTooLarge);
        }
        other => panic!("expected a typed capability error item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_result_is_isolated_from_completed_callback_results() {
    let mut vm = compiled_vm(
        r#"
        pub fn queued_ok() -> int {
            11;
        }
        pub fn queued_fail(input: int) -> int {
            1 / input;
        }
        pub fn run() -> int {
            42;
        }
        "#,
    );
    let queued_ok = vm
        .resolve_exported_callable("queued_ok")
        .expect("queued success callable should resolve");
    let queued_fail = vm
        .resolve_exported_callable("queued_fail")
        .expect("queued failure callable should resolve");
    vm.queue_callable(queued_ok, vec![])
        .expect("first callback should enter the queue");
    vm.queue_callable(queued_fail, vec![Value::Int(0)])
        .expect("second callback should enter the queue");

    assert!(matches!(
        vm.drain_callable_queue(),
        Err(VmError::DivisionByZero)
    ));

    let run = vm
        .resolve_exported_callable("run")
        .expect("run callable should resolve");
    let mut invocation = vm
        .start_invocation(run, vec![])
        .expect("new invocation should start with prior callback results queued");
    assert!(matches!(
        invocation
            .poll_next()
            .expect("invocation poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
    drop(invocation);
    assert_eq!(
        vm.take_callable_result(),
        Some(Value::Int(11)),
        "the earlier callback result must remain available through the callback accessor"
    );
    assert_eq!(vm.take_callable_result(), None);
}

#[test]
fn start_invocation_rejects_a_foreign_vm_callable_before_mutating_state() {
    let owner = compiled_vm("pub fn run() -> int { 7; }");
    let foreign = owner
        .resolve_exported_callable("run")
        .expect("owner callable should resolve");
    let mut target = compiled_vm("pub fn run() -> int { 42; }");
    let own = target
        .resolve_exported_callable("run")
        .expect("target callable should resolve");
    assert!(matches!(
        (&foreign, &own),
        (Value::Callable(foreign), Value::Callable(own))
            if foreign.prototype_id == own.prototype_id
    ));

    assert!(matches!(
        target.start_invocation(foreign, vec![]),
        Err(VmError::InvalidCallable)
    ));
    assert!(target.execution_frames().is_empty());
    assert!(target.stack().is_empty());
    assert_eq!(target.queued_callable_count(), 0);
    assert_eq!(target.take_callable_result(), None);

    let mut invocation = target
        .start_invocation(own, vec![])
        .expect("the target VM callable should still be usable");
    assert!(matches!(
        invocation
            .poll_next()
            .expect("invocation poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
}

#[test]
fn start_invocation_rejects_a_foreign_callable_nested_in_an_array_before_mutating_state() {
    let owner = compiled_vm("pub fn run(input: int) -> int { 7; }");
    let foreign = owner
        .resolve_exported_callable("run")
        .expect("owner callable should resolve");
    let mut target = compiled_vm("pub fn run(input: int) -> int { 42; }");
    let own = target
        .resolve_exported_callable("run")
        .expect("target callable should resolve");
    let before_ip = target.ip();
    let before_locals = target.locals().to_vec();
    let nested = Value::array(vec![Value::array(vec![foreign])]);

    assert!(matches!(
        target.start_invocation(own, vec![nested]),
        Err(VmError::InvalidCallable)
    ));
    assert_eq!(target.ip(), before_ip);
    assert_eq!(target.locals(), before_locals.as_slice());
    assert!(target.execution_frames().is_empty());
    assert!(target.stack().is_empty());
    assert_eq!(target.call_depth(), 0);
    assert_eq!(target.queued_callable_count(), 0);
    assert_eq!(target.take_callable_result(), None);
}

#[test]
fn start_invocation_rejects_foreign_callables_in_map_keys_and_values() {
    let owner = compiled_vm("pub fn run(input: int) -> int { 7; }");
    let foreign = owner
        .resolve_exported_callable("run")
        .expect("owner callable should resolve");
    let mut target = compiled_vm("pub fn run(input: int) -> int { 42; }");
    let own = target
        .resolve_exported_callable("run")
        .expect("target callable should resolve");
    let before_ip = target.ip();
    let before_locals = target.locals().to_vec();
    let nested = Value::map(vec![
        (foreign.clone(), Value::Int(1)),
        (Value::string("value"), foreign),
    ]);

    assert!(matches!(
        target.start_invocation(own, vec![nested]),
        Err(VmError::InvalidCallable)
    ));
    assert_eq!(target.ip(), before_ip);
    assert_eq!(target.locals(), before_locals.as_slice());
    assert!(target.execution_frames().is_empty());
    assert!(target.stack().is_empty());
    assert_eq!(target.call_depth(), 0);
}

#[test]
fn start_invocation_rejects_a_callable_after_vm_reset() {
    let mut target = compiled_vm("pub fn run(input: int) -> int { 42; }");
    let stale = target
        .resolve_exported_callable("run")
        .expect("target callable should resolve");
    target
        .reset_for_reuse()
        .expect("reset should complete without pending host work");
    let current = target
        .resolve_exported_callable("run")
        .expect("reset callable should resolve");
    let before_ip = target.ip();
    let before_locals = target.locals().to_vec();
    let before_frame_count = target.execution_frames().len();

    assert!(matches!(
        target.start_invocation(stale, vec![Value::Null]),
        Err(VmError::InvalidCallable)
    ));
    assert_eq!(target.ip(), before_ip);
    assert_eq!(target.locals(), before_locals.as_slice());
    assert_eq!(target.execution_frames().len(), before_frame_count);
    assert!(target.stack().is_empty());
    assert_eq!(target.call_depth(), 0);
    assert!(matches!(current, Value::Callable(_)));
}

#[test]
fn start_invocation_rejects_a_foreign_captured_closure_before_mutating_state() {
    let source = r#"
        let seed = 7;
        let closure = |value| value + seed;
    "#;
    let owner_compiled = compile_source_for_repl_with_locals(source, &[])
        .expect("owner closure source should compile");
    let mut owner = Vm::new(
        owner_compiled
            .compiled
            .program
            .with_local_count(owner_compiled.compiled.locals),
    );
    assert_eq!(
        owner.run().expect("owner root should halt"),
        vm::VmStatus::Halted
    );
    let foreign = owner
        .locals()
        .iter()
        .find(|value| matches!(value, Value::Callable(callable) if callable.env.is_some()))
        .cloned()
        .expect("owner should expose a captured closure local");

    let target_compiled = compile_source_for_repl_with_locals(source, &[])
        .expect("target closure source should compile");
    let mut target = Vm::new(
        target_compiled
            .compiled
            .program
            .with_local_count(target_compiled.compiled.locals),
    );
    assert_eq!(
        target.run().expect("target root should halt"),
        vm::VmStatus::Halted
    );
    assert!(matches!(
        target.start_invocation(foreign, vec![Value::Int(1)]),
        Err(VmError::InvalidCallable)
    ));
    assert!(target.execution_frames().is_empty());
    assert!(target.stack().is_empty());
    assert_eq!(target.take_callable_result(), None);
}

/// Fails every host call with a plain embedding error.
struct FailingHost;

impl vm::HostStackFunction for FailingHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        Err(vm::VmError::HostError("boom".to_string()))
    }
}

#[cfg(feature = "async")]
#[path = "support/async_test_bridge.rs"]
mod async_test_bridge;

/// Waits asynchronously through the embedding-owned host bridge.
#[cfg(feature = "async")]
struct AsyncWaitHost;

#[cfg(feature = "async")]
impl vm::HostStackFunction for AsyncWaitHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        vm.submit_host_future(Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(vm::HostFutureOutput::returning(vm::CallReturn::one(
                Value::Int(7),
            )))
        }))
    }
}

#[cfg(feature = "async")]
struct RecordingWake(Arc<AtomicUsize>);

#[cfg(feature = "async")]
impl Wake for RecordingWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(feature = "async")]
#[test]
fn invocation_context_poll_forwards_the_caller_waker_to_waiting_host_operation() {
    let program = compile_source(
        r#"
        use stream;
        fn wait_host() -> int;
        pub fn run() -> string {
            stream::emit("a");
            wait_host();
            stream::emit("b");
            "done";
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.bind_stack_function("wait_host", Box::new(AsyncWaitHost));
    async_test_bridge::install(&mut vm);
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");
    let mut noop_context = Context::from_waker(Waker::noop());
    assert!(matches!(
        invocation
            .poll_next_with_context(&mut noop_context)
            .expect("event poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value))))
            if value == Value::string("a")
    ));

    let wake_count = Arc::new(AtomicUsize::new(0));
    let waker = Waker::from(Arc::new(RecordingWake(Arc::clone(&wake_count))));
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        invocation
            .poll_next_with_context(&mut cx)
            .expect("waiting poll should succeed"),
        InvocationPoll::Pending
    ));

    let deadline = Instant::now() + Duration::from_secs(10);
    while wake_count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        wake_count.load(Ordering::SeqCst) > 0,
        "the waiting host operation must wake the caller's waker"
    );
    assert!(matches!(
        invocation
            .poll_next_with_context(&mut cx)
            .expect("resumed poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value))))
            if value == Value::string("b")
    ));
}

#[cfg(feature = "async")]
#[test]
fn invocation_waiting_host_operation_returns_pending_and_preserves_item_order() {
    let program = compile_source(
        r#"
        use stream;
        fn wait_host() -> int;
        pub fn run() -> string {
            stream::emit("a");
            wait_host();
            stream::emit("b");
            "done";
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.bind_stack_function("wait_host", Box::new(AsyncWaitHost));
    async_test_bridge::install(&mut vm);
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("a")
    ));

    // The outstanding host operation maps to Pending; drive it and poll again.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut polled_pending = false;
    let next = loop {
        assert!(
            Instant::now() < deadline,
            "waiting invocation must resume through the host driver"
        );
        match invocation.poll_next().expect("poll should succeed") {
            InvocationPoll::Pending => {
                polled_pending = true;
                std::thread::sleep(Duration::from_millis(1));
            }
            ready => break ready,
        }
    };
    assert!(
        polled_pending,
        "the waiting host op must surface as Pending"
    );
    assert!(matches!(
        next,
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("b")
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value)))) if value == Value::string("done")
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[cfg(feature = "async")]
struct DelayedInvocationCancellationBridge {
    runtime: tokio::runtime::Runtime,
    futures: std::collections::HashMap<vm::HostOpId, vm::HostFuture>,
    acknowledgement: Arc<AtomicBool>,
    cancellations: Arc<Mutex<Vec<(vm::HostOpId, OperationCancelReason)>>>,
    cleanups: Arc<Mutex<Vec<(vm::HostOpId, HostAsyncOpTerminal)>>>,
}

#[cfg(feature = "async")]
impl DelayedInvocationCancellationBridge {
    fn new(
        acknowledgement: Arc<AtomicBool>,
        cancellations: Arc<Mutex<Vec<(vm::HostOpId, OperationCancelReason)>>>,
        cleanups: Arc<Mutex<Vec<(vm::HostOpId, HostAsyncOpTerminal)>>>,
    ) -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime should build"),
            futures: std::collections::HashMap::new(),
            acknowledgement,
            cancellations,
            cleanups,
        }
    }
}

#[cfg(feature = "async")]
impl HostAsyncBridge for DelayedInvocationCancellationBridge {
    fn submit_op(&mut self, op_id: vm::HostOpId, future: vm::HostFuture) -> vm::VmResult<()> {
        if self.futures.insert(op_id, future).is_some() {
            return Err(vm::VmError::HostError(format!(
                "duplicate submitted host op {op_id}"
            )));
        }
        Ok(())
    }

    fn poll_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<vm::CallReturn>> {
        Poll::Ready(Err(vm::VmError::HostError(
            "unexpected external operation".to_string(),
        )))
    }

    fn poll_submitted_op(
        &mut self,
        op_id: vm::HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<vm::HostFutureOutput>> {
        let poll = {
            let Some(future) = self.futures.get_mut(&op_id) else {
                return Poll::Ready(Err(vm::VmError::HostError(format!(
                    "unknown submitted host op {op_id}"
                ))));
            };
            let _guard = self.runtime.enter();
            future.as_mut().poll(cx)
        };
        if poll.is_ready() {
            self.futures.remove(&op_id);
        }
        poll
    }

    fn request_cancel_op(
        &mut self,
        op_id: vm::HostOpId,
        reason: OperationCancelReason,
    ) -> vm::VmResult<()> {
        self.cancellations
            .lock()
            .expect("cancellation lock")
            .push((op_id, reason));
        Ok(())
    }

    fn poll_cancel_op(
        &mut self,
        _op_id: vm::HostOpId,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::VmResult<()>> {
        if self.acknowledgement.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn cleanup_op(
        &mut self,
        op_id: vm::HostOpId,
        terminal: HostAsyncOpTerminal,
    ) -> vm::VmResult<()> {
        self.futures.remove(&op_id);
        self.cleanups
            .lock()
            .expect("cleanup lock")
            .push((op_id, terminal));
        Ok(())
    }
}

#[cfg(feature = "async")]
#[test]
fn invocation_cancellation_waits_for_bridge_acknowledgement() {
    let program = compile_source(
        r#"
        use stream;
        fn wait_host() -> int;
        pub fn run() -> string {
            stream::emit("before");
            wait_host();
            stream::emit("after");
            "done";
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let acknowledgement = Arc::new(AtomicBool::new(false));
    let cancellations = Arc::new(Mutex::new(Vec::new()));
    let cleanups = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::new(program);
    vm.bind_stack_function("wait_host", Box::new(AsyncWaitHost));
    vm.set_async_bridge(Box::new(DelayedInvocationCancellationBridge::new(
        Arc::clone(&acknowledgement),
        Arc::clone(&cancellations),
        Arc::clone(&cleanups),
    )))
    .expect("bridge installation should succeed");
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");
    assert!(matches!(
        invocation.poll_next().expect("event poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value))))
            if value == Value::string("before")
    ));
    assert!(matches!(
        invocation.poll_next().expect("waiting poll should succeed"),
        InvocationPoll::Pending
    ));

    invocation
        .cancel(OperationCancelReason::Deadline)
        .expect("cancellation request should be accepted");
    assert!(matches!(
        invocation.poll_next().expect("cancel poll should succeed"),
        InvocationPoll::Pending
    ));
    assert_eq!(
        *cancellations.lock().expect("cancellation lock"),
        vec![(1, OperationCancelReason::Deadline)]
    );
    assert!(cleanups.lock().expect("cleanup lock").is_empty());

    acknowledgement.store(true, Ordering::SeqCst);
    assert!(matches!(
        invocation
            .poll_next()
            .expect("acknowledged cancel poll should succeed"),
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Deadline,
        ))))
    ));
    assert!(matches!(
        invocation.poll_next().expect("fused poll should succeed"),
        InvocationPoll::Ready(None)
    ));
    assert_eq!(
        *cleanups.lock().expect("cleanup lock"),
        vec![(1, HostAsyncOpTerminal::Cancelled)]
    );
}

#[cfg(feature = "async")]
#[test]
fn dropped_invocation_keeps_readiness_blocked_until_bridge_cleanup() {
    let program = compile_source(
        r#"
        use stream;
        fn wait_host() -> int;
        pub fn run() -> string {
            stream::emit("before");
            wait_host();
            stream::emit("after");
            "done";
        }
        pub fn plain() -> int {
            41;
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let acknowledgement = Arc::new(AtomicBool::new(false));
    let cancellations = Arc::new(Mutex::new(Vec::new()));
    let cleanups = Arc::new(Mutex::new(Vec::new()));
    let mut store = Store::from_vm(Vm::new(program));
    store
        .vm_mut()
        .bind_stack_function("wait_host", Box::new(AsyncWaitHost));
    store
        .vm_mut()
        .set_async_bridge(Box::new(DelayedInvocationCancellationBridge::new(
            Arc::clone(&acknowledgement),
            Arc::clone(&cancellations),
            Arc::clone(&cleanups),
        )))
        .expect("bridge installation should succeed");
    assert_eq!(
        store.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let run_callable = store
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let plain_callable = store
        .resolve_exported_callable("plain")
        .expect("plain callable should resolve");
    let mut invocation = store
        .vm_mut()
        .start_invocation(run_callable, vec![])
        .expect("invocation should start");
    assert!(matches!(
        invocation.poll_next().expect("event poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value))))
            if value == Value::string("before")
    ));
    assert!(matches!(
        invocation.poll_next().expect("waiting poll should succeed"),
        InvocationPoll::Pending
    ));
    drop(invocation);

    assert!(
        !store.is_reusable(),
        "a dropped invocation must retain the bridge cancellation boundary"
    );
    assert!(matches!(
        store.vm_mut().run(),
        Err(vm::VmError::HostError(message)) if message.contains("not quiescent")
    ));
    assert!(matches!(
        store.vm_mut().resume(),
        Err(vm::VmError::HostError(message)) if message.contains("not quiescent")
    ));
    assert!(matches!(
        store.vm_mut().start_callable(plain_callable.clone(), &[]),
        Err(vm::VmError::HostError(message)) if message.contains("not quiescent")
    ));
    assert!(matches!(
        store.vm_mut().start_invocation(plain_callable.clone(), vec![]),
        Err(vm::VmError::HostError(message)) if message.contains("not quiescent")
    ));
    assert_eq!(
        *cancellations.lock().expect("cancellation lock"),
        vec![(1, OperationCancelReason::Requested)]
    );
    assert!(cleanups.lock().expect("cleanup lock").is_empty());

    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        store.vm_mut().poll_waiting_host_op(&mut context),
        Poll::Pending
    ));
    acknowledgement.store(true, Ordering::SeqCst);
    assert!(matches!(
        store.vm_mut().poll_waiting_host_op(&mut context),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(store.vm().waiting_host_op_id(), None);
    assert!(store.is_reusable(), "bridge cleanup should restore reuse");
    assert_eq!(
        *cleanups.lock().expect("cleanup lock"),
        vec![(1, HostAsyncOpTerminal::Cancelled)]
    );

    let mut second = store
        .vm_mut()
        .start_invocation(plain_callable, vec![])
        .expect("a new invocation may start after bridge cleanup");
    assert!(matches!(
        second.poll_next().expect("plain invocation should poll"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(41)))))
    ));
    assert!(matches!(
        second.poll_next().expect("plain invocation should fuse"),
        InvocationPoll::Ready(None)
    ));
    drop(second);
    assert_eq!(store.vm().waiting_host_op_id(), None);
}

#[test]
fn invocation_cancellation_is_consumed_at_the_invocation_boundary() {
    // Regression: after a cancelled invocation emits its typed error and
    // fuses, the VM-level cancellation reason must not leak into a later
    // invocation started on the same VM.
    let mut vm = compiled_vm(
        r#"
        use stream;
        pub fn run() -> string {
            stream::emit("before");
            while true {
                1;
            }
            "unreachable";
        }
        pub fn plain() -> int {
            42;
        }
        "#,
    );
    let cancellable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(cancellable, vec![])
        .expect("invocation should start");
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("before")
    ));

    invocation
        .cancel(OperationCancelReason::Requested)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Requested,
        )))) => {}
        other => panic!("expected a typed cancellation item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
    drop(invocation);

    // A fresh invocation on the same VM must not inherit the old reason: it
    // runs to completion instead of being cancelled on arrival.
    let plain = vm
        .resolve_exported_callable("plain")
        .expect("exported plain callable should resolve");
    let mut second = vm
        .start_invocation(plain, vec![])
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
fn invocation_cancel_during_event_pending_discards_the_pending_event() {
    // Cancellation is authoritative: a pending event that was placed but not
    // yet delivered must be discarded (through the drop-contract path) and
    // the stream must produce exactly one Cancelled item, then a fused end.
    let program = compile_source(
        r#"
        use stream;
        pub fn run() -> string {
            stream::emit({"a": 1, "b": 2});
            while true {
                1;
            }
            "unreachable";
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.set_drop_contract_events_enabled(true);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default runtime host registry should bind");
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let drops_before_cancel = vm.drop_contract_event_count();
    // `start_callable` runs to the first `stream::emit` yield, so the
    // invocation is already in EventPending with the map payload.
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    invocation
        .cancel(OperationCancelReason::Requested)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Requested,
        )))) => {}
        other => panic!("cancellation must supersede the pending event, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
    drop(invocation);

    // The discarded event payload (map plus its two key/value pairs) must be
    // dropped through the VM drop-contract path, not leaked.
    assert!(
        vm.drop_contract_event_count() >= drops_before_cancel + 5,
        "the discarded pending event payload must be dropped through the drop contract path"
    );
}

#[test]
fn invocation_cancel_during_complete_pending_discards_the_pending_complete() {
    // Cancellation is authoritative over a not-yet-delivered Complete item:
    // the callable result is discarded and the stream produces exactly one
    // Cancelled item, then a fused end.
    let mut vm = compiled_vm(
        r#"
        pub fn run() -> map<int> {
            {"a": 1, "b": 2};
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    // The callable completes during `start_callable`, so the invocation is
    // already in CompletePending with the return map.
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    invocation
        .cancel(OperationCancelReason::Deadline)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Deadline,
        )))) => {}
        other => panic!("cancellation must supersede the pending complete, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

/// Fails asynchronously on the first poll of its submitted host operation.
#[cfg(feature = "async")]
struct AsyncFailHost;

#[cfg(feature = "async")]
impl vm::HostStackFunction for AsyncFailHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        vm.submit_host_future(Box::pin(async move {
            Err(vm::VmError::HostError("bridge future failed".to_string()))
        }))
    }
}

#[cfg(feature = "async")]
#[test]
fn invocation_host_op_first_poll_failure_keeps_typed_host_error() {
    // Regression: the waiting host op is polled once with a noop waker; if the
    // first poll fails and clears the waiting state, the typed mapping must
    // still surface (here a `Host` error) on the invocation stream.
    let program = compile_source(
        r#"
        fn fail_host() -> int;
        pub fn run() -> int {
            fail_host();
            42;
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.bind_stack_function("fail_host", Box::new(AsyncFailHost));
    async_test_bridge::install(&mut vm);
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Host { message }))) => {
            assert_eq!(message, "bridge future failed");
        }
        other => panic!("expected a typed host error item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[cfg(feature = "async")]
#[test]
fn invocation_cancellation_while_waiting_produces_one_typed_error_item() {
    let program = compile_source(
        r#"
        use stream;
        fn wait_host() -> int;
        pub fn run() -> string {
            stream::emit("a");
            wait_host();
            "unreachable";
        }
        "#,
    )
    .expect("invocation source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.bind_stack_function("wait_host", Box::new(AsyncWaitHost));
    async_test_bridge::install(&mut vm);
    assert_eq!(
        vm.run().expect("root frame should halt"),
        vm::VmStatus::Halted
    );

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("a")
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Pending
    ));

    invocation
        .cancel(OperationCancelReason::Deadline)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            OperationCancelReason::Deadline,
        )))) => {}
        other => panic!("expected a typed cancellation item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[derive(Debug)]
struct PendingCloseResource {
    ready: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl vm::resource::close::HostResource for PendingCloseResource {
    fn begin_close(
        &mut self,
        _reason: vm::ResourceCloseReason,
    ) -> vm::resource::error::ResourceResult<vm::resource::close::CloseProgress> {
        Ok(vm::resource::close::CloseProgress::Pending)
    }

    fn poll_close(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::resource::error::ResourceResult<()>> {
        if self.ready.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug)]
struct FailingCloseResource;

impl vm::resource::close::HostResource for FailingCloseResource {
    fn begin_close(
        &mut self,
        _reason: vm::ResourceCloseReason,
    ) -> vm::resource::error::ResourceResult<vm::resource::close::CloseProgress> {
        Ok(vm::resource::close::CloseProgress::Pending)
    }

    fn poll_close(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<vm::resource::error::ResourceResult<()>> {
        Poll::Ready(Err(vm::resource::error::ResourceError::new(
            vm::resource::error::ResourceErrorCode::ResourceCleanupFailed,
            "test",
            "scope cleanup failed",
        )))
    }
}

struct FailingCancelOperation;

impl HostOperation for FailingCancelOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(&mut self, _reason: OperationCancelReason) -> OperationResult<()> {
        Err(OperationError::new(
            OperationErrorCode::OperationDriverFailed,
            "test",
            "operation cancellation failed",
        ))
    }

    fn is_quiescent(&self) -> bool {
        true
    }
}

impl Drop for PendingCloseResource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn reset_does_not_reuse_vm_scope_before_generic_quiescence() {
    let ready = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut vm = Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]));
    vm.host_context()
        .push_resource(PendingCloseResource {
            ready: Arc::clone(&ready),
            dropped: Arc::clone(&dropped),
        })
        .expect("resource should enter the active execution scope");

    let _ = vm.reset_for_reuse();
    assert!(
        vm.scope_reset_pending(),
        "reset must retain a closing scope"
    );
    assert!(
        !dropped.load(Ordering::SeqCst),
        "pending resources must stay owned"
    );
    assert!(matches!(vm.run(), Err(vm::VmError::ExecutionScope(_))));

    ready.store(true, Ordering::SeqCst);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(
        vm.poll_reset_for_reuse(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert!(!vm.scope_reset_pending());
    assert!(
        dropped.load(Ordering::SeqCst),
        "close must release after quiescence"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(
        vm.run().expect("reused VM should run"),
        vm::VmStatus::Halted
    );
}

#[test]
fn reset_surfaces_first_operation_error_and_does_not_install_a_fresh_scope() {
    let mut vm = Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]));
    vm.execution_scope()
        .start_operation(OperationSpec::new(FailingCancelOperation))
        .expect("operation should enter the active execution scope");
    vm.execution_scope()
        .push_resource(FailingCloseResource)
        .expect("resource should enter the active execution scope");

    let reset_error = vm
        .reset_for_reuse()
        .expect_err("reset must report cleanup failures");
    let VmError::ExecutionScope(vm::execution_scope::ExecutionScopeError::Close(
        vm::execution_scope::ScopeCloseOutcome::SuccessWithErrors(failure),
    )) = reset_error
    else {
        panic!("reset must preserve the terminal scope close outcome");
    };
    assert_eq!(failure.failed, 2);
    assert!(matches!(
        failure.first,
        vm::execution_scope::ScopeCloseError::Operation(error)
            if error.code() == OperationErrorCode::OperationDriverFailed
    ));

    let mut cx = Context::from_waker(Waker::noop());
    match vm.poll_reset_for_reuse(&mut cx) {
        Poll::Ready(Err(VmError::ExecutionScope(
            vm::execution_scope::ExecutionScopeError::Close(
                vm::execution_scope::ScopeCloseOutcome::SuccessWithErrors(_),
            ),
        ))) => {}
        other => panic!("cleanup failures must remain observable, got {other:?}"),
    }
    assert!(
        vm.scope_reset_pending(),
        "a failed close must not publish a reusable replacement scope"
    );
}

#[test]
fn reset_clears_queued_callable_state_before_store_reuse() {
    let program = compile_source("pub fn queued() -> int { 7 }")
        .expect("queued callable program should compile");
    let mut store = Store::from_vm(Vm::new(program.program));
    let callback = store
        .script_callback_by_name::<(), i64>("queued")
        .expect("queued callable should be exported");
    let prepared = callback.prepare(()).expect("callback should prepare");
    store
        .enqueue_callback(prepared)
        .expect("callback should enter the VM queue");
    assert!(
        !store.is_reusable(),
        "存在 queued callback 时 store 不应进入复用池"
    );

    store
        .reset_for_reuse()
        .expect("reset should clear queued callable state");

    assert!(
        !callback.is_subscribed(),
        "reset must invalidate queued callback aliases"
    );
    assert_eq!(
        store.run().expect("reset VM should reach root halt"),
        vm::VmStatus::Halted
    );
    assert!(store.is_reusable(), "reset 完成且队列清空后 store 应可复用");
    assert!(
        store
            .drain_callbacks()
            .expect("queue drain should succeed")
            .is_empty(),
        "reset must discard queued callable state"
    );
}

#[test]
fn store_is_not_reusable_while_completed_callback_results_wait() {
    let program =
        compile_source("pub fn ok() -> int { 7 } pub fn fail(input: int) -> int { 1 / input }")
            .expect("callback result program should compile");
    let mut store = Store::from_vm(Vm::new(program.program));
    store.run().expect("root frame should complete");

    let ok = store
        .script_callback_by_name::<(), i64>("ok")
        .expect("ok callback should be available");
    let fail = store
        .script_callback_by_name::<(i64,), i64>("fail")
        .expect("failing callback should be available");
    store
        .enqueue_callback(ok.prepare(()).expect("ok callback should prepare"))
        .expect("ok callback should enter the queue");
    store
        .enqueue_callback(
            fail.prepare((0_i64,))
                .expect("failing callback should prepare"),
        )
        .expect("failing callback should enter the queue");

    assert!(
        store.drain_callbacks().is_err(),
        "第二个 callback 失败时 drain 应返回错误"
    );
    assert!(
        !store.is_reusable(),
        "存在待领取 callback result 时 store 不应进入复用池"
    );
    assert_eq!(
        store
            .take_callback_result::<i64>()
            .expect("completed callback result should be readable"),
        Some(7)
    );
}

#[test]
fn store_does_not_publish_callbacks_while_scope_reset_is_pending() {
    let program = compile_source("pub fn queued() -> int { 7 }")
        .expect("queued callable program should compile");
    let mut store = Store::from_vm(Vm::new(program.program));
    let ready = Arc::new(AtomicBool::new(false));
    store
        .vm_mut()
        .host_context()
        .push_resource(PendingCloseResource {
            ready: Arc::clone(&ready),
            dropped: Arc::new(AtomicBool::new(false)),
        })
        .expect("resource should enter the active execution scope");

    assert!(store.reset_for_reuse().is_ok());
    assert!(store.vm().scope_reset_pending());
    assert!(!store.is_reusable());
    assert!(
        store.script_callback_by_name::<(), i64>("queued").is_err(),
        "a store must not install a new callback registry before scope quiescence"
    );

    ready.store(true, Ordering::SeqCst);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(
        store.poll_reset_for_reuse(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert!(store.is_reusable());
    let callback = store
        .script_callback_by_name::<(), i64>("queued")
        .expect("callbacks should be available after scope quiescence");
    assert!(
        callback.is_subscribed(),
        "完成 reset 后创建的 callback 应处于有效状态"
    );
    for _ in 0..2 {
        assert!(matches!(
            store.poll_reset_for_reuse(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(
            callback.is_subscribed(),
            "重复 poll 不得替换完成 reset 后的 callback registry"
        );
    }
}

#[test]
fn repeated_completed_reset_poll_preserves_new_callback_registry() {
    let program = compile_source("pub fn queued() -> int { 7 }")
        .expect("queued callable program should compile");
    let mut store = Store::from_vm(Vm::new(program.program));

    store.reset_for_reuse().expect("同步 reset 应成功完成");
    let callback = store
        .script_callback_by_name::<(), i64>("queued")
        .expect("完成 reset 后应能创建 callback");
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..3 {
        assert!(matches!(
            store.poll_reset_for_reuse(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(
            callback.is_subscribed(),
            "完成 reset 的重复 poll 不得使 callback 失效"
        );
    }

    store.run().expect("reset 后 root frame 应完成");
    assert_eq!(
        callback
            .call(&mut store, ())
            .expect("callback 应在重复 poll 后仍可调用"),
        7
    );
}

struct YieldingHost;

impl vm::HostStackFunction for YieldingHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        Ok(vm::CallOutcome::Yield)
    }
}

struct ObservingHost {
    reusable: Arc<AtomicBool>,
}

impl vm::HostStackFunction for ObservingHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        self.reusable.store(vm.is_reusable(), Ordering::SeqCst);
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(1))))
    }
}

#[test]
fn direct_host_execution_is_not_reusable_while_callback_runs() {
    let program = compile_source("fn pause() -> int; pause();")
        .expect("host callable program should compile")
        .program;
    let reusable = Arc::new(AtomicBool::new(false));
    let mut vm = Vm::new(program);
    vm.bind_stack_function(
        "pause",
        Box::new(ObservingHost {
            reusable: Arc::clone(&reusable),
        }),
    );
    assert_eq!(
        vm.run().expect("host callable should complete"),
        vm::VmStatus::Halted
    );
    assert!(
        !reusable.load(Ordering::SeqCst),
        "host callback 执行期间 VM 不应可复用"
    );
}

#[test]
fn active_script_frames_make_vm_non_reusable() {
    let program = compile_source("fn pause() -> int; pub fn run() -> int { pause(); 42 }")
        .expect("yielding callable program should compile")
        .program;
    let mut vm = Vm::new(program);
    vm.bind_stack_function("pause", Box::new(YieldingHost));
    assert_eq!(
        vm.run().expect("root frame should complete"),
        vm::VmStatus::Halted
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("run callable should resolve");

    assert_eq!(
        vm.start_callable(callable, &[])
            .expect("script call should yield"),
        vm::VmStatus::Yielded
    );
    assert!(!vm.is_reusable(), "存在活动 script frame 时 VM 不应可复用");
}

#[cfg(feature = "async")]
#[test]
fn submitted_bridge_operations_make_vm_non_reusable() {
    let program = compile_source("pub fn noop() -> int { 1 }")
        .expect("bridge state program should compile")
        .program;
    let mut vm = Vm::new(program);
    async_test_bridge::install(&mut vm);
    vm.submit_host_future(Box::pin(async {
        Ok(vm::HostFutureOutput::returning(vm::CallReturn::one(
            Value::Int(1),
        )))
    }))
    .expect("future should enter the host bridge");

    assert!(
        !vm.is_reusable(),
        "存在 submitted bridge operation 时 VM 不应进入复用池"
    );
}
