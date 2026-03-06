#![cfg(feature = "runtime")]
mod common;
use common::*;

#[test]
fn scheme_direct_subset_runtime_cases_work() {
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
            name: "float_char_and_hex_escape_literals",
            source: r#"
                (define f 1.25)
                (define c #\x41)
                (define s "\x42")
                f
                c
                s
            "#,
            flavor: SourceFlavor::Scheme,
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
fn scheme_direct_subset_rejection_cases_work() {
    let unsupported_syntax = [
        "unsupported Scheme syntax",
        "unsupported identifier",
        "reserved",
    ];

    let cases = [
        (
            "require_import_forms_not_supported",
            r#"
                (require (prefix-in vm. "vm"))
                (vm.add_one 41)
            "#,
            &unsupported_syntax[..],
        ),
        (
            "general_function_calls_not_supported",
            r#"
                (add_one 41)
            "#,
            &unsupported_syntax[..],
        ),
        (
            "len_call_not_supported_in_direct_subset",
            r#"
                (define value "hello")
                (len value)
            "#,
            &unsupported_syntax[..],
        ),
    ];

    for (name, source, expected_any) in cases {
        expect_parse_error_contains_any_case(name, source, SourceFlavor::Scheme, expected_any);
    }
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
