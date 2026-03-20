use std::time::{SystemTime, UNIX_EPOCH};

use vm::{
    AotArtifactError, BytecodeBuilder, CallOutcome, HostFunction, JitConfig, JitNyiReason,
    JitTraceTerminal, Value, Vm, VmStatus, compile_source,
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

fn disable_trace_jit(vm: &mut Vm) {
    vm.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 128,
    });
}

fn install_aot(vm: &mut Vm) {
    disable_trace_jit(vm);
    vm.compile_aot().expect("aot compile should succeed");
    assert!(vm.has_aot_program(), "aot program should be installed");
}

fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
    let start = instr_ip as usize + 1;
    code[start..start + 4].copy_from_slice(&target.to_le_bytes());
}

struct ManualTraceProgram {
    program: vm::Program,
    root_ip: usize,
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
    let _exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, loop_if_false_ip, root_ip);

    ManualTraceProgram {
        program: vm::Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code)
            .with_local_count(1),
        root_ip: root_ip as usize,
    }
}

fn backward_brfalse_non_root_program() -> (vm::Program, usize) {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldloc(0);
    let target_ip = bc.position();
    bc.ldc(1);
    bc.add();
    bc.dup();
    bc.stloc(0);
    bc.dup();
    bc.ldc(2);
    bc.ceq();
    let branch_ip = bc.position();
    bc.brfalse(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, branch_ip, target_ip);

    (
        vm::Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code)
            .with_local_count(1),
        target_ip as usize,
    )
}

fn unique_artifact_path() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    path.push(format!("pd-vm-aot-{}-{stamp}.bin", std::process::id()));
    path
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
            trace.op_names().iter().any(|op| op == "loop_if_false"),
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

    let (program, target_ip) = backward_brfalse_non_root_program();
    let mut vm = Vm::new(program);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    let resume_ips = vm.aot_resume_ips().expect("resume ips should exist");
    assert!(
        resume_ips.contains(&0),
        "entry ip should remain resumable, dump:\n{}",
        vm.dump_aot_info()
    );
    assert!(
        !resume_ips.contains(&target_ip),
        "non-call backward branch target should not be externally resumable, dump:\n{}",
        vm.dump_aot_info()
    );
}

#[test]
fn aot_keeps_backward_brfalse_outside_trace_as_guard_false() {
    if !native_jit_supported() {
        return;
    }

    let case = loop_if_false_root_program();
    let mut vm = Vm::new(case.program);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    let resume_ips = vm.aot_resume_ips().expect("resume ips should exist");
    assert!(
        resume_ips.contains(&0),
        "entry ip should remain resumable, dump:\n{}",
        vm.dump_aot_info()
    );
    assert!(
        !resume_ips.contains(&case.root_ip),
        "loop root should not be externally resumable without a host call boundary, dump:\n{}",
        vm.dump_aot_info()
    );
}

#[test]
fn jit_skips_tracing_when_builtin_override_disables_ssa_path() {
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
        assert!(
            snapshot.traces.is_empty(),
            "builtin overrides should disable SSA trace compilation, dump:\n{}",
            vm.dump_jit_info()
        );
        assert!(
            snapshot.attempts.is_empty(),
            "builtin overrides should avoid trace compile attempts, dump:\n{}",
            vm.dump_jit_info()
        );
        assert_eq!(
            vm.jit_native_exec_count(),
            0,
            "builtin override should force interpreter execution"
        );
    }
}

#[test]
fn aot_bundle_roundtrips_loop_if_false_traces() {
    if !native_jit_supported() {
        return;
    }

    let first_case = loop_if_false_root_program();
    let mut compiled_vm = Vm::new(first_case.program);
    install_aot(&mut compiled_vm);
    let expected_resume_ips = compiled_vm
        .aot_resume_ips()
        .expect("resume ips should exist")
        .to_vec();

    let artifact_path = unique_artifact_path();
    compiled_vm
        .save_aot_artifact_to_file(&artifact_path)
        .expect("artifact save should succeed");

    let second_case = loop_if_false_root_program();
    let mut loaded_vm = Vm::new(second_case.program);
    disable_trace_jit(&mut loaded_vm);
    loaded_vm
        .load_aot_artifact_from_file(&artifact_path)
        .expect("artifact load should succeed");
    std::fs::remove_file(&artifact_path).expect("artifact cleanup should succeed");

    assert_eq!(
        loaded_vm
            .aot_resume_ips()
            .expect("loaded resume ips should exist"),
        expected_resume_ips.as_slice()
    );

    let status = loaded_vm.run().expect("loaded aot should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(loaded_vm.stack(), &[Value::Int(4)]);
    assert!(
        loaded_vm.aot_exec_count() > 0,
        "loaded artifact should execute natively"
    );
}

#[test]
fn aot_bundle_rejects_program_hash_mismatch() {
    if !native_jit_supported() {
        return;
    }

    let source_case = loop_if_false_root_program();
    let mut source_vm = Vm::new(source_case.program);
    install_aot(&mut source_vm);
    let bytes = source_vm
        .encode_aot_artifact()
        .expect("artifact encode should succeed");

    let (other_program, _) = backward_brfalse_non_root_program();
    let mut target_vm = Vm::new(other_program);
    disable_trace_jit(&mut target_vm);
    let err = target_vm
        .load_aot_artifact(&bytes)
        .expect_err("different programs should reject the artifact");
    assert!(matches!(
        err,
        AotArtifactError::IncompatibleProgramHash { .. }
    ));
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
