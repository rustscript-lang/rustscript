#![cfg(feature = "runtime")]

//! Invocation item stream contract tests.
//!
//! An invocation behaves like `Stream<Item = Result<InvocationItem, InvocationError>>`:
//! zero or more `Event` items, then exactly one `Complete` item or one typed error,
//! then a fused end of stream. Input enters through ordinary callable arguments and
//! polling drives execution (backpressure).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vm::{
    CancellationReason, HostFunctionRegistry, InvocationError, InvocationItem, InvocationPoll,
    Value, Vm, VmError, compile_source,
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
        pub fn run(input: map) -> map {
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
    // `stream::emit` binds lazily through the default host fallback; the custom
    // host binding is not part of the registry plan.
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
        .cancel(CancellationReason::Requested)
        .expect("cancellation should be accepted");
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
        .cancel(CancellationReason::Requested)
        .expect("cancellation should be accepted");
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
        .cancel(CancellationReason::Requested)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            CancellationReason::Requested,
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
        pub fn run() -> map {
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
        .cancel(CancellationReason::Deadline)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            CancellationReason::Deadline,
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
fn invocation_host_op_first_poll_failure_keeps_typed_capability_error() {
    // Regression: the waiting operation id must be captured after `run()`
    // registers the host op. If the first poll fails and clears the waiting
    // state, `map_invocation_error` must still recover the structured
    // `OperationStatus::Failed` error instead of flattening it to a string.
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
        InvocationPoll::Ready(Some(Err(InvocationError::Capability(error)))) => {
            assert_eq!(error.code(), vm::RuntimeErrorCode::OperationFailed);
            assert_eq!(error.operation(), "runtime::host_bridge");
            assert!(
                error.value().is_some(),
                "the typed failure must carry the operation id"
            );
        }
        other => panic!(
            "expected a typed capability error for the first-poll host op failure, got {other:?}"
        ),
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
        .cancel(CancellationReason::Deadline)
        .expect("cancellation should be accepted");
    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
            CancellationReason::Deadline,
        )))) => {}
        other => panic!("expected a typed cancellation item, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}
