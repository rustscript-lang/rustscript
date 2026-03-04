#![cfg(feature = "runtime")]
mod common;
use common::*;

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

    let err = vm
        .run()
        .expect_err("run should stop once fuel reaches zero");
    assert!(matches!(
        err,
        vm::VmError::OutOfFuel {
            needed: 1,
            remaining: 0
        }
    ));
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
fn store_api_exposes_fuel_checkpoint_and_recharge() {
    let constants = vec![Value::Int(1)];
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.ret();

    let program = Program::new(constants, bc.finish());
    let mut store = Store::new(Vm::new(program), String::from("ctx"));
    store.set_fuel(1);
    let checkpoint = store.checkpoint();

    let err = store
        .run()
        .expect_err("first run should stop when fuel is depleted");
    assert!(matches!(
        err,
        vm::VmError::OutOfFuel {
            needed: 1,
            remaining: 0
        }
    ));
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

    let err = vm.run().expect_err("vm should stop on batched fuel charge");
    assert!(matches!(
        err,
        vm::VmError::OutOfFuel {
            needed: 3,
            remaining: 2
        }
    ));
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
