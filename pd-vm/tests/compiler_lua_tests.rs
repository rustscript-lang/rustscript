#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn lua_direct_subset_runtime_cases_work() {
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
    ];

    run_runtime_cases(&cases);
}

#[test]
fn lua_direct_subset_rejection_cases_work() {
    let cases = [
        ParseErrorCase {
            name: "assignment_to_undeclared_local",
            source: r#"
                value = 1
            "#,
            flavor: SourceFlavor::Lua,
            expected_contains_all: &["unknown local 'value'"],
        },
        ParseErrorCase {
            name: "require_syntax_not_supported",
            source: r#"
                local vm = require("vm")
                vm.add_one(41)
            "#,
            flavor: SourceFlavor::Lua,
            expected_contains_all: &["unsupported Lua syntax"],
        },
        ParseErrorCase {
            name: "function_syntax_not_supported",
            source: r#"
                function inc(v)
                    return v + 1
                end
                inc(41)
            "#,
            flavor: SourceFlavor::Lua,
            expected_contains_all: &["unsupported Lua syntax"],
        },
        ParseErrorCase {
            name: "general_calls_not_supported_in_direct_subset",
            source: r#"
                local value = "hello"
                len(value)
            "#,
            flavor: SourceFlavor::Lua,
            expected_contains_all: &["unsupported Lua syntax"],
        },
    ];

    for case in &cases {
        expect_parse_error_case(case);
    }
}

#[test]
fn lua_complex_fixture_is_rejected_without_rewrite_path() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.lua");
    let err = match compile_source_file(&path) {
        Ok(_) => panic!("fixture should be rejected in direct subset"),
        Err(err) => err,
    };

    match err {
        vm::SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            assert!(
                parse.message.contains("unsupported Lua syntax"),
                "{}",
                parse.message
            );
        }
        vm::SourcePathError::Source(vm::SourceError::Compile(other)) => {
            panic!("unexpected nested compile error: {other:?}");
        }
        vm::SourcePathError::Io(other) => panic!("unexpected io error: {other}"),
        vm::SourcePathError::MissingExtension => panic!("unexpected missing extension"),
        vm::SourcePathError::UnsupportedExtension(other) => {
            panic!("unexpected unsupported extension: {other}")
        }
        vm::SourcePathError::ImportCycle(other) => {
            panic!("unexpected import cycle at: {}", other.display())
        }
        vm::SourcePathError::NonRustScriptModule(other) => {
            panic!("unexpected non-rustscript module: {}", other.display())
        }
        vm::SourcePathError::ImportWithoutParent(other) => {
            panic!("unexpected import-without-parent path: {}", other.display())
        }
        vm::SourcePathError::InvalidImportSyntax { message, .. } => {
            panic!("unexpected invalid import syntax: {message}")
        }
    }
}
