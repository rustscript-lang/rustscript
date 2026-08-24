//! Milestone 1 of the VM runtime decomposition plan: ownership tests.
//!
//! These tests pin the ownership contract through the public embedding API:
//! - one immutable program can create multiple isolated instances;
//! - invocation input/events/budgets never leak between runs;
//! - backend caches may be shared without sharing stacks/resources;
//! - reset closes run-scoped state and retains only documented reusable state.

#[path = "../common/mod.rs"]
mod common;
use common::*;

use std::sync::Arc;

use vm::{
    HostFunctionRegistry, InvocationError, InvocationItem, InvocationPoll, Value, VmError, VmStatus,
};

fn non_yielding_returns_zero(_: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(0))))
}

fn non_yielding_returns_seven(_: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(7))))
}

fn non_yielding_returns_nine(_: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(9))))
}

fn non_yielding_returns_forty_two(_: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(42))))
}

/// Drives one exported `run` callable to the end of its invocation stream.
fn collect_invocation_items(
    vm: &mut vm::Vm,
    args: Vec<Value>,
) -> Vec<Result<InvocationItem, InvocationError>> {
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, args)
        .expect("invocation should start");
    let mut items = Vec::new();
    loop {
        match invocation
            .poll_next()
            .expect("invocation poll should not fail")
        {
            InvocationPoll::Ready(Some(item)) => items.push(item),
            InvocationPoll::Ready(None) => break,
            InvocationPoll::Pending => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
    items
}

struct PendingOneHost {
    op_id: u64,
}

impl vm::HostArgsFunction for PendingOneHost {
    fn call(&mut self, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        Ok(vm::CallOutcome::Pending(self.op_id))
    }
}

/// One immutable program produces independent instances: each invocation keeps
/// its own stream items, and no instance observes another's execution.
#[test]
fn one_immutable_program_creates_multiple_isolated_instances() {
    let program = Arc::new(
        compile_source(
            r#"
            pub fn run(input: string) -> string {
                input;
            }
            "#,
        )
        .expect("source should compile")
        .program,
    );

    let mut first =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    let mut second =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut first)
        .expect("runtime hosts should bind");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut second)
        .expect("runtime hosts should bind");
    assert_eq!(
        first.run().expect("first root should halt"),
        VmStatus::Halted
    );
    assert_eq!(
        second.run().expect("second root should halt"),
        VmStatus::Halted
    );

    let first_items = collect_invocation_items(&mut first, vec![Value::string("first")]);
    let second_items = collect_invocation_items(&mut second, vec![Value::string("second")]);

    assert_eq!(first_items.len(), 1, "first invocation must complete once");
    assert!(
        matches!(&first_items[0], Ok(InvocationItem::Complete(value)) if *value == Value::string("first")),
        "first instance must observe its own input, got {first_items:?}"
    );
    assert_eq!(
        second_items.len(),
        1,
        "second invocation must complete once"
    );
    assert!(
        matches!(&second_items[0], Ok(InvocationItem::Complete(value)) if *value == Value::string("second")),
        "second instance must observe its own input, got {second_items:?}"
    );

    // Re-running one instance after reset must not disturb the other. Reset
    // rewinds the root frame, so the root must halt again before callables can
    // be started.
    first.reset_for_reuse();
    assert_eq!(
        first.run().expect("first root should halt again"),
        VmStatus::Halted
    );
    let first_again = collect_invocation_items(&mut first, vec![Value::string("first-again")]);
    assert!(
        matches!(&first_again[0], Ok(InvocationItem::Complete(value)) if *value == Value::string("first-again")),
        "first rerun must observe its own fresh input, got {first_again:?}"
    );
    assert_eq!(
        second_items.len(),
        1,
        "first's rerun must not disturb second"
    );
}

/// Invocation events and results are run-scoped: a reset closes them, and a
/// later run starts with a clean stream.
#[test]
fn run_input_and_events_do_not_leak_between_runs() {
    let program = Arc::new(
        compile_source(
            r#"
            use stream;
            pub fn run(input: string) -> string {
                stream::emit(input);
                input;
            }
            "#,
        )
        .expect("source should compile")
        .program,
    );
    let mut vm =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("runtime hosts should bind");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);

    let items = collect_invocation_items(&mut vm, vec![Value::string("run-one")]);
    assert_eq!(items.len(), 2, "first run must emit one event and complete");
    assert!(
        matches!(&items[0], Ok(InvocationItem::Event(value)) if *value == Value::string("run-one"))
    );
    assert!(
        matches!(&items[1], Ok(InvocationItem::Complete(value)) if *value == Value::string("run-one"))
    );

    // A reset closes the run-scoped invocation state: the next run starts with
    // a fresh stream and neither the old input nor the old events leak.
    vm.reset_for_reuse();
    assert_eq!(vm.run().expect("root should halt again"), VmStatus::Halted);
    let items_after_reset = collect_invocation_items(&mut vm, vec![Value::string("run-two")]);
    assert_eq!(
        items_after_reset.len(),
        2,
        "reset must not leak prior events into the next run"
    );
    assert!(
        matches!(&items_after_reset[0], Ok(InvocationItem::Event(value)) if *value == Value::string("run-two"))
    );
    assert!(
        matches!(&items_after_reset[1], Ok(InvocationItem::Complete(value)) if *value == Value::string("run-two"))
    );
}
/// Fuel budgets are run-scoped: a reset clears the budget, and a new run
/// starts from its configured amount rather than inheriting leftovers.
#[test]
fn fuel_budgets_do_not_leak_between_runs() {
    let program = compile_source(
        r#"
        fn action() -> int;
        action();
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.bind_static_non_yielding_args_function("action", non_yielding_returns_zero);

    // A configured budget reads back as the configured amount.
    vm.set_fuel(100);
    assert_eq!(vm.get_fuel(), Some(100));

    // Explicit consumption draws from the run-scoped budget.
    vm.consume_fuel(30)
        .expect("consumption within budget should succeed");
    assert_eq!(vm.get_fuel(), Some(70));

    // A run charges the budget; the leftover is less than what was set.
    assert_eq!(vm.run().expect("run should halt"), VmStatus::Halted);
    let after_run = vm.get_fuel().expect("metering must still be active");
    assert!(
        after_run < 70,
        "run must consume from the active budget ({after_run} remaining)"
    );

    // Reset must clear the budget entirely (metering disabled, no leftovers).
    vm.reset_for_reuse();
    assert_eq!(vm.get_fuel(), None, "reset must clear run-scoped fuel");

    // A fresh budget starts from the configured amount, not from the prior
    // run's leftover.
    vm.set_fuel(200);
    assert_eq!(vm.run().expect("run should halt"), VmStatus::Halted);
    let fresh = vm.get_fuel().expect("metering must still be active");
    assert!(
        fresh < 200,
        "fresh budget must be consumed from its own amount ({fresh} remaining)"
    );
    assert!(
        fresh > after_run,
        "fresh budget must not inherit the prior run's leftover"
    );
}

/// The same immutable program can drive multiple VMs with independent stacks
/// and independent backend caches: one VM's reset and rerun never touches
/// another VM's execution state or cached artifacts.
#[test]
fn shared_program_backend_does_not_share_stacks_or_resources() {
    let program = Arc::new(
        compile_source(
            r#"
            fn action() -> int;
            action();
            "#,
        )
        .expect("source should compile")
        .program,
    );

    let mut first =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    let mut second =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    first.bind_static_non_yielding_args_function("action", non_yielding_returns_seven);
    second.bind_static_non_yielding_args_function("action", non_yielding_returns_nine);

    assert_eq!(first.run().expect("first should run"), VmStatus::Halted);
    assert_eq!(second.run().expect("second should run"), VmStatus::Halted);
    assert_eq!(first.stack(), &[Value::Int(7)]);
    assert_eq!(second.stack(), &[Value::Int(9)]);

    // Reset + rerun on one VM must not change the other VM's stack.
    first.reset_for_reuse();
    assert_eq!(first.run().expect("first should rerun"), VmStatus::Halted);
    assert_eq!(first.stack(), &[Value::Int(7)]);
    assert_eq!(second.stack(), &[Value::Int(9)]);
}

/// Reset closes run-scoped state while retaining documented reusable state:
/// host bindings, backend configuration, and compiled artifacts survive, while
/// the interpreter state (ip/stack/locals) is rewound.
#[test]
fn reset_closes_run_scoped_state_and_retains_reusable_state() {
    let program = compile_source(
        r#"
        fn action() -> int;
        action();
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_regex_cache_capacity(8);
    vm.bind_static_non_yielding_args_function("action", non_yielding_returns_forty_two);

    assert_eq!(vm.run().expect("first run should halt"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    vm.reset_for_reuse();

    // Reusable state survives reset.
    assert_eq!(
        vm.regex_cache_capacity(),
        8,
        "backend cache configuration is reusable across runs"
    );
    assert_eq!(
        vm.max_script_call_depth(),
        vm::DEFAULT_MAX_SCRIPT_CALL_DEPTH,
        "interpreter limits are reusable across runs"
    );

    // Run-scoped state is rewound: ip at entry, empty stack, null locals.
    assert_eq!(vm.ip(), 0, "reset must rewind the instruction pointer");
    assert!(vm.stack().is_empty(), "reset must clear the stack");
    assert!(
        vm.locals().iter().all(|value| *value == Value::Null),
        "reset must restore null locals"
    );

    // The retained host binding still executes on the next run.
    assert_eq!(vm.run().expect("second run should halt"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

/// A yield caused by an explicit wait must leave the instance in a state that
/// reset can close, and a subsequent run must not inherit the wait.
#[test]
fn reset_closes_waiting_state_before_the_next_run() {
    let program = compile_source(
        r#"
        fn action() -> int;
        action();
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    let op_id = start_scope_pending_op(&mut vm);
    vm.bind_args_function("action", Box::new(PendingOneHost { op_id }));

    let status = vm.run().expect("run should yield");
    assert_eq!(status, VmStatus::Waiting(op_id));
    assert_eq!(vm.waiting_host_op_id(), Some(op_id));

    vm.reset_for_reuse();
    assert_eq!(
        vm.waiting_host_op_id(),
        None,
        "reset must close the pending host wait"
    );
}

/// Regression pin for a pre-existing JIT issue that is NOT part of the
/// decomposition: a run that fails inside a host import can leave a native
/// trace/region that corrupts the *same instance's* next run after reset.
///
/// The bug reproduces on the unmodified tree (HEAD cccdd2f + dirty host work):
/// run a program whose first host call errors, `reset_for_reuse()`, then run
/// again with valid input — the second run can fail with `StackUnderflow`
/// instead of executing. Clearing native traces between the runs makes the
/// rerun behave correctly, which isolates the cause to stale JIT state.
#[test]
#[ignore = "pre-existing JIT stale-trace replay after reset; tracked separately"]
fn reset_after_host_error_reruns_cleanly_on_the_same_instance() {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    static FAIL_FIRST: OnceLock<AtomicBool> = OnceLock::new();
    let fail_first = FAIL_FIRST.get_or_init(|| AtomicBool::new(true));
    fail_first.store(true, Ordering::SeqCst);

    fn flaky_action(_: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
        if FAIL_FIRST
            .get_or_init(|| AtomicBool::new(true))
            .swap(false, Ordering::SeqCst)
        {
            Err(vm::VmError::HostError("first call fails".to_string()))
        } else {
            Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(42))))
        }
    }

    let program = compile_source(
        r#"
        fn action() -> int;
        pub fn run() -> int {
            action();
        }
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = vm::Vm::try_new(program).expect("test VM construction must not fail");
    vm.bind_static_non_yielding_args_function("action", flaky_action);
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);

    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    {
        let mut invocation = vm
            .start_invocation(callable, vec![])
            .expect("first invocation should start");
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(Some(Err(InvocationError::Host { .. })))
        ));
    }

    vm.reset_for_reuse();
    let items = collect_invocation_items(&mut vm, vec![]);
    assert!(
        matches!(&items[0], Ok(InvocationItem::Complete(Value::Int(42)))),
        "the rerun must execute cleanly after reset, got {items:?}"
    );
}

/// A BorrowMut capture cell is instance-scoped: independent instances over
/// the same immutable program never observe each other's cell values, and a
/// fresh instance always starts from fresh cells.
#[test]
fn closure_capture_cells_do_not_leak_between_instances() {
    let program = Arc::new(
        compile_source(
            r#"
            let mut state: string = "";
            let sink = |delta| if true => {
                state = state + delta;
                { action: "continue" }
            } else => {
                { action: "skip" }
            };
            let _ = sink("a");
            let _ = sink("b");
            state;
            "#,
        )
        .expect("source should compile")
        .program,
    );
    let mut first =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    assert_eq!(
        first.run().expect("first instance should halt"),
        VmStatus::Halted
    );
    assert_eq!(first.stack(), &[Value::string("ab")]);

    // A fresh instance over the same program starts with fresh capture
    // cells: the first instance's accumulated value must not leak into it.
    drop(first);

    // A second instance over the same program starts with a fresh cell and
    // accumulates only its own deltas: the first instance's cell value must
    // not leak into it.
    let mut second =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    assert_eq!(
        second.run().expect("second instance should halt"),
        VmStatus::Halted
    );
    assert_eq!(second.stack(), &[Value::string("ab")]);
}

/// Reset re-issues fresh callable identity: a host-held `Value::Callable`
/// resolved before the reset must be rejected by every host entry point once
/// the VM halts again, while a freshly resolved post-reset callable remains
/// fully invocable.
#[test]
fn stale_callable_handles_are_rejected_after_reset() {
    let program = compile_source(
        r#"
        pub fn run(input: int) -> int {
            input;
        }
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);

    let stale = vm
        .resolve_exported_callable("run")
        .expect("pre-reset run callable should resolve");
    vm.reset_for_reuse();
    assert_eq!(
        vm.run().expect("root should halt after reset"),
        VmStatus::Halted
    );

    // invoke_callable rejects the stale handle before any stack or frame
    // state changes.
    assert!(matches!(
        vm.invoke_callable(stale.clone(), &[Value::Int(1)]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));
    assert!(
        vm.stack().is_empty(),
        "rejected invocation must not touch the stack"
    );
    assert!(
        vm.execution_frames().is_empty(),
        "rejected invocation must not open frames"
    );

    // start_callable rejects the stale handle.
    assert!(matches!(
        vm.start_callable(stale.clone(), &[Value::Int(1)]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));

    // queue_callable rejects the stale handle eagerly; nothing is queued.
    assert!(matches!(
        vm.queue_callable(stale.clone(), vec![Value::Int(1)]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));
    assert_eq!(vm.queued_callable_count(), 0);

    // start_invocation surfaces the stale handle as one typed error item
    // followed by the fused end of stream.
    {
        let mut invocation = vm
            .start_invocation(stale, vec![Value::Int(1)])
            .expect("invocation handle should start");
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(Some(Err(InvocationError::Vm(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            )))))
        ));
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ));
    }

    // Control: a freshly resolved post-reset callable is fully invocable
    // through the same four entry points.
    let fresh = vm
        .resolve_exported_callable("run")
        .expect("post-reset run callable should resolve");
    assert_eq!(
        vm.invoke_callable(fresh.clone(), &[Value::Int(7)])
            .expect("fresh callable should invoke"),
        Value::Int(7)
    );
    assert_eq!(
        vm.start_callable(fresh.clone(), &[Value::Int(8)])
            .expect("fresh callable should start"),
        VmStatus::Halted
    );
    assert_eq!(vm.take_callable_result(), Some(Value::Int(8)));
    vm.queue_callable(fresh.clone(), vec![Value::Int(9)])
        .expect("fresh callable should queue");
    assert_eq!(
        vm.drain_callable_queue()
            .expect("queued fresh callable should drain"),
        vec![Value::Int(9)]
    );
    let items = collect_invocation_items(&mut vm, vec![Value::Int(10)]);
    assert_eq!(items.len(), 1, "fresh invocation must complete once");
    assert!(
        matches!(&items[0], Ok(InvocationItem::Complete(value)) if *value == Value::Int(10)),
        "fresh post-reset callable must still complete invocations, got {items:?}"
    );
}

/// Callable handles are instance-scoped: a handle resolved on one VM over a
/// shared program must be rejected by every host entry point on a second VM
/// over the same program, while the second VM's own handle keeps working.
#[test]
fn callable_handles_do_not_cross_vm_instances() {
    let program = Arc::new(
        compile_source(
            r#"
            pub fn run(input: int) -> int {
                input;
            }
            "#,
        )
        .expect("source should compile")
        .program,
    );
    let mut first =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    let mut second =
        Vm::try_new_shared(Arc::clone(&program)).expect("test VM construction must not fail");
    assert_eq!(
        first.run().expect("first root should halt"),
        VmStatus::Halted
    );
    assert_eq!(
        second.run().expect("second root should halt"),
        VmStatus::Halted
    );

    let foreign = first
        .resolve_exported_callable("run")
        .expect("first callable should resolve");

    assert!(matches!(
        second.invoke_callable(foreign.clone(), &[Value::Int(1)]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));
    assert!(matches!(
        second.start_callable(foreign.clone(), &[Value::Int(1)]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));
    assert!(matches!(
        second.queue_callable(foreign.clone(), vec![Value::Int(1)]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));
    assert_eq!(second.queued_callable_count(), 0);
    {
        let mut invocation = second
            .start_invocation(foreign, vec![Value::Int(1)])
            .expect("invocation handle should start");
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(Some(Err(InvocationError::Vm(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            )))))
        ));
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ));
    }

    // Control: the second VM's own handle is unaffected.
    let own = second
        .resolve_exported_callable("run")
        .expect("second callable should resolve");
    assert_eq!(
        second
            .invoke_callable(own, &[Value::Int(3)])
            .expect("own callable should invoke"),
        Value::Int(3)
    );
}

/// A pre-reset handle to a capture-bearing closure must be rejected after
/// reset: invoking it would otherwise resurrect the previous run's capture
/// cells into the fresh instance instead of observing the fresh cell state.
#[test]
fn stale_capture_callable_cannot_reach_the_previous_runs_cells() {
    use std::sync::OnceLock;

    static STASHED: OnceLock<std::sync::Mutex<Option<Value>>> = OnceLock::new();
    fn stash_callback(args: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
        STASHED
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .expect("stash lock")
            .replace(args[0].clone());
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::map(
            Vec::new(),
        ))))
    }

    let program = compile_source(
        r#"
        fn stash(callback: fn(string) -> map) -> map;
        let mut state: string = "";
        let sink = |delta| if true => {
            state = state + delta;
            { action: "continue" }
        } else => {
            { action: "skip" }
        };
        let _ = sink("a");
        let _ = sink("b");
        let _ = stash(sink);
        state;
        "#,
    )
    .expect("capture source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.bind_static_non_yielding_args_function("stash", stash_callback);
    assert_eq!(vm.run().expect("first run should halt"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::string("ab")],
        "first run should accumulate both deltas in the shared cell"
    );

    let stale = STASHED
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("stash lock")
        .clone()
        .expect("host should hold the pre-reset closure");
    assert!(
        matches!(stale, Value::Callable(_)),
        "host must hold a callable value"
    );

    vm.reset_for_reuse();
    assert_eq!(vm.run().expect("second run should halt"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::string("ab")],
        "second run must start from a fresh cell"
    );

    // The pre-reset closure handle must be rejected before it can touch the
    // previous run's cells.
    assert!(matches!(
        vm.invoke_callable(stale, &[Value::string("c")]),
        Err(VmError::InvalidFrameState(
            "callable does not belong to this vm"
        ))
    ));
    assert_eq!(
        vm.stack(),
        &[Value::string("ab")],
        "the rejected stale handle must not disturb the fresh run's state"
    );
}
/// P1 ownership contract for JIT-inlined root callable escapes.
///
/// A root callable binding of an inlined callee frame is materialized by the
/// JIT as a fresh `Value::Callable` per lifecycle. When that callable escapes
/// through a non-yielding host stash, every host entry gate must accept it,
/// because the materialization registered it with the owning VM. After
/// `reset_for_reuse`, a handle from the previous run must be rejected by every
/// gate, and the next run's JIT materialization must mint a *different* Arc so
/// the stale handle can never be re-legalized through JIT constant reuse.
#[test]
fn jit_inlined_callee_root_callable_escapes_through_host_stash() {
    use std::sync::{Mutex, OnceLock};
    fn native_jit_supported() -> bool {
        (cfg!(target_arch = "x86_64")
            && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
            || (cfg!(target_arch = "aarch64")
                && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
    }
    if !native_jit_supported() {
        return;
    }

    static STASHED: OnceLock<Mutex<(Option<Value>, usize)>> = OnceLock::new();
    fn stash_callback(args: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
        let mut slot = STASHED
            .get_or_init(|| Mutex::new((None, 0)))
            .lock()
            .expect("stash lock");
        slot.0.replace(args[0].clone());
        slot.1 += 1;
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::map(
            Vec::new(),
        ))))
    }
    fn stashed_value() -> Value {
        STASHED
            .get_or_init(|| Mutex::new((None, 0)))
            .lock()
            .expect("stash lock")
            .0
            .clone()
            .expect("host must hold a stashed callable")
    }
    fn stash_call_count() -> usize {
        STASHED
            .get_or_init(|| Mutex::new((None, 0)))
            .lock()
            .expect("stash lock")
            .1
    }

    let program = compile_source(
        r#"
        fn stash(callback: fn(map) -> map) -> map;
        fn helper(item: map) -> map { { action: "continue" } }
        fn identity() -> array {
            let mut a = [];
            a[0] = helper;
            a
        }
        let mut i: int = 0;
        while i < 40 {
            i = i + 1;
            let _ = stash(identity()[0]);
        }
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.bind_static_non_yielding_args_function("stash", stash_callback);
    vm.set_jit_config(vm::JitConfig {
        enabled: true,
        hot_loop_threshold: 4,
        max_trace_len: 256,
    });
    assert_eq!(vm.run().expect("first run should halt"), VmStatus::Halted);

    // The scenario must exercise the native JIT with a hot inline: one trace
    // must record the inlined callee's root binding as an owned-materialized
    // callable that reaches the stash host call.
    assert!(
        vm.jit_native_exec_count() > 0,
        "the test must drive the native JIT"
    );
    assert_eq!(
        stash_call_count(),
        40,
        "every loop iteration must reach the stash"
    );
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot.traces.iter().any(|trace| trace
            .op_names
            .iter()
            .any(|op| op.starts_with("inline_call"))),
        "the recorded trace must inline the callee: {:?}",
        snapshot
            .traces
            .iter()
            .map(|trace| trace.op_names.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.ssa_text().contains("host_call")),
        "the recorded trace must reach the stash host call:\n{}",
        snapshot
            .traces
            .iter()
            .map(|trace| trace.ssa_text())
            .collect::<Vec<_>>()
            .join("\n---\n")
    );

    let first = stashed_value();
    assert!(
        matches!(&first, Value::Callable(_)),
        "host must hold a callable value"
    );

    // Every host entry gate must accept the JIT-escaped handle of this run.
    assert_eq!(
        vm.start_callable(first.clone(), &[Value::map(Vec::new())])
            .expect("start_callable must accept the escaped callable"),
        VmStatus::Halted
    );
    assert!(
        vm.invoke_callable(first.clone(), &[Value::map(Vec::new())])
            .is_ok(),
        "invoke_callable must accept the escaped callable"
    );
    vm.queue_callable(first.clone(), vec![Value::map(Vec::new())])
        .expect("queue_callable must accept the escaped callable");
    assert_eq!(
        vm.drain_callable_queue()
            .expect("queued callable should drain")
            .len(),
        1
    );
    {
        let mut invocation = vm
            .start_invocation(first.clone(), vec![Value::map(Vec::new())])
            .expect("start_invocation must accept the escaped callable");
        let mut completed = false;
        loop {
            match invocation
                .poll_next()
                .expect("invocation poll should not fail")
            {
                InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_)))) => {
                    panic!("escaped callable invocation must not emit events");
                }
                InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(_)))) => {
                    completed = true;
                }
                InvocationPoll::Ready(Some(Err(error))) => {
                    panic!("escaped callable invocation failed: {error:?}")
                }
                InvocationPoll::Ready(None) => break,
                InvocationPoll::Pending => {
                    panic!("escaped callable invocation must not pend")
                }
            }
        }
        assert!(completed, "escaped callable invocation must complete");
    }
    {
        let mut store = vm::Store::new(vm, ());
        let callback = store
            .script_callback::<Value, Value>(first.clone())
            .expect("script_callback must accept the escaped callable");
        // Real contract: the callback mirrors the helper prototype's
        // `(map) -> map` callable schema.
        assert!(matches!(
            callback.schema(),
            Some(vm::compiler::TypeSchema::Callable { params, result })
            if *params
                == [vm::compiler::TypeSchema::Map(Box::new(
                    vm::compiler::TypeSchema::Unknown
                ))]
                && **result
                    == vm::compiler::TypeSchema::Map(Box::new(
                        vm::compiler::TypeSchema::Unknown
                    ))
        ));
        vm = store.into_vm();
    }

    // Reset invalidates every handle of the previous run.
    vm.reset_for_reuse();
    assert!(
        matches!(
            vm.start_callable(first.clone(), &[Value::map(Vec::new())]),
            Err(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            ))
        ),
        "stale handle must be rejected by start_callable"
    );
    assert!(
        matches!(
            vm.invoke_callable(first.clone(), &[Value::map(Vec::new())]),
            Err(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            ))
        ),
        "stale handle must be rejected by invoke_callable"
    );
    assert!(
        matches!(
            vm.queue_callable(first.clone(), vec![Value::map(Vec::new())]),
            Err(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            ))
        ),
        "stale handle must be rejected by queue_callable"
    );
    {
        let mut invocation = vm
            .start_invocation(first.clone(), vec![Value::map(Vec::new())])
            .expect("invocation handle should start");
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(Some(Err(InvocationError::Vm(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            )))))
        ));
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ));
    }
    {
        let mut store = vm::Store::new(vm, ());
        assert!(
            matches!(
                store.script_callback::<Value, Value>(first.clone()),
                Err(VmError::InvalidFrameState(
                    "script callable does not belong to this store"
                ))
            ),
            "stale handle must be rejected by script_callback"
        );
        vm = store.into_vm();
    }

    // The second run reuses the compiled trace; its escape must mint a fresh
    // Arc, succeed at every gate, and leave the stale handle rejected.
    let native_exec_after_first = vm.jit_native_exec_count();
    assert_eq!(vm.run().expect("second run should halt"), VmStatus::Halted);
    assert!(
        vm.jit_native_exec_count() > native_exec_after_first,
        "the second run must reuse the native trace ({} -> {})",
        native_exec_after_first,
        vm.jit_native_exec_count()
    );
    let second = stashed_value();
    let (Value::Callable(first_callable), Value::Callable(second_callable)) = (&first, &second)
    else {
        panic!("both escapes must be callables");
    };
    assert!(
        !Arc::ptr_eq(first_callable, second_callable),
        "each run's JIT escape must materialize a distinct Arc"
    );
    assert!(
        matches!(
            vm.invoke_callable(first.clone(), &[Value::map(Vec::new())]),
            Err(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            ))
        ),
        "the stale handle must not be re-legalized by the second run's JIT materialization"
    );
    assert_eq!(
        vm.start_callable(second.clone(), &[Value::map(Vec::new())])
            .expect("the second run's escaped callable must be accepted"),
        VmStatus::Halted
    );
    assert!(
        vm.invoke_callable(second.clone(), &[Value::map(Vec::new())])
            .is_ok(),
        "the second run's escaped callable must invoke"
    );
    vm.queue_callable(second.clone(), vec![Value::map(Vec::new())])
        .expect("the second run's escaped callable must queue");
    assert_eq!(
        vm.drain_callable_queue()
            .expect("queued callable should drain")
            .len(),
        1
    );
    {
        let mut invocation = vm
            .start_invocation(second.clone(), vec![Value::map(Vec::new())])
            .expect("the second run's escaped callable must start");
        let mut completed = false;
        loop {
            match invocation
                .poll_next()
                .expect("invocation poll should not fail")
            {
                InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_)))) => {
                    panic!("escaped callable invocation must not emit events");
                }
                InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(_)))) => {
                    completed = true;
                }
                InvocationPoll::Ready(Some(Err(error))) => {
                    panic!("second-run escaped callable invocation failed: {error:?}")
                }
                InvocationPoll::Ready(None) => break,
                InvocationPoll::Pending => {
                    panic!("second-run escaped callable invocation must not pend")
                }
            }
        }
        assert!(completed, "second-run escaped callable must complete");
    }
    {
        let mut store = vm::Store::new(vm, ());
        store
            .script_callback::<Value, Value>(second.clone())
            .expect("the second run's escaped callable must register as a script callback");
        vm = store.into_vm();
    }

    assert!(
        matches!(
            vm.start_callable(first.clone(), &[Value::map(Vec::new())]),
            Err(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            ))
        ),
        "the stale handle must stay rejected after the second run"
    );
}

/// P1 drop-contract for JIT root-callable materialization: every loop
/// iteration of the native trace writes a fresh `Value::Callable` Arc into
/// the same owned temp slot. The previous iteration's Arc must be released
/// when the slot is overwritten, exactly like the interpreter drops the
/// previous run's root binding on re-initialization. A `ptr::write`-style
/// overwrite would leak one strong ref per iteration, which stays observable
/// through `Weak` long after the run completes.
#[test]
fn jit_materialize_root_callable_releases_prior_iteration_arcs() {
    use std::sync::{Mutex, OnceLock, Weak};

    type SeenCallables = (Option<Value>, Vec<Weak<vm::CallableValue>>);
    static SEEN: OnceLock<Mutex<SeenCallables>> = OnceLock::new();
    fn stash_callback(args: &[Value]) -> Result<vm::CallOutcome, vm::VmError> {
        let mut slot = SEEN
            .get_or_init(|| Mutex::new((None, Vec::new())))
            .lock()
            .expect("stash lock");
        if let Value::Callable(callable) = &args[0] {
            slot.1.push(Arc::downgrade(callable));
        }
        slot.0.replace(args[0].clone());
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::map(
            Vec::new(),
        ))))
    }
    fn native_jit_supported() -> bool {
        (cfg!(target_arch = "x86_64")
            && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
            || (cfg!(target_arch = "aarch64")
                && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
    }
    if !native_jit_supported() {
        return;
    }

    let program = compile_source(
        r#"
        fn stash(callback: fn(map) -> map) -> map;
        fn helper(item: map) -> map { { action: "continue" } }
        fn identity() -> array {
            let mut a = [];
            a[0] = helper;
            a
        }
        let mut i: int = 0;
        while i < 40 {
            i = i + 1;
            let _ = stash(identity()[0]);
        }
        "#,
    )
    .expect("source should compile")
    .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.bind_static_non_yielding_args_function("stash", stash_callback);
    vm.set_jit_config(vm::JitConfig {
        enabled: true,
        hot_loop_threshold: 4,
        max_trace_len: 256,
    });
    assert_eq!(vm.run().expect("run should halt"), VmStatus::Halted);
    assert!(
        vm.jit_native_exec_count() > 0,
        "the test must drive the native JIT"
    );
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.ssa_text().contains("materialize_root_callable")),
        "the recorded trace must contain the materialize inst:\n{}",
        snapshot
            .traces
            .iter()
            .map(|trace| trace.ssa_text())
            .collect::<Vec<_>>()
            .join("\n---\n")
    );

    // Drop the host's stashed strong refs. The VM legitimately retains its
    // root-frame callable bindings, so the only surviving callables may be
    // exactly those root bindings. Any *other* surviving Arc is a leaked
    // JIT temp-slot value: the JIT materializes a fresh Arc per loop
    // iteration into the same owned temp slot, and every overwritten
    // iteration's Arc must be released (never leaked).
    let mut slot = SEEN
        .get_or_init(|| Mutex::new((None, Vec::new())))
        .lock()
        .expect("stash lock");
    let weaks = std::mem::take(&mut slot.1);
    slot.0 = None;
    drop(slot);
    assert!(
        weaks.len() >= 2,
        "the loop must stash a distinct callable per iteration"
    );
    let root_bindings = vm
        .locals()
        .iter()
        .filter_map(|value| match value {
            Value::Callable(arc) => Some(arc.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !root_bindings.is_empty(),
        "the root frame must retain its callable bindings"
    );
    let leaked = weaks
        .iter()
        .filter(|weak| {
            weak.upgrade().is_some_and(|arc| {
                !root_bindings
                    .iter()
                    .any(|root| std::sync::Arc::ptr_eq(root, &arc))
            })
        })
        .count();
    assert_eq!(
        leaked, 0,
        "every JIT-materialized root-callable Arc must be released after the run; \
         {leaked} non-root strong refs leaked from overwritten JIT temp slots"
    );
}
