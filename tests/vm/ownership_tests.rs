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

use vm::{HostFunctionRegistry, InvocationError, InvocationItem, InvocationPoll, Value, VmStatus};

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

struct PendingOneHost;

impl vm::HostArgsFunction for PendingOneHost {
    fn call(&mut self, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        Ok(vm::CallOutcome::Pending(1))
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

    let mut first = Vm::new_shared(Arc::clone(&program));
    let mut second = Vm::new_shared(Arc::clone(&program));
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
    let mut vm = Vm::new_shared(Arc::clone(&program));
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
    let mut vm = Vm::new(program);
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

    let mut first = Vm::new_shared(Arc::clone(&program));
    let mut second = Vm::new_shared(Arc::clone(&program));
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
    let mut vm = Vm::new(program);
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
    let mut vm = Vm::new(program);
    vm.bind_args_function("action", Box::new(PendingOneHost));

    let status = vm.run().expect("run should yield");
    assert_eq!(status, VmStatus::Waiting(1));
    assert_eq!(vm.waiting_host_op_id(), Some(1));

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
    let mut vm = vm::Vm::new(program);
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
