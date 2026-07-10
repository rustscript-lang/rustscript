use vm::embedded::{
    HostBinding, HostError, Value as EmbeddedValue, Vm as EmbeddedVm, VmError, VmStatus,
    decode_program,
};
use vm::{compile_source_for_repl, encode_program};

#[derive(Default)]
struct BoardState {
    pin: i64,
    high: bool,
}

fn compile_embedded(source: &str) -> vm::embedded::Program {
    let compiled = compile_source_for_repl(source).expect("RustScript source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("compiled program should encode");
    decode_program(&bytes).expect("embedded runtime should decode compiler VMBC")
}

fn gpio_set(
    state: &mut BoardState,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    let [EmbeddedValue::Int(pin), EmbeddedValue::Bool(high)] = args else {
        return Err(HostError::new("gpio_set expects int and bool"));
    };
    state.pin = *pin;
    state.high = *high;
    Ok(None)
}

#[test]
fn static_host_binding_mutates_board_context() {
    let program = compile_embedded(
        r#"
            fn gpio_set(pin: int, high: bool);
            gpio_set(25, true);
        "#,
    );
    let bindings = [HostBinding::new("gpio_set", 2, gpio_set)];
    let mut vm = EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings)
        .expect("host imports should bind");

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.context().pin, 25);
    assert!(vm.context().high);
}

#[test]
fn missing_import_and_arity_mismatch_fail_during_binding() {
    let program = compile_embedded(
        r#"
            fn gpio_set(pin: int, high: bool);
            gpio_set(25, true);
        "#,
    );
    assert!(matches!(
        EmbeddedVm::with_host_bindings(program.clone(), BoardState::default(), &[]),
        Err(VmError::UnboundImport(name)) if name == "gpio_set"
    ));

    let wrong_arity = [HostBinding::new("gpio_set", 1, gpio_set)];
    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &wrong_arity),
        Err(VmError::InvalidCallArity {
            expected: 1,
            got: 2,
            ..
        })
    ));
}

#[test]
fn fuel_can_pause_and_resume_a_finite_loop() {
    let program = compile_embedded(
        r#"
            let mut count = 0;
            while count < 4 {
                count = count + 1;
            }
            count;
        "#,
    );
    let mut vm = EmbeddedVm::new(program);
    vm.set_fuel(4);

    assert_eq!(
        vm.run(),
        Err(VmError::OutOfFuel {
            needed: 1,
            remaining: 0,
        })
    );
    assert_eq!(vm.fuel(), Some(0));

    vm.add_fuel(100).expect("fuel addition should succeed");
    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.stack().last(), Some(&EmbeddedValue::Int(4)));
}
