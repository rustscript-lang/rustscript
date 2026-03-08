#![cfg(feature = "runtime")]
use vm::{
    CallOutcome, HostFunction, JitConfig, JitTraceTerminal, OpCode, Value, Vm, VmStatus,
    compile_source,
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

struct PrintNoReturn;

impl HostFunction for PrintNoReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![]))
    }
}

#[test]
fn aot_compiles_whole_non_loop_program() {
    let source = r#"
        let mut x = 3;
        if x < 2 {
            x = x + 10;
        } else {
            x = x + 1;
        }
        x;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1_000,
        max_trace_len: 512,
    });

    let prepared = vm.prepare_aot().expect("AOT precompile should succeed");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(prepared > 0, "expected at least one AOT-compiled block");
        assert!(
            snapshot.traces.iter().any(|trace| trace.root_ip == 0),
            "expected an AOT block rooted at ip 0"
        );
        assert!(
            vm.jit_native_trace_count() >= prepared,
            "expected native traces for prepared AOT blocks"
        );
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected at least one native AOT execution"
        );
    }
}

#[test]
fn aot_handles_string_equality_paths() {
    let source = r#"
        let lhs = "javascript";
        let rhs = "javascript";
        if lhs == rhs {
            1;
        } else {
            0;
        }
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1_000,
        max_trace_len: 512,
    });

    let prepared = vm.prepare_aot().expect("AOT precompile should succeed");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1)]);

    if native_jit_supported() {
        assert!(prepared > 0, "expected at least one AOT-compiled block");
        assert!(
            vm.jit_native_exec_count() > 0,
            "expected at least one native AOT execution"
        );
    }
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
        assert!(dump.contains(" shl"), "expected trace dump to include shl");
    }
}

#[test]
fn compiler_emits_mod_and_or_and_jit_accepts_them() {
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
    assert!(
        compiled.program.code.contains(&(OpCode::Mod as u8)),
        "expected compiler to emit mod"
    );
    assert!(
        compiled.program.code.contains(&(OpCode::And as u8)),
        "expected compiler to emit and"
    );
    assert!(
        compiled.program.code.contains(&(OpCode::Or as u8)),
        "expected compiler to emit or"
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
        assert!(dump.contains(" mod"), "expected trace dump to include mod");
        assert!(dump.contains(" and"), "expected trace dump to include and");
        assert!(dump.contains(" or"), "expected trace dump to include or");
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
