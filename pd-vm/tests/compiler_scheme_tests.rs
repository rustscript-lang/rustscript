#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn scheme_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "define_set_and_arithmetic",
            source: r#"
                (define a 1)
                (set! a (+ a 41))
                a
            "#,
            flavor: SourceFlavor::Scheme,
            expected_stack: vec![Value::Int(42)],
            expected_locals: Some(1),
        },
        RuntimeCase {
            name: "if_and_begin",
            source: r#"
                (define a 1)
                (if (< a 2)
                    (begin
                        (set! a (+ a 41))
                        a)
                    0)
            "#,
            flavor: SourceFlavor::Scheme,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "while_loop",
            source: r#"
                (define i 0)
                (while (< i 3)
                    (set! i (+ i 1)))
                i
            "#,
            flavor: SourceFlavor::Scheme,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "modulo",
            source: r#"
                (define m (modulo 17 5))
                (+ m 2)
            "#,
            flavor: SourceFlavor::Scheme,
            expected_stack: vec![Value::Int(4)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "float_char_and_string_literals",
            source: r#"
                (define f 1.25)
                (define c #\A)
                (define s "B")
                f
                c
                s
            "#,
            flavor: SourceFlavor::Scheme,
            expected_stack: vec![
                Value::Float(1.25),
                Value::Int(65),
                Value::String("B".to_string()),
            ],
            expected_locals: None,
        },
        RuntimeCase {
            name: "empty param lambda captures outer value",
            source: r#"
                (define x 41)
                (define f (lambda () (+ x 1)))
                (f)
            "#,
            flavor: SourceFlavor::Scheme,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
    ];

    run_runtime_cases(&cases);
}

#[test]
fn scheme_rejection_cases_work() {
    let cases = [(
        "len_call_not_supported_in_frontend",
        r#"
                (define value "hello")
                (len value)
            "#,
        &["len", "unknown function"][..],
    )];

    for (name, source, expected_any) in cases {
        expect_parse_error_contains_any_case(name, source, SourceFlavor::Scheme, expected_any);
    }
}

#[test]
fn scheme_complex_fixture_runs() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.scm");
    let compiled = compile_source_file(&path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    for func in &compiled.functions {
        match func.name.as_str() {
            "add_one" => {
                vm.register_function(Box::new(AddOne));
            }
            "print" => {
                vm.register_function(Box::new(PrintBuiltin));
            }
            "runtime::sleep" => {
                vm.register_function(Box::new(RuntimeSleep));
            }
            other => panic!("unexpected function {other}"),
        }
    }
    loop {
        match vm.run().expect("vm should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("vm should complete host operation"),
        }
    }
    assert_eq!(vm.stack(), &[Value::Int(12)]);
}

#[test]
fn scheme_non_strict_comparisons_work() {
    let case = RuntimeCase {
        name: "non strict comparisons work",
        source: r#"
            (define le (<= 1 1))
            (define ge (>= 2 1))
            (if (and le ge)
                42
                0)
        "#,
        flavor: SourceFlavor::Scheme,
        expected_stack: vec![Value::Int(42)],
        expected_locals: None,
    };
    run_runtime_case(&case);
}
