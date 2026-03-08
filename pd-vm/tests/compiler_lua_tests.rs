#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn lua_runtime_cases_work() {
    let cases = vec![
        RuntimeCase {
            name: "assignment_and_arithmetic",
            source: r#"
                local a = 1
                a = a + 41
                a
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![Value::Int(42)],
            expected_locals: Some(1),
        },
        RuntimeCase {
            name: "if_else_and_logic",
            source: r#"
                local a = 2
                if a > 1 and a < 3 then
                    42
                else
                    0
                end
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "while_loop",
            source: r#"
                local i = 0
                while i < 3 do
                    i = i + 1
                end
                i
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![Value::Int(3)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "do_end_block",
            source: r#"
                local value = 1
                do
                    value = value + 41
                end
                value
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "float_char_and_hex_escape_literals",
            source: r#"
                local f = 1.25
                local c = '\x41'
                local s = "\x42"
                f
                c
                s
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![
                Value::Float(1.25),
                Value::String("A".to_string()),
                Value::String("B".to_string()),
            ],
            expected_locals: None,
        },
        RuntimeCase {
            name: "elseif_and_elif_alias",
            source: r#"
                local a = 2
                if a == 1 then
                    0
                elseif a == 2 then
                    1
                else
                    2
                end

                if a == 1 then
                    0
                elif a == 2 then
                    42
                else
                    0
                end
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![Value::Int(1), Value::Int(42)],
            expected_locals: None,
        },
        RuntimeCase {
            name: "empty param closure captures outer value",
            source: r#"
                local x = 41
                local f = function() return x + 1 end
                f()
            "#,
            flavor: SourceFlavor::Lua,
            expected_stack: vec![Value::Int(42)],
            expected_locals: None,
        },
    ];

    run_runtime_cases(&cases);
}

#[test]
fn lua_rejection_cases_work() {
    let cases = [ParseErrorCase {
        name: "assignment_to_undeclared_local",
        source: r#"
                value = 1
            "#,
        flavor: SourceFlavor::Lua,
        expected_contains_all: &["unknown local 'value'"],
    }];

    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn lua_complex_fixture_runs() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.lua");
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
