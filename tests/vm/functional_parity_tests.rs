#[path = "../common/mod.rs"]
mod common;
use common::*;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vm::JitConfig;

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

fn run_halted_vm_with_flavor(source: &str, flavor: SourceFlavor, jit_config: JitConfig) -> Vm {
    let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(jit_config);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    vm
}

#[test]
fn interpreter_and_jit_match_for_loop_branch_arithmetic_program() {
    let source = r#"
        let mut i = 1;
        let mut acc = 0;
        while i < 80 {
            let v = i * 3;
            if (v % 5) == 0 {
                acc = acc + (v / 2);
            } else {
                acc = acc + (v * 2);
            }
            i = i + 1;
        }
        acc;
    "#;

    let interpreted = run_halted_vm_with_flavor(
        source,
        SourceFlavor::RustScript,
        JitConfig {
            enabled: false,
            hot_loop_threshold: 8,
            max_trace_len: 256,
        },
    );
    let interpreted_stack = interpreted.stack().to_vec();

    let jitted = run_halted_vm_with_flavor(
        source,
        SourceFlavor::RustScript,
        JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );

    assert_eq!(jitted.stack(), interpreted_stack.as_slice());
    if native_jit_supported() {
        let snapshot = jitted.jit_snapshot();
        assert!(
            snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.result.is_ok()),
            "expected at least one successful trace compile, dump:\n{}",
            jitted.dump_jit_info()
        );
        assert!(
            !snapshot.traces.is_empty(),
            "expected at least one recorded trace, dump:\n{}",
            jitted.dump_jit_info()
        );
        assert!(
            jitted.jit_native_exec_count() > 0,
            "expected native trace execution count to increase"
        );
    }
}

struct YieldThenOne {
    yielded_once: bool,
    return_count: Arc<AtomicUsize>,
}

impl HostFunction for YieldThenOne {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        if !self.yielded_once {
            self.yielded_once = true;
            return Ok(CallOutcome::Yield);
        }
        self.return_count.fetch_add(1, Ordering::Relaxed);
        Ok(CallOutcome::Return(vec![Value::Int(1)].into()))
    }
}

#[test]
fn jit_handles_yielding_host_calls_without_replaying_extra_returns() {
    let source = r#"
        fn tick();
        let mut i = 0;
        let mut sum = 0;
        while i < 8 {
            sum = sum + tick();
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let return_count = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    for func in &compiled.functions {
        match func.name.as_str() {
            "tick" => {
                let _ = vm.register_function(Box::new(YieldThenOne {
                    yielded_once: false,
                    return_count: Arc::clone(&return_count),
                }));
            }
            _ => panic!("unexpected function {}", func.name),
        }
    }

    let first = vm.run().expect("first run should yield");
    assert_eq!(first, VmStatus::Yielded);
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(8)]);
    assert_eq!(return_count.load(Ordering::Relaxed), 8);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.result.is_ok()),
            "expected at least one successful trace compile, dump:\n{}",
            vm.dump_jit_info()
        );
        assert!(
            snapshot.traces.iter().any(|trace| trace.has_yielding_call),
            "expected a trace containing yielding host call metadata, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

struct PendingOnceThenAddOne {
    pending_emitted: bool,
    call_count: Arc<AtomicUsize>,
}

impl HostFunction for PendingOnceThenAddOne {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let value = match args {
            [Value::Int(value)] => *value,
            _ => return Err(vm::VmError::TypeMismatch("int")),
        };
        if !self.pending_emitted {
            self.pending_emitted = true;
            return Ok(CallOutcome::Pending(4242));
        }
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)].into()))
    }
}

#[test]
fn jit_pending_host_call_waits_and_resumes_without_replay() {
    let source = r#"
        fn maybe_wait(x);
        let mut i = 0;
        let mut sum = 0;
        while i < 5 {
            sum = sum + maybe_wait(i);
            i = i + 1;
        }
        sum;
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    let call_count = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    for func in &compiled.functions {
        match func.name.as_str() {
            "maybe_wait" => {
                let _ = vm.register_function(Box::new(PendingOnceThenAddOne {
                    pending_emitted: false,
                    call_count: Arc::clone(&call_count),
                }));
            }
            _ => panic!("unexpected function {}", func.name),
        }
    }

    let mut status = vm.run().expect("first run should start");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume after yield should succeed");
            }
            VmStatus::Waiting(op_id) => {
                assert_eq!(op_id, 4242);
                vm.complete_host_op(op_id, vec![Value::Int(1)])
                    .expect("pending host op completion should succeed");
                status = vm.resume().expect("resume after pending should succeed");
            }
        }
    }

    assert_eq!(vm.stack(), &[Value::Int(15)]);
    assert_eq!(
        call_count.load(Ordering::Relaxed),
        5,
        "pending host call should not be replayed after completion"
    );
}

fn builtin_exists_override(_vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    Ok(CallOutcome::Return(vec![Value::Bool(true)].into()))
}

#[test]
fn jit_uses_interpreter_trace_path_when_builtin_override_is_bound() {
    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 32 {
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
    vm.bind_builtin_static_override("io::exists", builtin_exists_override)
        .expect("builtin override should bind");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(496)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.is_empty(),
            "builtin overrides should disable SSA trace recording entirely, dump:\n{}",
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
