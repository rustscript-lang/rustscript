#![cfg(feature = "runtime")]
mod common;
use common::*;

fn expect_scheme_direct_only_error(source: &str) {
    let err = match compile_source_with_flavor(source, SourceFlavor::Scheme) {
        Ok(_) => panic!("source should be rejected by Scheme direct frontend"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                parse.message.contains("unsupported Scheme syntax")
                    || parse.message.contains("unsupported identifier")
                    || parse.message.contains("reserved"),
                "{}",
                parse.message
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn scheme_direct_subset_define_set_and_arithmetic_work() {
    let source = r#"
        (define a 1)
        (set! a (+ a 41))
        a
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");
    assert_eq!(compiled.locals, 1);

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn scheme_direct_subset_if_and_begin_work() {
    let source = r#"
        (define a 1)
        (if (< a 2)
            (begin
                (set! a (+ a 41))
                a)
            0)
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

#[test]
fn scheme_direct_subset_while_loop_work() {
    let source = r#"
        (define i 0)
        (while (< i 3)
            (set! i (+ i 1)))
        i
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);
}

#[test]
fn scheme_direct_subset_modulo_works() {
    let source = r#"
        (define m (modulo 17 5))
        (+ m 2)
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn scheme_require_import_forms_are_not_supported_anymore() {
    let source = r#"
        (require (prefix-in vm. "vm"))
        (vm.add_one 41)
    "#;

    expect_scheme_direct_only_error(source);
}

#[test]
fn scheme_general_function_calls_are_not_supported_in_direct_subset() {
    let source = r#"
        (add_one 41)
    "#;

    expect_scheme_direct_only_error(source);
}

#[test]
fn scheme_float_char_and_hex_escape_literals_are_supported() {
    let source = r#"
        (define f 1.25)
        (define c #\x41)
        (define s "\x42")
        f
        c
        s
    "#;

    let compiled =
        compile_source_with_flavor(source, SourceFlavor::Scheme).expect("compile should succeed");

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
fn scheme_len_call_is_not_supported_in_direct_subset() {
    let source = r#"
        (define value "hello")
        (len value)
    "#;

    expect_scheme_direct_only_error(source);
}

#[test]
fn scheme_complex_fixture_is_rejected_without_rewrite_path() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example_complex.scm");
    let err = match compile_source_file(&path) {
        Ok(_) => panic!("fixture should be rejected in direct subset"),
        Err(err) => err,
    };

    match err {
        vm::SourcePathError::Source(vm::SourceError::Parse(parse)) => {
            assert!(
                parse.message.contains("unsupported Scheme syntax")
                    || parse.message.contains("identifier")
                    || parse.message.contains("reserved"),
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
