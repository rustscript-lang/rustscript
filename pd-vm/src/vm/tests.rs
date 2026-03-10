use super::*;
use crate::builtins::BuiltinFunction;
use std::sync::{Arc, Mutex, OnceLock};

fn native_cache_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_resets_when_program_changes() {
    let _guard = native_cache_test_lock()
        .lock()
        .expect("native cache test lock should succeed");
    jit::runtime::clear_native_trace_cache_for_tests();

    let source_one = r#"
        let mut i = 0;
        while i < 8 {
            i = i + 1;
        }
        let mut j = 0;
        while j < 8 {
            j = j + 1;
        }
        i + j;
    "#;
    let source_two = r#"
        let mut k = 0;
        while k < 8 {
            k = k + 1;
        }
        k;
    "#;

    let compiled_one = crate::compile_source(source_one).expect("source one should compile");
    let compiled_two = crate::compile_source(source_two).expect("source two should compile");

    let mut vm_one = Vm::new(compiled_one.program);
    vm_one.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status_one = vm_one.run().expect("first vm should run");
    assert_eq!(status_one, VmStatus::Halted);
    let vm_one_trace_count = vm_one.jit_native_trace_count();
    assert!(
        vm_one_trace_count > 0,
        "first vm should produce native traces"
    );

    let (cache_program_after_one, cache_entries_after_one) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_one,
        Some(vm_one.program_cache_key),
        "cache should be keyed to first program after first run"
    );
    assert_eq!(
        cache_entries_after_one, vm_one_trace_count,
        "cache entry count should match first program traces"
    );

    let mut vm_two = Vm::new(compiled_two.program);
    vm_two.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_ne!(
        vm_one.program_cache_key, vm_two.program_cache_key,
        "test programs should have different cache keys"
    );
    let status_two = vm_two.run().expect("second vm should run");
    assert_eq!(status_two, VmStatus::Halted);
    let vm_two_trace_count = vm_two.jit_native_trace_count();
    assert!(
        vm_two_trace_count > 0,
        "second vm should produce native traces"
    );

    let (cache_program_after_two, cache_entries_after_two) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_two,
        Some(vm_two.program_cache_key),
        "cache should switch to second program key"
    );
    assert_eq!(
        cache_entries_after_two, vm_two_trace_count,
        "cache should only contain traces from the active program"
    );
}

#[test]
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_reuses_entries_for_same_program() {
    let _guard = native_cache_test_lock()
        .lock()
        .expect("native cache test lock should succeed");
    jit::runtime::clear_native_trace_cache_for_tests();

    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 10 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;
    let compiled = crate::compile_source(source).expect("source should compile");

    let mut vm_one = Vm::new(compiled.program.clone());
    vm_one.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status_one = vm_one.run().expect("first vm should run");
    assert_eq!(status_one, VmStatus::Halted);
    let vm_one_trace_count = vm_one.jit_native_trace_count();
    assert!(
        vm_one_trace_count > 0,
        "first vm should produce native traces"
    );

    let (cache_program_after_one, cache_entries_after_one) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_one,
        Some(vm_one.program_cache_key),
        "cache should be keyed to the first program"
    );
    assert_eq!(
        cache_entries_after_one, vm_one_trace_count,
        "cache entry count should match first vm traces"
    );

    let mut vm_two = Vm::new(compiled.program);
    vm_two.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm_two.program_cache_key, vm_one.program_cache_key,
        "same program should use identical cache key"
    );

    let status_two = vm_two.run().expect("second vm should run");
    assert_eq!(status_two, VmStatus::Halted);
    let vm_two_trace_count = vm_two.jit_native_trace_count();
    assert_eq!(
        vm_two_trace_count, vm_one_trace_count,
        "same program should compile same native trace count"
    );

    let (cache_program_after_two, cache_entries_after_two) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_two,
        Some(vm_two.program_cache_key),
        "cache key should remain the same for identical program"
    );
    assert_eq!(
        cache_entries_after_two, cache_entries_after_one,
        "cache entries should be reused instead of duplicated"
    );
}

fn step_once(vm: &mut Vm) -> VmResult<ExecOutcome> {
    let opcode = vm.read_u8()?;
    vm.execute_interpreter_instruction(opcode)
}

fn assert_shared_heap_backing(lhs: &Value, rhs: &Value) {
    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared string backing");
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared array backing");
        }
        (Value::Map(lhs), Value::Map(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared map backing");
        }
        _ => panic!("expected matching heap values, got lhs={lhs:?} rhs={rhs:?}"),
    }
}

#[test]
fn interpreter_ldc_shares_string_constant_backing() {
    let program = Program::new(
        vec![Value::string("shared")],
        vec![OpCode::Ldc as u8, 0, 0, 0, 0, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);

    let outcome = step_once(&mut vm).expect("ldc should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    let constant = vm
        .program
        .constants
        .first()
        .expect("program should keep a constant");
    assert_shared_heap_backing(constant, &vm.stack()[0]);
}

#[test]
fn interpreter_dup_shares_array_backing() {
    let program = Program::new(vec![], vec![OpCode::Dup as u8, OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    vm.stack
        .push(Value::array(vec![Value::Int(1), Value::Int(2)]));

    let outcome = step_once(&mut vm).expect("dup should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.stack().len(), 2);
    assert_shared_heap_backing(&vm.stack()[0], &vm.stack()[1]);
}

#[test]
fn shared_string_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.locals[0] = Value::string("alive");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(5)]);
}

#[test]
fn shared_array_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.locals[0] = Value::array(vec![Value::Int(1), Value::Int(2)]);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn shared_map_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Count.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.locals[0] = Value::map(vec![(Value::string("k"), Value::Int(9))]);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(1)]);
}

#[test]
fn interpreter_fuses_ldloc_dup_stloc_same_slot_without_fuel() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    let map_value = Value::map(vec![(Value::string("k"), Value::Int(9))]);
    vm.locals[0] = map_value.clone();

    let outcome = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.ip, 5, "fusion should skip dup+stloc bytes");
    assert_eq!(vm.locals[0], map_value, "local value should remain in slot");
    assert_eq!(
        vm.stack(),
        &[map_value],
        "stack should receive copied value"
    );
    assert_shared_heap_backing(&vm.locals[0], &vm.stack()[0]);
    assert_eq!(
        vm.drop_contract_event_count(),
        0,
        "copy fusion should not synthesize drop events"
    );

    let halted = step_once(&mut vm).expect("ret should execute");
    assert!(matches!(halted, ExecOutcome::Halted));
}

#[test]
fn interpreter_does_not_fuse_ldloc_dup_stloc_when_fuel_enabled() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.locals[0] = Value::Int(42);
    vm.set_fuel(32);

    let ldloc = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(ldloc, ExecOutcome::Continue));
    assert_eq!(vm.ip, 2, "fuel metering path must not skip dup+stloc");
    assert_eq!(
        vm.locals[0],
        Value::Null,
        "ldloc move should clear local slot"
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    let dup = step_once(&mut vm).expect("dup should execute");
    assert!(matches!(dup, ExecOutcome::Continue));
    assert_eq!(vm.ip, 3);
    assert_eq!(vm.stack(), &[Value::Int(42), Value::Int(42)]);

    let stloc = step_once(&mut vm).expect("stloc should execute");
    assert!(matches!(stloc, ExecOutcome::Continue));
    assert_eq!(vm.ip, 5);
    assert_eq!(vm.locals[0], Value::Int(42));
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn interpreter_copy_like_ldloc_dup_stloc_shares_map_backing_with_fuel() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.locals[0] = Value::map(vec![(Value::string("k"), Value::Int(9))]);
    vm.set_fuel(32);

    let _ = step_once(&mut vm).expect("ldloc should execute");
    let _ = step_once(&mut vm).expect("dup should execute");
    let _ = step_once(&mut vm).expect("stloc should execute");

    assert_eq!(vm.stack().len(), 1);
    assert_shared_heap_backing(&vm.locals[0], &vm.stack()[0]);
}

#[test]
fn interpreter_does_not_fuse_ldloc_dup_stloc_when_epoch_enabled() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(program);
    vm.locals[0] = Value::Int(42);
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");

    let ldloc = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(ldloc, ExecOutcome::Continue));
    assert_eq!(vm.ip, 2, "epoch interruption must not skip dup+stloc");
    assert_eq!(vm.locals[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn interpreter_fuses_call_ret_without_fuel() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.stack.push(Value::string("tail"));

    let outcome = step_once(&mut vm).expect("call should execute");
    assert!(matches!(outcome, ExecOutcome::Halted));
    assert_eq!(vm.ip, 5, "tail-call fusion should consume trailing ret");
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn interpreter_fuses_call_ret_when_fuel_enabled_if_tail_tick_available() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(1);
    vm.stack.push(Value::string("tail"));

    // `step_once` bypasses the outer run-loop pre-tick, so this fuel only covers fused `ret`.
    let call = step_once(&mut vm).expect("call should execute");
    assert!(matches!(call, ExecOutcome::Halted));
    assert_eq!(vm.ip, 5, "tail-call fusion should consume trailing ret");
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(vm.get_fuel(), Some(0));
}

#[test]
fn interpreter_call_ret_fusion_preserves_ip_when_tail_tick_exhausted() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(0);
    vm.stack.push(Value::string("tail"));

    let err = match step_once(&mut vm) {
        Ok(_) => panic!("tail tick should fail with out-of-fuel"),
        Err(err) => err,
    };
    assert!(matches!(err, VmError::OutOfFuel { .. }));
    assert_eq!(
        vm.ip, 4,
        "ret must remain pending when tail tick cannot be charged"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn interpreter_call_ret_fusion_preserves_ip_when_epoch_deadline_is_reached() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");
    vm.stack.push(Value::string("tail"));

    let err = match step_once(&mut vm) {
        Ok(_) => panic!("tail tick should fail with epoch deadline reached"),
        Err(err) => err,
    };
    assert!(matches!(err, VmError::EpochDeadlineReached { .. }));
    assert_eq!(
        vm.ip, 4,
        "ret must remain pending when the epoch check trips during fused tail execution"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn run_consumes_two_ticks_for_call_ret_when_fuel_enabled() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(2);
    vm.stack.push(Value::string("tail"));

    let status = vm.run().expect("run should complete");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(
        vm.get_fuel(),
        Some(0),
        "call+ret should spend two ticks with fuel metering enabled"
    );
}

#[test]
fn run_yields_before_ret_in_call_ret_sequence_when_out_of_fuel() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_fuel(1);
    vm.stack.push(Value::string("tail"));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.ip, 4,
        "fuel exhaustion should happen before trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(vm.get_fuel(), Some(0));

    vm.add_fuel(1).expect("recharging fuel should succeed");
    let resumed = vm.resume().expect("resume should execute trailing ret");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn run_yields_before_ret_in_call_ret_sequence_when_epoch_deadline_is_reached() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::new(program);
    vm.set_epoch_check_interval(2)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    assert_eq!(vm.increment_epoch(), 1);
    vm.stack.push(Value::string("tail"));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.ip, 4,
        "epoch interruption should happen before trailing ret"
    );
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Epoch));
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let resumed = vm
        .resume()
        .expect("resume should auto re-arm the epoch deadline and execute trailing ret");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn call_ret_fusion_pattern_requires_immediate_ret() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let with_ret = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm_with_ret = Vm::new(with_ret);
    vm_with_ret.ip = 4;
    assert!(vm_with_ret.can_fuse_call_ret_pattern());

    let wrong_next = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Nop as u8],
    );
    let mut vm_wrong_next = Vm::new(wrong_next);
    vm_wrong_next.ip = 4;
    assert!(!vm_wrong_next.can_fuse_call_ret_pattern());

    let no_next = Program::new(vec![], vec![OpCode::Call as u8, call_lo, call_hi, 1]);
    let mut vm_no_next = Vm::new(no_next);
    vm_no_next.ip = 4;
    assert!(!vm_no_next.can_fuse_call_ret_pattern());
}

#[test]
fn ldloc_copy_pattern_match_is_strict_to_dup_stloc_same_slot() {
    let base = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::new(base);
    vm.ip = 2;
    assert!(vm.can_fuse_ldloc_copy_pattern(0));

    let mismatch_slot = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2);
    let mut vm_slot_mismatch = Vm::new(mismatch_slot);
    vm_slot_mismatch.ip = 2;
    assert!(!vm_slot_mismatch.can_fuse_ldloc_copy_pattern(0));

    let wrong_middle = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Pop as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm_wrong_middle = Vm::new(wrong_middle);
    vm_wrong_middle.ip = 2;
    assert!(!vm_wrong_middle.can_fuse_ldloc_copy_pattern(0));
}
