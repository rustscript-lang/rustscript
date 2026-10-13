#![cfg(all(feature = "runtime", feature = "cranelift-jit"))]

use vm::{BytecodeBuilder, JitConfig, Program, Value, Vm, VmError, VmStatus, compile_source};

fn assert_ssa(machine: &Vm) {
    let dump = machine.dump_aot_info();
    assert!(dump.contains("lowering=ssa"), "{dump}");
    assert!(!dump.contains("interpreter-boundary"), "{dump}");
}

fn exported_vm(source: &str, native: bool) -> Vm {
    let mut machine = source_vm(source);
    if native {
        machine.compile_aot().unwrap();
        assert_ssa(&machine);
    }
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    machine
}

fn callable_result(source: &str, name: &str, args: &[Value], native: bool) -> Value {
    // Feed legitimate public Program constants through an ordinary root call.
    // The callee's logical Number schema stays unchanged for every invocation.
    let arguments = (0..args.len())
        .map(|index| (41 + index).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!("{source}\nlet callable = {name}; callable({arguments});");
    let compiled = compile_source(&source).unwrap();
    let mut program = compiled.program.with_local_count(compiled.locals);
    for constant in &mut program.constants {
        if let Value::Int(value) = constant
            && (41..41 + args.len() as i64).contains(value)
        {
            *constant = args[(*value - 41) as usize].clone();
        }
    }
    let mut machine = Vm::new_with_jit_config(
        program,
        JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    machine.set_fuel(100_000);
    if native {
        machine.compile_aot().unwrap();
        assert_ssa(&machine);
    }
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().len(), 1);
    assert_eq!(machine.call_depth(), 0);
    if native {
        assert!(machine.aot_exec_count() >= 3);
    }
    let result = machine.stack()[0].clone();
    eprintln!(
        "{name} {args:?}: {result:?} native returns={}",
        machine.aot_exec_count()
    );
    result
}

#[test]
fn materialized_number_comparisons_preserve_integer_precision_and_mixed_float_semantics() {
    let source = "pub fn less(lhs: number, rhs: number) -> bool { lhs < rhs }\n\
                  pub fn greater(lhs: number, rhs: number) -> bool { lhs > rhs }";
    let pairs = [
        (
            Value::Int(9_007_199_254_740_992),
            Value::Int(9_007_199_254_740_993),
        ),
        (Value::Int(i64::MIN), Value::Int(i64::MIN + 1)),
        (Value::Int(i64::MAX - 1), Value::Int(i64::MAX)),
        (Value::Int(i64::MIN), Value::Int(i64::MAX)),
        (Value::Int(1), Value::Float(1.5)),
        (Value::Float(1.5), Value::Int(2)),
        (Value::Float(-2.5), Value::Float(3.5)),
        (
            Value::Int(9_007_199_254_740_993),
            Value::Float(9_007_199_254_740_992.0),
        ),
        (Value::Float(f64::NAN), Value::Int(1)),
        (Value::Int(1), Value::Float(f64::NAN)),
        (Value::Float(f64::NEG_INFINITY), Value::Float(f64::INFINITY)),
        (Value::Float(-0.0), Value::Int(0)),
        (Value::Float(0.0), Value::Float(-0.0)),
    ];
    for (lhs, rhs) in pairs {
        for args in [[lhs.clone(), rhs.clone()], [rhs, lhs]] {
            for name in ["less", "greater"] {
                let expected = callable_result(source, name, &args, false);
                let actual = callable_result(source, name, &args, true);
                assert_eq!(actual, expected, "{name} {args:?}");
            }
        }
    }
}

#[test]
fn dynamic_number_relations_cover_literal_operand_positions() {
    for (body, args, expected) in [
        ("n < 2", "1", true),
        ("2 > n", "1", true),
        ("2 < n", "3", true),
        ("n > 2", "3", true),
        ("n < 2", "1.5", true),
        ("2 > n", "1.5", true),
        ("n < 2.0", "1", true),
        ("2.0 > n", "1", true),
        ("n > 2.0", "1", false),
        ("2.0 < n", "1", false),
    ] {
        assert_native(
            &format!("fn compare(n: number) -> bool {{ {body} }} compare({args});"),
            &[Value::Bool(expected)],
        );
    }
}

#[test]
fn unrelated_frames_and_materialized_callables_do_not_specialize_number_arguments() {
    assert_native(
        "fn text(v: string) -> string { v }\n\
         fn integer(v: int) -> int { v }\n\
         fn less(a: number, b: number) -> bool { a < b }\n\
         let label = text(\"other frame\"); let count = integer(3);\n\
         let compare = less;\n\
         let first = compare(9007199254740992, 9007199254740993);\n\
         let second = compare(1.5, 2); first && second;",
        &[Value::Bool(true)],
    );
}

#[test]
fn number_factorial_preserves_float_and_fractional_results() {
    let source = "pub fn factorial(n: number) -> number { if n < 2 => { 1 } else => { n * factorial(n - 1) } }";
    for (arg, expected) in [
        (Value::Int(6), Value::Int(720)),
        (Value::Float(6.0), Value::Float(720.0)),
        (Value::Float(3.5), Value::Float(8.75)),
        (Value::Int(1), Value::Int(1)),
        (Value::Float(-0.0), Value::Int(1)),
    ] {
        let args = [arg];
        assert_eq!(callable_result(source, "factorial", &args, false), expected);
        let actual = callable_result(source, "factorial", &args, true);
        assert_eq!(actual, expected);
    }
}

#[test]
fn dynamic_number_arithmetic_preserves_wrapping_and_result_tags() {
    let source = "pub fn subtract(a: number, b: number) -> number { a - b }\n\
                  pub fn multiply(a: number, b: number) -> number { a * b }";
    for args in [
        [Value::Int(0), Value::Int(0)],
        [Value::Int(0), Value::Float(0.0)],
        [Value::Float(-0.0), Value::Float(0.0)],
        [Value::Int(i64::MIN), Value::Int(1)],
        [Value::Int(i64::MAX), Value::Int(2)],
        [Value::Int(9_007_199_254_740_993), Value::Int(1)],
        [Value::Int(6), Value::Float(0.5)],
        [Value::Float(6.0), Value::Int(2)],
        [Value::Float(-0.0), Value::Int(2)],
        [Value::Int(2), Value::Float(f64::INFINITY)],
        [Value::Float(f64::NAN), Value::Int(2)],
    ] {
        for name in ["subtract", "multiply"] {
            let expected = callable_result(source, name, &args, false);
            let actual = callable_result(source, name, &args, true);
            match (&actual, &expected) {
                (Value::Float(actual), Value::Float(expected)) => {
                    if expected.is_nan() {
                        assert!(actual.is_nan());
                    } else {
                        assert_eq!(actual.to_bits(), expected.to_bits());
                    }
                }
                _ => assert_eq!(actual, expected, "{name} {args:?}"),
            }
        }
    }
}

fn dynamic_program(op: &str) -> Program {
    let mut code = BytecodeBuilder::new();
    code.ldloc(0);
    code.ldloc(1);
    match op {
        "less" => code.clt(),
        "greater" => code.cgt(),
        "subtract" => code.sub(),
        "multiply" => code.mul(),
        _ => unreachable!(),
    }
    code.ret();
    Program::new(vec![], code.finish()).with_local_count(2)
}

#[test]
fn invalid_dynamic_tags_reject_safely_at_the_numeric_instruction() {
    for op in ["less", "greater", "subtract", "multiply"] {
        for invalid in [
            Value::Null,
            Value::Bool(true),
            Value::string("invalid"),
            Value::array(vec![Value::Int(1)]),
        ] {
            for args in [
                [invalid.clone(), Value::Int(1)],
                [Value::Float(1.5), invalid],
            ] {
                let mut reference = Vm::new(dynamic_program(op));
                let mut native = Vm::new(dynamic_program(op));
                for machine in [&mut reference, &mut native] {
                    machine.set_jit_config(JitConfig {
                        enabled: false,
                        hot_loop_threshold: 1,
                        max_trace_len: 512,
                    });
                    machine.set_local(0, args[0].clone()).unwrap();
                    machine.set_local(1, args[1].clone()).unwrap();
                    machine.set_fuel(100_000);
                }
                assert!(matches!(
                    reference.run(),
                    Err(VmError::TypeMismatch("number"))
                ));
                native.compile_aot().unwrap();
                assert_ssa(&native);
                let error = native
                    .run()
                    .expect_err("invalid tags must not produce a numeric result");
                assert!(matches!(error, VmError::JitNative(_)), "{error:?}");
                assert_eq!(native.ip(), 4, "failure must identify numeric opcode");
                assert!(native.stack().is_empty());
                assert_eq!(
                    native.aot_exec_count(),
                    1,
                    "error return count is not success"
                );
                eprintln!(
                    "{op} {args:?}: {error:?} native error returns={}",
                    native.aot_exec_count()
                );
            }
        }
    }
}

struct NumericOperand {
    index: u8,
    value: Value,
    calls: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl vm::HostFunction for NumericOperand {
    fn call(&mut self, _: &mut Vm, _: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        self.calls.lock().unwrap().push(self.index);
        Ok(vm::CallOutcome::Return(vec![self.value.clone()].into()))
    }
}

#[test]
fn dynamic_numeric_operands_execute_once_in_left_to_right_order() {
    for (op, expected) in [
        ("less", Value::Bool(false)),
        ("greater", Value::Bool(true)),
        ("subtract", Value::Int(1)),
        ("multiply", Value::Int(6)),
    ] {
        for native in [false, true] {
            let mut code = BytecodeBuilder::new();
            code.call(0, 0);
            code.call(1, 0);
            match op {
                "less" => code.clt(),
                "greater" => code.cgt(),
                "subtract" => code.sub(),
                "multiply" => code.mul(),
                _ => unreachable!(),
            }
            code.ret();
            let program = Program::with_imports_and_debug(
                vec![],
                code.finish(),
                (0..2)
                    .map(|index| vm::HostImport {
                        name: format!("operand{index}"),
                        arity: 0,
                        return_type: vm::ValueType::Unknown,
                    })
                    .collect(),
                None,
            );
            let mut machine = Vm::new_with_jit_config(
                program,
                JitConfig {
                    enabled: false,
                    hot_loop_threshold: 1,
                    max_trace_len: 512,
                },
            );
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            for (index, value) in [Value::Int(3), Value::Int(2)].into_iter().enumerate() {
                machine.register_function(Box::new(NumericOperand {
                    index: index as u8,
                    value,
                    calls: calls.clone(),
                }));
            }
            machine.set_fuel(100_000);
            if native {
                machine.compile_aot().unwrap();
                assert_ssa(&machine);
            }
            assert_eq!(machine.run().unwrap(), VmStatus::Halted);
            assert_eq!(machine.stack(), std::slice::from_ref(&expected));
            assert_eq!(*calls.lock().unwrap(), vec![0, 1]);
            if native {
                assert!(machine.aot_exec_count() >= 3);
            }
        }
    }
}

#[test]
fn dynamic_arithmetic_results_survive_loop_carried_number_slots() {
    assert_native(
        "fn descend(n: number) -> number { let mut current = n; while current > 1 { current = current - 1; } current } descend(6.0);",
        &[Value::Float(1.0)],
    );
}

#[test]
fn the_same_number_factorial_frame_accepts_multiple_runtime_representations() {
    assert_native(
        "fn factorial(n: number) -> number { if n < 2 => { 1 } else => { n * factorial(n - 1) } }\n\
         let integer = factorial(6); let floating = factorial(6.0); [integer, floating, factorial(3.5)];",
        &[Value::array(vec![
            Value::Int(720),
            Value::Float(720.0),
            Value::Float(8.75),
        ])],
    );
}

#[test]
fn host_callable_invocation_retains_its_existing_interpreter_policy() {
    let mut machine = exported_vm("pub fn below(n: number) -> bool { n < 2 }", true);
    let callable = machine.resolve_exported_callable("below").unwrap();
    let before = machine.aot_exec_count();
    assert_eq!(
        machine.invoke_callable(callable, &[Value::Int(1)]).unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        machine.aot_exec_count(),
        before,
        "host invocation currently disables native execution"
    );
    eprintln!(
        "host invoke_callable boundary: native returns before={before} after={}",
        machine.aot_exec_count()
    );
}

#[test]
fn number_factorial_direct_float_source_is_native() {
    for (argument, result) in [("6.0", 720.0), ("3.5", 8.75)] {
        assert_native(
            &format!(
                "fn factorial(n: number) -> number {{ if n < 2 => {{ 1 }} else => {{ n * factorial(n - 1) }} }} factorial({argument});"
            ),
            &[Value::Float(result)],
        );
    }
}

#[test]
fn number_schema_rejects_invalid_callable_arguments_without_native_success() {
    for native in [false, true] {
        let mut machine = exported_vm("pub fn below(n: number) -> bool { n < 2 }", native);
        let callable = machine.resolve_exported_callable("below").unwrap();
        let before = machine.aot_exec_count();
        let error = machine
            .invoke_callable(callable, &[Value::Bool(true)])
            .unwrap_err();
        assert!(matches!(
            error,
            VmError::TypeMismatch("callable argument schema")
        ));
        eprintln!("Number callable admission native={native}: {error:?}");
        assert_eq!(machine.aot_exec_count(), before);
        assert_eq!(machine.call_depth(), 0);
    }
}

fn source_vm(source: &str) -> Vm {
    let compiled = compile_source(source).expect("compile neutral source");
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_jit_config(JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    machine.set_fuel(100_000);
    machine
}

fn assert_native(source: &str, expected: &[Value]) {
    let mut reference = source_vm(source);
    assert_eq!(reference.run().unwrap(), VmStatus::Halted);
    assert_eq!(reference.stack(), expected);
    let mut machine = source_vm(source);
    machine
        .compile_aot()
        .expect("compile dynamic Number natively");
    let dump = machine.dump_aot_info();
    assert!(dump.contains("lowering=ssa"), "{dump}");
    assert!(!dump.contains("interpreter-boundary"), "{dump}");
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), expected);
    assert!(machine.aot_exec_count() > 0, "{dump}");
    eprintln!(
        "native returns={} result={:?}\n{dump}",
        machine.aot_exec_count(),
        machine.stack()
    );
}

#[test]
fn number_factorial_preserves_integer_result_natively() {
    assert_native(
        "fn factorial(n: number) -> number { if n < 2 => { 1 } else => { n * factorial(n - 1) } } factorial(6);",
        &[Value::Int(720)],
    );
}

#[test]
fn number_parameter_relational_comparison_is_native() {
    assert_native(
        "fn below(n: number) -> bool { n < 2 } below(1);",
        &[Value::Bool(true)],
    );
}
