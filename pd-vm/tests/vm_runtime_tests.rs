#![cfg(feature = "runtime")]
mod common;
use common::*;
use vm::OpCode;

#[test]
fn arithmetic_works() {
    let constants = vec![Value::Int(2), Value::Int(3)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.add();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(5)]);
}

#[test]
fn shift_ops_and_msil_literals_work() {
    let source = r#"
        ldc 3
        ldc 2
        shl
        ldc 1
        shr
        ret
    "#;

    let program = assemble(source).expect("assemble should succeed");
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);
}

#[test]
fn arithmetic_supports_float_and_mixed_numeric() {
    let constants = vec![Value::Float(1.5), Value::Int(2), Value::Float(8.0)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.add();
    bc.ldc(2);
    bc.clt();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn brfalse_skips_block() {
    let constants = vec![Value::Bool(false), Value::Int(1), Value::Int(2)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.brfalse(16);
    bc.ldc(1);
    bc.ret();
    bc.ldc(2);
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn call_can_yield_and_resume() {
    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();

    let program = Program::new(Vec::new(), bc.finish());
    let mut vm = Vm::new(program);
    vm.register_function(Box::new(YieldOnce { yielded: false }));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);

    let status = vm.resume().expect("resume should halt");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn call_can_wait_for_host_op_and_resume_without_replay() {
    struct PendingOnce {
        called: bool,
    }

    impl HostFunction for PendingOnce {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            if self.called {
                return Err(vm::VmError::HostError(
                    "pending host should not be replayed".to_string(),
                ));
            }
            self.called = true;
            Ok(CallOutcome::Pending(99))
        }
    }

    let mut bc = BytecodeBuilder::new();
    bc.call(0, 0);
    bc.ret();
    let program = Program::new(Vec::new(), bc.finish());

    let mut vm = Vm::new(program);
    vm.register_function(Box::new(PendingOnce { called: false }));

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(99));

    vm.complete_host_op(99, vec![Value::Int(7)])
        .expect("host op completion should succeed");
    let resumed = vm.resume().expect("resume should halt after completion");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

#[test]
fn namespaced_builtin_io_call_can_be_overridden_by_host_binding() {
    struct ExistsOverride;

    impl HostFunction for ExistsOverride {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            assert_eq!(args, &[Value::String("request_body".to_string())]);
            Ok(CallOutcome::Return(vec![Value::Bool(false)]))
        }
    }

    let compiled = compile_source(
        r#"
        use io;
        io::exists("request_body");
    "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("io::exists", Box::new(ExistsOverride));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(false)]);
}

#[test]
fn namespaced_builtin_json_encode_call_can_be_overridden_by_host_binding() {
    struct JsonEncodeOverride;

    impl HostFunction for JsonEncodeOverride {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            assert_eq!(args, &[Value::String("request_body".to_string())]);
            Ok(CallOutcome::Return(vec![Value::String(
                "\"override\"".to_string(),
            )]))
        }
    }

    let compiled = compile_source(
        r#"
        use json;
        json::encode("request_body");
    "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("json::encode", Box::new(JsonEncodeOverride));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("\"override\"".to_string())]);
}

#[test]
fn runtime_sleep_host_import_is_available_by_default() {
    let compiled = compile_source(
        r#"
        use runtime;
        runtime::sleep(0);
    "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn runtime_sleep_host_import_can_be_overridden_by_host_binding() {
    struct RuntimeSleepOverride;

    impl HostFunction for RuntimeSleepOverride {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            assert_eq!(args, &[Value::Int(3)]);
            Ok(CallOutcome::Return(vec![Value::Int(7)]))
        }
    }

    let compiled = compile_source(
        r#"
        use runtime;
        runtime::sleep(3);
    "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    vm.bind_function("runtime::sleep", Box::new(RuntimeSleepOverride));

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

#[test]
fn host_function_registry_includes_default_runtime_sleep() {
    let compiled = compile_source(
        r#"
        use runtime;
        runtime::sleep(0);
    "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    let mut registry = HostFunctionRegistry::new();
    registry
        .bind_vm_cached(&mut vm)
        .expect("registry should bind runtime::sleep");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn json_encode_rejects_non_string_map_keys() {
    let compiled = compile_source(
        r#"
        use json;
        let payload = { 1: "one" };
        json::encode(payload);
    "#,
    )
    .expect("source should compile");

    let mut vm = Vm::new(compiled.program);
    let err = vm
        .run()
        .expect_err("json::encode should reject non-string map keys");
    match err {
        vm::VmError::HostError(message) => {
            assert!(message.contains("map keys must be strings"), "{message}");
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn json_encode_rejects_duplicate_map_keys() {
    const BUILTIN_JSON_ENCODE: u16 = 0xFFF7;

    let duplicate_map = Value::Map(vec![
        (Value::String("k".to_string()), Value::Int(1)),
        (Value::String("k".to_string()), Value::Int(2)),
    ]);
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(BUILTIN_JSON_ENCODE, 1);
    bc.ret();

    let mut vm = Vm::new(Program::new(vec![duplicate_map], bc.finish()));
    let err = vm
        .run()
        .expect_err("json::encode should reject duplicate map keys");
    match err {
        vm::VmError::HostError(message) => {
            assert!(message.contains("duplicate key 'k'"), "{message}");
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn json_decode_rejects_duplicate_object_keys() {
    let compiled = compile_source(
        r#"
        use json;
        json::decode("{\"k\":1,\"k\":2}");
    "#,
    )
    .expect("source should compile");

    let mut vm = Vm::new(compiled.program);
    let err = vm
        .run()
        .expect_err("json::decode should reject duplicate object keys");
    match err {
        vm::VmError::HostError(message) => {
            assert!(message.contains("duplicate object key 'k'"), "{message}");
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn bind_builtin_override_rejects_unknown_namespaced_builtin() {
    struct Dummy;

    impl HostFunction for Dummy {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
            Ok(CallOutcome::Return(vec![]))
        }
    }

    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let err = vm
        .bind_builtin_override("io::not_real", Box::new(Dummy))
        .expect_err("unknown builtin override name should fail");
    assert!(matches!(err, vm::VmError::HostError(_)));
}

#[test]
fn assembler_resolves_labels() {
    let mut asm = Assembler::new();
    asm.push_const(Value::Bool(false));
    asm.brfalse_label("target");
    asm.push_const(Value::Int(1));
    asm.ret();
    asm.label("target").expect("label should register");
    asm.push_const(Value::Int(2));
    asm.ret();

    let program = asm.finish_program().expect("assembler should finish");
    let mut vm = Vm::new(program);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn assemble_text_program() {
    let source = r#"
        ldc 2
        ldc 3
        add
        ret
    "#;

    let program = assemble(source).expect("assemble should succeed");
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(5)]);
}

#[test]
fn assemble_text_with_labels() {
    let source = r#"
        ldc false
        brfalse target
        ldc 1
        ret
        .label target
        ldc 2
        ret
    "#;

    let program = assemble(source).expect("assemble should succeed");
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn assemble_text_with_data_and_string() {
    let source = r#"
        .data
        string greeting "hello"
        .code
        ldc greeting
        ret
    "#;

    let program = assemble(source).expect("assemble should succeed");
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::String("hello".to_string())]);
}

#[test]
fn assemble_rejects_legacy_opcode_literals() {
    let source = r#"
        const 1
        halt
    "#;
    let err = assemble(source).expect_err("legacy opcodes should be rejected");
    assert!(err.message.contains("unknown opcode"));
}

#[test]
fn fuel_budget_exhausts_and_recharge_allows_resume() {
    let constants = vec![Value::Int(9)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.pop();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    vm.set_fuel(2);

    let status = vm
        .run()
        .expect("run should cooperatively yield once fuel reaches zero");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(vm.get_fuel(), Some(0));

    vm.recharge_fuel(1).expect("recharge should succeed");
    let status = vm.run().expect("run should halt after recharge");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.get_fuel(), Some(0));
}

#[test]
fn fuel_checkpoint_and_restore_work() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    vm.set_fuel(10);
    let checkpoint = vm.fuel_checkpoint();

    vm.consume_fuel(4)
        .expect("manual fuel consumption should succeed");
    assert_eq!(vm.get_fuel(), Some(6));

    vm.restore_fuel(checkpoint);
    assert_eq!(vm.get_fuel(), Some(10));
}

#[test]
fn consume_fuel_tick_advances_checkpointed_metering() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    vm.set_fuel_check_interval(3)
        .expect("interval update should succeed");
    vm.set_fuel(6);

    let checkpoint = vm.fuel_checkpoint();
    vm.consume_fuel_tick()
        .expect("first tick should only advance coarse-grained debt");
    assert_eq!(vm.get_fuel(), Some(5));

    vm.consume_fuel_tick()
        .expect("second tick should only advance coarse-grained debt");
    assert_eq!(vm.get_fuel(), Some(4));

    vm.restore_fuel(checkpoint);
    assert_eq!(vm.get_fuel(), Some(6));

    vm.consume_fuel_tick()
        .expect("restored checkpoint should still advance one tick");
    assert_eq!(vm.get_fuel(), Some(5));
}

#[test]
fn store_api_exposes_fuel_checkpoint_and_recharge() {
    let constants = vec![Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut store = Store::new(Vm::new(program), String::from("ctx"));
    store.set_fuel(1);
    let checkpoint = store.checkpoint();

    let status = store
        .run()
        .expect("first run should cooperatively yield when fuel is depleted");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(store.get_fuel(), Some(0));

    store.recharge(1).expect("store recharge should succeed");
    let status = store.run().expect("store run should finish after recharge");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(store.data(), "ctx");

    store.restore_checkpoint(checkpoint);
    assert_eq!(store.get_fuel(), Some(1));
}

#[test]
fn fuel_check_interval_can_be_configured() {
    let constants = vec![Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.pop();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    vm.set_fuel_check_interval(3)
        .expect("interval update should succeed");
    vm.set_fuel(3);

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.get_fuel(), Some(0));
}

#[test]
fn coarse_fuel_checking_trades_precision_for_overhead() {
    let constants = vec![Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.pop();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    vm.set_fuel_check_interval(3)
        .expect("interval update should succeed");
    vm.set_fuel(2);

    let status = vm
        .run()
        .expect("vm should cooperatively yield on batched fuel charge");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(vm.get_fuel(), Some(0));

    vm.recharge_fuel(1).expect("recharge should succeed");
    let resumed = vm.run().expect("run should halt after recharge");
    assert_eq!(resumed, VmStatus::Halted);
}

#[test]
fn fuel_check_interval_zero_is_rejected() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let err = vm
        .set_fuel_check_interval(0)
        .expect_err("zero interval should fail");
    assert!(matches!(err, vm::VmError::InvalidFuelCheckInterval(0)));
}

#[test]
fn fuel_checkpoint_restores_interval() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    vm.set_fuel_check_interval(7)
        .expect("interval update should succeed");
    vm.set_fuel(22);
    let checkpoint = vm.checkpoint();

    vm.set_fuel_check_interval(2)
        .expect("interval update should succeed");
    vm.consume_fuel(5)
        .expect("manual fuel consumption should succeed");
    assert_eq!(vm.fuel_check_interval(), 2);

    vm.restore_checkpoint(checkpoint);
    assert_eq!(vm.fuel_check_interval(), 7);
    assert_eq!(vm.get_fuel(), Some(22));
}

#[test]
fn epoch_deadline_exhausts_and_auto_rearm_allows_resume() {
    let constants = vec![Value::Int(9)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.pop();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    assert_eq!(vm.increment_epoch(), 1);

    let status = vm
        .run()
        .expect("run should cooperatively yield once the epoch reaches the deadline");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));

    let status = vm.run().expect("run should halt after auto re-arming");
    assert_eq!(status, VmStatus::Halted);
}

#[test]
fn epoch_checkpoint_and_restore_work() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    assert_eq!(vm.increment_epoch_by(10), 10);
    vm.set_epoch_deadline(5)
        .expect("setting epoch deadline should succeed");
    let checkpoint = vm.epoch_checkpoint();

    assert_eq!(vm.increment_epoch_by(3), 13);
    vm.set_epoch_deadline(1)
        .expect("updating epoch deadline should succeed");
    assert_eq!(vm.epoch_deadline(), Some(14));

    vm.restore_epoch(checkpoint);
    assert_eq!(vm.epoch_deadline(), Some(15));
}

#[test]
fn store_api_exposes_epoch_checkpoint_and_deadline() {
    let constants = vec![Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut store = Store::new(Vm::new(program), String::from("ctx"));
    store
        .set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    let checkpoint = store.epoch_checkpoint();
    assert_eq!(store.increment_epoch(), 1);

    let status = store
        .run()
        .expect("first run should cooperatively yield when the epoch reaches the deadline");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(store.vm().last_yield_reason(), Some(vm::VmYieldReason::Epoch));

    let status = store
        .run()
        .expect("store run should finish after auto re-arming");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(store.data(), "ctx");

    store.restore_epoch(checkpoint);
    assert_eq!(store.epoch_deadline(), Some(1));
}

#[test]
fn epoch_check_interval_can_be_configured() {
    let constants = vec![Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.pop();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    vm.set_epoch_check_interval(3)
        .expect("interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    assert_eq!(vm.increment_epoch(), 1);

    let status = vm
        .run()
        .expect("vm should cooperatively yield on the coarse epoch checkpoint");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));

    let resumed = vm.run().expect("run should halt after auto re-arming");
    assert_eq!(resumed, VmStatus::Halted);
}

#[test]
fn epoch_deadline_zero_auto_rearms_without_manual_reconfiguration() {
    let constants = vec![Value::Int(7)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.pop();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");

    let first = vm.run().expect("first run should yield at the expired epoch deadline");
    assert_eq!(first, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));

    let second = vm
        .resume()
        .expect("resume should auto re-arm the same zero-length deadline and yield again");
    assert_eq!(second, VmStatus::Yielded);
    assert_eq!(vm.last_yield_reason(), Some(vm::VmYieldReason::Epoch));

    vm.clear_epoch_deadline();
    let halted = vm.run().expect("run should halt once epoch interruption is cleared");
    assert_eq!(halted, VmStatus::Halted);
}

#[test]
fn epoch_check_interval_zero_is_rejected() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let err = vm
        .set_epoch_check_interval(0)
        .expect_err("zero interval should fail");
    assert!(matches!(err, vm::VmError::InvalidEpochCheckInterval(0)));
}

#[test]
fn epoch_checkpoint_restores_interval() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    assert_eq!(vm.increment_epoch_by(3), 3);
    vm.set_epoch_check_interval(7)
        .expect("interval update should succeed");
    vm.set_epoch_deadline(4)
        .expect("setting epoch deadline should succeed");
    let checkpoint = vm.epoch_checkpoint();

    vm.set_epoch_check_interval(2)
        .expect("interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("updating epoch deadline should succeed");
    assert_eq!(vm.epoch_check_interval(), 2);

    vm.restore_epoch(checkpoint);
    assert_eq!(vm.epoch_check_interval(), 7);
    assert_eq!(vm.epoch_deadline(), Some(7));
}

#[test]
fn float_division_by_zero_produces_signed_infinities() {
    let constants = vec![Value::Float(1.0), Value::Float(0.0), Value::Float(-0.0)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.div();
    bc.ldc(0);
    bc.ldc(2);
    bc.div();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    let [Value::Float(pos_inf), Value::Float(neg_inf)] = vm.stack() else {
        panic!("expected infinities on the stack, got {:?}", vm.stack());
    };
    assert!(pos_inf.is_infinite() && pos_inf.is_sign_positive());
    assert!(neg_inf.is_infinite() && neg_inf.is_sign_negative());
}

#[test]
fn float_modulo_by_zero_produces_nan() {
    let constants = vec![Value::Float(1.0), Value::Float(0.0)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.modulo();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    let [Value::Float(value)] = vm.stack() else {
        panic!("expected NaN on the stack, got {:?}", vm.stack());
    };
    assert!(value.is_nan(), "expected NaN, got {value}");
}

#[test]
fn not_flips_booleans_and_rejects_non_booleans() {
    let bool_program = assemble(
        r#"
        ldc true
        not
        ret
    "#,
    )
    .expect("assemble should succeed");
    let mut bool_vm = Vm::new(bool_program);
    let bool_status = bool_vm.run().expect("boolean not should succeed");
    assert_eq!(bool_status, VmStatus::Halted);
    assert_eq!(bool_vm.stack(), &[Value::Bool(false)]);

    let invalid_program = Program::new(
        vec![Value::Int(0)],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Not as u8,
            OpCode::Ret as u8,
        ],
    );
    let mut invalid_vm = Vm::new(invalid_program);
    let err = invalid_vm.run().expect_err("non-boolean not should fail");
    assert!(matches!(err, vm::VmError::TypeMismatch("bool")));
}

#[test]
fn shift_right_variants_distinguish_arithmetic_and_logical_behavior() {
    let constants = vec![Value::Int(-8), Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.shr();
    bc.ldc(0);
    bc.ldc(1);
    bc.lshr();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(-4), Value::Int(i64::MAX - 3)]);
}

#[test]
fn shift_amount_must_be_between_zero_and_sixty_three() {
    let negative_program = Program::new(
        vec![Value::Int(1), Value::Int(-1)],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Shl as u8,
            OpCode::Ret as u8,
        ],
    );
    let mut negative_vm = Vm::new(negative_program);
    let negative_err = negative_vm.run().expect_err("negative shift should fail");
    assert!(matches!(negative_err, vm::VmError::InvalidShift(-1)));

    let large_program = Program::new(
        vec![Value::Int(1), Value::Int(64)],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Shr as u8,
            OpCode::Ret as u8,
        ],
    );
    let mut large_vm = Vm::new(large_program);
    let large_err = large_vm.run().expect_err("large shift should fail");
    assert!(matches!(large_err, vm::VmError::InvalidShift(64)));
}

#[test]
fn brfalse_rejects_non_boolean_condition() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Brfalse as u8,
            11,
            0,
            0,
            0,
            OpCode::Ret as u8,
            OpCode::Ret as u8,
        ],
    );
    let mut vm = Vm::new(program);
    let err = vm.run().expect_err("brfalse should require a bool");
    assert!(matches!(err, vm::VmError::TypeMismatch("bool")));
}

#[test]
fn nan_is_not_equal_to_itself() {
    let constants = vec![Value::Float(f64::NAN)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.dup();
    bc.ceq();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(false)]);
}

#[test]
fn resume_on_halted_vm_returns_bytecode_bounds() {
    let program = assemble(
        r#"
        ret
    "#,
    )
    .expect("assemble should succeed");
    let mut vm = Vm::new(program);
    let status = vm.run().expect("initial run should halt");
    assert_eq!(status, VmStatus::Halted);

    let err = vm.resume().expect_err("resuming a halted vm should fail");
    assert!(matches!(err, vm::VmError::BytecodeBounds));
}

#[test]
fn map_equality_ignores_entry_order() {
    let constants = vec![
        Value::Map(vec![
            (Value::String("a".to_string()), Value::Int(1)),
            (Value::String("b".to_string()), Value::Int(2)),
        ]),
        Value::Map(vec![
            (Value::String("b".to_string()), Value::Int(2)),
            (Value::String("a".to_string()), Value::Int(1)),
        ]),
    ];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.ceq();
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut vm = Vm::new(program);
    let status = vm.run().expect("vm should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn get_and_set_use_the_first_duplicate_map_entry() {
    const BUILTIN_GET: u16 = 0xFFE6;
    const BUILTIN_SET: u16 = 0xFFE7;

    let map = Value::Map(vec![
        (Value::String("k".to_string()), Value::Int(1)),
        (Value::String("k".to_string()), Value::Int(2)),
        (Value::String("z".to_string()), Value::Int(3)),
    ]);
    let constants = vec![map, Value::String("k".to_string()), Value::Int(9)];

    let mut get_bc = BytecodeBuilder::new();
    get_bc.ldc(0);
    get_bc.ldc(1);
    get_bc.call(BUILTIN_GET, 2);
    get_bc.ret();
    let mut get_vm = Vm::new(Program::new(constants.clone(), get_bc.finish()));
    let get_status = get_vm.run().expect("get should succeed");
    assert_eq!(get_status, VmStatus::Halted);
    assert_eq!(get_vm.stack(), &[Value::Int(1)]);

    let mut set_bc = BytecodeBuilder::new();
    set_bc.ldc(0);
    set_bc.ldc(1);
    set_bc.ldc(2);
    set_bc.call(BUILTIN_SET, 3);
    set_bc.ret();
    let mut set_vm = Vm::new(Program::new(constants, set_bc.finish()));
    let set_status = set_vm.run().expect("set should succeed");
    assert_eq!(set_status, VmStatus::Halted);
    let [Value::Map(entries)] = set_vm.stack() else {
        panic!("expected map result, got {:?}", set_vm.stack());
    };
    assert_eq!(
        entries,
        &vec![
            (Value::String("k".to_string()), Value::Int(9)),
            (Value::String("k".to_string()), Value::Int(2)),
            (Value::String("z".to_string()), Value::Int(3)),
        ]
    );
}

#[test]
fn set_rejects_sparse_array_indexes() {
    const BUILTIN_SET: u16 = 0xFFE7;

    let constants = vec![
        Value::Array(vec![Value::Int(10), Value::Int(20)]),
        Value::Int(4),
        Value::Int(99),
    ];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ldc(1);
    bc.ldc(2);
    bc.call(BUILTIN_SET, 3);
    bc.ret();

    let mut vm = Vm::new(Program::new(constants, bc.finish()));
    let err = vm.run().expect_err("sparse array set should fail");
    match err {
        vm::VmError::HostError(message) => {
            assert!(message.contains("array index 4 out of bounds"), "{message}");
        }
        other => panic!("unexpected vm error: {other}"),
    }
}

#[test]
fn int_div_and_mod_overflow_report_integer_overflow() {
    for (opcode, operation) in [
        (OpCode::Div as u8, "division"),
        (OpCode::Mod as u8, "remainder"),
    ] {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.ldc(1);
        if opcode == OpCode::Div as u8 {
            bc.div();
        } else {
            bc.modulo();
        }
        bc.ret();

        let mut vm = Vm::new(Program::new(
            vec![Value::Int(i64::MIN), Value::Int(-1)],
            bc.finish(),
        ));
        let err = vm.run().expect_err("i64::MIN with -1 should overflow");
        assert!(
            matches!(err, vm::VmError::IntegerOverflow(found) if found == operation),
            "expected integer overflow in {operation}, got {err:?}"
        );
    }
}

#[test]
fn program_new_infers_locals_through_new_zero_operand_opcodes() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Not as u8,
            OpCode::Ldloc as u8,
            5,
            OpCode::Lshr as u8,
            OpCode::Ret as u8,
        ],
    );
    assert_eq!(program.local_count, 6);
}
