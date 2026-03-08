#![cfg(feature = "runtime")]

use vm::{BytecodeBuilder, JitConfig, JitNyiReason, Value, Vm, VmStatus, compile_source};

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[test]
fn jit_records_backward_guard_nyi_and_falls_back_to_interpreter() {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.brfalse(0);
    bc.ldc(1);
    bc.ret();
    let program = vm::Program::new(vec![Value::Bool(true), Value::Int(7)], bc.finish());

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 64,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.is_empty(),
            "backward guard should be NYI and not produce traces"
        );
        assert!(
            snapshot.attempts.iter().any(|attempt| matches!(
                attempt.result,
                Err(JitNyiReason::BackwardGuard { target: 0 })
            )),
            "expected BackwardGuard NYI attempt, dump:\n{}",
            vm.dump_jit_info()
        );
        assert_eq!(
            vm.jit_native_exec_count(),
            0,
            "NYI compile should not execute native traces"
        );
    }
}

#[test]
fn aot_prepare_records_backward_guard_nyi_and_skips_compilation() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.brfalse(0);
    bc.ldc(1);
    bc.ret();
    let program = vm::Program::new(vec![Value::Bool(true), Value::Int(7)], bc.finish());

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 64,
    });

    let _prepared = vm.prepare_aot().expect("AOT prepare should succeed");
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot.traces.iter().all(|trace| trace.root_ip != 0),
        "backward guard root should not produce an AOT trace: {:?}",
        snapshot.traces
    );
    assert!(
        snapshot.attempts.iter().any(|attempt| matches!(
            attempt.result,
            Err(JitNyiReason::BackwardGuard { target: 0 })
        )),
        "expected BackwardGuard NYI attempt, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn jit_records_trace_too_long_nyi_and_preserves_results() {
    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 6 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 2,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(15)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.is_empty(),
            "trace-too-long NYI should block trace compilation"
        );
        assert!(
            snapshot.attempts.iter().any(|attempt| matches!(
                attempt.result,
                Err(JitNyiReason::TraceTooLong { limit: 2 })
            )),
            "expected TraceTooLong NYI attempt, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn jit_rejects_zero_hot_loop_threshold_with_explicit_nyi_reason() {
    let source = r#"
        let mut i = 0;
        while i < 4 {
            i = i + 1;
        }
        i;
    "#;
    let compiled = compile_source(source).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 0,
        max_trace_len: 64,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.is_empty(),
            "invalid threshold should prevent compiled traces"
        );
        assert!(
            snapshot
                .attempts
                .iter()
                .any(|attempt| matches!(attempt.result, Err(JitNyiReason::HotLoopThresholdZero))),
            "expected HotLoopThresholdZero NYI attempt, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}
