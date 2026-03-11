use vm::jit::TraceStep;
use vm::{
    BytecodeBuilder, CallOutcome, HostFunction, JitConfig, JitNyiReason, JitTraceTerminal, Value,
    Vm, VmStatus, compile_source,
};

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

fn configure_jit(vm: &mut Vm) {
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 128,
    });
}

fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
    let start = instr_ip as usize + 1;
    code[start..start + 4].copy_from_slice(&target.to_le_bytes());
}

struct ManualTraceProgram {
    program: vm::Program,
    root_ip: usize,
    target_ip: usize,
    exit_ip: usize,
}

fn loop_if_false_root_program() -> ManualTraceProgram {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let root_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.ldloc(0);
    bc.ldc(2);
    bc.ceq();
    let loop_if_false_ip = bc.position();
    bc.brfalse(0);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, loop_if_false_ip, root_ip);

    ManualTraceProgram {
        program: vm::Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code)
            .with_local_count(1),
        root_ip: root_ip as usize,
        target_ip: root_ip as usize,
        exit_ip: exit_ip as usize,
    }
}

fn loop_if_false_internal_target_program() -> ManualTraceProgram {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(3);
    bc.pop();
    bc.ldc(0);
    bc.stloc(0);
    let target_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.ldloc(0);
    bc.ldc(2);
    bc.ceq();
    let loop_if_false_ip = bc.position();
    bc.brfalse(0);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, loop_if_false_ip, target_ip);

    ManualTraceProgram {
        program: vm::Program::new(
            vec![Value::Int(0), Value::Int(1), Value::Int(4), Value::Int(999)],
            code,
        )
        .with_local_count(1),
        root_ip: 0,
        target_ip: target_ip as usize,
        exit_ip: exit_ip as usize,
    }
}

fn backward_guard_outside_trace_program() -> ManualTraceProgram {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let target_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    let entry_jump_ip = bc.position();
    bc.br(0);

    let root_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(2);
    bc.clt();
    let exit_guard_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(0);
    bc.ldc(3);
    bc.ceq();
    let outside_guard_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    let loop_back_ip = bc.position();
    bc.br(0);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, entry_jump_ip, root_ip);
    patch_branch_target(&mut code, exit_guard_ip, exit_ip);
    patch_branch_target(&mut code, outside_guard_ip, target_ip);
    patch_branch_target(&mut code, loop_back_ip, root_ip);

    ManualTraceProgram {
        program: vm::Program::new(
            vec![Value::Int(0), Value::Int(1), Value::Int(4), Value::Int(2)],
            code,
        )
        .with_local_count(1),
        root_ip: root_ip as usize,
        target_ip: target_ip as usize,
        exit_ip: exit_ip as usize,
    }
}

struct UnusedBuiltinOverride;

impl HostFunction for UnusedBuiltinOverride {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![]))
    }
}

#[test]
fn jit_supports_backward_brfalse_to_trace_root() {
    let case = loop_if_false_root_program();
    let mut vm = Vm::new(case.program);
    configure_jit(&mut vm);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        let trace = snapshot
            .traces
            .iter()
            .find(|trace| trace.root_ip == case.root_ip)
            .expect("expected a compiled loop trace");
        assert_eq!(trace.terminal, JitTraceTerminal::BranchExit);
        assert!(
            trace.steps.iter().any(|step| matches!(
                step,
                TraceStep::LoopIfFalse { target_ip, exit_ip }
                    if *target_ip == case.target_ip && *exit_ip == case.exit_ip
            )),
            "expected loop_if_false in trace, dump:\n{}",
            vm.dump_jit_info()
        );
        assert!(
            snapshot
                .attempts
                .iter()
                .all(|attempt| attempt.result.is_ok()),
            "expected backward brfalse to compile without NYI attempts, dump:\n{}",
            vm.dump_jit_info()
        );
        let dump = vm.dump_jit_info();
        assert!(
            !dump.contains("BackwardGuard") && !dump.contains("brfalse (backward target)"),
            "backward brfalse NYI references should be gone, dump:\n{dump}"
        );
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected native execution for backward brfalse loop, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn aot_supports_backward_brfalse_to_earlier_non_root_step() {
    if !native_jit_supported() {
        return;
    }

    let case = loop_if_false_internal_target_program();
    let mut vm = Vm::new(case.program);
    configure_jit(&mut vm);

    let prepared = vm.prepare_aot().expect("AOT prepare should succeed");
    assert!(
        prepared > 0,
        "expected AOT compilation for internal loop target"
    );
    let snapshot = vm.jit_snapshot();
    let trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.root_ip == case.root_ip)
        .expect("expected a root trace from AOT prepare");
    assert_ne!(
        case.root_ip, case.target_ip,
        "test must target a non-root step"
    );
    assert!(
        trace.steps.iter().any(|step| matches!(
            step,
            TraceStep::LoopIfFalse { target_ip, exit_ip }
                if *target_ip == case.target_ip && *exit_ip == case.exit_ip
        )),
        "expected loop_if_false to target an earlier non-root step, dump:\n{}",
        vm.dump_jit_info()
    );

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native AOT execution for non-root loop target, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn aot_keeps_backward_brfalse_outside_trace_as_guard_false() {
    if !native_jit_supported() {
        return;
    }

    let case = backward_guard_outside_trace_program();
    let mut vm = Vm::new(case.program);
    configure_jit(&mut vm);

    let prepared = vm.prepare_aot().expect("AOT prepare should succeed");
    assert!(
        prepared > 0,
        "expected AOT compilation for out-of-trace guard"
    );
    let snapshot = vm.jit_snapshot();
    let trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.root_ip == case.root_ip)
        .expect("expected a compiled trace rooted at the forward entry block");
    assert!(
        trace.steps.iter().any(|step| matches!(
            step,
            TraceStep::GuardFalse { exit_ip } if *exit_ip == case.target_ip
        )),
        "expected backward target outside the trace to stay as guard_false, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        !trace.steps.iter().any(|step| matches!(
            step,
            TraceStep::LoopIfFalse { target_ip, .. } if *target_ip == case.target_ip
        )),
        "unexpected loop_if_false for out-of-trace target, dump:\n{}",
        vm.dump_jit_info()
    );

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn trace_interpreter_path_executes_loop_if_false() {
    let case = loop_if_false_root_program();
    let mut vm = Vm::new(case.program);
    configure_jit(&mut vm);
    vm.bind_builtin_override("json::encode", Box::new(UnusedBuiltinOverride))
        .expect("json::encode should be a valid builtin override");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        let trace = snapshot
            .traces
            .iter()
            .find(|trace| trace.root_ip == case.root_ip)
            .expect("expected the loop trace to compile");
        assert!(
            trace.steps.iter().any(|step| matches!(
                step,
                TraceStep::LoopIfFalse { target_ip, .. } if *target_ip == case.target_ip
            )),
            "expected loop_if_false in interpreter-executed trace, dump:\n{}",
            vm.dump_jit_info()
        );
        assert!(
            trace.executions > 0,
            "expected interpreter trace execution count to advance, dump:\n{}",
            vm.dump_jit_info()
        );
        assert_eq!(
            vm.jit_native_exec_count(),
            0,
            "builtin override should force interpreter trace execution"
        );
    }
}

#[test]
fn aot_bundle_roundtrips_loop_if_false_traces() {
    if !native_jit_supported() {
        return;
    }

    let case = loop_if_false_internal_target_program();
    let mut vm = Vm::new(case.program);
    configure_jit(&mut vm);

    let bundle = vm
        .emit_aot_bundle()
        .expect("AOT bundle emit should succeed");
    let mut loaded = Vm::from_aot_bundle_bytes(&bundle).expect("AOT bundle load should succeed");

    let snapshot = loaded.jit_snapshot();
    let trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.root_ip == case.root_ip)
        .expect("expected loop trace in loaded AOT bundle");
    assert!(
        trace.steps.iter().any(|step| matches!(
            step,
            TraceStep::LoopIfFalse { target_ip, exit_ip }
                if *target_ip == case.target_ip && *exit_ip == case.exit_ip
        )),
        "expected loop_if_false to survive AOT roundtrip, dump:\n{}",
        loaded.dump_jit_info()
    );

    let status = loaded.run().expect("loaded AOT vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(loaded.stack(), &[Value::Int(4)]);
    assert!(
        loaded.jit_native_exec_count() > 0,
        "expected loaded AOT trace to execute natively, dump:\n{}",
        loaded.dump_jit_info()
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
