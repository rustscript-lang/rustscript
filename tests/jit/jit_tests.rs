use std::{cell::Cell, sync::Arc};

use vm::{
    BytecodeBuilder, CallOutcome, CallReturn, HostFunction, JitConfig, JitTraceTerminal, OpCode,
    Program, Value, ValueType, Vm, VmStatus, VmYieldReason, builtin_call_index, compile_source,
    disassemble_program,
};

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

fn is_jit_state_boundary_bridge(name: &str) -> bool {
    matches!(
        name,
        "frame_state"
            | "leave_frame"
            | "restore_active_exit_state"
            | "restore_active_sparse_exit_state"
            | "restore_exit_state"
            | "restore_sparse_exit_state"
    )
}

#[test]
fn jit_snapshot_literal_preserves_public_source_shape() {
    let snapshot = vm::jit::JitSnapshot {
        arch: "test",
        config: JitConfig::default(),
        traces: Vec::new(),
        attempts: Vec::new(),
        metrics: vm::jit::JitMetrics::default(),
        nyi_reference: Vec::new(),
    };

    assert!(snapshot.traces.is_empty());
}

fn any_trace_op(snapshot: &vm::jit::JitSnapshot, op: &str) -> bool {
    snapshot
        .traces
        .iter()
        .any(|trace| trace.op_names().iter().any(|found| found == op))
}

fn any_trace_ssa_contains(snapshot: &vm::jit::JitSnapshot, needle: &str) -> bool {
    snapshot
        .traces
        .iter()
        .any(|trace| trace.ssa_text().contains(needle))
}

fn assert_native_ssa_call_boundary_trace(vm: &Vm, snapshot: &vm::jit::JitSnapshot, label: &str) {
    let dump = vm.dump_jit_info();
    assert!(
        snapshot.traces.iter().any(|trace| trace.has_call),
        "{label} should record at least one call-boundary SSA trace, dump:\n{dump}"
    );
    assert!(
        snapshot
            .traces
            .iter()
            .filter(|trace| trace.has_call)
            .all(|trace| trace.terminal == JitTraceTerminal::BranchExit),
        "{label} call-boundary traces should terminate through branch exits, dump:\n{dump}"
    );
    assert!(
        dump.contains("lowering=ssa"),
        "{label} should lower through SSA, dump:\n{dump}"
    );
    assert!(
        vm.jit_native_trace_count() > 0,
        "{label} should compile at least one native trace, dump:\n{dump}"
    );
    assert!(
        vm.jit_native_exec_count() > 0,
        "{label} should execute natively before the exit boundary, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "{label} should not need interpreter fallback diagnostics, dump:\n{dump}"
    );
}

fn assert_native_ssa_specialized_trace(
    vm: &Vm,
    snapshot: &vm::jit::JitSnapshot,
    label: &str,
    expected_ops: &[&str],
) {
    let dump = vm.dump_jit_info();
    assert!(
        snapshot.traces.iter().all(|trace| !trace.has_call),
        "{label} should avoid call-boundary SSA traces, dump:\n{dump}"
    );
    assert!(
        dump.contains("lowering=ssa"),
        "{label} should lower through SSA, dump:\n{dump}"
    );
    assert!(
        vm.jit_native_trace_count() > 0,
        "{label} should compile at least one native trace, dump:\n{dump}"
    );
    assert!(
        vm.jit_native_exec_count() > 0,
        "{label} should execute natively, dump:\n{dump}"
    );
    for op in expected_ops {
        assert!(
            any_trace_op(snapshot, op),
            "{label} should record SSA op '{op}', dump:\n{dump}"
        );
    }
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "{label} should not need interpreter fallback diagnostics, dump:\n{dump}"
    );
}

fn first_native_code_bytes(dump: &str) -> Option<usize> {
    for line in dump.lines() {
        let marker = "code_bytes=";
        let Some(start) = line.find(marker) else {
            continue;
        };
        let raw = &line[start + marker.len()..];
        let value = raw.split_whitespace().next()?;
        if let Ok(bytes) = value.parse::<usize>() {
            return Some(bytes);
        }
    }
    None
}

fn first_native_code_hex(dump: &str) -> Option<String> {
    for line in dump.lines() {
        let marker = "    code:";
        let Some(start) = line.find(marker) else {
            continue;
        };
        return Some(line[start + marker.len()..].trim().to_string());
    }
    None
}

fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
    let start = instr_ip as usize + 1;
    code[start..start + 4].copy_from_slice(&target.to_le_bytes());
}

fn force_local_types(program: Program, hints: &[(usize, ValueType)]) -> Program {
    let mut type_map = program.type_map.clone().unwrap_or_default();
    if type_map.local_types.len() < program.local_count {
        type_map
            .local_types
            .resize(program.local_count, ValueType::Unknown);
    }
    for (slot, ty) in hints {
        type_map.local_types[*slot] = *ty;
    }
    program.with_type_map(type_map)
}

fn force_operand_types(program: Program, hints: &[(usize, (ValueType, ValueType))]) -> Program {
    let mut type_map = program.type_map.clone().unwrap_or_default();
    for (ip, (lhs, rhs)) in hints {
        type_map.operand_types.insert(*ip, (*lhs, *rhs));
    }
    program.with_type_map(type_map)
}

struct PrintNoReturn;

impl HostFunction for PrintNoReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![].into()))
    }
}

struct YieldOnce {
    yielded: bool,
}

impl HostFunction for YieldOnce {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        if self.yielded {
            Ok(CallOutcome::Return(vec![Value::Int(42)].into()))
        } else {
            self.yielded = true;
            Ok(CallOutcome::Yield)
        }
    }
}

struct PendingOnce {
    called: bool,
    op_id: u64,
}

impl HostFunction for PendingOnce {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        if self.called {
            return Err(vm::VmError::HostError(
                "pending host should not be replayed".to_string(),
            ));
        }
        self.called = true;
        Ok(CallOutcome::Pending(self.op_id))
    }
}

fn disable_trace_jit(vm: &mut Vm) {
    vm.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
}

fn install_aot(vm: &mut Vm) {
    disable_trace_jit(vm);
    vm.compile_aot().expect("aot compile should succeed");
    assert!(vm.has_aot_program(), "aot program should be installed");
}

fn counting_loop_program(limit: i64, host_call_each_iteration: bool) -> Program {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);

    let loop_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);

    if host_call_each_iteration {
        bc.call(0, 0);
    }

    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.br(loop_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(limit)], code).with_local_count(1)
}

#[test]
fn aot_compiles_whole_non_loop_program() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let lhs = 5;
        let rhs = 8;
        if lhs < rhs {
            lhs + rhs;
        } else {
            0;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let resume_ips = vm
        .aot_resume_ips()
        .expect("aot resume ip table should exist")
        .to_vec();
    assert!(
        resume_ips.contains(&0),
        "entry ip should be resumable, dump:\n{}",
        vm.dump_aot_info()
    );

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(13)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");

    let dump = vm.dump_aot_info();
    assert!(dump.contains("whole-program aot: enabled"));
    assert!(dump.contains("code_bytes="));
}

#[test]
fn aot_handles_string_equality_paths() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let label = "west";
        if label == "east" {
            1;
        } else if label == "west" {
            2;
        } else {
            3;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_handles_structural_array_equality_paths() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let lhs = [1, 2];
        let rhs = [1, 2];
        if lhs == rhs {
            7;
        } else {
            9;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_inlines_typed_numeric_steps_without_bridge_fallback() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let lhs = 1.5;
        let rhs = 2.25;
        let out = lhs * rhs + rhs / lhs - lhs;
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(3.375)]);
    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, _)| *name == "ldc" || is_jit_state_boundary_bridge(name)),
        "typed aot arithmetic should only fall back for initial stack growth, not math ops: {bridge_hits:?}"
    );
}

#[test]
fn aot_inlines_same_local_array_set_without_builtin_boundary() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut values = [1, 2, 3];
        let mut i = 0;
        while i < 64 {
            values[1] = i;
            i = i + 1;
        }
        values[1];
    "#;

    let compiled = compile_source(source).expect("array-set aot compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    assert_eq!(vm.run().expect("aot vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(63)]);
    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert_eq!(
        bridge_hits
            .iter()
            .find_map(|(name, count)| (*name == "set").then_some(*count)),
        Some(1),
        "only the final non-delayed Set should cross the builtin bridge: {bridge_hits:?}"
    );
}

#[test]
fn aot_inlines_same_local_map_set_in_loop() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut m: map<int> = {};
        for i in 0..64 {
            m[i] = i + 1;
        }
        let mut m2: map<int> = {};
        for i in 0..64 {
            m2[i] = i + 1;
        }
    "#;

    let compiled = compile_source(source).expect("map-set aot compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);
    assert_eq!(vm.run().expect("aot vm should run"), VmStatus::Halted);
    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert_eq!(
        bridge_hits
            .iter()
            .find_map(|(name, count)| (*name == "set").then_some(*count)),
        None,
        "loop-delayed MapSet should not cross the builtin bridge: {bridge_hits:?}"
    );
}

#[test]
fn aot_inlines_same_local_array_push_in_loop() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let push_call = vm::BuiltinFunction::ArrayPush.call_index();
    let get_call = builtin_call_index("get").expect("get builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);

    let loop_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(0);
    bc.ldloc(1);
    bc.ldc(3);
    bc.stloc(0);
    bc.call(push_call, 2);
    bc.stloc(0);
    bc.ldloc(1);
    bc.ldc(4);
    bc.add();
    bc.stloc(1);
    bc.br(loop_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(5);
    bc.call(get_call, 2);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::array(Vec::new()),
            Value::Int(0),
            Value::Int(64),
            Value::Null,
            Value::Int(1),
            Value::Int(63),
        ],
        code,
    )
    .with_local_count(2);
    let program = force_local_types(program, &[(0, ValueType::Array), (1, ValueType::Int)]);
    let mut vm = Vm::new(program);
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    assert_eq!(vm.run().expect("aot vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(63)]);
    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert_eq!(
        bridge_hits
            .iter()
            .find_map(|(name, count)| (*name == "array_push").then_some(*count)),
        None,
        "loop-delayed ArrayPush should not cross the builtin bridge: {bridge_hits:?}"
    );
}

#[test]
fn aot_handles_scalar_local_clear_sequences() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut outer = 0;
        let mut sum = 0;
        while outer < 4 {
            let mut i = 0;
            while i < 64 {
                let a = i + 7;
                let b = a - 3;
                sum = sum + b;
                i = i + 1;
            }
            outer = outer + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(9088)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_handles_mixed_numeric_less_than_loops() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut count = 0;
        while i < 5.5 {
            count = count + 1;
            i = i + 1;
        }
        count;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_handles_dynamic_numeric_builtin_results_in_compares() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use math;
        let delta = math::abs(10.0 - 12.5);
        let mut out = 0;
        if delta < 3.0 {
            out = 1;
        }
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_handles_mixed_numeric_arithmetic_promotions() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let value = 2.0 * 3 + 1;
        let scaled = value / 2;
        scaled;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(3.5)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_handles_tagged_array_elements_in_float_arithmetic() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let values = [1.5, 2.0, 4.0];
        let scale = 0.5;
        let out = values[2] * scale + values[0];
        out;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(3.5)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_handles_zero_result_assert_calls_in_loops() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 8 {
            assert(i >= 0);
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(8)]);
    assert!(vm.aot_exec_count() > 0, "aot should execute natively");
}

#[test]
fn aot_executes_typed_string_concat_without_helper_bridge() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut acc = "";
        while i < 4 {
            acc = acc + "ab";
            i = i + 1;
        }
        acc;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("abababab")]);

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| *name == "add" && *count > 0),
        "expected string concat to avoid generic add bridge, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_aot_info()
    );
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| *name == "sconcat" && *count > 0),
        "expected string concat to avoid sconcat helper bridge, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_aot_info()
    );
}

#[test]
fn aot_executes_typed_bytes_concat_without_helper_bridge() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let mut i = 0;
        let mut payload = bytes::from_hex("");
        while i < 5 {
            payload = payload + bytes::from_hex("00ff");
            i = i + 1;
        }
        payload;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::bytes(vec![
            0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF
        ])]
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| *name == "add" && *count > 0),
        "expected bytes concat to avoid generic add bridge, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_aot_info()
    );
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| name.contains("concat") && *count > 0),
        "expected bytes concat to avoid concat helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_aot_info()
    );
}

#[test]
fn aot_executes_typed_bytes_sequence_builtins_without_builtin_bridge() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let payload = bytes::from_hex("00ff10");
        let total = payload.length;
        let byte = (&payload)[1].copy();
        let present = payload.has(2);
        let part = payload[1:3];
        total;
        part;
        byte;
        present;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::Int(3),
            Value::bytes(vec![0xFF, 0x10]),
            Value::Int(255),
            Value::Bool(true),
        ]
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["len", "slice", "get", "has"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected bytes builtin '{bridge_name}' to avoid builtin helper bridge, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_aot_info()
        );
    }
}

#[test]
fn aot_executes_typed_string_sequence_builtins_without_builtin_bridge() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let text = "a界🙂";
        let total = text.length;
        let ch = (&text)[1].copy();
        let part = text[1:3];
        total;
        part;
        ch;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(3), Value::string("界🙂"), Value::string("界"),]
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["len", "slice", "get"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected string builtin '{bridge_name}' to avoid builtin helper bridge, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_aot_info()
        );
    }
}

#[test]
fn aot_executes_typed_bytes_array_codec_builtins_without_builtin_bridge() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let arr = bytes::to_array_u8(bytes::from_array_u8([1, 2, 255]));
        let payload = bytes::from_array_u8(arr);
        arr;
        payload;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_bridge_stats_enabled(true);
    install_aot(&mut vm);

    let status = vm.run().expect("aot vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(255)]),
            Value::bytes(vec![1, 2, 255]),
        ]
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["bytes_from_array_u8", "bytes_to_array_u8"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected bytes array codec '{bridge_name}' to avoid builtin helper bridge, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_aot_info()
        );
    }
}

#[test]
fn aot_replays_host_yield_and_resumes_at_call_site() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();

    let mut vm = Vm::new(Program::new(Vec::new(), bc.finish()));
    vm.register_function(Box::new(YieldOnce { yielded: false }));
    install_aot(&mut vm);

    let first = vm.run().expect("first aot run should yield");
    assert_eq!(first, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Host));

    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(vm.aot_exec_count() >= 2, "aot should re-enter after yield");
}

#[test]
fn aot_waits_for_pending_host_and_resumes_without_replay() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();

    let op_id = 77;
    let mut vm = Vm::new(Program::new(Vec::new(), bc.finish()));
    vm.register_function(Box::new(PendingOnce {
        called: false,
        op_id,
    }));
    install_aot(&mut vm);

    let waiting = vm.run().expect("first aot run should wait");
    assert_eq!(waiting, VmStatus::Waiting(op_id));

    vm.complete_host_op(op_id, vec![Value::Int(7)])
        .expect("host op completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

#[test]
fn aot_honors_fuel_metering_at_host_call_boundaries_only() {
    if !native_jit_supported() {
        return;
    }

    let mut vm = Vm::new(counting_loop_program(4, true));
    vm.register_function(Box::new(PrintNoReturn));
    install_aot(&mut vm);
    vm.set_fuel_check_interval(100)
        .expect("fuel interval update should succeed");
    vm.set_fuel(1);

    let mut yielded = 0u64;
    loop {
        match vm.run().expect("aot run should succeed") {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yielded = yielded.saturating_add(1);
                assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Fuel));
                vm.recharge_fuel(1).expect("recharge should succeed");
            }
            VmStatus::Waiting(op_id) => panic!("unexpected host wait on op {op_id}"),
        }
    }

    assert_eq!(
        yielded, 3,
        "fuel should only yield before the next host call"
    );
    assert_eq!(vm.stack().last(), Some(&Value::Int(4)));
    assert!(
        vm.aot_exec_count() > 1,
        "aot should resume after fuel yields"
    );
}

#[test]
fn aot_honors_epoch_interruption_at_host_call_boundaries_only() {
    if !native_jit_supported() {
        return;
    }

    let mut vm = Vm::new(counting_loop_program(2, true));
    vm.register_function(Box::new(PrintNoReturn));
    install_aot(&mut vm);
    vm.set_epoch_check_interval(100)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");

    let first = vm.run().expect("first aot run should yield");
    assert_eq!(first, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Epoch));

    let second = vm.resume().expect("resume should yield again");
    assert_eq!(second, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Epoch));

    vm.clear_epoch_deadline();
    let halted = vm.run().expect("run should halt after clearing epoch");
    assert_eq!(halted, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(2)));
}

#[test]
fn aot_ignores_fuel_interval_inside_no_call_loops() {
    if !native_jit_supported() {
        return;
    }

    let mut vm = Vm::new(counting_loop_program(20, false));
    install_aot(&mut vm);
    vm.set_fuel_check_interval(1)
        .expect("fuel interval update should succeed");
    vm.set_fuel(1);

    let status = vm.run().expect("aot run should succeed");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().last(), Some(&Value::Int(20)));
    assert_eq!(
        vm.get_fuel(),
        Some(1),
        "no-call aot loops should not consume fuel inside native execution"
    );
}

#[test]
fn aot_survives_reset_for_reuse() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 4 {
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    let first = vm.run().expect("first aot run should halt");
    assert_eq!(first, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    let first_execs = vm.aot_exec_count();
    assert!(first_execs > 0, "first run should execute aot");

    vm.reset_for_reuse();
    assert!(vm.has_aot_program(), "reset should preserve aot program");

    let second = vm.run().expect("second aot run should halt");
    assert_eq!(second, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert!(
        vm.aot_exec_count() > first_execs,
        "second run should reuse the installed aot artifact"
    );
}

#[test]
fn aot_preserves_drop_contract_parity_for_loop_locals() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 5 {
            let tmp = { v: i };
            i = i + 1;
        }
        i;
    "#;

    let compiled_interp = compile_source(source).expect("compile should succeed");
    let mut interp_vm = Vm::new(
        compiled_interp
            .program
            .with_local_count(compiled_interp.locals),
    );
    disable_trace_jit(&mut interp_vm);
    interp_vm.set_drop_contract_events_enabled(true);
    let interp_status = interp_vm.run().expect("interpreter run should halt");
    assert_eq!(interp_status, VmStatus::Halted);
    let interp_drops = interp_vm.drop_contract_event_count();

    let compiled_aot = compile_source(source).expect("compile should succeed");
    let mut aot_vm = Vm::new(compiled_aot.program.with_local_count(compiled_aot.locals));
    aot_vm.set_drop_contract_events_enabled(true);
    install_aot(&mut aot_vm);
    let aot_status = aot_vm.run().expect("aot run should halt");
    assert_eq!(aot_status, VmStatus::Halted);

    assert_eq!(aot_vm.stack(), interp_vm.stack());
    assert_eq!(
        aot_vm.drop_contract_event_count(),
        interp_drops,
        "aot drop behavior should match the interpreter"
    );
}

#[test]
fn trace_jit_compiles_hot_loop_and_is_dumpable() {
    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 20 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(190)]);

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    if native_jit_supported() {
        assert!(
            !snapshot.traces.is_empty(),
            "expected at least one compiled trace, dump:\n{dump}"
        );
        assert!(dump.contains("compiled traces:"));
        assert!(dump.contains("trace#"));
        assert!(dump.contains("native trace#"));
    } else {
        assert!(snapshot.traces.is_empty());
    }
}

#[test]
fn trace_jit_native_path_honors_fuel_metering() {
    let source = r#"
        let mut i = 0;
        while i < 100 {
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    if native_jit_supported() {
        vm.set_fuel(1_000_000);
        let warmup = vm.run().expect("warmup should halt");
        assert_eq!(warmup, VmStatus::Halted);
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected warmup to compile and execute native traces, dump:\n{}",
            vm.dump_jit_info()
        );
        vm.reset_for_reuse();
    }
    vm.set_fuel_check_interval(1)
        .expect("fuel interval update should succeed");
    vm.set_fuel(10);

    let mut yielded = 0_u64;
    loop {
        match vm
            .run()
            .expect("run should cooperatively yield under low fuel")
        {
            VmStatus::Yielded => {
                yielded = yielded.saturating_add(1);
                if !native_jit_supported() || vm.jit_native_exec_count() > 0 {
                    break;
                }
                assert!(
                    yielded < 512,
                    "low-fuel run did not reach native execution after {yielded} yields, dump:\n{}",
                    vm.dump_jit_info()
                );
                vm.recharge_fuel(10).expect("recharge should succeed");
            }
            VmStatus::Halted => panic!("expected cooperative yield before halt"),
            VmStatus::Waiting(op_id) => panic!("unexpected host wait on op {op_id}"),
        }
    }
    assert!(yielded > 0, "expected at least one cooperative fuel yield");

    if native_jit_supported() {
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected native JIT execution under fuel metering, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn trace_jit_preserves_local_move_semantics_across_fuel_yields() {
    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 50 {
            let a = "x";
            let b = a;
            if b == "x" {
                sum = sum + 1;
            }
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_fuel_check_interval(1)
        .expect("fuel interval update should succeed");
    vm.set_fuel(1);

    let mut yielded = 0_u64;
    loop {
        match vm.run().expect("run should succeed") {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yielded = yielded.saturating_add(1);
                assert!(
                    yielded < 2_048,
                    "move/yield loop made no progress after {yielded} yields, dump:\n{}",
                    vm.dump_jit_info()
                );
                vm.recharge_fuel(10).expect("recharge should succeed");
            }
            VmStatus::Waiting(op_id) => panic!("unexpected host wait on op {op_id}"),
        }
    }

    assert!(yielded > 0, "expected at least one cooperative fuel yield");
    assert_eq!(
        vm.stack().last(),
        Some(&Value::Int(50)),
        "move-heavy loop should preserve final result across yields"
    );

    if native_jit_supported() {
        assert!(
            vm.jit_native_exec_count() > 0,
            "string-equality loop should use native value_eq, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn changing_fuel_interval_recompiles_native_trace_variant() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut acc = 0;
        while i < 40 {
            acc = acc + i;
            acc = acc + 1;
            i = i + 1;
        }
        acc;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    vm.set_fuel_check_interval(1)
        .expect("fuel interval update should succeed");
    vm.set_fuel(1_000_000);
    let status = vm.run().expect("first run should halt");
    assert_eq!(status, VmStatus::Halted);
    let dump_first = vm.dump_jit_info();
    let bytes_first =
        first_native_code_bytes(&dump_first).expect("first run should produce native code bytes");

    vm.reset_for_reuse();
    vm.set_fuel_check_interval(8)
        .expect("fuel interval update should succeed");
    vm.set_fuel(1_000_000);
    let status = vm.run().expect("second run should halt");
    assert_eq!(status, VmStatus::Halted);
    let dump_second = vm.dump_jit_info();
    let bytes_second =
        first_native_code_bytes(&dump_second).expect("second run should produce native code bytes");

    assert_ne!(
        bytes_second, bytes_first,
        "expected interval-specific native traces; interval=1 bytes={bytes_first}, interval=8 bytes={bytes_second}\nfirst dump:\n{dump_first}\nsecond dump:\n{dump_second}"
    );
}

#[test]
fn trace_jit_native_path_honors_epoch_interruption() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 100 {
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_epoch_check_interval(8)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    assert_eq!(vm.increment_epoch(), 1);

    let status = vm
        .run()
        .expect("run should cooperatively yield once the epoch reaches the deadline");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native JIT execution under epoch interruption, dump:\n{}",
        vm.dump_jit_info()
    );

    let status = vm
        .run()
        .expect("second run should halt after auto re-arming");
    assert_eq!(status, VmStatus::Halted);
}

#[test]
fn native_trace_epoch_zero_deadline_auto_rearms_without_manual_reconfiguration() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 10 {
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let warmup = vm
        .run()
        .expect("warmup run should halt and compile native traces");
    assert_eq!(warmup, VmStatus::Halted);
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected warmup run to compile and execute native traces, dump:\n{}",
        vm.dump_jit_info()
    );

    vm.reset_for_reuse();
    vm.set_epoch_check_interval(1)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");

    let first = vm.run().expect("first run should yield");
    assert_eq!(first, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));

    let second = vm
        .resume()
        .expect("resume should auto re-arm the zero-length epoch deadline");
    assert_eq!(second, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native JIT execution under repeated epoch yields, dump:\n{}",
        vm.dump_jit_info()
    );

    vm.clear_epoch_deadline();
    let halted = vm
        .run()
        .expect("run should halt after clearing epoch interruption");
    assert_eq!(halted, VmStatus::Halted);
}

#[test]
fn trace_jit_preserves_local_move_semantics_across_epoch_yields() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 50 {
            let a = "x";
            let b = a;
            if b == "x" {
                sum = sum + 1;
            }
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_epoch_check_interval(8)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");

    let mut yielded = 0_u64;
    loop {
        match vm.run().expect("run should succeed") {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yielded = yielded.saturating_add(1);
                assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));
                assert!(
                    yielded < 2_048,
                    "move/yield loop made no progress after {yielded} yields, dump:\n{}",
                    vm.dump_jit_info()
                );
                vm.set_epoch_deadline(if yielded < 4 { 0 } else { 1 })
                    .expect("re-arming epoch deadline should succeed");
            }
            VmStatus::Waiting(op_id) => panic!("unexpected host wait on op {op_id}"),
        }
    }

    assert!(yielded > 0, "expected at least one cooperative epoch yield");
    assert_eq!(
        vm.stack().last(),
        Some(&Value::Int(50)),
        "move-heavy loop should preserve final result across epoch yields"
    );
    assert!(
        vm.jit_native_exec_count() > 0,
        "string-equality loop should use native value_eq, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn changing_epoch_interval_recompiles_native_trace_variant() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut acc = 0;
        while i < 40 {
            acc = acc + i;
            acc = acc + 1;
            i = i + 1;
        }
        acc;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    vm.set_epoch_check_interval(1)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    let status = vm.run().expect("first run should halt");
    assert_eq!(status, VmStatus::Halted);
    let dump_first = vm.dump_jit_info();
    let bytes_first =
        first_native_code_bytes(&dump_first).expect("first run should produce native code bytes");

    vm.reset_for_reuse();
    vm.set_epoch_check_interval(8)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    let status = vm.run().expect("second run should halt");
    assert_eq!(status, VmStatus::Halted);
    let dump_second = vm.dump_jit_info();
    let bytes_second =
        first_native_code_bytes(&dump_second).expect("second run should produce native code bytes");

    assert_ne!(
        bytes_second, bytes_first,
        "expected interval-specific native traces; interval=1 bytes={bytes_first}, interval=8 bytes={bytes_second}\nfirst dump:\n{dump_first}\nsecond dump:\n{dump_second}"
    );
}

#[test]
fn fuel_and_epoch_compile_distinct_native_trace_variants() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut acc = 0;
        while i < 40 {
            acc = acc + i;
            i = i + 1;
        }
        acc;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");

    let mut fuel_vm = Vm::new(compiled.program.clone().with_local_count(compiled.locals));
    fuel_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    fuel_vm
        .set_fuel_check_interval(8)
        .expect("fuel interval update should succeed");
    fuel_vm.set_fuel(10_000);
    let fuel_status = fuel_vm.run().expect("fuel-mode run should halt");
    assert_eq!(fuel_status, VmStatus::Halted);
    let fuel_dump = fuel_vm.dump_jit_info();
    let fuel_code =
        first_native_code_hex(&fuel_dump).expect("fuel-mode run should emit native code bytes");

    let mut epoch_vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    epoch_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    epoch_vm
        .set_epoch_check_interval(8)
        .expect("epoch interval update should succeed");
    epoch_vm
        .set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    let epoch_status = epoch_vm.run().expect("epoch-mode run should halt");
    assert_eq!(epoch_status, VmStatus::Halted);
    let epoch_dump = epoch_vm.dump_jit_info();
    let epoch_code =
        first_native_code_hex(&epoch_dump).expect("epoch-mode run should emit native code bytes");

    assert_ne!(
        fuel_code, epoch_code,
        "fuel and epoch should compile distinct native inline interruption blocks\nfuel:\n{fuel_dump}\nepoch:\n{epoch_dump}"
    );
}

#[test]
fn compiler_uses_shl_for_power_of_two_multiply_and_jit_accepts_it() {
    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 8 {
            sum = sum + i * 8;
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert!(
        compiled.program.code.contains(&(OpCode::Shl as u8)),
        "expected compiler to emit shl for power-of-two multiply"
    );

    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(224)]);

    if native_jit_supported() {
        let dump = vm.dump_jit_info();
        assert!(
            any_trace_op(&vm.jit_snapshot(), "shl")
                || any_trace_op(&vm.jit_snapshot(), "ishl")
                || any_trace_op(&vm.jit_snapshot(), "ilocal_shl_imm")
                || dump.contains(" shl"),
            "expected trace dump to include SSA shl ops, dump:\n{dump}"
        );
    }
}

#[test]
fn compiler_emits_mod_and_short_circuit_logic_and_jit_accepts_them() {
    let source = r#"
        let mut i = 1;
        let mut sum = 0;
        while i < 12 {
            let is_evenish = ((i % 2) == 0) && true;
            let is_small = (i < 3) || is_evenish;
            if is_small {
                sum = sum + 1;
            } else {
                sum = sum + 2;
            }
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let disassembly = disassemble_program(&compiled.program);
    assert!(
        compiled.program.code.contains(&(OpCode::Mod as u8)),
        "expected compiler to emit mod"
    );
    assert!(
        disassembly.contains("brfalse "),
        "expected compiler to emit branch-based short-circuit logic"
    );
    assert!(
        !disassembly.contains(" and\n"),
        "short-circuit lowering should not emit eager and"
    );
    assert!(
        !disassembly.contains(" or\n"),
        "short-circuit lowering should not emit eager or"
    );

    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(16)]);

    if native_jit_supported() {
        assert!(
            any_trace_op(&vm.jit_snapshot(), "mod")
                || any_trace_op(&vm.jit_snapshot(), "imod")
                || any_trace_op(&vm.jit_snapshot(), "mod_imm")
                || any_trace_op(&vm.jit_snapshot(), "imod_imm")
                || any_trace_ssa_contains(&vm.jit_snapshot(), "imod_imm ")
                || any_trace_ssa_contains(&vm.jit_snapshot(), "mod_imm "),
            "expected trace dump to include SSA mod ops, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn trace_jit_supports_host_call_loops_with_branch_exit_traces() {
    let source = r#"
        fn print(x);
        let mut i = 0;
        while i < 4 {
            print(i);
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => vm.register_function(Box::new(PrintNoReturn)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    if native_jit_supported() {
        assert_native_ssa_call_boundary_trace(&vm, &snapshot, "host call loop");
        assert!(
            snapshot
                .traces
                .iter()
                .all(|trace| trace.terminal == JitTraceTerminal::BranchExit),
            "host call loop traces should terminate through branch exits, dump:\n{dump}"
        );
    }
}

fn increment_non_yielding(args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let Value::Int(value) = args[0] else {
        return Err(vm::VmError::TypeMismatch("int"));
    };
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(value + 1))))
}

fn less_non_yielding(args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let [Value::Int(lhs), Value::Int(rhs)] = args else {
        return Err(vm::VmError::TypeMismatch("two ints"));
    };
    Ok(CallOutcome::Return(CallReturn::one(Value::Bool(lhs < rhs))))
}

fn string_len_non_yielding(args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let [Value::String(value)] = args else {
        return Err(vm::VmError::TypeMismatch("string"));
    };
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(
        value.len() as i64,
    ))))
}

fn mixed_float_non_yielding(args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let [
        Value::Int(integer),
        Value::Float(float),
        Value::Bool(boolean),
    ] = args
    else {
        return Err(vm::VmError::TypeMismatch("int, float, bool"));
    };
    Ok(CallOutcome::Return(CallReturn::one(Value::Float(
        *integer as f64 + float + if *boolean { 1.0 } else { 0.0 },
    ))))
}

thread_local! {
    static UNSTABLE_HOST_RETURN_COUNT: Cell<u32> = const { Cell::new(0) };
}

fn unstable_return_non_yielding(_: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let call = UNSTABLE_HOST_RETURN_COUNT.with(|count| {
        let call = count.get();
        count.set(call.saturating_add(1));
        call
    });
    let value = if call < 8 {
        Value::Int(1)
    } else {
        Value::Bool(false)
    };
    Ok(CallOutcome::Return(CallReturn::one(value)))
}

#[test]
fn trace_jit_enforces_scalar_host_return_contract_after_dirty_local_write() {
    if !native_jit_supported() {
        return;
    }
    UNSTABLE_HOST_RETURN_COUNT.with(|count| count.set(0));
    let compiled = compile_source(
        r#"
            fn unstable() -> int;
            let mut dirty = 0;
            while dirty < 100 {
                dirty = dirty + 1;
                dirty = dirty + unstable();
            }
            dirty;
        "#,
    )
    .expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.bind_static_non_yielding_args_function("unstable", unstable_return_non_yielding);

    assert!(matches!(vm.run(), Err(vm::VmError::TypeMismatch("int"))));
    assert!(
        vm.jit_native_exec_count() > 0,
        "return mismatch should occur after native execution:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_passes_mixed_host_args_and_float_return_as_scalars() {
    if !native_jit_supported() {
        return;
    }
    let compiled = compile_source(
        r#"
            fn mixed(integer: int, float: float, boolean: bool) -> float;
            let mut i = 0;
            let mut total = 0.0;
            while i < 100 {
                total = total + mixed(i, 0.5, true);
                i = i + 1;
            }
            total;
        "#,
    )
    .expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.bind_static_non_yielding_args_function("mixed", mixed_float_non_yielding);

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(5_100.0)]);
    assert!(
        vm.jit_snapshot().traces.iter().any(|trace| {
            trace
                .ssa_text()
                .lines()
                .any(|line| line.contains(":f64 = host_call"))
        }),
        "mixed host arguments should return an SSA float:\n{}",
        vm.dump_jit_info()
    );
    assert!(vm.jit_native_exec_count() > 0);
}

#[test]
fn trace_jit_passes_tagged_host_args_to_scalar_return() {
    if !native_jit_supported() {
        return;
    }
    let compiled = compile_source(
        r#"
            fn string_len(value: string) -> int;
            let mut i = 0;
            while i < 100 {
                i = i + string_len("x");
            }
            i;
        "#,
    )
    .expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.bind_static_non_yielding_args_function("string_len", string_len_non_yielding);

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100)]);
    assert!(
        vm.jit_snapshot().traces.iter().any(|trace| {
            trace
                .ssa_text()
                .lines()
                .any(|line| line.contains(":i64 = host_call"))
        }),
        "tagged argument host call should return an SSA scalar:\n{}",
        vm.dump_jit_info()
    );
    assert!(vm.jit_native_exec_count() > 0);
}

#[test]
fn trace_jit_passes_i64_host_args_and_bool_return_as_scalars() {
    if !native_jit_supported() {
        return;
    }
    let compiled = compile_source(
        r#"
            fn less(lhs: int, rhs: int) -> bool;
            let mut i = 0;
            let mut matched = 0;
            while i < 100 {
                if less(i, 50) {
                    matched = matched + 1;
                }
                i = i + 1;
            }
            matched;
        "#,
    )
    .expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.bind_static_non_yielding_args_function("less", less_non_yielding);

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(50)]);
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot.traces.iter().any(|trace| {
            trace
                .ssa_text()
                .lines()
                .any(|line| line.contains(":bool = host_call"))
        }),
        "bool host return should remain an SSA scalar:\n{}",
        vm.dump_jit_info()
    );
    assert!(vm.jit_native_exec_count() > 0);
}

#[test]
fn trace_jit_keeps_non_yielding_static_args_calls_inside_loop_trace() {
    if !native_jit_supported() {
        return;
    }
    let compiled = compile_source(
        r#"
            fn increment(value) -> int;
            let mut i = 0;
            while i < 100 {
                i = increment(i);
            }
            i;
        "#,
    )
    .expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.bind_static_non_yielding_args_function("increment", increment_non_yielding);

    let status = vm.run();
    assert!(
        status.is_ok(),
        "vm should run: {status:?}\n{}",
        vm.dump_jit_info()
    );
    assert_eq!(status.unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100)]);
    let snapshot = vm.jit_snapshot();
    let host_trace = snapshot
        .traces
        .iter()
        .find(|trace| {
            trace.terminal == JitTraceTerminal::LoopBack
                && trace.op_names().iter().any(|op| op == "host_call")
                && trace.ssa_text().contains("host_call")
        })
        .unwrap_or_else(|| {
            panic!(
                "non-yielding call should remain inside a loop-back trace, dump:\n{}",
                vm.dump_jit_info()
            )
        });
    let host_ssa = host_trace.ssa_text();
    assert!(
        host_ssa
            .lines()
            .any(|line| line.contains(":i64 = host_call")),
        "typed host return should remain an unboxed SSA scalar:\n{host_ssa}"
    );
    let host_result_tail = host_ssa
        .split_once("host_call")
        .map(|(_, tail)| tail)
        .expect("host-call SSA line should exist");
    assert!(
        !host_result_tail.contains("unbox_int"),
        "typed host result should not pass through tagged return storage:\n{host_ssa}"
    );
    assert!(vm.jit_native_exec_count() > 0);
}

#[test]
fn trace_jit_sparse_exit_preserves_clean_scalar_and_heap_locals() {
    if !native_jit_supported() {
        return;
    }

    let preserved = std::sync::Arc::new("preserved".to_string());
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    let root_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(3);
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);
    bc.call(0, 0);
    bc.ldloc(0);
    bc.ldc(2);
    bc.add();
    bc.stloc(0);
    bc.br(root_ip);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ldloc(1);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::Int(0),
            Value::String(preserved.clone()),
            Value::Int(1),
            Value::Int(4),
        ],
        code,
    )
    .with_local_count(2);
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.register_function(Box::new(PrintNoReturn));

    assert_eq!(vm.run().expect("sparse clean-local run"), VmStatus::Halted);
    assert_eq!(vm.stack()[0], Value::Int(4));
    let Value::String(result) = &vm.stack()[1] else {
        panic!("expected preserved string result");
    };
    assert!(std::sync::Arc::ptr_eq(result, &preserved));

    let snapshot = vm.jit_snapshot();
    let call_trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.has_call)
        .expect("expected call-boundary trace");
    assert_eq!(call_trace.ssa_dirty_local_materialization_count(), 0);
    assert_native_ssa_call_boundary_trace(&vm, &snapshot, "sparse clean-local exit");
}

#[test]
fn trace_jit_sparse_exit_restores_one_dirty_scalar_local() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let root_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(2);
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.call(0, 0);
    bc.br(root_ip);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program =
        Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code).with_local_count(1);
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.register_function(Box::new(PrintNoReturn));

    assert_eq!(vm.run().expect("sparse scalar run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let snapshot = vm.jit_snapshot();
    let call_trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.has_call)
        .expect("expected call-boundary trace");
    assert_eq!(call_trace.ssa_dirty_local_materialization_count(), 1);
    assert_native_ssa_call_boundary_trace(&vm, &snapshot, "sparse scalar exit");
}

#[test]
fn trace_jit_sparse_heap_exit_transfers_ownership_across_reuse() {
    if !native_jit_supported() {
        return;
    }

    let old = std::sync::Arc::new("old".to_string());
    let replacement = std::sync::Arc::new("replacement".to_string());
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(2);
    bc.stloc(2);
    let root_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(4);
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(2);
    bc.stloc(1);
    bc.call(0, 0);
    bc.ldloc(0);
    bc.ldc(3);
    bc.add();
    bc.stloc(0);
    bc.br(root_ip);
    let exit_ip = bc.position();
    bc.ldloc(1);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::Int(0),
            Value::String(old.clone()),
            Value::String(replacement.clone()),
            Value::Int(1),
            Value::Int(3),
        ],
        code,
    )
    .with_local_count(3);
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.register_function(Box::new(PrintNoReturn));

    let mut retained_counts = None;
    for run in 0..2 {
        assert_eq!(
            vm.run().expect("sparse heap run"),
            VmStatus::Halted,
            "run {run}"
        );
        let Value::String(result) = &vm.stack()[0] else {
            panic!("expected replacement string result");
        };
        assert!(std::sync::Arc::ptr_eq(result, &replacement));
        for local_index in [1, 2] {
            let Value::String(local) = &vm.locals()[local_index] else {
                panic!("expected replacement string local {local_index}");
            };
            assert!(std::sync::Arc::ptr_eq(local, &replacement));
        }
        let counts = (
            std::sync::Arc::strong_count(&old),
            std::sync::Arc::strong_count(&replacement),
        );
        if let Some(expected) = retained_counts {
            assert_eq!(counts, expected, "owners must not grow across reuse");
        } else {
            retained_counts = Some(counts);
        }

        let snapshot = vm.jit_snapshot();
        let call_trace = snapshot
            .traces
            .iter()
            .find(|trace| trace.has_call)
            .expect("expected call-boundary trace");
        assert_eq!(call_trace.ssa_dirty_local_materialization_count(), 1);
        assert_native_ssa_call_boundary_trace(&vm, &snapshot, "sparse heap exit");

        if run == 0 {
            vm.reset_for_reuse();
            assert_eq!(std::sync::Arc::strong_count(&old), counts.0);
            assert_eq!(std::sync::Arc::strong_count(&replacement), counts.1 - 3);
        }
    }
}

#[test]
fn trace_jit_nested_call_loops_use_branch_exit_segments() {
    let source = r#"
        fn print(x);
        let mut i = 0;
        let mut sum = 0;
        while i < 3 {
            let mut j = 0;
            while j < 4 {
                print(j);
                sum = sum + j;
                j = j + 1;
            }
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    for func in &compiled.functions {
        match func.name.as_str() {
            "print" => vm.register_function(Box::new(PrintNoReturn)),
            _ => panic!("unexpected function {}", func.name),
        };
    }

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(18)]);

    if native_jit_supported() {
        let dump = vm.dump_jit_info();
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.len() >= 2,
            "nested call loops should record multiple SSA branch-exit traces, dump:\n{dump}"
        );
        assert_native_ssa_call_boundary_trace(&vm, &snapshot, "nested call loops");
    }
}

#[test]
fn trace_jit_records_typed_int_add_steps() {
    if !native_jit_supported() {
        return;
    }

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
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(15)]);

    let snapshot = vm.jit_snapshot();
    assert!(
        any_trace_op(&snapshot, "iadd")
            || any_trace_op(&snapshot, "iadd_imm")
            || any_trace_op(&snapshot, "ilocal_add_imm")
            || any_trace_ssa_contains(&snapshot, "iadd "),
        "expected a typed integer add SSA trace, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_uses_ssa_lowering_for_supported_numeric_loop() {
    if !native_jit_supported() {
        return;
    }

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
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(15)]);
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution for SSA candidate loop, dump:\n{}",
        vm.dump_jit_info()
    );

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    assert!(
        dump.contains("lowering=ssa"),
        "expected at least one SSA-lowered native trace, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.boxed_load_site_count > 0,
        "expected SSA diagnostics to report boxed load sites, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.boxed_store_site_count > 0,
        "expected SSA diagnostics to report boxed store sites, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.trace_exit_count > 0,
        "expected SSA diagnostics to report trace exits, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.native_loop_back_count > 0,
        "expected SSA diagnostics to report native loop-backs, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_links_between_nested_loop_native_traces() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut outer = 0;
        let mut i = 0;
        let mut sum = 0;
        while outer < 8 {
            i = 0;
            while i < 40000 {
                let a = i + 7;
                let b = a - 3;
                let c = b * 8;
                let d = c / 8;
                let e = d + i;
                let n = 0 - e;
                let p = 0 - n;
                sum = sum + p;
                i = i + 1;
            }
            outer = outer + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12_800_960_000)]);
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected nested loop workload to execute native traces, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_link_handoff_count() > 0,
        "expected direct native handoff between nested-loop traces, dump:\n{}",
        vm.dump_jit_info()
    );

    let dump = vm.dump_jit_info();
    assert!(
        dump.contains("native trace handoffs:"),
        "expected native handoff diagnostics in JIT dump, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_reports_exact_parent_exit_profiles() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn choose(i) {
            let mut result = 7;
            if i % 2 == 0 {
                result = 3;
            }
            if i % 3 == 1 {
                result = 5;
            }
            result
        }

        let mut i = 0;
        let mut total = 0;
        while i < 64 {
            total = total + choose(i);
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("branch profile fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(
        vm.run().expect("branch profile fixture should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(318)]);

    let snapshot = vm.jit_snapshot();
    let exit_profiles = vm.jit_exit_profiles();
    assert!(
        !exit_profiles.is_empty(),
        "expected exact parent-exit profiles:\n{}",
        vm.dump_jit_info()
    );
    for profile in &exit_profiles {
        let parent = &snapshot.traces[profile.parent_trace_id];
        assert!(profile.exit_id < parent.ssa_exit_count() as u32);
        assert!(profile.executions > 0);
    }
    assert!(
        snapshot
            .traces
            .iter()
            .enumerate()
            .filter(|(_, trace)| trace.frame_key != u64::MAX)
            .any(|(parent_trace_id, _)| {
                let parent_profiles = exit_profiles
                    .iter()
                    .filter(|profile| profile.parent_trace_id == parent_trace_id)
                    .collect::<Vec<_>>();
                parent_profiles.len() >= 2
                    && parent_profiles.iter().any(|profile| profile.executions > 1)
            }),
        "expected two distinct exits and one repeated exact exit under a callable parent:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_direct_side_link_bypasses_rust_dispatch() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 4096 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("direct side-link fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_native_bridge_stats_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(16_384)]);
    let first_direct_links = vm.jit_native_direct_link_count();
    let first_handoffs = vm.jit_native_link_handoff_count();
    let first_bridge_stats = vm.jit_native_bridge_stats_snapshot();
    assert!(first_direct_links > 4_000, "{}", vm.dump_jit_info());
    assert_eq!(vm.jit_native_region_count(), 0, "{}", vm.dump_jit_info());
    assert!(
        first_bridge_stats
            .iter()
            .any(|(name, count)| *name == "frame_state" && *count > 0),
        "direct-link characterization must count native frame-state bridges: {first_bridge_stats:?}"
    );
    assert!(
        first_bridge_stats
            .iter()
            .any(|(name, count)| { *name == "restore_active_sparse_exit_state" && *count > 0 }),
        "direct-link characterization must count sparse exit restores: {first_bridge_stats:?}"
    );

    vm.clear_jit_native_bridge_stats();
    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(16_384)]);
    assert!(
        vm.jit_native_direct_link_count() - first_direct_links > 4_000,
        "{}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_link_handoff_count() - first_handoffs <= 2,
        "{}",
        vm.dump_jit_info()
    );
    let steady_bridge_stats = vm.jit_native_bridge_stats_snapshot();
    let steady_frame_state = steady_bridge_stats
        .iter()
        .find_map(|(name, count)| (*name == "frame_state").then_some(*count))
        .unwrap_or(0);
    let steady_sparse_restores = steady_bridge_stats
        .iter()
        .find_map(|(name, count)| (*name == "restore_active_sparse_exit_state").then_some(*count))
        .unwrap_or(0);
    assert!(
        steady_frame_state <= 4,
        "direct-linked child entries must inherit values without reloading VM frame state: \
         frame_state={steady_frame_state} stats={steady_bridge_stats:?}\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steady_sparse_restores <= 2,
        "direct-linked edges must skip sparse VM restoration: \
         restores={steady_sparse_restores} stats={steady_bridge_stats:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_tail_link_cycle_has_bounded_host_stack() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 1000000 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("tail-link cycle fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4_000_000)]);
    assert!(
        vm.jit_native_direct_link_count() > 999_000,
        "{}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_link_handoff_count() <= 4,
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_side_link_invalidation_clears_incoming_slots() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 4096 {
            if i % 2 == 0 { total = total + 3; } else { total = total + 5; }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("side-link invalidation fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert!(vm.jit_native_active_direct_link_slot_count() > 0);

    vm.set_jit_native_direct_links_enabled(false);
    assert_eq!(vm.jit_native_active_direct_link_slot_count(), 0);
    assert_eq!(vm.jit_native_trace_count(), 0);
}

#[test]
fn trace_jit_side_link_generation_prevents_stale_entry_reuse() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 4096 {
            if i % 2 == 0 { total = total + 3; } else { total = total + 5; }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("side-link generation fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert!(vm.jit_native_active_direct_link_slot_count() > 0);

    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 257,
    });
    assert_eq!(vm.jit_native_active_direct_link_slot_count(), 0);
    assert_eq!(vm.jit_native_direct_link_count(), 0);
    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(16_384)]);
    assert!(vm.jit_native_direct_link_count() > 4_000);
    assert!(vm.jit_native_active_direct_link_slot_count() > 0);
}

#[test]
fn trace_jit_side_link_respects_callable_frame_and_interrupt_boundaries() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn run(limit) {
            let mut i = 0;
            let mut total = 0;
            while i < limit {
                if i % 2 == 0 { total = total + 3; } else { total = total + 5; }
                i = i + 1;
            }
            total
        }
        run(4096);
    "#;
    let compiled = compile_source(source).expect("direct boundary fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });
    vm.set_fuel_check_interval(1).unwrap();
    vm.set_fuel(1_000_000);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(16_384)]);
    assert!(vm.jit_native_direct_link_count() > 4_000);

    vm.reset_for_reuse();
    vm.set_fuel(8);
    let mut fuel_yields = 0_u64;
    loop {
        match vm.run().expect("direct-link fuel run should make progress") {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                fuel_yields = fuel_yields.saturating_add(1);
                assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Fuel));
                assert!(fuel_yields < 4_096, "{}", vm.dump_jit_info());
                vm.recharge_fuel(128).unwrap();
            }
            VmStatus::Waiting(op_id) => panic!("unexpected host wait {op_id}"),
        }
    }
    assert_eq!(vm.stack(), &[Value::Int(16_384)]);
    assert!(fuel_yields > 0);
    assert!(vm.jit_native_direct_link_count() > 4_000);
}

#[test]
fn trace_jit_region_links_hot_same_frame_side_exit() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 256 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("region fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().expect("first region run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_024)]);
    let first_handoffs = vm.jit_native_link_handoff_count();
    let first_native_execs = vm.jit_native_exec_count();
    let first_region_entries = vm.jit_native_region_entry_count();
    let first_internal_edges = vm.jit_native_internal_region_edge_count();
    let first_fallbacks = vm.jit_helper_fallback_count();
    assert!(
        first_handoffs > 0,
        "expected warmup to cross native trace boundaries:\n{}",
        vm.dump_jit_info()
    );
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    assert!(first_region_entries > 0, "{}", vm.dump_jit_info());
    assert!(first_internal_edges > 0, "{}", vm.dump_jit_info());

    vm.reset_for_reuse();
    assert_eq!(vm.run().expect("second region run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_024)]);

    let second_run_handoffs = vm
        .jit_native_link_handoff_count()
        .saturating_sub(first_handoffs);
    assert!(
        second_run_handoffs <= 2,
        "expected cyclic same-frame region fusion to bound external handoffs, second_run_handoffs={second_run_handoffs}:\n{}",
        vm.dump_jit_info()
    );
    assert!(vm.jit_native_exec_count() > first_native_execs);
    assert!(vm.jit_native_region_entry_count() > first_region_entries);
    assert!(vm.jit_native_internal_region_edge_count() > first_internal_edges);
    assert_eq!(vm.jit_helper_fallback_count(), first_fallbacks);
}

#[test]
fn trace_jit_region_cycle_propagates_disjoint_dirty_locals() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn branch_counts(limit) {
            let mut i = 0;
            let mut even = 0;
            let mut odd = 0;
            while i < limit {
                if i % 2 == 0 {
                    even = even + 1;
                } else {
                    odd = odd + 1;
                }
                i = i + 1;
            }
            even * 10000 + odd
        }
        branch_counts(4096);
    "#;
    let compiled = compile_source(source).expect("disjoint dirty region compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("first disjoint dirty region run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(20_482_048)],
        "{}",
        vm.dump_jit_info()
    );
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());

    vm.reset_for_reuse();
    assert_eq!(
        vm.run().expect("second disjoint dirty region run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(20_482_048)],
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_region_unlinked_exit_restores_callable_frame() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn probe(limit, stop) {
            let mut i = 0;
            let mut total = 0;
            while i < limit {
                if i == stop {
                    break;
                }
                if i % 2 == 0 {
                    total = total + 3;
                } else {
                    total = total + 5;
                }
                i = i + 1;
            }
            total * 1000 + i
        }
        probe(257, 50) + 1;
    "#;
    let compiled = compile_source(source).expect("unlinked callable exit fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(200_051)], "{}", vm.dump_jit_info());
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    assert!(vm.jit_native_internal_region_edge_count() > 0);
    assert_eq!(vm.jit_helper_fallback_count(), 0, "{}", vm.dump_jit_info());
}

#[test]
fn trace_jit_region_preserves_owned_value_drop_contract() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let payload = ["owned"];
        let mut i = 0;
        let mut total = 0;
        while i < 50 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        [payload, total];
    "#;
    let compiled = compile_source(source).expect("owned region fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    let Value::Array(output) = &vm.stack()[0] else {
        panic!("expected owned array output: {}", vm.dump_jit_info());
    };
    let output = Arc::clone(output);
    assert_eq!(output.as_slice()[1], Value::Int(200));
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    assert!(vm.jit_native_internal_region_edge_count() > 0);
    assert_eq!(Arc::strong_count(&output), 2);

    vm.set_drop_contract_events_enabled(true);
    assert_eq!(vm.jit_native_region_count(), 0);
    vm.reset_for_reuse();
    assert_eq!(Arc::strong_count(&output), 1);
    drop(vm);
    assert_eq!(Arc::strong_count(&output), 1);
}

#[test]
fn trace_jit_region_progress_prevents_callable_frame_backoff() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn run(limit, payload) {
            let mut i = 0;
            let mut total = 0;
            while i < limit {
                if i % 2 == 0 {
                    total = total + 3;
                } else {
                    total = total + 5;
                }
                i = i + 1;
            }
            total + payload[0]
        }
        run(256, [1]);
    "#;
    let compiled = compile_source(source).expect("region progress fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_025)]);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    let first_execs = vm.jit_native_exec_count();
    let first_edges = vm.jit_native_internal_region_edge_count();

    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_025)]);
    assert!(
        vm.jit_native_exec_count() > first_execs,
        "{}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_internal_region_edge_count() > first_edges,
        "{}",
        vm.dump_jit_info()
    );
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
}

#[test]
fn trace_jit_inherited_direct_progress_prevents_callable_frame_backoff() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn run(limit, payload) {
            let mut i = 0;
            let mut total = 0;
            while i < limit {
                if i % 2 == 0 {
                    total = total + 3;
                } else {
                    total = total + 5;
                }
                i = i + 1;
            }
            total + payload[0]
        }
        let mut rounds = 0;
        let mut sum = 0;
        while rounds < 32 {
            sum = sum + run(64, [1]);
            rounds = rounds + 1;
        }
        sum;
    "#;
    let compiled = compile_source(source).expect("direct progress fixture should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_native_bridge_stats_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(8_224)]);
    assert!(
        vm.jit_native_active_direct_link_slot_count() > 0,
        "{}",
        vm.dump_jit_info()
    );
    let first_direct_links = vm.jit_native_direct_link_count();

    vm.clear_jit_native_bridge_stats();
    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(8_224)]);
    assert!(
        vm.jit_native_direct_link_count() - first_direct_links > 500,
        "{}",
        vm.dump_jit_info()
    );
    let bridge_stats = vm.jit_native_bridge_stats_snapshot();
    let frame_state = bridge_stats
        .iter()
        .find_map(|(name, count)| (*name == "frame_state").then_some(*count))
        .unwrap_or(0);
    assert!(
        frame_state <= 40,
        "callable direct graph must keep inherited state across escaped side paths: \
         frame_state={frame_state} stats={bridge_stats:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_region_respects_fuel_and_epoch_interrupts() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 256 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("region interrupt fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    vm.set_fuel_check_interval(1).unwrap();
    vm.set_fuel(1_000_000);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());

    vm.reset_for_reuse();
    vm.set_fuel(8);
    let mut fuel_yields = 0_u64;
    loop {
        match vm.run().expect("fuel region run should make progress") {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                fuel_yields = fuel_yields.saturating_add(1);
                assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Fuel));
                assert!(fuel_yields < 1_024, "{}", vm.dump_jit_info());
                vm.recharge_fuel(16).unwrap();
            }
            VmStatus::Waiting(op_id) => panic!("unexpected host wait {op_id}"),
        }
    }
    assert!(fuel_yields > 0);
    assert_eq!(vm.stack(), &[Value::Int(1_024)]);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());

    vm.clear_fuel();
    vm.reset_for_reuse();
    vm.set_epoch_check_interval(1).unwrap();
    vm.set_epoch_deadline(1_000_000).unwrap();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());

    vm.reset_for_reuse();
    vm.set_epoch_deadline(0).unwrap();
    assert_eq!(vm.run().unwrap(), VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Epoch));
    vm.clear_epoch_deadline();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_024)]);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
}

#[test]
fn trace_jit_region_reports_compile_and_code_telemetry() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 128 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("region telemetry fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert!(vm.jit_native_code_bytes() > 0, "{}", vm.dump_jit_info());
    assert!(
        vm.jit_native_region_code_bytes() > 0,
        "{}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_compile_time_ns() > 0,
        "{}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_region_compile_time_ns() > 0,
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_region_republishes_after_native_settings_change() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 256 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("region fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });
    vm.set_fuel_check_interval(64).unwrap();
    vm.set_fuel(1_000_000);

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    let first_edges = vm.jit_native_internal_region_edge_count();

    vm.reset_for_reuse();
    vm.set_fuel_check_interval(1).unwrap();
    vm.set_fuel(1_000_000);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_024)]);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    assert!(vm.jit_native_internal_region_edge_count() > first_edges);
}

#[test]
fn trace_jit_region_invalidation_releases_owner_and_can_republish() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 256 {
            if i % 2 == 0 {
                total = total + 3;
            } else {
                total = total + 5;
            }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("region fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 256,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());

    vm.set_drop_contract_events_enabled(true);
    assert_eq!(vm.jit_native_region_count(), 0);
    vm.set_drop_contract_events_enabled(false);
    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1_024)]);
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
}

#[test]
fn trace_jit_restores_tagged_heap_locals_on_ssa_exit() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(2);
    bc.stloc(2);
    let root_ip = bc.position();
    bc.ldloc(2);
    bc.ldc(4);
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(0);
    bc.stloc(1);
    bc.ldloc(2);
    bc.ldc(3);
    bc.add();
    bc.stloc(2);
    bc.br(root_ip);
    let exit_ip = bc.position();
    bc.ldloc(1);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::string("seed"),
            Value::string(""),
            Value::Int(0),
            Value::Int(1),
            Value::Int(4),
        ],
        code,
    )
    .with_local_count(3);

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("seed")]);
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native SSA execution for tagged-local rebinding loop, dump:\n{}",
        vm.dump_jit_info()
    );

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    assert!(
        dump.contains("lowering=ssa"),
        "expected tagged-local rebinding loop to lower through SSA, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "tagged-local rebinding loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_restores_array_and_map_locals_on_ssa_exit() {
    if !native_jit_supported() {
        return;
    }

    let array_value = Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    let map_value = Value::map(vec![(Value::string("k"), Value::Int(9))]);

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(2);
    bc.stloc(2);
    let root_ip = bc.position();
    bc.ldloc(2);
    bc.ldc(4);
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(2);
    bc.ldc(3);
    bc.add();
    bc.stloc(2);
    bc.br(root_ip);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ldloc(1);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            array_value.clone(),
            map_value.clone(),
            Value::Int(0),
            Value::Int(1),
            Value::Int(4),
        ],
        code,
    )
    .with_local_count(3);

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[array_value, map_value]);
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native SSA execution for array/map exit restore, dump:\n{}",
        vm.dump_jit_info()
    );

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    assert!(
        dump.contains("lowering=ssa"),
        "expected array/map restore loop to lower through SSA, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "array/map restore loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_supports_array_len_get_has_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let arr = [10, 20, 30];
        let mut i = 0;
        let mut sum = 0;
        while i < arr.length {
            if arr.has(i) {
                sum = sum + arr[i];
            }
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("array trace compile should succeed");
    let program = force_local_types(
        compiled.program,
        &[
            (0, ValueType::Array),
            (1, ValueType::Int),
            (2, ValueType::Int),
        ],
    );
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("array trace vm should run");
    assert_eq!(status, VmStatus::Halted);
    let dump = vm.dump_jit_info();
    assert_eq!(vm.stack(), &[Value::Int(60)], "{dump}");

    let snapshot = vm.jit_snapshot();
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution for array builtin loop, dump:\n{dump}"
    );
    assert!(
        dump.contains("lowering=ssa"),
        "expected array builtin loop to lower through SSA, dump:\n{dump}"
    );
    assert!(
        any_trace_op(&snapshot, "array_len")
            && any_trace_op(&snapshot, "array_has")
            && any_trace_op(&snapshot, "array_get"),
        "expected array builtin loop to record specialized array ops, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "array builtin loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_specializes_same_local_array_set_through_loop_back() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut values = [1, 2, 3];
        let mut i = 0;
        while i < 64 {
            values[1] = i;
            i = i + 1;
        }
        values[1];
    "#;

    let compiled = compile_source(source).expect("array-set trace compile should succeed");
    let program = force_local_types(
        compiled.program,
        &[(0, ValueType::Array), (1, ValueType::Int)],
    );
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("array-set trace vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(63)]);

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    assert!(
        any_trace_op(&snapshot, "array_set"),
        "expected array set to remain in SSA, dump:\n{dump}"
    );
    let mutation_trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.op_names.iter().any(|op| op == "array_set"))
        .expect("array-set trace should be present");
    assert!(
        !mutation_trace.has_call && mutation_trace.terminal == JitTraceTerminal::LoopBack,
        "array-set trace should remain native through its backedge, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.native_loop_back_count > 0,
        "array-set loop should stay native across backedges, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "array-set loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_array_set_preserves_cow_alias() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let set_call = builtin_call_index("set").expect("set builtin should exist");
    let get_call = builtin_call_index("get").expect("get builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldloc(0);
    bc.stloc(1);
    bc.ldc(1);
    bc.stloc(2);

    let loop_ip = bc.position();
    bc.ldloc(2);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldc(3);
    bc.ldloc(2);
    bc.ldc(4);
    bc.stloc(0);
    bc.call(set_call, 3);
    bc.stloc(0);
    bc.ldloc(2);
    bc.ldc(5);
    bc.add();
    bc.stloc(2);
    bc.br(loop_ip);

    let exit_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(3);
    bc.call(get_call, 2);
    bc.ldloc(0);
    bc.ldc(3);
    bc.call(get_call, 2);
    bc.add();
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            Value::Int(0),
            Value::Int(64),
            Value::Int(1),
            Value::Null,
            Value::Int(1),
        ],
        code,
    )
    .with_local_count(3);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Array),
            (1, ValueType::Array),
            (2, ValueType::Int),
        ],
    );
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("COW array-set trace should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(65)]);
    let snapshot = vm.jit_snapshot();
    let mutation_trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.op_names.iter().any(|op| op == "array_set"))
        .expect("array-set trace should be present");
    assert_eq!(mutation_trace.terminal, JitTraceTerminal::LoopBack);
    assert!(!mutation_trace.has_call);
}

#[test]
fn trace_jit_does_not_consume_non_moved_array_set_container() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let set_call = builtin_call_index("set").expect("set builtin should exist");
    let get_call = builtin_call_index("get").expect("get builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);

    let loop_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldc(3);
    bc.ldloc(1);
    bc.call(set_call, 3);
    bc.pop();
    bc.ldloc(1);
    bc.ldc(4);
    bc.add();
    bc.stloc(1);
    bc.br(loop_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(3);
    bc.call(get_call, 2);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
            Value::Int(0),
            Value::Int(64),
            Value::Int(1),
            Value::Int(1),
        ],
        code,
    )
    .with_local_count(2);
    let program = force_local_types(program, &[(0, ValueType::Array), (1, ValueType::Int)]);
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("non-moved array-set trace should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(2)]);
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot
            .traces
            .iter()
            .all(|trace| !trace.op_names.iter().any(|op| op == "array_set")),
        "non-moved Set must not consume the local container: {}",
        vm.dump_jit_info()
    );
    assert!(snapshot.traces.iter().any(|trace| trace.has_call));
}

#[test]
fn trace_jit_specializes_same_local_array_push_through_loop_back() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let push_call = vm::BuiltinFunction::ArrayPush.call_index();
    let get_call = builtin_call_index("get").expect("get builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);

    let loop_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldloc(1);
    bc.ldc(3);
    bc.stloc(0);
    bc.call(push_call, 2);
    bc.stloc(0);
    bc.ldloc(1);
    bc.ldc(4);
    bc.add();
    bc.stloc(1);
    bc.br(loop_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(5);
    bc.call(get_call, 2);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::array(Vec::new()),
            Value::Int(0),
            Value::Int(64),
            Value::Null,
            Value::Int(1),
            Value::Int(63),
        ],
        code,
    )
    .with_local_count(2);
    let program = force_local_types(program, &[(0, ValueType::Array), (1, ValueType::Int)]);
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("array-push trace vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(63)]);

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    let mutation_trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.op_names.iter().any(|op| op == "array_push"))
        .expect("array-push trace should be present");
    assert!(
        !mutation_trace.has_call && mutation_trace.terminal == JitTraceTerminal::LoopBack,
        "array-push trace should remain native through its backedge, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.native_loop_back_count > 0,
        "array-push loop should stay native across backedges, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "array-push loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_specializes_same_local_map_set_through_loop_back() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut values = {"key": 0, "drop": 1};
        let mut i = 0;
        while i < 64 {
            values["key"] = i;
            values["drop"] = null;
            i = i + 1;
        }
        values["key"];
    "#;

    let compiled = compile_source(source).expect("map-set trace compile should succeed");
    let program = force_local_types(
        compiled.program,
        &[(0, ValueType::Map), (1, ValueType::Int)],
    );
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm
        .run()
        .unwrap_or_else(|err| panic!("map-set trace vm failed: {err:?}\n{}", vm.dump_jit_info()));
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(63)]);

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    let mutation_trace = snapshot
        .traces
        .iter()
        .find(|trace| trace.op_names.iter().any(|op| op == "map_set"))
        .expect("map-set trace should be present");
    assert_eq!(
        mutation_trace
            .op_names
            .iter()
            .filter(|op| op.as_str() == "map_set")
            .count(),
        2,
        "map-set trace should include overwrite and null-delete mutations, dump:\n{dump}"
    );
    assert!(
        !mutation_trace.has_call && mutation_trace.terminal == JitTraceTerminal::LoopBack,
        "map-set trace should remain native through its backedge, dump:\n{dump}"
    );
    assert!(
        snapshot.metrics.native_loop_back_count > 0,
        "map-set loop should stay native across backedges, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "map-set loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_supports_map_len_get_has_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let data = {"a": 7, "b": 11, "c": 13};
        let mut i = 0;
        let mut sum = 0;
        while i < data.length {
            if i == 0 {
                if data.has("a") {
                    sum = sum + data["a"];
                }
            } else if i == 1 {
                if data.has("b") {
                    sum = sum + data["b"];
                }
            } else {
                if data.has("c") {
                    sum = sum + data["c"];
                }
            }
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("map trace compile should succeed");
    let program = force_local_types(
        compiled.program,
        &[
            (0, ValueType::Map),
            (1, ValueType::Int),
            (2, ValueType::Int),
        ],
    );
    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("map trace vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(31)]);

    let dump = vm.dump_jit_info();
    let snapshot = vm.jit_snapshot();
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution for map builtin loop, dump:\n{dump}"
    );
    assert!(
        dump.contains("lowering=ssa"),
        "expected map builtin loop to lower through SSA, dump:\n{dump}"
    );
    assert!(
        any_trace_op(&snapshot, "map_len")
            && any_trace_op(&snapshot, "map_has")
            && any_trace_op(&snapshot, "map_get"),
        "expected map builtin loop to record specialized map ops, dump:\n{dump}"
    );
    assert_eq!(
        snapshot.metrics.helper_fallback_count, 0,
        "map builtin loop should not need interpreter fallback, dump:\n{dump}"
    );
}

#[test]
fn trace_jit_supports_float_and_string_loops_through_ssa() {
    if !native_jit_supported() {
        return;
    }

    let float_source = r#"
        let mut i = 0;
        let mut sum = 0.0;
        while i < 4 {
            sum = sum + 1.25;
            i = i + 1;
        }
        sum;
    "#;
    let string_source = r#"
        let mut i = 0;
        let mut out = "";
        while i < 3 {
            out = out + "x";
            i = i + 1;
        }
        out;
    "#;

    let compiled_float = compile_source(float_source).expect("float compile should succeed");
    let mut float_vm = Vm::new(compiled_float.program);
    float_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let float_status = float_vm.run().expect("float vm should run");
    assert_eq!(float_status, VmStatus::Halted);
    assert_eq!(float_vm.stack(), &[Value::Float(5.0)]);
    let float_snapshot = float_vm.jit_snapshot();
    assert!(
        float_vm.jit_native_exec_count() > 0,
        "float loop should execute through native SSA, dump:\n{}",
        float_vm.dump_jit_info()
    );
    assert!(
        any_trace_ssa_contains(&float_snapshot, "fadd "),
        "float loop should record SSA float ops, dump:\n{}",
        float_vm.dump_jit_info()
    );
    assert!(
        float_vm.dump_jit_info().contains("lowering=ssa"),
        "float loop should lower through SSA, dump:\n{}",
        float_vm.dump_jit_info()
    );

    let compiled_string = compile_source(string_source).expect("string compile should succeed");
    let mut string_vm = Vm::new(compiled_string.program);
    string_vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let string_status = string_vm.run().expect("string vm should run");
    assert_eq!(string_status, VmStatus::Halted);
    assert_eq!(string_vm.stack(), &[Value::string("xxx")]);
    let string_snapshot = string_vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &string_vm,
        &string_snapshot,
        "string add loop",
        &["type_of", "to_string_identity", "string_concat"],
    );
}

#[test]
fn trace_jit_supports_bytes_heavy_call_boundary_exits_without_fallback() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let mut i = 0;
        let mut payload = bytes::from_hex("");
        let mut amount = 0;
        let mut part = bytes::from_hex("");
        let mut byte = 0;
        let mut present = false;
        while i < 4 {
            payload = payload + bytes::from_hex("ab");
            amount = payload.length;
            let view = payload.copy();
            byte = (&view)[0].copy();
            present = payload.has(2);
            part = view[1:3];
            i = i + 1;
        }
        payload;
        amount;
        part;
        byte;
        present;
    "#;

    let compiled = compile_source(source).expect("bytes trace compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("bytes trace vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::bytes(vec![0xAB, 0xAB, 0xAB, 0xAB]),
            Value::Int(4),
            Value::bytes(vec![0xAB, 0xAB]),
            Value::Int(0xAB),
            Value::Bool(true),
        ]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_call_boundary_trace(&vm, &snapshot, "bytes-heavy loop");
}

#[test]
fn trace_jit_keeps_join_path_inline_for_straight_line_if_diamond() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut acc = 0;
        while i < 64 {
            acc = acc + 250;
            if acc > 1000 {
                acc = acc - 1000;
            }
            i = i + 1;
        }
        acc;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1000)]);

    let snapshot = vm.jit_snapshot();
    let loop_trace = snapshot
        .traces
        .iter()
        .find(|trace| {
            trace.terminal == JitTraceTerminal::LoopBack
                && trace.op_names().iter().any(|op| op == "guard_true")
        })
        .expect("expected loop trace with join-path guard");
    assert!(
        loop_trace.op_names().iter().any(|op| op == "jump_root"),
        "expected loop trace to continue through the join path, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_float_math_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut acc = 6.0;
        while i < 4 {
            acc = acc + 2.0;
            acc = acc - 0.5;
            acc = acc * 2.0;
            acc = acc / 4.0;
            acc = acc % 3.0;
            acc = -acc;
            i = i + 1;
        }
        acc;
    "#;

    let compiled = compile_source(source).expect("float math compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("float math vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(-0.46875)]);
    let snapshot = vm.jit_snapshot();
    assert!(
        vm.jit_native_exec_count() > 0,
        "float math loop should execute through native SSA, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "fadd ")
            && any_trace_ssa_contains(&snapshot, "fsub ")
            && any_trace_ssa_contains(&snapshot, "fmul ")
            && any_trace_ssa_contains(&snapshot, "fdiv ")
            && any_trace_ssa_contains(&snapshot, "fmod ")
            && any_trace_ssa_contains(&snapshot, "fneg "),
        "float math loop should record SSA float math ops, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "float loop should not use native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_string_call_boundary_exits() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut text = "";
        while i < 6 {
            text = text + "x";
            i = i + 1;
        }
        text;
    "#;

    let compiled = compile_source(source).expect("string concat compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("string concat vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("xxxxxx")]);
    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "string concat loop",
        &["type_of", "to_string_identity", "string_concat"],
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "string concat loop should not need native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_bytes_call_boundary_exits() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let mut i = 0;
        let mut payload = bytes::from_hex("");
        while i < 5 {
            payload = payload + bytes::from_hex("00ff");
            i = i + 1;
        }
        payload;
    "#;

    let compiled = compile_source(source).expect("bytes concat compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("bytes concat vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::bytes(vec![
            0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF
        ])]
    );
    let snapshot = vm.jit_snapshot();
    assert_native_ssa_call_boundary_trace(&vm, &snapshot, "bytes concat loop");

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "bytes concat loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_bytes_sequence_call_boundary_exits() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let mut i = 0;
        let mut total = 0;
        let mut part = bytes::from_hex("");
        let mut byte = 0;
        let mut present = false;
        while i < 4 {
            let payload = bytes::from_hex("00ff10");
            total = payload.length;
            byte = (&payload)[1].copy();
            present = payload.has(2);
            part = payload[1:3];
            i = i + 1;
        }
        total;
        part;
        byte;
        present;
    "#;

    let compiled = compile_source(source).expect("bytes builtin compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("bytes builtin vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::Int(3),
            Value::bytes(vec![0xFF, 0x10]),
            Value::Int(255),
            Value::Bool(true),
        ]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_call_boundary_trace(&vm, &snapshot, "bytes builtin loop");

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "bytes builtin loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_string_sequence_call_boundary_exits() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut total = 0;
        let mut part = "";
        let mut ch = "";
        while i < 4 {
            let text = "a界🙂";
            total = text.length;
            ch = (&text)[1].copy();
            part = text[1:3];
            i = i + 1;
        }
        total;
        part;
        ch;
    "#;

    let compiled = compile_source(source).expect("string builtin compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("string builtin vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(3), Value::string("界🙂"), Value::string("界"),]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "string builtin loop",
        &["string_len", "string_get", "string_slice"],
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "string builtin loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_bytes_len_get_slice_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let len_call = builtin_call_index("len").expect("len builtin should exist");
    let get_call = builtin_call_index("get").expect("get builtin should exist");
    let slice_call = builtin_call_index("slice").expect("slice builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(1);
    bc.stloc(2);
    bc.ldc(1);
    bc.stloc(3);
    bc.ldc(6);
    bc.stloc(4);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(5);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    let len_ip = bc.position();
    bc.call(len_call, 1);
    bc.stloc(2);

    bc.ldloc(0);
    bc.ldc(2);
    let get_ip = bc.position();
    bc.call(get_call, 2);
    bc.stloc(3);

    bc.ldloc(0);
    bc.ldc(2);
    bc.ldc(4);
    let slice_ip = bc.position();
    bc.call(slice_call, 3);
    bc.stloc(4);

    bc.ldloc(1);
    bc.ldc(3);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(2);
    bc.ldloc(4);
    bc.ldloc(3);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::bytes(vec![0x00, 0xFF, 0x10]),
            Value::Int(0),
            Value::Int(1),
            Value::Int(1),
            Value::Int(2),
            Value::Int(4),
            Value::bytes(vec![]),
        ],
        code,
    )
    .with_local_count(5);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Bytes),
            (1, ValueType::Int),
            (2, ValueType::Int),
            (3, ValueType::Int),
            (4, ValueType::Bytes),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (len_ip as usize, (ValueType::Bytes, ValueType::Unknown)),
            (get_ip as usize, (ValueType::Bytes, ValueType::Int)),
            (slice_ip as usize, (ValueType::Bytes, ValueType::Int)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("bytes builtin vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::Int(3),
            Value::bytes(vec![0xFF, 0x10]),
            Value::Int(255),
        ]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "bytes builtin loop",
        &["bytes_len", "bytes_get", "bytes_slice"],
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "unbox_ptr "),
        "bytes builtin loop should guard a bytes heap pointer at trace entry, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "bytes builtin loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_bytes_has_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let has_call = builtin_call_index("has").expect("has builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(2);
    bc.stloc(2);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldc(4);
    let has_ip = bc.position();
    bc.call(has_call, 2);
    bc.stloc(2);

    bc.ldloc(1);
    bc.ldc(5);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(2);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::bytes(vec![0x00, 0xFF, 0x10]),
            Value::Int(0),
            Value::Bool(false),
            Value::Int(4),
            Value::Int(2),
            Value::Int(1),
        ],
        code,
    )
    .with_local_count(3);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Bytes),
            (1, ValueType::Int),
            (2, ValueType::Bool),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (has_ip as usize, (ValueType::Bytes, ValueType::Int)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("bytes has vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(&vm, &snapshot, "bytes has loop", &["bytes_has"]);
    assert!(
        any_trace_ssa_contains(&snapshot, "unbox_ptr "),
        "bytes has loop should guard a bytes heap pointer at trace entry, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "bytes has loop should not need native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_uses_call_operand_type_for_string_len_with_reused_local_slot() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let len_call = builtin_call_index("len").expect("len builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(1);
    bc.stloc(2);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    let len_ip = bc.position();
    bc.call(len_call, 1);
    bc.stloc(2);

    bc.ldloc(1);
    bc.ldc(2);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(2);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::string("a界🙂"),
            Value::Int(0),
            Value::Int(1),
            Value::Int(4),
        ],
        code,
    )
    .with_local_count(3);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Null),
            (1, ValueType::Int),
            (2, ValueType::Int),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (len_ip as usize, (ValueType::String, ValueType::Unknown)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("string len vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);

    let snapshot = vm.jit_snapshot();
    let root_traces = snapshot
        .traces
        .iter()
        .filter(|trace| trace.root_ip == root_ip as usize)
        .collect::<Vec<_>>();
    assert!(
        root_traces.iter().any(|trace| {
            trace.terminal == JitTraceTerminal::LoopBack
                && trace.op_names().iter().any(|op| op == "string_len")
                && trace.op_names().iter().any(|op| op == "jump_root")
        }),
        "reused-slot string len trace should specialize and reach loopback, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        root_traces.iter().all(|trace| !trace.has_call),
        "reused-slot string len root should not retain a call-only trace, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        root_traces
            .iter()
            .any(|trace| trace.ssa_text().contains("string_len")),
        "reused-slot string len root should contain string_len SSA, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        root_traces
            .iter()
            .any(|trace| trace.ssa_text().contains("unbox_ptr ")),
        "reused-slot string len root should retain the runtime heap tag guard, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_string_len_get_slice_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let len_call = builtin_call_index("len").expect("len builtin should exist");
    let get_call = builtin_call_index("get").expect("get builtin should exist");
    let slice_call = builtin_call_index("slice").expect("slice builtin should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(1);
    bc.stloc(2);
    bc.ldc(5);
    bc.stloc(3);
    bc.ldc(5);
    bc.stloc(4);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(6);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    let len_ip = bc.position();
    bc.call(len_call, 1);
    bc.stloc(2);

    bc.ldloc(0);
    bc.ldc(4);
    let get_ip = bc.position();
    bc.call(get_call, 2);
    bc.stloc(3);

    bc.ldloc(0);
    bc.ldc(4);
    bc.ldc(3);
    let slice_ip = bc.position();
    bc.call(slice_call, 3);
    bc.stloc(4);

    bc.ldloc(1);
    bc.ldc(3);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(2);
    bc.ldloc(4);
    bc.ldloc(3);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::string("a界🙂"),
            Value::Int(0),
            Value::Int(1),
            Value::Int(2),
            Value::Int(1),
            Value::string(""),
            Value::Int(4),
        ],
        code,
    )
    .with_local_count(5);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::String),
            (1, ValueType::Int),
            (2, ValueType::Int),
            (3, ValueType::String),
            (4, ValueType::String),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (len_ip as usize, (ValueType::String, ValueType::Unknown)),
            (get_ip as usize, (ValueType::String, ValueType::Int)),
            (slice_ip as usize, (ValueType::String, ValueType::Int)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("string builtin vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(3), Value::string("界🙂"), Value::string("界"),]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "string builtin loop",
        &["string_len", "string_get", "string_slice"],
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "unbox_ptr "),
        "string builtin loop should guard a string heap pointer at trace entry, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "string builtin loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_manual_string_concat_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldc(4);
    let concat_ip = bc.position();
    bc.add();
    bc.stloc(0);

    bc.ldloc(1);
    bc.ldc(2);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::string(""),
            Value::Int(0),
            Value::Int(1),
            Value::Int(4),
            Value::string("x"),
        ],
        code,
    )
    .with_local_count(2);
    let program = force_local_types(program, &[(0, ValueType::String), (1, ValueType::Int)]);
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (concat_ip as usize, (ValueType::String, ValueType::String)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("manual string concat vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("xxxx")]);

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "manual string concat loop",
        &["string_concat"],
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "unbox_ptr "),
        "manual string concat loop should unbox string operands into heap pointers, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "manual string concat loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_manual_bytes_concat_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldc(4);
    let concat_ip = bc.position();
    bc.add();
    bc.stloc(0);

    bc.ldloc(1);
    bc.ldc(2);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::bytes(vec![]),
            Value::Int(0),
            Value::Int(1),
            Value::Int(5),
            Value::bytes(vec![0x00, 0xFF]),
        ],
        code,
    )
    .with_local_count(2);
    let program = force_local_types(program, &[(0, ValueType::Bytes), (1, ValueType::Int)]);
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (concat_ip as usize, (ValueType::Bytes, ValueType::Bytes)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("manual bytes concat vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::bytes(vec![
            0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF,
        ])]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "manual bytes concat loop",
        &["bytes_concat"],
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "unbox_ptr "),
        "manual bytes concat loop should unbox bytes operands into heap pointers, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "manual bytes concat loop should not need native helper bridges for call-boundary execution, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_bytes_array_codecs_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    let from_call =
        builtin_call_index("bytes::from_array_u8").expect("bytes::from_array_u8 should exist");
    let to_call =
        builtin_call_index("bytes::to_array_u8").expect("bytes::to_array_u8 should exist");
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(2);
    bc.stloc(2);

    let root_ip = bc.position();
    bc.ldloc(2);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    let from_ip = bc.position();
    bc.call(from_call, 1);
    bc.stloc(1);

    bc.ldloc(1);
    let to_ip = bc.position();
    bc.call(to_call, 1);
    bc.stloc(0);

    bc.ldloc(2);
    bc.ldc(4);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(2);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ldloc(1);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(255)]),
            Value::bytes(vec![]),
            Value::Int(0),
            Value::Int(3),
            Value::Int(1),
        ],
        code,
    )
    .with_local_count(3);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Array),
            (1, ValueType::Bytes),
            (2, ValueType::Int),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (from_ip as usize, (ValueType::Array, ValueType::Unknown)),
            (to_ip as usize, (ValueType::Bytes, ValueType::Unknown)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("bytes array codec vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::array(vec![Value::Int(1), Value::Int(2), Value::Int(255)]),
            Value::bytes(vec![1, 2, 255]),
        ]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "bytes array codec loop",
        &["bytes_from_array_u8", "bytes_to_array_u8"],
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "bytes array codec loop should not need native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_shift_ops_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(1);
    bc.stloc(2);
    bc.ldc(1);
    bc.stloc(3);

    let root_ip = bc.position();
    bc.ldloc(1);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldc(2);
    let shr_ip = bc.position();
    bc.shr();
    bc.stloc(2);

    bc.ldloc(0);
    bc.ldc(2);
    let lshr_ip = bc.position();
    bc.lshr();
    bc.stloc(3);

    bc.ldloc(1);
    bc.ldc(2);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(1);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(2);
    bc.ldloc(3);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![Value::Int(-8), Value::Int(0), Value::Int(1), Value::Int(3)],
        code,
    )
    .with_local_count(4);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Int),
            (1, ValueType::Int),
            (2, ValueType::Int),
            (3, ValueType::Int),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (shr_ip as usize, (ValueType::Int, ValueType::Int)),
            (lshr_ip as usize, (ValueType::Int, ValueType::Int)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("shift vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::Int(-4),
            Value::Int(((i64::from(-8i8) as u64) >> 1) as i64)
        ]
    );

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(&vm, &snapshot, "shift loop", &["ishr_imm", "ilshr_imm"]);

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "shift loop should not need native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_eager_bool_ops_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    bc.ldc(1);
    bc.stloc(1);
    bc.ldc(2);
    bc.stloc(2);
    bc.ldc(1);
    bc.stloc(3);

    let root_ip = bc.position();
    bc.ldloc(2);
    bc.ldc(3);
    let clt_ip = bc.position();
    bc.clt();
    let guard_ip = bc.position();
    bc.brfalse(0);

    bc.ldloc(0);
    bc.ldloc(1);
    let and_ip = bc.position();
    bc.and();
    bc.ldloc(0);
    let or_ip = bc.position();
    bc.or();
    let _not_ip = bc.position();
    bc.not();
    bc.stloc(3);

    bc.ldloc(2);
    bc.ldc(4);
    let add_ip = bc.position();
    bc.add();
    bc.stloc(2);
    bc.br(root_ip);

    let exit_ip = bc.position();
    bc.ldloc(3);
    bc.ret();

    let mut code = bc.finish();
    patch_branch_target(&mut code, guard_ip, exit_ip);
    let program = Program::new(
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(4),
            Value::Int(1),
        ],
        code,
    )
    .with_local_count(4);
    let program = force_local_types(
        program,
        &[
            (0, ValueType::Bool),
            (1, ValueType::Bool),
            (2, ValueType::Int),
            (3, ValueType::Bool),
        ],
    );
    let program = force_operand_types(
        program,
        &[
            (clt_ip as usize, (ValueType::Int, ValueType::Int)),
            (and_ip as usize, (ValueType::Bool, ValueType::Bool)),
            (or_ip as usize, (ValueType::Bool, ValueType::Bool)),
            (add_ip as usize, (ValueType::Int, ValueType::Int)),
        ],
    );

    let mut vm = Vm::new(program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("bool vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(false)]);

    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "eager bool loop",
        &["bool_and", "bool_or", "bool_not"],
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "bool_not "),
        "eager bool loop should record a bool_not SSA op, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "eager bool loop should not need native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_supports_float_comparisons_in_ssa() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        let mut value = 1.5;
        let mut less = false;
        let mut greater = false;
        let mut equal = false;
        while i < 4 {
            less = value < 3.0;
            greater = value > 1.0;
            equal = value == 2.0;
            value = value + 0.5;
            i = i + 1;
        }
        less;
        greater;
        equal;
    "#;

    let compiled = compile_source(source).expect("float compare compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);

    let status = vm.run().expect("float compare vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Bool(false), Value::Bool(true), Value::Bool(false)]
    );
    let snapshot = vm.jit_snapshot();
    assert!(
        vm.jit_native_exec_count() > 0,
        "float compare loop should execute through native SSA, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        any_trace_ssa_contains(&snapshot, "fcmp_lt ")
            && any_trace_ssa_contains(&snapshot, "fcmp_gt ")
            && any_trace_ssa_contains(&snapshot, "fcmp_eq "),
        "float compare loop should record SSA float compare ops, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "float compare loop should not use native helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_nested_loop_with_one_live_caller_operand() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        fn sum_to(limit: int) -> int {
            let mut sum = 0;
            for i in 0..limit {
                sum = sum + i;
            }
            sum
        }

        100 + sum_to(10);
    "#;
    let compiled = compile_source(source).expect("live entry stack source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("live entry stack program should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(145)]);
    let snapshot = vm.jit_snapshot();
    assert!(vm.jit_native_exec_count() > 0, "{}", vm.dump_jit_info());
    assert!(
        snapshot.traces.iter().any(|trace| trace.frame_key == 0),
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_nested_loop_with_two_live_caller_operands() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        fn sum_to(limit: int) -> int {
            let mut sum = 0;
            for i in 0..limit {
                sum = sum + i;
            }
            sum
        }

        1 + (2 + sum_to(10));
    "#;
    let compiled = compile_source(source).expect("depth-two entry stack source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("depth-two entry stack program should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(48)]);
    let snapshot = vm.jit_snapshot();
    assert!(vm.jit_native_exec_count() > 0, "{}", vm.dump_jit_info());
    assert!(
        snapshot.traces.iter().any(|trace| trace.frame_key == 0),
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_nested_loop_with_heap_caller_operand() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        fn sum_to(limit: int) -> int {
            let mut sum = 0;
            for i in 0..limit {
                sum = sum + i;
            }
            sum
        }

        ["left", 7, sum_to(10)];
    "#;
    let compiled = compile_source(source).expect("heap entry stack source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    let status = vm.run().expect("heap entry stack program should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::array(vec![
            Value::string("left"),
            Value::Int(7),
            Value::Int(45),
        ])]
    );
    let snapshot = vm.jit_snapshot();
    assert!(vm.jit_native_exec_count() > 0, "{}", vm.dump_jit_info());
    assert!(
        snapshot.traces.iter().any(|trace| trace.frame_key == 0),
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_reuses_nested_frame_trace_after_reset() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        fn sum_to(limit: int) -> int {
            let mut sum = 0;
            for i in 0..limit {
                sum = sum + i;
            }
            sum
        }

        100 + sum_to(10);
    "#;
    let compiled = compile_source(source).expect("reusable entry stack source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(vm.run().expect("first run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(145)]);
    let first_native_exec_count = vm.jit_native_exec_count();
    assert!(first_native_exec_count > 0, "{}", vm.dump_jit_info());

    vm.reset_for_reuse();
    assert_eq!(vm.run().expect("second run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(145)]);
    assert!(vm.jit_native_exec_count() > first_native_exec_count);
}

#[test]
fn literal_string_builtins_match_interpreter_semantics() {
    let source = r#"
        string_contains("a界🙂z", "界🙂");
        string_contains("abc", "");
        string_contains("abc", "x");
        string_replace_literal("aaaa", "aa", "aaX");
        string_replace_literal("a界a", "", "x");
        string_replace_literal("a界a", "z", "x");
        string_lower_ascii("AZ界Ä🙂");
        string_split_literal("甲｜乙｜丙", "｜");
    "#;
    let compiled = compile_source(source).expect("literal string builtin compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    disable_trace_jit(&mut vm);
    assert_eq!(
        vm.run().expect("literal string vm should run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false),
            Value::string("aaXaaX"),
            Value::string("a界a"),
            Value::string("a界a"),
            Value::string("az界Ä🙂"),
            Value::array(vec![
                Value::string("甲"),
                Value::string("乙"),
                Value::string("丙")
            ]),
        ]
    );
}

#[test]
fn aot_literal_string_builtins_match_interpreter_semantics() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        string_contains("a界🙂z", "界🙂");
        string_replace_literal("aaaa", "aa", "aaX");
        string_lower_ascii("AZ界Ä🙂");
        string_split_literal("甲｜乙｜丙", "｜");
    "#;
    let compiled = compile_source(source).expect("literal string builtin compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);

    assert_eq!(
        vm.run().expect("literal string AOT vm should run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[
            Value::Bool(true),
            Value::string("aaXaaX"),
            Value::string("az界Ä🙂"),
            Value::array(vec![
                Value::string("甲"),
                Value::string("乙"),
                Value::string("丙")
            ]),
        ]
    );
    assert!(vm.aot_exec_count() > 0);
}

#[test]
fn trace_jit_specializes_literal_string_builtins_without_call_boundary() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut found = false;
        let mut replaced = "";
        let mut lowered = "";
        let mut pieces: [string] = [];
        while i < 4 {
            found = string_contains("a界🙂z", "界🙂");
            replaced = string_replace_literal("aaaa", "aa", "aaX");
            lowered = string_lower_ascii("AZ界Ä🙂");
            pieces = string_split_literal("甲｜乙｜丙", "｜");
            i = i + 1;
        }
        found;
        replaced;
        lowered;
        pieces;
    "#;
    let compiled = compile_source(source).expect("literal string builtin compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    vm.set_jit_native_bridge_stats_enabled(true);
    assert_eq!(
        vm.run().expect("literal string jit should run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[
            Value::Bool(true),
            Value::string("aaXaaX"),
            Value::string("az界Ä🙂"),
            Value::array(vec![
                Value::string("甲"),
                Value::string("乙"),
                Value::string("丙")
            ]),
        ]
    );
    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "literal string builtin loop",
        &[
            "string_contains",
            "string_replace_literal",
            "string_lower_ascii",
            "string_split_literal",
        ],
    );
    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        bridge_hits
            .iter()
            .all(|(name, count)| *count == 0 || is_jit_state_boundary_bridge(name)),
        "literal string builtin loop should not use native helper bridges: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_specializes_loop_carried_string_builtins() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let values: [string] = ["abc"];
        let mut text: string = (&values)[0];
        let mut i = 0;
        let mut found = false;
        let mut same = false;
        while i < 8 {
            same = (&text) == "abc";
            found = string_contains(&text, "a");
            text = string_replace_literal(text, "x", "x");
            i = i + 1;
        }
        found;
        same;
        text;
    "#;
    let compiled =
        compile_source(source).expect("loop-carried string builtin compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm.run()
            .expect("loop-carried string builtin jit should run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack(),
        &[Value::Bool(true), Value::Bool(true), Value::string("abc")]
    );
    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "loop-carried string builtin",
        &["value_eq", "string_contains", "string_replace_literal"],
    );
}

#[test]
fn trace_jit_links_dynamic_concat_callable_graph() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn encode_map(values: map<string>) -> string {
            let keys = (&values).keys;
            let mut out = "";
            for i in 0..keys.length {
                let key: string = (&keys)[i];
                out = out + key + "=" + (&values)[key] + "\n";
            }
            out
        }
        let values: map<string> = { "a": "one", "b": "two" };
        let mut i = 0;
        let mut out = "";
        while i < 8 {
            out = encode_map(&values);
            i = i + 1;
        }
        string_contains(&out, "a=one");
    "#;
    let compiled = compile_source(source).expect("dynamic concat fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status = vm.run().unwrap_or_else(|error| {
        panic!(
            "dynamic concat fixture failed at ip {} stack={:?}: {error:?}\n{}",
            vm.ip(),
            vm.stack(),
            vm.dump_jit_info(),
        )
    });
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot.traces.iter().any(|trace| {
            trace.terminal == JitTraceTerminal::CallValue && trace.executions >= 8
        }),
        "dynamic concat root trace should link into its callable graph:\n{}",
        vm.dump_jit_info(),
    );
    assert!(vm.jit_native_link_handoff_count() > 0);
    assert!(vm.dump_jit_info().contains("interpreter fallbacks: 0"));
}

#[test]
fn trace_jit_folds_known_type_of_guards_after_map_get() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let values = { "key": "value", "number": 1 };
        let keys = ["key", "key"];
        let mut i = 0;
        let mut matched = false;
        while i < 8 {
            let key = (&keys)[i % 2];
            matched = type((&values)[key]) == "string";
            i = i + 1;
        }
        matched;
    "#;
    let compiled = compile_source(source).expect("known type guard fixture should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm.run().expect("known type guard fixture should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "known type guard after map get",
        &["map_get", "type_of", "value_eq"],
    );
}

#[test]
fn trace_jit_specializes_regex_builtins_without_call_boundary() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        use re;
        let mut i = 0;
        let mut matched = false;
        let mut replaced = "";
        while i < 8 {
            matched = re::match("(?i)^rustscript$", "RustScript");
            replaced = re::replace("\\s+", "a b", "");
            i = i + 1;
        }
        matched;
        replaced;
    "#;
    let compiled = compile_source(source).expect("regex match compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm.run().expect("regex match jit should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Bool(true), Value::string("ab")]);
    let snapshot = vm.jit_snapshot();
    assert_native_ssa_specialized_trace(
        &vm,
        &snapshot,
        "regex builtin loop",
        &["regex_match", "regex_replace"],
    );
    assert_eq!(vm.regex_cache_entry_count(), 2);
    assert_eq!(vm.regex_cache_compile_count(), 2);
    assert!(vm.regex_cache_hit_count() >= 14);
}

#[test]
fn trace_jit_executes_hot_loop_inside_script_callable_frame() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn sum_to(limit: int) -> int {
            let mut i = 0;
            let mut total = 0;
            while i < limit {
                total = total + i;
                i = i + 1;
            }
            total
        }
        sum_to(100);
    "#;
    let compiled = compile_source(source).expect("nested-frame loop should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("nested-frame trace should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(4_950)]);
    assert!(vm.jit_native_exec_count() > 0);
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot.traces.iter().any(|trace| {
            trace.frame_key == 0
                && trace.terminal == JitTraceTerminal::LoopBack
                && trace.executions > 0
        }),
        "expected an executed prototype-keyed loopback trace: {}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_region_cycles_without_external_handoffs() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn alternating_sum(limit: int) -> int {
            let mut i = 0;
            let mut total = 0;
            while i < limit {
                if i % 2 == 0 {
                    total = total + i;
                } else {
                    total = total - i;
                }
                i = i + 1;
            }
            total
        }
        alternating_sum(4096);
    "#;
    let compiled = compile_source(source).expect("exit-heavy callable loop should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(false);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("exit-heavy callable loop should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(-2_048)]);
    let first_native_execs = vm.jit_native_exec_count();
    let first_region_entries = vm.jit_native_region_entry_count();
    let first_internal_edges = vm.jit_native_internal_region_edge_count();
    let first_handoffs = vm.jit_native_link_handoff_count();
    let first_fallbacks = vm.jit_helper_fallback_count();
    assert_eq!(vm.jit_native_region_count(), 1, "{}", vm.dump_jit_info());
    assert!(first_native_execs > 0);
    assert!(first_region_entries > 0, "{}", vm.dump_jit_info());
    assert!(first_internal_edges > 0, "{}", vm.dump_jit_info());

    vm.reset_for_reuse();
    assert_eq!(
        vm.run().expect("linked callable loop should run again"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(-2_048)]);
    let second_handoffs = vm
        .jit_native_link_handoff_count()
        .saturating_sub(first_handoffs);
    assert!(
        second_handoffs <= 2,
        "callable cycle should remain in one native region, second_handoffs={second_handoffs}: {}",
        vm.dump_jit_info()
    );
    assert!(vm.jit_native_exec_count() > first_native_execs);
    assert!(vm.jit_native_region_entry_count() > first_region_entries);
    assert!(vm.jit_native_internal_region_edge_count() > first_internal_edges);
    assert_eq!(vm.jit_helper_fallback_count(), first_fallbacks);
}

#[test]
fn trace_jit_direct_links_cross_frame_call_and_return_edges() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        let delta = 3;
        let add: fn(int) -> int = |value| value + delta;
        while i < 4096 {
            total = add(total);
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("direct callable loop should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12_288)]);
    let first_direct = vm.jit_native_direct_link_count();
    let first_handoffs = vm.jit_native_link_handoff_count();

    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12_288)]);
    let direct_delta = vm.jit_native_direct_link_count() - first_direct;
    let handoff_delta = vm.jit_native_link_handoff_count() - first_handoffs;
    assert!(direct_delta > 12_000, "{}", vm.dump_jit_info());
    assert!(handoff_delta <= 3, "{}", vm.dump_jit_info());
    assert_eq!(vm.jit_native_region_count(), 0);
}

#[test]
fn trace_jit_direct_link_slots_clear_and_republish_after_mode_toggle() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        while i < 512 {
            if i % 2 == 0 { total = total + 3; } else { total = total + 5; }
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).unwrap();
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_native_direct_links_enabled(true);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2_048)]);
    assert!(vm.jit_native_direct_link_count() > 500);

    vm.set_jit_native_direct_links_enabled(false);
    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2_048)]);
    assert_eq!(vm.jit_native_direct_link_count(), 0);

    vm.set_jit_native_direct_links_enabled(true);
    vm.reset_for_reuse();
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2_048)]);
    assert!(
        vm.jit_native_direct_link_count() > 500,
        "{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_call_value_natively_inside_loop() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        let mut i = 0;
        let mut total = 0;
        let delta = 3;
        let add: fn(int) -> int = |value| value + delta;
        while i < 16 {
            total = add(total);
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("callable loop should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("callable loop trace should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(48)]);
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot.traces.iter().any(|trace| {
            trace.terminal == JitTraceTerminal::CallValue
                && trace.op_names.iter().any(|name| name == "call_value")
                && trace.executions > 0
        }),
        "expected native callable trace: {}",
        vm.dump_jit_info()
    );
    assert!(
        vm.jit_native_link_handoff_count() > 0,
        "expected callable native handoff: {}",
        vm.dump_jit_info()
    );
    assert!(vm.dump_jit_info().contains("interpreter fallbacks: 0"));
}

#[test]
fn trace_jit_call_value_waits_and_resumes_host_callable_without_replay() {
    if !native_jit_supported() {
        return;
    }

    struct PendingCallableHost;

    impl HostFunction for PendingCallableHost {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
            let value = match args {
                [Value::Int(value)] => *value,
                _ => {
                    return Err(vm::VmError::HostError(
                        "pending callable expected int".to_string(),
                    ));
                }
            };
            Ok(CallOutcome::Pending(900 + value as u64))
        }
    }

    let source = r#"
        fn action(value: int) -> int;
        let function: fn(int) -> int = action;
        let mut i = 0;
        let mut total = 0;
        while i < 2 {
            total = total + function(i);
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("pending callable loop should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.register_function(Box::new(PendingCallableHost));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("first callable host call should wait"),
        VmStatus::Waiting(900)
    );
    vm.complete_host_op(900, CallReturn::one(Value::Int(10)))
        .expect("first pending call should complete");
    assert_eq!(
        vm.resume().expect("second callable host call should wait"),
        VmStatus::Waiting(901)
    );
    vm.complete_host_op(901, CallReturn::one(Value::Int(20)))
        .expect("second pending call should complete");
    assert_eq!(
        vm.resume().expect("callable host loop should finish"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(30)]);
    assert!(
        vm.jit_snapshot().traces.iter().any(|trace| {
            trace.terminal == JitTraceTerminal::CallValue && trace.executions >= 2
        })
    );
    assert!(vm.dump_jit_info().contains("interpreter fallbacks: 0"));
}

#[test]
fn trace_jit_call_value_yields_and_resumes_host_callable_without_losing_frame_state() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn action() -> int;
        let function: fn() -> int = action;
        let mut i = 0;
        let mut total = 0;
        while i < 2 {
            total = total + function();
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("yielding callable loop should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.register_function(Box::new(YieldOnce { yielded: false }));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("callable host should yield"),
        VmStatus::Yielded
    );
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Host));
    assert_eq!(
        vm.resume().expect("callable host loop should resume"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(84)]);
    assert!(vm.dump_jit_info().contains("interpreter fallbacks: 0"));
}

#[test]
fn trace_jit_links_nested_dynamic_script_callables_without_interpreter_handoff() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn add_one(value: int) -> int { value + 1 }
        fn add_two(value: int) -> int { value + 2 }
        fn apply(function: fn(int) -> int, value: int) -> int { function(value) }
        let mut i = 0;
        let mut total = 0;
        while i < 16 {
            let selected = if i < 8 => { add_one } else => { add_two };
            total = apply(selected, total);
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("dynamic callable graph should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("dynamic callable graph should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(24)]);
    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot
            .traces
            .iter()
            .any(|trace| trace.frame_key != u64::MAX)
    );
    assert!(
        vm.jit_native_link_handoff_count() > 0,
        "expected linked callable graph: {}",
        vm.dump_jit_info()
    );
    assert!(vm.dump_jit_info().contains("interpreter fallbacks: 0"));
}

#[test]
fn trace_jit_links_finite_mutual_recursion_without_interpreter_handoff() {
    if !native_jit_supported() {
        return;
    }
    let source = r#"
        fn even(value: int) -> int {
            if value == 0 => { 1 } else => { odd(value - 1) }
        }
        fn odd(value: int) -> int {
            if value == 0 => { 0 } else => { even(value - 1) }
        }
        let mut i = 0;
        let mut total = 0;
        while i < 8 {
            total = total + even(8);
            i = i + 1;
        }
        total;
    "#;
    let compiled = compile_source(source).expect("mutual recursion should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });

    assert_eq!(
        vm.run().expect("mutual recursion should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(8)]);
    assert!(
        vm.jit_native_link_handoff_count() > 0,
        "expected recursive native handoffs: {}",
        vm.dump_jit_info()
    );
    assert!(vm.dump_jit_info().contains("interpreter fallbacks: 0"));
}
