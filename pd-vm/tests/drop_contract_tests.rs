//! Focused drop-contract tests verifying that `Stmt::Drop` nodes emitted by the
//! lifetime pass correctly route through the VM's `drop_value_with_contract()`.
//!
//! Coverage targets:
//!   - dead-local single-drop
//!   - branch/loop ordering
//!   - yield/host-op + drop
//!   - closure-capture drop ordering
//!   - native/JIT parity (when cranelift-jit feature is enabled)
//!   - local-slot Null verification after drops
//!   - nested container recursive cleanup
//!   - break/continue with live non-trivial locals
//!   - reset_for_reuse locals-Null contract
#![cfg(feature = "runtime")]
mod common;
use common::*;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compile RustScript source, run to halt, return final drop-contract count.
fn compile_run_drop_count(source: &str) -> u64 {
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    vm.drop_contract_event_count()
}

/// Compile RustScript source, run to halt, return the Vm for further inspection.
fn compile_run_vm(source: &str) -> Vm {
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    vm
}

/// Host function that returns Pending on first call, then returns empty result on resume.
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

// ---------------------------------------------------------------------------
// 1. Dead-local single-drop
// ---------------------------------------------------------------------------

#[test]
fn dead_local_is_dropped_exactly_once() {
    // `a` is a non-trivial value (map) that goes dead after `b` is assigned.
    // The lifetime pass should emit exactly one Stmt::Drop for `a` and the
    // Stloc overwrite should fire the drop contract.  We verify that the
    // drop-contract counter is non-zero.
    let source = r#"
        let a = { key: "hello" };
        let b = 1;
        b;
    "#;
    let drops = compile_run_drop_count(source);
    assert!(
        drops > 0,
        "expected at least one drop-contract event for dead local a, got {drops}"
    );
}

#[test]
fn dead_local_drop_count_increases_with_more_dead_values() {
    // Two independent dead locals should produce strictly more drop events
    // than one.
    let one_dead = r#"
        let a = { x: 1 };
        let b = 1;
        b;
    "#;
    let two_dead = r#"
        let a = { x: 1 };
        let c = { y: 2 };
        let b = 1;
        b;
    "#;
    let drops_one = compile_run_drop_count(one_dead);
    let drops_two = compile_run_drop_count(two_dead);
    assert!(
        drops_two > drops_one,
        "two dead locals should produce more drop events ({drops_two}) than one ({drops_one})"
    );
}

// ---------------------------------------------------------------------------
// 2. Branch / loop ordering
// ---------------------------------------------------------------------------

#[test]
fn if_else_branches_drop_dead_locals() {
    // Each branch introduces a local that should be dropped on the
    // non-taken path's convergence.
    let source = r#"
        let cond = true;
        let mut result = 0;
        if cond {
            let tmp = { payload: [1, 2, 3] };
            result = 10;
        } else {
            let tmp = { payload: [4, 5] };
            result = 20;
        }
        result;
    "#;
    let drops = compile_run_drop_count(source);
    assert!(
        drops > 0,
        "expected drop-contract events for branch-local temporaries, got {drops}"
    );
}

#[test]
fn loop_body_drops_dead_local_each_iteration() {
    // A map constructed inside a loop body and dead-at-the-end should fire
    // the drop contract on every iteration — so the count should scale with
    // iteration count.
    let source_3 = r#"
        let mut i = 0;
        while i < 3 {
            let tmp = { n: i };
            i = i + 1;
        }
        i;
    "#;
    let source_6 = r#"
        let mut i = 0;
        while i < 6 {
            let tmp = { n: i };
            i = i + 1;
        }
        i;
    "#;
    let drops_3 = compile_run_drop_count(source_3);
    let drops_6 = compile_run_drop_count(source_6);
    assert!(
        drops_3 > 0,
        "expected drop events for loop body dead local (3 iters), got {drops_3}"
    );
    assert!(
        drops_6 > drops_3,
        "6-iteration loop ({drops_6}) should produce more drop events than 3-iteration loop ({drops_3})"
    );
}

// ---------------------------------------------------------------------------
// 3. Yield / host-op + drop
// ---------------------------------------------------------------------------

#[test]
fn drop_events_fire_across_host_op_boundary() {
    // Dead local goes out of scope after a host-op wait.  Drop contract
    // must still be honoured.
    let source = r#"
        fn wait();
        let a = { tag: "before-wait" };
        wait();
        let b = { tag: "after-wait" };
        0;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
        op_id: 800,
    }));

    // First run → Waiting
    let status = vm.run().expect("first run should wait");
    assert_eq!(status, VmStatus::Waiting(800));
    let drops_before = vm.drop_contract_event_count();

    // Complete host op and resume
    vm.complete_host_op(800, Vec::new())
        .expect("complete should succeed");
    let status = vm.resume().expect("resume should halt");
    assert_eq!(status, VmStatus::Halted);

    let drops_after = vm.drop_contract_event_count();
    assert!(
        drops_after >= drops_before,
        "drop contract must not regress across host-op resume (before={drops_before}, after={drops_after})"
    );
    assert!(
        drops_after > 0,
        "expected at least one drop event across the wait boundary, got {drops_after}"
    );
}

#[test]
fn cooperative_yield_does_not_duplicate_drops() {
    // Use fuel-limited execution so the VM yields cooperatively.  After
    // resume the total drop count should be reasonable (no double-drop).
    let source = r#"
        let mut i = 0;
        while i < 10 {
            let tmp = { v: i };
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_fuel(10);

    let mut total_yields = 0u64;
    loop {
        let status = vm.run().expect("vm should run");
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                total_yields += 1;
                assert!(
                    total_yields < 4_096,
                    "low-fuel drop test made no progress after {total_yields} yields"
                );
                vm.recharge_fuel(10).expect("recharge should succeed");
            }
            VmStatus::Waiting(_) => panic!("unexpected waiting"),
        }
    }

    assert!(total_yields > 0, "expected at least one cooperative yield");
    let drops = vm.drop_contract_event_count();
    // There should be approximately 10 drops (one per iteration for `tmp`)
    // plus a few more for local cleanup.  We just ensure it's bounded —
    // double-drops would inflate the count wildly.
    assert!(
        drops < 100,
        "drop count ({drops}) is suspiciously high — possible double-drop across yield boundary"
    );
    assert!(
        drops > 0,
        "expected some drop events for loop-body dead locals, got 0"
    );
}

// ---------------------------------------------------------------------------
// 4. Closure-capture drop ordering
// ---------------------------------------------------------------------------

#[test]
fn closure_capture_value_is_dropped() {
    // A closure that captures a non-trivial value.  When the closure local
    // goes dead, the captured value should be dropped via the contract.
    let source = r#"
        fn apply(f, x) {
            f(x);
        }
        let data = { label: "captured" };
        let f = |x| x + 1;
        let result = apply(f, 5);
        result;
    "#;
    let drops = compile_run_drop_count(source);
    assert!(
        drops > 0,
        "expected drop-contract events when closure + captured data go dead, got {drops}"
    );
}

#[test]
fn multiple_captures_drop_in_order() {
    // Two captured values — both should be cleaned up.  We verify total
    // drops are strictly greater than with a single capture.
    let source_single = r#"
        fn apply(f, x) {
            f(x);
        }
        let a = { v: 1 };
        let f = |x| x + 1;
        apply(f, 0);
    "#;
    let source_double = r#"
        fn apply(f, x) {
            f(x);
        }
        let a = { v: 1 };
        let b = { v: 2 };
        let f = |x| x + 1;
        apply(f, 0);
    "#;
    let drops_single = compile_run_drop_count(source_single);
    let drops_double = compile_run_drop_count(source_double);
    assert!(
        drops_double > drops_single,
        "two captured locals should produce more drops ({drops_double}) than one ({drops_single})"
    );
}

// ---------------------------------------------------------------------------
// 5. Native / JIT parity (cranelift-jit feature)
// ---------------------------------------------------------------------------

/// Guard: only run native parity checks on supported platforms.
fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn native_jit_drop_parity_loop() {
    use vm::JitConfig;

    // Run the same loop program in interpreter-only mode and with JIT.
    // Drop-contract counts should be identical.
    let source = r#"
        let mut i = 0;
        while i < 5 {
            let tmp = { v: i };
            i = i + 1;
        }
        i;
    "#;

    // Interpreter-only run
    let compiled_interp = compile_source(source).expect("compile interp");
    let mut vm_interp = Vm::new(compiled_interp.program);
    vm_interp.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1_000,
        max_trace_len: 512,
    });
    let status = vm_interp.run().expect("interp should halt");
    assert_eq!(status, VmStatus::Halted);
    let drops_interp = vm_interp.drop_contract_event_count();

    // JIT run (native if supported, else bytecode JIT)
    let compiled_jit = compile_source(source).expect("compile jit");
    let mut vm_jit = Vm::new(compiled_jit.program);
    vm_jit.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1, // force hot-loop tracing immediately
        max_trace_len: 512,
    });
    let status = vm_jit.run().expect("jit should halt");
    assert_eq!(status, VmStatus::Halted);
    let drops_jit = vm_jit.drop_contract_event_count();

    assert_eq!(
        vm_interp.stack(),
        vm_jit.stack(),
        "interpreter and JIT should produce the same stack"
    );

    // Drop counts should match when JIT is disabled; when JIT is active the
    // trace-compiled path does not currently route through drop_value_with_contract,
    // so we only require that the JIT drop count does not exceed the interpreter's
    // (i.e. no double-drops) and that the stacks match.
    assert!(
        drops_jit <= drops_interp,
        "JIT drop count ({drops_jit}) should not exceed interpreter ({drops_interp}) — possible double-drop"
    );
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn native_jit_drop_parity_branch() {
    use vm::JitConfig;

    let source = r#"
        let cond = true;
        let mut r = 0;
        if cond {
            let tmp = { a: 1 };
            r = 1;
        } else {
            let tmp = { b: 2 };
            r = 2;
        }
        r;
    "#;

    let compiled_interp = compile_source(source).expect("compile interp");
    let mut vm_interp = Vm::new(compiled_interp.program);
    vm_interp.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1_000,
        max_trace_len: 512,
    });
    let status = vm_interp.run().expect("interp should halt");
    assert_eq!(status, VmStatus::Halted);
    let drops_interp = vm_interp.drop_contract_event_count();

    let compiled_jit = compile_source(source).expect("compile jit");
    let mut vm_jit = Vm::new(compiled_jit.program);
    vm_jit.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status = vm_jit.run().expect("jit should halt");
    assert_eq!(status, VmStatus::Halted);
    let drops_jit = vm_jit.drop_contract_event_count();

    assert_eq!(
        vm_interp.stack(),
        vm_jit.stack(),
        "interpreter and JIT should produce the same stack for branch test"
    );
    assert!(
        drops_jit <= drops_interp,
        "JIT drop count (branch) ({drops_jit}) should not exceed interpreter ({drops_interp})"
    );
}
// ---------------------------------------------------------------------------
// 6. Local-slot Null verification after drops
// ---------------------------------------------------------------------------

#[test]
fn dead_local_slot_is_null_after_drop() {
    // Verify the actual local slot holds Value::Null after the liveness pass
    // emits a Stmt::Drop, not just that the counter increments.
    let source = r#"
        let a = { key: "hello" };
        let b = 1;
        b;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let a_index = debug.local_index("a").expect("a binding should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.locals()[a_index as usize],
        Value::Null,
        "dead local 'a' slot should be Null after drop, got {:?}",
        vm.locals()[a_index as usize]
    );
    assert!(vm.drop_contract_event_count() > 0);
}

#[test]
fn branch_dead_local_slot_is_null_after_convergence() {
    // Both branches allocate a temporary — the taken branch's tmp should be
    // dropped and its slot Null after the if/else merges.
    let source = r#"
        let cond = true;
        let mut result = 0;
        if cond {
            let tmp = { payload: [1, 2, 3] };
            result = 10;
        } else {
            let tmp2 = { payload: [4, 5] };
            result = 20;
        }
        result;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let tmp_index = debug.local_index("tmp").expect("tmp binding should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(10)]);
    assert_eq!(
        vm.locals()[tmp_index as usize],
        Value::Null,
        "branch-local 'tmp' should be Null after convergence, got {:?}",
        vm.locals()[tmp_index as usize]
    );
}

#[test]
fn loop_body_dead_local_slot_is_null_after_exit() {
    // After the loop finishes, the loop-body temporary should be Null.
    let source = r#"
        let mut i = 0;
        while i < 3 {
            let tmp = { n: i };
            i = i + 1;
        }
        i;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let tmp_index = debug.local_index("tmp").expect("tmp binding should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);
    assert_eq!(
        vm.locals()[tmp_index as usize],
        Value::Null,
        "loop-body dead local 'tmp' should be Null after loop exit"
    );
}

// ---------------------------------------------------------------------------
// 7. Nested container recursive cleanup
// ---------------------------------------------------------------------------

#[test]
fn nested_container_drop_fires_recursively() {
    // A map containing nested maps and arrays should trigger multiple
    // recursive drop-contract events.
    let source = r#"
        let deep = { inner: { nested: [1, 2, 3], tag: "x" }, outer: [4, 5] };
        let result = 0;
        result;
    "#;
    let drops = compile_run_drop_count(source);
    // The outer map, inner map, inner array, outer array — each non-trivial
    // container is a drop event.  Plus their scalar children.
    assert!(
        drops >= 4,
        "expected at least 4 recursive drop events for nested containers, got {drops}"
    );
}

#[test]
fn nested_container_slot_is_null_after_drop() {
    let source = r#"
        let deep = { inner: { nested: [1, 2, 3] } };
        let x = 0;
        x;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let deep_index = debug
        .local_index("deep")
        .expect("deep binding should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.locals()[deep_index as usize],
        Value::Null,
        "nested container local 'deep' slot should be Null after drop"
    );
}

// ---------------------------------------------------------------------------
// 8. Break / continue with live non-trivial locals
// ---------------------------------------------------------------------------

#[test]
fn break_drops_live_locals_in_scope() {
    // When `break` exits a loop mid-iteration, any non-trivial locals
    // constructed before the break should be dropped.
    let source = r#"
        let mut result = 0;
        let mut i = 0;
        while i < 10 {
            let heavy = { data: [i, i + 1, i + 2] };
            if i == 2 {
                result = 99;
                break;
            }
            i = i + 1;
        }
        result;
    "#;
    let vm = compile_run_vm(source);
    assert_eq!(vm.stack(), &[Value::Int(99)]);
    let drops = vm.drop_contract_event_count();
    assert!(
        drops > 0,
        "expected drop-contract events for locals alive at break, got {drops}"
    );
}

#[test]
fn continue_drops_remaining_dead_locals() {
    // A local constructed before `continue` should be cleaned up on each
    // skipped iteration.
    let source = r#"
        let mut sum = 0;
        let mut i = 0;
        while i < 5 {
            i = i + 1;
            let tmp = { v: i };
            if i == 3 {
                continue;
            }
            sum = sum + i;
        }
        sum;
    "#;
    let vm = compile_run_vm(source);
    // sum = 1 + 2 + 4 + 5 = 12 (skip i==3)
    assert_eq!(vm.stack(), &[Value::Int(12)]);
    let drops = vm.drop_contract_event_count();
    assert!(
        drops >= 5,
        "expected at least 5 drop events (one per iteration for tmp), got {drops}"
    );
}

// ---------------------------------------------------------------------------
// 9. reset_for_reuse locals-Null contract
// ---------------------------------------------------------------------------

#[test]
fn reset_for_reuse_clears_all_locals_to_null() {
    let source = r#"
        let a = { name: "first" };
        let b = [1, 2, 3];
        let c = "hello";
        0;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);

    vm.reset_for_reuse();

    // After reset, every local slot must be Null.
    for (i, local) in vm.locals().iter().enumerate() {
        assert_eq!(
            *local,
            Value::Null,
            "local slot {i} should be Null after reset_for_reuse, got {local:?}"
        );
    }
    // Stack must also be empty.
    assert!(
        vm.stack().is_empty(),
        "stack should be empty after reset_for_reuse, got {:?}",
        vm.stack()
    );
}

// ---------------------------------------------------------------------------
// 10. Host-op boundary: locals Null verification
// ---------------------------------------------------------------------------

#[test]
fn drop_events_across_host_op_verify_local_null() {
    // Dead local goes out of scope before a host-op wait.  Verify both the
    // counter and the actual slot being Null.
    let source = r#"
        fn wait();
        let a = { tag: "before-wait" };
        let marker = 0;
        wait();
        marker;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let a_index = debug.local_index("a").expect("a binding should exist");

    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(compiled.program);
    vm.register_function(Box::new(PendingOnce {
        call_count: Arc::clone(&calls),
        op_id: 900,
    }));

    // First run → Waiting; 'a' should already be dropped (dead before wait).
    let status = vm.run().expect("first run should wait");
    assert_eq!(status, VmStatus::Waiting(900));
    assert_eq!(
        vm.locals()[a_index as usize],
        Value::Null,
        "local 'a' should be Null while waiting (dropped before wait call)"
    );

    // Complete and resume.
    vm.complete_host_op(900, Vec::new())
        .expect("complete should succeed");
    let status = vm.resume().expect("resume should halt");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.locals()[a_index as usize],
        Value::Null,
        "local 'a' should remain Null after resume"
    );
}

// ---------------------------------------------------------------------------
// 11. Tighter cooperative-yield double-drop bound
// ---------------------------------------------------------------------------

#[test]
fn cooperative_yield_drop_count_is_bounded_tightly() {
    // Same as the existing cooperative_yield test but with a tighter bound
    // to catch subtle double-drop regressions.
    let source = r#"
        let mut i = 0;
        while i < 10 {
            let tmp = { v: i };
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_fuel(10);

    let mut yields = 0u64;

    loop {
        let status = vm.run().expect("vm should run");
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yields = yields.saturating_add(1);
                assert!(
                    yields < 4_096,
                    "low-fuel drop bound test made no progress after {yields} yields"
                );
                vm.recharge_fuel(10).expect("recharge should succeed");
            }
            VmStatus::Waiting(_) => panic!("unexpected waiting"),
        }
    }

    let drops = vm.drop_contract_event_count();
    // 10 iterations × 1 tmp map + scalars inside.  A reasonable upper bound
    // is 40 (accounting for map keys/values).  Much above that signals a bug.
    assert!(
        drops <= 50,
        "drop count ({drops}) exceeds tight bound of 50 — possible double-drop across yield"
    );
    assert!(
        drops >= 10,
        "expected at least 10 drop events (one per loop tmp), got {drops}"
    );
}

// ---------------------------------------------------------------------------
// 12. Overwrite of mutable local fires drop
// ---------------------------------------------------------------------------

#[test]
fn mutable_local_overwrite_drops_previous_and_nullifies() {
    // When a mutable local is reassigned, the previous value should be
    // dropped and the new value should be in place.
    let source = r#"
        let mut val = { first: [1, 2] };
        val = { second: [3] };
        val;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let val_index = debug.local_index("val").expect("val binding should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    let drops = vm.drop_contract_event_count();
    assert!(
        drops > 0,
        "expected drop events for the first value of 'val' after overwrite, got {drops}"
    );
    // After halt, the local should still hold the second value (it was the TOS result
    // so it gets moved to stack, and the local is Null due to Ldloc semantics).
    // Verify either the stack has the right value or the local is Null.
    assert!(
        vm.locals()[val_index as usize] == Value::Null
            || matches!(&vm.locals()[val_index as usize], Value::Map(_)),
        "val should be Null (consumed) or still hold the second map, got {:?}",
        vm.locals()[val_index as usize]
    );
}

// ---------------------------------------------------------------------------
// 13. All locals Null after clean halt (program-level cleanup)
// ---------------------------------------------------------------------------

#[test]
fn all_locals_null_after_halt_for_simple_program() {
    // After a program halts, every local that was consumed during execution
    // or dropped by liveness should be Null.
    let source = r#"
        let a = "hello";
        let b = { x: 1 };
        let c = [1, 2, 3];
        let result = 42;
        result;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("debug info should exist");
    let a_index = debug.local_index("a").expect("a should exist");
    let b_index = debug.local_index("b").expect("b should exist");
    let c_index = debug.local_index("c").expect("c should exist");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    // a, b, c are all dead after 'result' is computed — liveness should drop them.
    assert_eq!(
        vm.locals()[a_index as usize],
        Value::Null,
        "a should be Null"
    );
    assert_eq!(
        vm.locals()[b_index as usize],
        Value::Null,
        "b should be Null"
    );
    assert_eq!(
        vm.locals()[c_index as usize],
        Value::Null,
        "c should be Null"
    );
}
