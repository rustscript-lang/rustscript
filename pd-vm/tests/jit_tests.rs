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

struct PrintNoReturn;

impl HostFunction for PrintNoReturn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![]))
    }
}

#[test]
fn trace_jit_compiles_hot_loop_and_is_dumpable() {
    let source = r#"
        let i = 0;
        let sum = 0;
        while i < 20 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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
fn compiler_uses_shl_for_power_of_two_multiply_and_jit_accepts_it() {
    let source = r#"
        let i = 0;
        let sum = 0;
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

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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
        let i = 1;
        let sum = 0;
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

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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
        let i = 0;
        while i < 4 {
            print(i);
            i = i + 1;
        }
        i;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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
        let i = 0;
        let sum = 0;
        while i < 3 {
            let j = 0;
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
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
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
