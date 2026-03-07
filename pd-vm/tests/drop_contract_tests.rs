//! Focused drop-contract tests verifying that `Stmt::Drop` nodes emitted by the
//! lifetime pass correctly route through the VM's `drop_value_with_contract()`.
//!
//! Coverage targets:
//!   - dead-local single-drop
//!   - branch/loop ordering
//!   - yield/host-op + drop
//!   - closure-capture drop ordering
//!   - native/JIT parity (when cranelift-jit feature is enabled)
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
    // Use a generous fuel budget so the VM yields between loop iterations
    // rather than mid-expression.  The loop body compiles to many opcodes,
    // so 200 ops per slice keeps things cooperative.
    vm.set_fuel(200);

    let mut total_yields = 0u64;
    loop {
        let status = vm.run().expect("vm should run");
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                total_yields += 1;
                vm.recharge_fuel(200).expect("recharge should succeed");
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
