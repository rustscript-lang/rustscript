//! Milestones 1-2 of the semantic module system: structured `use` parsing with
//! spans/clauses, deterministic module identities, same-stem uniqueness, and
//! the dedicated host-namespace path.

#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};

use common::*;
use vm::{
    ImportClause, ParserDialect, SharedParserOptions, UsePathSegment, parse_source_with_dialect,
};

/// Minimal dialect for driving the shared frontend parser from tests.
struct TestDialect;

impl ParserDialect for TestDialect {}

static TEST_DIALECT: TestDialect = TestDialect;

fn rustscript_options() -> SharedParserOptions {
    SharedParserOptions {
        source_id: 0,
        allow_implicit_externs: false,
        allow_implicit_semicolons: false,
        enforce_mutable_bindings: true,
        import_scan_mode: false,
    }
}

fn temp_module_root(prefix: &str) -> PathBuf {
    let unique = format!(
        "{prefix}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&root).expect("temp module root should be created");
    root.canonicalize().unwrap_or(root)
}

fn write_source(path: &Path, source: &str, description: &str) {
    std::fs::write(path, source).unwrap_or_else(|err| panic!("{description} should write: {err}"));
}

fn remove_module_root(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn parser_records_structured_use_nodes_with_spans_and_clauses() {
    let source = "use self::pkg::nested as nested;\n\
                  use sibling::{value as v, other};\n\
                  use super::shared;\n\
                  use io;\n\
                  1;\n";
    let ir = parse_source_with_dialect(source, &TEST_DIALECT, rustscript_options())
        .expect("source should parse");

    let decls = &ir.use_declarations;
    assert_eq!(
        decls.len(),
        4,
        "every use directive becomes a structured node"
    );

    // self-qualified namespace import.
    assert_eq!(
        decls[0].path,
        vec![
            UsePathSegment::Self_,
            UsePathSegment::Ident("pkg".to_string()),
            UsePathSegment::Ident("nested".to_string()),
        ]
    );
    assert!(
        matches!(&decls[0].clause, ImportClause::Namespace(alias) if alias == "nested"),
        "namespace alias clause expected"
    );
    assert_eq!(decls[0].line, 1);

    // Named import list with an alias.
    assert_eq!(
        decls[1].path,
        vec![UsePathSegment::Ident("sibling".to_string())]
    );
    match &decls[1].clause {
        ImportClause::Named(named) => {
            assert_eq!(named.len(), 2);
            assert_eq!(named[0].imported, "value");
            assert_eq!(named[0].local, "v");
            assert_eq!(named[1].imported, "other");
            assert_eq!(named[1].local, "other");
        }
        other => panic!("expected named clause, got {other:?}"),
    }
    assert_eq!(decls[1].line, 2);

    // super-qualified and bare builtin imports.
    assert_eq!(
        decls[2].path,
        vec![
            UsePathSegment::Super,
            UsePathSegment::Ident("shared".to_string())
        ]
    );
    assert!(matches!(decls[2].clause, ImportClause::AllPublic));
    assert!(matches!(decls[3].clause, ImportClause::AllPublic));
    assert_eq!(decls[3].line, 4);

    // Every span covers exactly its directive text in the source.
    for decl in decls {
        assert!(
            decl.span.lo < decl.span.hi,
            "span must cover the directive: {decl:?}"
        );
        let text = &source[decl.span.lo..decl.span.hi];
        assert!(
            text.starts_with("use ") && text.ends_with(';'),
            "span must cover the full directive, got {text:?}"
        );
    }
}

#[test]
fn parser_import_scan_mode_tolerates_file_module_calls() {
    // The source-loader discovery parse must accept calls to functions that
    // only the later prelude/rewrite step resolves: unknown direct calls and
    // namespace calls through multi-segment file-module paths.
    let source = "use self::nested as nested;\n\
                  nested::run();\n\
                  imported_helper(1);\n\
                  1;\n";
    let options = SharedParserOptions {
        allow_implicit_externs: true,
        import_scan_mode: true,
        ..rustscript_options()
    };
    let ir = parse_source_with_dialect(source, &TEST_DIALECT, options)
        .expect("scan mode must tolerate unresolved module calls");
    assert_eq!(ir.use_declarations.len(), 1);
    assert_eq!(
        ir.use_declarations[0].path,
        vec![
            UsePathSegment::Self_,
            UsePathSegment::Ident("nested".to_string())
        ]
    );
}

#[test]
fn same_stem_modules_in_different_directories_compile_and_run() {
    let root = temp_module_root("semantic_m12_same_stem");
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");

    let a_module = a_dir.join("util.rss");
    let b_module = b_dir.join("util.rss");
    write_source(&a_module, "pub fn alpha() { 11; }\n", "a/util source");
    write_source(&b_module, "pub fn beta() { 22; }\n", "b/util source");

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::alpha();\nbu::beta();\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("same-stem modules should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(11), Value::Int(22)],
        "both same-stem modules must resolve independently"
    );

    remove_module_root(&root);
}

#[test]
fn host_namespace_imports_keep_dedicated_resolution_path() {
    let source = "use io;\nio::exists(\".\");\n";
    let compiled = compile_source(source).expect("host namespace import should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let mut registry = HostFunctionRegistry::empty();
    vm::register_io_builtin_module(&mut registry).expect("standard IO registration should succeed");
    registry
        .bind_vm_cached(&mut vm)
        .expect("standard exact host imports should bind");
    #[cfg(feature = "async")]
    super::async_test_bridge::install(&mut vm);
    loop {
        match vm.run().expect("vm should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_) => vm
                .wait_for_host_op_blocking()
                .expect("exact IO operation should complete"),
        }
    }
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn aliased_file_module_stem_does_not_shadow_exact_host_namespace() {
    let root = temp_module_root("semantic_m12_alias_host_namespace");
    let fixtures = root.join("fixtures");
    std::fs::create_dir_all(&fixtures).expect("fixtures directory should be created");
    write_source(
        &fixtures.join("io.rss"),
        "pub fn marker() { 7; }\n",
        "aliased io module source",
    );

    let main_path = root.join("main.rss");
    let source = "use self::fixtures::io as file_io;\nuse io;\nlet present = io::exists(\".\");\nfile_io::marker();\npresent;\n";
    write_source(&main_path, source, "aliased host namespace source");

    let compiled = compile_source_file(&main_path)
        .expect("an aliased file module must not hide exact host io::exists");
    let standard = vm::standard_host_catalog();
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "io::exists")
        .expect("exact host io::exists import should be emitted");
    let schema = import
        .schema
        .as_ref()
        .expect("host import should carry the V13 schema");
    assert_eq!(
        schema.fingerprint,
        standard.fingerprint(),
        "host import must carry the standard catalog fingerprint"
    );
    assert_eq!(schema.params.len(), 1);
    assert_eq!(schema.return_type, vm::compiler::TypeSchema::Bool);
    assert!(
        compiled
            .program
            .imports
            .iter()
            .all(|import| import.name != "file_io::marker"),
        "file-module calls must remain module calls, not host imports"
    );

    remove_module_root(&root);
}

#[test]
fn named_import_with_alias_through_self_resolves_structurally() {
    let root = temp_module_root("semantic_m12_named_alias");
    let module_path = root.join("module.rss");
    write_source(
        &module_path,
        "pub fn echo(value) { value; }\n",
        "module source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::module::{echo as e};\ne(42);\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("named alias import should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    remove_module_root(&root);
}

#[test]
fn structured_import_syntax_rejects_crate_paths() {
    let err = match compile_source("use crate::x;\n1;\n") {
        Ok(_) => panic!("crate:: paths should be rejected"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("crate:: paths are not supported"),
        "unexpected error: {message}"
    );
}

#[test]
fn structured_import_syntax_rejects_import_keyword() {
    let root = temp_module_root("semantic_m12_import_keyword");
    let main_path = root.join("main.rss");
    write_source(&main_path, "import \"./module.rss\";\n1;\n", "main source");
    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("legacy import syntax should be rejected"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("expected ';' after expression"),
        "unexpected parser diagnostic: {message}"
    );
    assert_eq!(
        err.sources().unwrap().file(0).unwrap().name,
        main_path.to_string_lossy()
    );
    remove_module_root(&root);
}
