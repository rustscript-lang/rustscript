use vm::jit::{TraceBytesCodecKind, TraceConcatKind, TraceStep, TraceTextBytesKind};
use vm::{
    BytecodeBuilder, CallOutcome, HostFunction, JitConfig, JitTraceTerminal, OpCode, Program,
    Value, Vm, VmStatus, VmYieldReason, compile_source, disassemble_program,
};

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
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

struct PrintNoReturn;

impl HostFunction for PrintNoReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![]))
    }
}

struct YieldOnce {
    yielded: bool,
}

impl HostFunction for YieldOnce {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        if self.yielded {
            Ok(CallOutcome::Return(vec![Value::Int(42)]))
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
        bridge_hits.iter().all(|(name, _)| *name == "ldc"),
        "typed aot arithmetic should only fall back for initial stack growth, not math ops: {bridge_hits:?}"
    );
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
fn aot_honors_fuel_metering_and_resume() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 20 {
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);
    vm.set_fuel_check_interval(1)
        .expect("fuel interval update should succeed");
    vm.set_fuel(5);

    let mut yielded = 0u64;
    loop {
        match vm.run().expect("aot run should succeed") {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yielded = yielded.saturating_add(1);
                assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Fuel));
                vm.recharge_fuel(5).expect("recharge should succeed");
            }
            VmStatus::Waiting(op_id) => panic!("unexpected host wait on op {op_id}"),
        }
    }

    assert!(yielded > 0, "expected at least one fuel yield");
    assert_eq!(vm.stack().last(), Some(&Value::Int(20)));
    assert!(
        vm.aot_exec_count() > 1,
        "aot should resume after fuel yields"
    );
}

#[test]
fn aot_honors_epoch_interruption_and_resume() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        let mut i = 0;
        while i < 8 {
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    install_aot(&mut vm);
    vm.set_epoch_check_interval(1)
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
    assert_eq!(vm.stack().last(), Some(&Value::Int(8)));
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
            "expected native trace execution for move-heavy loop, dump:\n{}",
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

    assert!(
        bytes_second < bytes_first,
        "expected fewer injected fuel checks with interval 8; interval=1 bytes={bytes_first}, interval=8 bytes={bytes_second}\nfirst dump:\n{dump_first}\nsecond dump:\n{dump_second}"
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
        "expected native trace execution for move-heavy loop, dump:\n{}",
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

    assert!(
        bytes_second < bytes_first,
        "expected fewer injected epoch checks with interval 8; interval=1 bytes={bytes_first}, interval=8 bytes={bytes_second}\nfirst dump:\n{dump_first}\nsecond dump:\n{dump_second}"
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
            dump.contains(" shl") || dump.contains(" ldloc_shl_imm"),
            "expected trace dump to include shl or fused shl-imm, dump:\n{dump}"
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
        let dump = vm.dump_jit_info();
        assert!(
            dump.contains(" mod") || dump.contains(" mod_imm") || dump.contains(" ldloc_mod_imm"),
            "expected trace dump to include mod or fused mod-imm, dump:\n{dump}"
        );
    }
}

#[test]
fn trace_jit_supports_host_calls_with_native_mixed_mode() {
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
        assert!(
            snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.result.is_ok()),
            "expected at least one successful trace compile, dump:\n{dump}"
        );
        assert!(
            snapshot.traces.iter().any(|trace| trace.has_call),
            "expected at least one call-containing trace, dump:\n{dump}"
        );
        assert!(
            dump.contains(" call"),
            "expected trace dump to include call"
        );
        assert!(
            vm.jit_native_trace_count() > 0,
            "expected call trace to compile to native code"
        );
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected native call trace to execute at least once"
        );
    }
}

#[test]
fn trace_jit_nested_loops_use_branch_exit_segments() {
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
            snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.result.is_ok()),
            "expected successful trace compiles for nested loops, dump:\n{dump}"
        );
        assert!(
            snapshot
                .traces
                .iter()
                .any(|trace| trace.terminal == JitTraceTerminal::BranchExit),
            "expected at least one branch-exit trace for nested loop handoff, dump:\n{dump}"
        );
        assert!(
            snapshot
                .traces
                .iter()
                .any(|trace| trace.terminal == JitTraceTerminal::LoopBack),
            "expected at least one loop-back trace, dump:\n{dump}"
        );
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
        snapshot
            .traces
            .iter()
            .flat_map(|trace| trace.steps.iter())
            .any(|step| matches!(step, TraceStep::IAdd)),
        "expected a typed integer add trace step, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_records_typed_float_and_string_add_steps() {
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
        float_snapshot
            .traces
            .iter()
            .flat_map(|trace| trace.steps.iter())
            .any(|step| {
                matches!(
                    step,
                    TraceStep::FAdd | TraceStep::FAddImm(_) | TraceStep::FLocalAddImm { .. }
                )
            }),
        "expected a typed float add or fused float-add-imm trace step, dump:\n{}",
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
    assert!(
        string_snapshot
            .traces
            .iter()
            .flat_map(|trace| trace.steps.iter())
            .any(|step| { matches!(step, TraceStep::Concat(TraceConcatKind::String)) }),
        "expected a typed string concat trace step, dump:\n{}",
        string_vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_records_typed_bytes_concat_and_builtin_steps() {
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
    let steps = snapshot
        .traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .collect::<Vec<_>>();
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Concat(TraceConcatKind::Bytes))),
        "expected typed bytes concat trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Len(TraceTextBytesKind::Bytes))),
        "expected typed bytes len trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Slice(TraceTextBytesKind::Bytes))),
        "expected typed bytes slice trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Get(TraceTextBytesKind::Bytes))),
        "expected typed bytes get trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| matches!(step, TraceStep::HasBytes)),
        "expected typed bytes has trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::BuiltinCall { .. })),
        "expected bytes::from_hex to stay on the BuiltinCall path, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        !steps
            .iter()
            .any(|step| matches!(step, TraceStep::BytesCodec(TraceBytesCodecKind::FromHex))),
        "expected bytes::from_hex to avoid typed bytes codec lowering, dump:\n{}",
        vm.dump_jit_info()
    );
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
                && trace
                    .steps
                    .iter()
                    .any(|step| matches!(step, TraceStep::GuardTrue { .. }))
        })
        .expect("expected loop trace with join-path guard");
    assert!(
        loop_trace
            .steps
            .iter()
            .any(|step| matches!(step, TraceStep::JumpToRoot)),
        "expected loop trace to continue through the join path, dump:\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_typed_float_math_without_helper_bridge() {
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
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution, dump:\n{}",
        vm.dump_jit_info()
    );

    let snapshot = vm.jit_snapshot();
    let steps = snapshot
        .traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .collect::<Vec<_>>();
    assert!(
        steps.iter().any(|step| {
            matches!(
                step,
                TraceStep::FAdd | TraceStep::FAddImm(_) | TraceStep::FLocalAddImm { .. }
            )
        }),
        "expected float add trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| {
            matches!(
                step,
                TraceStep::FSub | TraceStep::FSubImm(_) | TraceStep::FLocalSubImm { .. }
            )
        }),
        "expected float sub trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| {
            matches!(
                step,
                TraceStep::FMul | TraceStep::FMulImm(_) | TraceStep::FLocalMulImm { .. }
            )
        }),
        "expected float mul trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| {
            matches!(
                step,
                TraceStep::FDiv | TraceStep::FDivImm(_) | TraceStep::FLocalDivImm { .. }
            )
        }),
        "expected float div trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| {
            matches!(
                step,
                TraceStep::FMod | TraceStep::FModImm(_) | TraceStep::FLocalModImm { .. }
            )
        }),
        "expected float mod trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| **step == TraceStep::FNeg),
        "expected float neg trace step, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["add", "sub", "mul", "div", "mod", "neg"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected float op '{bridge_name}' to lower natively, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn trace_jit_executes_typed_string_concat_without_generic_add_bridge() {
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
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution, dump:\n{}",
        vm.dump_jit_info()
    );

    let snapshot = vm.jit_snapshot();
    assert!(
        snapshot
            .traces
            .iter()
            .flat_map(|trace| trace.steps.iter())
            .any(|step| matches!(step, TraceStep::Concat(TraceConcatKind::String))),
        "expected typed string concat trace step, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| *name == "add" && *count > 0),
        "expected string concat to avoid generic add bridge, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| *name == "sconcat" && *count > 0),
        "expected string concat to avoid sconcat helper bridge, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_typed_bytes_concat_without_helper_bridge() {
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
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| *name == "add" && *count > 0),
        "expected bytes concat to avoid generic add bridge, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
    assert!(
        !bridge_hits
            .iter()
            .any(|(name, count)| name.contains("concat") && *count > 0),
        "expected bytes concat to avoid concat helper bridges, bridge hits: {bridge_hits:?}\n{}",
        vm.dump_jit_info()
    );
}

#[test]
fn trace_jit_executes_typed_bytes_sequence_builtins_without_builtin_bridge() {
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
    let steps = snapshot
        .traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .collect::<Vec<_>>();
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Len(TraceTextBytesKind::Bytes))),
        "expected typed bytes len trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Slice(TraceTextBytesKind::Bytes))),
        "expected typed bytes slice trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Get(TraceTextBytesKind::Bytes))),
        "expected typed bytes get trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps.iter().any(|step| matches!(step, TraceStep::HasBytes)),
        "expected typed bytes has trace step, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["len", "slice", "get", "has"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected bytes builtin '{bridge_name}' to avoid builtin helper bridge, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn trace_jit_executes_typed_string_sequence_builtins_without_builtin_bridge() {
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
    let steps = snapshot
        .traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .collect::<Vec<_>>();
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Len(TraceTextBytesKind::String))),
        "expected typed string len trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Slice(TraceTextBytesKind::String))),
        "expected typed string slice trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::Get(TraceTextBytesKind::String))),
        "expected typed string get trace step, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["len", "slice", "get"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected string builtin '{bridge_name}' to avoid builtin helper bridge, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn trace_jit_executes_typed_bytes_array_codec_builtins_without_builtin_bridge() {
    if !native_jit_supported() {
        return;
    }

    let source = r#"
        use bytes;
        let mut i = 0;
        let mut arr = [0];
        let mut payload = bytes::from_array_u8([0]);
        while i < 3 {
            arr = bytes::to_array_u8(bytes::from_array_u8([1, 2, 255]));
            payload = bytes::from_array_u8(arr);
            i = i + 1;
        }
        arr;
        payload;
    "#;

    let compiled = compile_source(source).expect("bytes array codec compile should succeed");
    let mut vm = Vm::new(compiled.program);
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
    let steps = snapshot
        .traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .collect::<Vec<_>>();
    for kind in [
        TraceBytesCodecKind::FromArrayU8,
        TraceBytesCodecKind::ToArrayU8,
    ] {
        assert!(
            steps
                .iter()
                .any(|step| matches!(step, TraceStep::BytesCodec(found) if *found == kind)),
            "expected bytes codec step {kind:?}, dump:\n{}",
            vm.dump_jit_info()
        );
    }

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["bytes_from_array_u8", "bytes_to_array_u8"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected bytes array codec '{bridge_name}' to avoid builtin helper bridge, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn trace_jit_executes_typed_float_comparisons_without_helper_bridge() {
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
    assert!(
        vm.jit_native_exec_count() > 0,
        "expected native execution, dump:\n{}",
        vm.dump_jit_info()
    );

    let snapshot = vm.jit_snapshot();
    let steps = snapshot
        .traces
        .iter()
        .flat_map(|trace| trace.steps.iter())
        .collect::<Vec<_>>();
    assert!(
        steps
            .iter()
            .any(|step| { matches!(step, TraceStep::FClt | TraceStep::FLocalCltImm { .. }) }),
        "expected float less-than trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| { matches!(step, TraceStep::FCgt | TraceStep::FLocalCgtImm { .. }) }),
        "expected float greater-than trace step, dump:\n{}",
        vm.dump_jit_info()
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TraceStep::FCeq | TraceStep::Ceq)),
        "expected float equality trace step, dump:\n{}",
        vm.dump_jit_info()
    );

    let bridge_hits = vm.jit_native_bridge_stats_snapshot();
    for bridge_name in ["clt", "cgt", "ceq"] {
        assert!(
            !bridge_hits
                .iter()
                .any(|(name, count)| *name == bridge_name && *count > 0),
            "expected float compare '{bridge_name}' to lower natively, bridge hits: {bridge_hits:?}\n{}",
            vm.dump_jit_info()
        );
    }
}
