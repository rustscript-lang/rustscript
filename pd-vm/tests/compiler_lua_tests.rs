#![cfg(feature = "runtime")]
mod common;
use common::*;

fn expect_lua_direct_only_error(source: &str) {
    let err = match compile_source_with_flavor(source, SourceFlavor::Lua) {
        Ok(_) => panic!("source should be rejected by Lua direct frontend"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("unsupported Lua syntax"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn lua_direct_subset_assignment_and_arithmetic_work() {
    let source = r#"
        local a = 1
        a = a + 41
        a
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");
    assert_eq!(compiled.locals, 1);

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn lua_direct_subset_if_else_and_logic_work() {
    let source = r#"
        local a = 2
        if a > 1 and a < 3 then
            42
        else
            0
        end
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn lua_direct_subset_while_loop_work() {
    let source = r#"
        local i = 0
        while i < 3 do
            i = i + 1
        end
        i
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);
}

#[test]
fn lua_direct_subset_do_end_block_work() {
    let source = r#"
        local value = 1
        do
            value = value + 41
        end
        value
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn lua_direct_subset_float_char_and_hex_escape_literals_work() {
    let source = r#"
        local f = 1.25
        local c = '\x41'
        local s = "\x42"
        f
        c
        s
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Lua).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[
            Value::Float(1.25),
            Value::String("A".to_string()),
            Value::String("B".to_string())
        ]
    );
}

#[test]
fn lua_assignment_to_undeclared_local_is_rejected() {
    let source = r#"
        value = 1
    "#;

    let err = match compile_source_with_flavor(source, SourceFlavor::Lua) {
        Ok(_) => panic!("assignment to undeclared local should fail"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("unknown local 'value'"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn lua_require_syntax_is_not_supported_anymore() {
    let source = r#"
        local vm = require("vm")
        vm.add_one(41)
    "#;

    expect_lua_direct_only_error(source);
}

#[test]
fn lua_function_syntax_is_not_supported_anymore() {
    let source = r#"
        function inc(v)
            return v + 1
        end
        inc(41)
    "#;

    expect_lua_direct_only_error(source);
}

#[test]
fn lua_calls_are_not_supported_in_direct_subset() {
    let source = r#"
        local value = "hello"
        len(value)
    "#;

    expect_lua_direct_only_error(source);
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
