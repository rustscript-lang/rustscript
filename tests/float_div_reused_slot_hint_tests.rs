#![cfg(feature = "runtime")]

use vm::{OpCode, Value, ValueType, Vm, VmStatus, compile_source, disassemble_program};

fn div_hints(program: &vm::Program) -> Vec<(usize, Option<(ValueType, ValueType)>)> {
    let hints = program
        .type_map
        .as_ref()
        .map(|type_map| &type_map.operand_types);
    let code = &program.code;
    let mut ip = 0usize;
    let mut found = Vec::new();
    while ip < code.len() {
        let start = ip;
        let opcode = code[ip];
        ip += 1;
        let width = match opcode {
            x if x == OpCode::Ldc as u8 || x == OpCode::Br as u8 || x == OpCode::Brfalse as u8 => 4,
            x if x == OpCode::Ldloc as u8
                || x == OpCode::Stloc as u8
                || x == OpCode::CallValue as u8 =>
            {
                1
            }
            x if x == OpCode::Call as u8 => 3,
            x if x == OpCode::CallScript as u8 => 5,
            _ => 0,
        };
        if ip + width > code.len() {
            break;
        }
        if opcode == OpCode::Div as u8 {
            let hint = hints.and_then(|hints| hints.get(&start).copied());
            found.push((start, hint));
        }
        ip += width;
    }
    found
}

fn compile_and_run(
    source: &str,
) -> (
    vm::CompiledProgram,
    Result<VmStatus, vm::VmError>,
    Vec<Value>,
) {
    let compiled = compile_source(source).unwrap_or_else(|error| panic!("{source}\n{error}"));
    let mut machine = Vm::new(compiled.program.clone().with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let status = machine.run();
    let stack = machine.stack().to_vec();
    (compiled, status, stack)
}

/// An Int binding and a later Float binding can share one physical slot once
/// the Float value is dead. Reassigning the Int and dividing it by 1.0 must
/// keep an Int/Float hint and perform numeric division.
#[test]
fn reused_physical_slot_keeps_int_float_div_hint() {
    let source = r#"
        fn produce() -> int { 7 }
        fn probe() -> float {
            let mut checked: int = 0;
            let temporary: float = 1.5;
            let sink = temporary + 0.0;
            let pad0 = 1;
            let pad1 = 2;
            let pad2 = 3;
            let pad3 = 4;
            let pad4 = 5;
            let pad5 = 6;
            checked = produce();
            let mut out = sink;
            if pad0 + pad1 + pad2 + pad3 + pad4 + pad5 == 21 {
                out = checked / 1.0;
            }
            out
        }
        probe();
    "#;
    let (compiled, status, stack) = compile_and_run(source);
    let hints = div_hints(&compiled.program);
    let listing = disassemble_program(&compiled.program);
    assert!(
        hints
            .iter()
            .any(|(_, hint)| *hint == Some((ValueType::Int, ValueType::Float))),
        "expected an Int/Float div hint, got {hints:?}\nlocals={}\n{listing}",
        compiled.locals
    );
    assert!(
        hints
            .iter()
            .all(|(_, hint)| *hint != Some((ValueType::Float, ValueType::Float))),
        "Int local was hinted Float/Float: {hints:?}\nlocals={}\n{listing}",
        compiled.locals
    );
    let status = status.unwrap_or_else(|error| {
        panic!(
            "runtime {error:?}\nhints={hints:?}\nlocals={}\n{listing}",
            compiled.locals
        )
    });
    assert_eq!(status, VmStatus::Halted, "{listing}");
    assert_eq!(stack.last(), Some(&Value::Float(7.0)), "{listing}");
}

fn hint_values(
    hints: &[(usize, Option<(ValueType, ValueType)>)],
) -> Vec<Option<(ValueType, ValueType)>> {
    hints.iter().map(|(_, hint)| *hint).collect()
}

fn assert_div_result(source: &str, expected_hint: (ValueType, ValueType), expected: Value) {
    let (compiled, status, stack) = compile_and_run(source);
    let hints = div_hints(&compiled.program);
    let listing = disassemble_program(&compiled.program);
    assert!(
        hints.iter().any(|(_, hint)| *hint == Some(expected_hint)),
        "expected div hint {expected_hint:?}, got {hints:?}\nlocals={}\n{listing}",
        compiled.locals
    );
    let status = status.unwrap_or_else(|error| {
        panic!(
            "runtime {error:?}\nhints={hints:?}\nlocals={}\n{listing}",
            compiled.locals
        )
    });
    assert_eq!(status, VmStatus::Halted, "{listing}");
    assert_eq!(stack.last(), Some(&expected), "{listing}");
}

#[test]
fn int_float_div_without_slot_reuse_is_numeric() {
    assert_div_result(
        r#"
            fn produce() -> int { 7 }
            fn probe() -> float {
                let checked = produce();
                checked / 1.0
            }
            probe();
        "#,
        (ValueType::Int, ValueType::Float),
        Value::Float(7.0),
    );
}

#[test]
fn reused_physical_slot_keeps_float_float_div_hint() {
    assert_div_result(
        r#"
            fn produce() -> float { 8.0 }
            fn probe() -> float {
                let mut checked: float = 0.0;
                let temporary: int = 7;
                let sink = temporary + 0;
                let pad0 = 1.0;
                let pad1 = 2.0;
                let pad2 = 3.0;
                let pad3 = 4.0;
                let pad4 = 5.0;
                let pad5 = 6.0;
                checked = produce();
                let mut out = sink + pad0;
                if pad1 + pad2 + pad3 + pad4 + pad5 == 20.0 {
                    out = checked / 2.0;
                }
                out
            }
            probe();
        "#,
        (ValueType::Float, ValueType::Float),
        Value::Float(4.0),
    );
}

const NUMBER_DIV_CONTROL: &str = r#"
    fn produce() -> number { 7 }
    fn probe() -> float {
        let checked = produce();
        checked / 1.0
    }
    probe();
"#;

const NUMBER_DIV_REUSED_SLOT: &str = r#"
    fn produce() -> number { 7 }
    fn probe() -> float {
        let mut checked: number = 0;
        let temporary: float = 1.5;
        let sink = temporary + 0.0;
        let pad0 = 1;
        let pad1 = 2;
        let pad2 = 3;
        let pad3 = 4;
        let pad4 = 5;
        let pad5 = 6;
        checked = produce();
        let mut out = sink;
        if pad0 + pad1 + pad2 + pad3 + pad4 + pad5 == 21 {
            out = checked / 1.0;
        }
        out
    }
    probe();
"#;

/// A dead float and a later untyped number binding can share one physical
/// slot. The number is physically an int. Dividing it by 1.0 must keep the
/// same generic hint as a number division that never occupied a float slot.
#[test]
fn reused_physical_slot_number_producer_keeps_generic_div_hint() {
    let (control_compiled, control_status, control_stack) = compile_and_run(NUMBER_DIV_CONTROL);
    let control_hints = div_hints(&control_compiled.program);
    let control_values = hint_values(&control_hints);
    assert!(
        control_values
            .iter()
            .all(|hint| *hint != Some((ValueType::Float, ValueType::Float))),
        "number control was specialized Float/Float: {control_hints:?}"
    );
    let control_status = control_status.unwrap_or_else(|error| panic!("control runtime {error:?}"));
    assert_eq!(control_status, VmStatus::Halted);
    assert_eq!(control_stack.last(), Some(&Value::Float(7.0)));

    let (compiled, status, stack) = compile_and_run(NUMBER_DIV_REUSED_SLOT);
    let hints = div_hints(&compiled.program);
    let listing = disassemble_program(&compiled.program);
    let values = hint_values(&hints);
    assert_eq!(
        values, control_values,
        "expected generic number div hint {control_values:?}, got {hints:?}\n\
         guest={status:?}\nsource={NUMBER_DIV_REUSED_SLOT}\nlocals={}\n{listing}",
        compiled.locals
    );
    assert!(
        hints
            .iter()
            .all(|(_, hint)| *hint != Some((ValueType::Float, ValueType::Float))),
        "stale Float/Float hint on number reuse: {hints:?}\n\
         guest={status:?}\nsource={NUMBER_DIV_REUSED_SLOT}\nlocals={}\n{listing}",
        compiled.locals
    );
    let status = status.unwrap_or_else(|error| {
        panic!(
            "runtime {error:?}\nhints={hints:?}\nlocals={}\nsource={NUMBER_DIV_REUSED_SLOT}\n{listing}",
            compiled.locals
        )
    });
    assert_eq!(status, VmStatus::Halted, "{listing}");
    assert_eq!(stack.last(), Some(&Value::Float(7.0)), "{listing}");
}

/// `let mut n: number = 0; n = 2` is the source form of a declared number
/// binding followed by an int store. The declaration stays number: the div
/// hint matches a direct number binding, not a narrowed Int/Float hint.
#[test]
fn declared_number_reassignment_keeps_number_div_hint() {
    let declared = r#"
        fn probe() -> float {
            let n: number = 2;
            n / 1.0
        }
        probe();
    "#;
    let reassigned = r#"
        fn probe() -> float {
            let mut n: number = 0;
            n = 2;
            n / 1.0
        }
        probe();
    "#;
    let (declared_compiled, declared_status, declared_stack) = compile_and_run(declared);
    let declared_hints = div_hints(&declared_compiled.program);
    let declared_values = hint_values(&declared_hints);
    let declared_status =
        declared_status.unwrap_or_else(|error| panic!("declared runtime {error:?}"));
    assert_eq!(declared_status, VmStatus::Halted);
    assert_eq!(declared_stack.last(), Some(&Value::Float(2.0)));

    let (compiled, status, stack) = compile_and_run(reassigned);
    let hints = div_hints(&compiled.program);
    let listing = disassemble_program(&compiled.program);
    let values = hint_values(&hints);
    assert_eq!(
        values, declared_values,
        "declared number was narrowed: got {hints:?}, direct number hint {declared_values:?}\n\
         guest={status:?}\nsource={reassigned}\nlocals={}\n{listing}",
        compiled.locals
    );
    assert!(
        hints
            .iter()
            .all(|(_, hint)| *hint != Some((ValueType::Int, ValueType::Float))),
        "number declaration narrowed to Int/Float: {hints:?}\n{listing}"
    );
    let status = status.unwrap_or_else(|error| panic!("runtime {error:?}\n{listing}"));
    assert_eq!(status, VmStatus::Halted, "{listing}");
    assert_eq!(stack.last(), Some(&Value::Float(2.0)), "{listing}");
}

/// An untyped parameter is not a concrete Int. After a dead float reuses its
/// slot, dividing the restored value by 1.0 must keep the same generic hint
/// as an untyped division that never occupied a float slot.
#[test]
fn reused_physical_slot_untyped_producer_keeps_generic_div_hint() {
    let control = r#"
        fn probe(value) -> float {
            value / 1.0
        }
        probe(7);
    "#;
    let reused = r#"
        fn probe(value) -> float {
            let mut checked = value;
            let temporary: float = 1.5;
            let sink = temporary + 0.0;
            let pad0 = 1;
            let pad1 = 2;
            let pad2 = 3;
            let pad3 = 4;
            let pad4 = 5;
            let pad5 = 6;
            checked = value;
            let mut out = sink;
            if pad0 + pad1 + pad2 + pad3 + pad4 + pad5 == 21 {
                out = checked / 1.0;
            }
            out
        }
        probe(7);
    "#;
    let (control_compiled, control_status, control_stack) = compile_and_run(control);
    let control_values = hint_values(&div_hints(&control_compiled.program));
    assert!(
        control_values
            .iter()
            .all(|hint| *hint != Some((ValueType::Float, ValueType::Float))),
        "untyped control was specialized Float/Float: {control_values:?}"
    );
    let control_status = control_status.unwrap_or_else(|error| panic!("control runtime {error:?}"));
    assert_eq!(control_status, VmStatus::Halted);
    assert_eq!(control_stack.last(), Some(&Value::Float(7.0)));

    let (compiled, status, stack) = compile_and_run(reused);
    let hints = div_hints(&compiled.program);
    let listing = disassemble_program(&compiled.program);
    let values = hint_values(&hints);
    assert_eq!(
        values, control_values,
        "expected generic untyped div hint {control_values:?}, got {hints:?}\n\
         guest={status:?}\nsource={reused}\nlocals={}\n{listing}",
        compiled.locals
    );
    assert!(
        hints
            .iter()
            .all(|(_, hint)| *hint != Some((ValueType::Float, ValueType::Float))),
        "stale Float/Float hint on untyped reuse: {hints:?}\n\
         guest={status:?}\nsource={reused}\nlocals={}\n{listing}",
        compiled.locals
    );
    let status = status.unwrap_or_else(|error| {
        panic!(
            "runtime {error:?}\nhints={hints:?}\nlocals={}\nsource={reused}\n{listing}",
            compiled.locals
        )
    });
    assert_eq!(status, VmStatus::Halted, "{listing}");
    assert_eq!(stack.last(), Some(&Value::Float(7.0)), "{listing}");
}

const NUMBER_LITERAL_CONTROL: &str = r#"
    fn probe() -> float {
        let n: number = 2;
        n / 1.0
    }
    probe();
"#;

fn assert_reused_number_matches_generic_control(source: &str) {
    let (control_compiled, control_status, control_stack) = compile_and_run(NUMBER_LITERAL_CONTROL);
    let control_values = hint_values(&div_hints(&control_compiled.program));
    assert!(
        control_values
            .iter()
            .all(|hint| *hint != Some((ValueType::Int, ValueType::Float))),
        "number control narrowed to Int/Float: {control_values:?}"
    );
    let control_status = control_status.unwrap_or_else(|error| panic!("control runtime {error:?}"));
    assert_eq!(control_status, VmStatus::Halted);
    assert_eq!(control_stack.last(), Some(&Value::Float(2.0)));

    let (compiled, status, stack) = compile_and_run(source);
    let hints = div_hints(&compiled.program);
    let listing = disassemble_program(&compiled.program);
    assert!(
        compiled.locals > 8,
        "physical reuse requires compaction, locals={}\nsource={source}\n{listing}",
        compiled.locals
    );
    let values = hint_values(&hints);
    assert_eq!(
        values, control_values,
        "nested declared number was narrowed: got {hints:?}, no-reuse number hint {control_values:?}\n\
         guest={status:?}\nsource={source}\nlocals={}\n{listing}",
        compiled.locals
    );
    assert!(
        hints
            .iter()
            .all(|(_, hint)| *hint != Some((ValueType::Int, ValueType::Float))),
        "nested number declaration narrowed to Int/Float: {hints:?}\n\
         guest={status:?}\nsource={source}\nlocals={}\n{listing}",
        compiled.locals
    );
    let status = status.unwrap_or_else(|error| {
        panic!(
            "runtime {error:?}\nhints={hints:?}\nlocals={}\nsource={source}\n{listing}",
            compiled.locals
        )
    });
    assert_eq!(status, VmStatus::Halted, "{listing}");
    assert_eq!(stack.last(), Some(&Value::Float(2.0)), "{listing}");
}

/// Source form: if-expression branch, not a statement `if`. The branch is
/// `Expr::Block` nested in `Expr::IfElse`. A dead float reuses the number
/// binding's physical slot before the branch assigns `2`.
#[test]
fn reused_slot_if_expr_keeps_declared_number_div_hint() {
    assert_reused_number_matches_generic_control(
        r#"
            fn probe() -> float {
                let mut checked: number = 0;
                let temporary: float = 1.5;
                let sink = temporary + 0.0;
                let pad0 = 1;
                let pad1 = 2;
                let pad2 = 3;
                let pad3 = 4;
                let pad4 = 5;
                let pad5 = 6;
                let pad6 = 7;
                let out = if pad0 + pad1 + pad2 + pad3 + pad4 + pad5 + pad6 == 28 => {
                    checked = 2;
                    checked / 1.0
                } else => {
                    sink
                };
                out
            }
            probe();
        "#,
    );
}

/// Source form: Result match arm is an if-expression. The assignment is not a
/// direct ResultMatch child; the parser only accepts an expression arm. The
/// if-expression is reached only by entering ResultMatch.
#[test]
fn reused_slot_result_match_keeps_declared_number_div_hint() {
    assert_reused_number_matches_generic_control(
        r#"
            fn probe() -> float {
                let mut checked: number = 0;
                let temporary: float = 1.5;
                let sink = temporary + 0.0;
                let pad0 = 1;
                let pad1 = 2;
                let pad2 = 3;
                let pad3 = 4;
                let pad4 = 5;
                let pad5 = 6;
                let flag: Result<int, int> = Result::Ok(1);
                let out = match flag {
                    Result::Ok(_) => if pad0 + pad1 + pad2 + pad3 + pad4 + pad5 == 21 => {
                        checked = 2;
                        checked / 1.0
                    } else => {
                        sink
                    },
                    Result::Err(_) => sink,
                };
                out
            }
            probe();
        "#,
    );
}

/// Source form: the if-expression is a call argument, not a let initializer.
/// Reaching the assignment requires entering `Expr::Call` args and then
/// `Expr::IfElse`.
#[test]
fn reused_slot_call_arg_keeps_declared_number_div_hint() {
    assert_reused_number_matches_generic_control(
        r#"
            fn identity(value: float) -> float {
                value
            }
            fn probe() -> float {
                let mut checked: number = 0;
                let temporary: float = 1.5;
                let sink = temporary + 0.0;
                let pad0 = 1;
                let pad1 = 2;
                let pad2 = 3;
                let pad3 = 4;
                let pad4 = 5;
                let pad5 = 6;
                let pad6 = 7;
                let out = identity(if pad0 + pad1 + pad2 + pad3 + pad4 + pad5 + pad6 == 28 => {
                    checked = 2;
                    checked / 1.0
                } else => {
                    sink
                });
                out
            }
            probe();
        "#,
    );
}
