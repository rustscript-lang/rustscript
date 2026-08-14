#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};

use common::*;

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
    // Module identities are canonical for existing files; keep expected paths
    // canonical too so assertions match under symlinked temp directories.
    root.canonicalize().unwrap_or(root)
}

fn write_source(path: &Path, source: &str, description: &str) {
    std::fs::write(path, source).unwrap_or_else(|err| panic!("{description} should write: {err}"));
}

fn remove_module_root(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn compile_source_file_module_override_path_redirects_import_spec() {
    let root = temp_module_root("vm_rustscript_module_override_test");

    let override_module_path = root.join("edge_http_upstream_override.rss");
    write_source(
        &override_module_path,
        r#"
        pub fn as_stream() {
            "override-body";
        }
    "#,
        "override module source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use edge::http::upstream as upstream;
        upstream::as_stream();
    "#,
        "main source",
    );

    let options = CompileSourceFileOptions::new()
        .with_module_override_path("edge/http/upstream.rss", &override_module_path);
    let compiled =
        compile_source_file_with_options(&main_path, options).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "override module functions should be inlined into root program"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("override-body")]);

    remove_module_root(&root);
}

#[test]
fn nested_module_override_parse_error_preserves_source_text_and_path() {
    let root = temp_module_root("vm_rustscript_nested_override_error_test");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::virtual::nested as nested;
        nested::run();
    "#,
        "main source",
    );
    let override_source = "pub fn run( {";
    let options = CompileSourceFileOptions::new()
        .with_module_override_source("virtual/nested.rss", override_source);

    let error = match compile_source_file_with_options(&main_path, options) {
        Ok(_) => panic!("invalid override module should fail"),
        Err(error) => error,
    };
    match error {
        vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(parse),
            ..
        } => {
            assert!(parse.message.contains("virtual/nested.rss"));
        }
        error => panic!("expected source-aware nested error, got {error:?}"),
    }

    remove_module_root(&root);
}

#[test]
fn nested_module_strict_unknown_diagnostic_keeps_module_source() {
    let root = temp_module_root("vm_rustscript_nested_strict_diag_test");
    let main_path = root.join("main.rss");
    let nested_path = root.join("nested.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::run();
    "#,
        "main source",
    );
    write_source(
        &nested_path,
        "pub fn run() -> unknown { 1 }",
        "nested source",
    );

    let error = match compile_source_file(&main_path) {
        Ok(_) => panic!("unknown nested annotation should fail in strict RustScript"),
        Err(error) => error,
    };
    match error {
        vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(parse),
            ..
        } => {
            assert!(parse.message.contains(&nested_path.display().to_string()));
        }
        error => panic!("expected nested strict diagnostic, got {error:?}"),
    }

    remove_module_root(&root);
}

#[test]
fn strict_nested_diagnostic_path_is_consistent_across_option_entry_points() {
    let root_source = "use self::nested as nested;\nnested::run();\n";
    let nested_source = "pub fn run() -> unknown { 1 }";
    let options =
        CompileSourceFileOptions::new().with_module_override_source("nested.rss", nested_source);

    let in_memory_error = match vm::compile_source_with_flavor_and_options(
        root_source,
        SourceFlavor::RustScript,
        options.clone(),
    ) {
        Ok(_) => panic!("strict nested annotation should fail"),
        Err(error) => error,
    };
    match in_memory_error {
        vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(parse),
            ..
        } => {
            assert!(parse.message.contains("__pd_vm_inmemory__/nested.rss"));
        }
        error => panic!("expected nested strict diagnostic, got {error:?}"),
    }

    let root = temp_module_root("vm_rustscript_nested_strict_entry_test");
    let main_path = root.join("main.rss");
    let at_path_error = match vm::compile_source_at_path_with_flavor_and_options(
        &main_path,
        root_source,
        SourceFlavor::RustScript,
        options,
    ) {
        Ok(_) => panic!("strict nested annotation should fail"),
        Err(error) => error,
    };
    match at_path_error {
        vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(parse),
            ..
        } => {
            assert!(
                parse
                    .message
                    .contains(&root.join("nested.rss").display().to_string())
            );
        }
        error => panic!("expected nested strict diagnostic, got {error:?}"),
    }

    remove_module_root(&root);
}

#[test]
fn compile_source_file_rustscript_named_import_is_selective() {
    let root = temp_module_root("vm_rustscript_selective_import_test");

    let module_path = root.join("module.rss");
    write_source(
        &module_path,
        r#"
        pub fn add_one(x) {
            x + 1;
        }
        pub fn add_two(x) {
            x + 2;
        }
    "#,
        "module source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use module::{add_one};
        add_two(40);
    "#,
        "main source",
    );

    let err = match compile_source_file(main_path.as_path()) {
        Ok(_) => panic!("selective import should not expose unlisted exports"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(vm::ParseError { ref message, .. }),
            ..
        }
            if message.contains("unknown function 'add_two'")
        ),
        "expected unknown function error, got {err:?}"
    );

    remove_module_root(&root);
}

#[test]
fn compile_source_file_rustscript_named_import_preserves_generic_function_type_params() {
    let root = temp_module_root("vm_rustscript_generic_named_import_test");

    let module_path = root.join("module.rss");
    write_source(
        &module_path,
        r#"
        struct Box<T> { value: T }

        pub fn wrap<T>(value: T) {
            let copied = value;
            let out: Box<T> = { value: copied };
            out
        }
    "#,
        "module source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use module::{wrap};

        let wrapped = wrap::<string>("hello");
        wrapped.value.length + 1;
    "#,
        "main source",
    );

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "generic imported RustScript functions should inline without host imports"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(6)]);

    remove_module_root(&root);
}

#[test]
fn compile_source_file_rustscript_module_exports_only_pub_functions() {
    let root = temp_module_root("vm_rustscript_pub_export_test");

    let module_path = root.join("module.rss");
    write_source(
        &module_path,
        r#"
        fn private_add(x) {
            x + 1;
        }
        pub fn public_add(x) {
            private_add(x);
        }
    "#,
        "module source",
    );

    let ok_main_path = root.join("main_ok.rss");
    write_source(
        &ok_main_path,
        r#"
        use module;
        public_add(41);
    "#,
        "ok main source",
    );
    let compiled = compile_source_file(ok_main_path.as_path()).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "pure RustScript function module should not require host imports"
    );
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    let bad_main_path = root.join("main_bad.rss");
    write_source(
        &bad_main_path,
        r#"
        use module;
        private_add(41);
    "#,
        "bad main source",
    );
    let err = match compile_source_file(bad_main_path.as_path()) {
        Ok(_) => panic!("private import should fail"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(vm::ParseError { ref message, .. }),
            ..
        }
            if message.contains("unknown function 'private_add'")
        ),
        "expected unknown function error, got {err:?}"
    );

    remove_module_root(&root);
}

#[test]
fn rss_function_definition_uses_script_target_without_host_imports() {
    let source = r#"
        fn eq(lhs, rhs) {
            lhs == rhs;
        }
        fn is_empty(value) {
            eq(value, "");
        }
        pub fn non_empty(value) {
            eq(is_empty(value), false);
        }
        non_empty("x");
    "#;

    let compiled = compile_source(source).expect("compile should succeed");
    assert!(
        compiled.functions.is_empty(),
        "rss-defined functions should not be emitted as host imports"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
}

#[test]
fn compile_source_file_imported_module_slice_hidden_bindings_work() {
    let root = temp_module_root("vm_rustscript_imported_slice_test");

    let module_path = root.join("module.rss");
    write_source(
        &module_path,
        r#"
        pub fn tail_len(text) {
            text[1:].length + 1;
        }
    "#,
        "module source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use module;
        tail_len("abcd");
    "#,
        "main source",
    );

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    remove_module_root(&root);
}

#[test]
fn compile_source_file_imported_module_dynamic_slice_end_bindings_work() {
    let root = temp_module_root("vm_rustscript_imported_dynamic_slice_test");

    let module_path = root.join("module.rss");
    write_source(
        &module_path,
        r#"
        pub fn first_hex(text, i) {
            let hex_lookup = {
                "a": 10,
                "b": 11
            };
            hex_lookup[text[i:(i + 1)]];
        }
    "#,
        "module source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use module;
        first_hex("ab", 0);
    "#,
        "main source",
    );

    let compiled = compile_source_file(main_path.as_path()).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(10)]);

    remove_module_root(&root);
}

#[test]
fn nested_module_namespace_import_rewrites_sibling_calls() {
    let root = temp_module_root("vm_rustscript_nested_namespace_import_test");
    write_source(
        &root.join("sibling.rss"),
        r#"
        pub fn value() -> int { 7 }
    "#,
        "sibling source",
    );
    write_source(
        &root.join("nested.rss"),
        r#"
        use self::sibling as sibling;
        pub fn run() -> int { sibling::value() }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("nested namespace import should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);

    remove_module_root(&root);
}

#[test]
fn nested_module_named_import_rewrites_sibling_calls() {
    let root = temp_module_root("vm_rustscript_nested_named_import_test");
    write_source(
        &root.join("sibling.rss"),
        r#"
        pub fn value() -> int { 11 }
    "#,
        "sibling source",
    );
    write_source(
        &root.join("nested.rss"),
        r#"
        use self::sibling::{value};
        pub fn run() -> int { value() }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("nested named import should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(11)]);
    remove_module_root(&root);
}

#[test]
fn nested_module_super_import_resolves_parent_directory_sibling() {
    let root = temp_module_root("vm_rustscript_nested_super_import_test");
    write_source(
        &root.join("shared.rss"),
        r#"
        pub fn value() -> int { 13 }
    "#,
        "parent sibling source",
    );
    let package = root.join("pkg");
    std::fs::create_dir_all(&package).expect("package directory should be created");
    write_source(
        &package.join("nested.rss"),
        r#"
        use super::shared as shared;
        pub fn run() -> int { shared::value() }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::pkg::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("nested super import should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(13)]);

    remove_module_root(&root);
}

#[test]
fn nested_module_missing_sibling_reports_nested_source() {
    let root = temp_module_root("vm_rustscript_nested_missing_import_test");
    write_source(
        &root.join("nested.rss"),
        r#"
        use self::missing as missing;
        pub fn run() -> int { missing::value() }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let error = match compile_source_file(&main_path) {
        Ok(_) => panic!("missing nested sibling should fail"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            vm::SourcePathError::Io(ref io_error)
                if io_error.kind() == std::io::ErrorKind::NotFound
        ),
        "missing nested sibling should remain a filesystem error: {error:?}"
    );

    remove_module_root(&root);
}

#[test]
fn explicit_self_import_cycle_is_detected_after_path_normalization() {
    let root = temp_module_root("vm_rustscript_self_cycle_import_test");
    write_source(
        &root.join("a.rss"),
        r#"
        use self::b as b;
        pub fn run() -> int { b::run() }
    "#,
        "a source",
    );
    write_source(
        &root.join("b.rss"),
        r#"
        use self::a as a;
        pub fn run() -> int { a::run() }
    "#,
        "b source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::a as a;
        a::run();
    "#,
        "main source",
    );

    let error = match compile_source_file(&main_path) {
        Ok(_) => panic!("explicit self import cycle should fail"),
        Err(error) => error,
    };
    assert!(
        matches!(error, vm::SourcePathError::ImportCycle(_)),
        "expected import cycle error, got {error:?}"
    );

    remove_module_root(&root);
}

#[test]
fn nested_module_does_not_reexport_transitive_imports() {
    let root = temp_module_root("vm_rustscript_nested_export_boundary_test");
    write_source(
        &root.join("sibling.rss"),
        r#"
        pub fn leaf() -> int { 19 }
    "#,
        "sibling source",
    );
    write_source(
        &root.join("nested.rss"),
        r#"
        use self::sibling as sibling;
        pub fn run() -> int { sibling::leaf() }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::leaf();
    "#,
        "main source",
    );

    let error = match compile_source_file(&main_path) {
        Ok(_) => panic!("transitive import should not be re-exported"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(vm::ParseError { ref message, .. }),
            ..
        }
                if message.contains("nested::leaf") || message.contains("unknown namespace")
        ),
        "expected transitive export boundary error, got {error:?}"
    );

    remove_module_root(&root);
}

#[test]
fn nested_module_rewrite_preserves_utf8_values_byte_for_byte() {
    let root = temp_module_root("vm_rustscript_nested_utf8_import_test");
    write_source(
        &root.join("sibling.rss"),
        r#"
        // 猫のコメント: the sibling module is untouched by rewriting.
        pub fn echo(value: string) -> string { value }
    "#,
        "sibling source",
    );
    write_source(
        &root.join("nested.rss"),
        r#"
        /* 前置ブロック: 猫 */
        use self::sibling as sibling;
        use self::sibling::{echo as echo_named};
        pub fn run() -> string {
            let namespace_value = sibling::echo("猫");
            let named_value = echo_named("🐱 にゃん");
            // 行コメント: 猫
            let joined = namespace_value + named_value;
            joined
        }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("nested utf-8 imports should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::string("猫🐱 にゃん")],
        "UTF-8 literals must survive namespace and named import rewriting"
    );

    remove_module_root(&root);
}

#[test]
fn nested_module_consecutive_super_import_resolves_two_levels_up() {
    let root = temp_module_root("vm_rustscript_consecutive_super_import_test");
    write_source(
        &root.join("shared.rss"),
        r#"
        pub fn value() -> int { 17 }
    "#,
        "root sibling source",
    );
    let package = root.join("pkg").join("sub");
    std::fs::create_dir_all(&package).expect("package directory should be created");
    write_source(
        &package.join("nested.rss"),
        r#"
        use super::super::shared as shared;
        pub fn run() -> int { shared::value() }
    "#,
        "two-level nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::pkg::sub::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let compiled =
        compile_source_file(&main_path).expect("consecutive super import should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(17)]);

    remove_module_root(&root);
}

#[test]
fn path_aliases_resolve_to_single_module_identity() {
    let root = temp_module_root("vm_rustscript_path_alias_identity_test");
    write_source(
        &root.join("a.rss"),
        r#"
        pub fn value() -> int { 23 }
    "#,
        "module a source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::a as a;
        use a as a2;
        let x = a::value();
        let y = a2::value();
        x + y;
    "#,
        "main source",
    );

    let compiled =
        compile_source_file(&main_path).expect("lexically distinct path aliases should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(46)]);

    remove_module_root(&root);
}

#[test]
fn import_cycle_detected_across_lexically_distinct_aliases() {
    let root = temp_module_root("vm_rustscript_cycle_alias_identity_test");
    write_source(
        &root.join("a.rss"),
        r#"
        use self::b as b;
        pub fn run() -> int { b::run() }
    "#,
        "a source",
    );
    write_source(
        &root.join("b.rss"),
        r#"
        use a as a;
        pub fn run() -> int { a::run() }
    "#,
        "b source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::a as a;
        a::run();
    "#,
        "main source",
    );

    let error = match compile_source_file(&main_path) {
        Ok(_) => panic!("lexically distinct cycle aliases should fail"),
        Err(error) => error,
    };
    assert!(
        matches!(error, vm::SourcePathError::ImportCycle(_)),
        "expected import cycle error across alias forms, got {error:?}"
    );

    remove_module_root(&root);
}

#[test]
fn duplicate_import_aliases_are_idempotent() {
    let root = temp_module_root("vm_rustscript_duplicate_alias_import_test");
    write_source(
        &root.join("sibling.rss"),
        r#"
        pub fn value() -> int { 29 }
    "#,
        "sibling source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::sibling as sib;
        use self::sibling as sib;
        sib::value();
    "#,
        "main source",
    );

    let compiled =
        compile_source_file(&main_path).expect("duplicate import aliases should be idempotent");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(29)]);

    remove_module_root(&root);
}

#[test]
fn nested_module_host_namespace_import_stays_host() {
    let root = temp_module_root("vm_rustscript_nested_host_namespace_test");
    write_source(
        &root.join("nested.rss"),
        r#"
        use math;
        pub fn run() -> float { math::sqrt(81) }
    "#,
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::nested as nested;
        nested::run();
    "#,
        "main source",
    );

    let compiled =
        compile_source_file(&main_path).expect("nested host namespace import should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Float(9.0)]);

    remove_module_root(&root);
}

#[test]
fn frame_local_dispatch_module_split_pressure_is_bounded() {
    // The same 77-function/32-branch call graph as the single-file frame-local
    // dispatch test, split across semantic modules. Named-call pressure must
    // be independent of import discovery order and linker local-base
    // assignment: callee body footprints stay inside their own frames.
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("modules")
        .join("frame_local_dispatch");
    let main_path = fixture_root.join("main.rss");
    let compiled = compile_source_file(&main_path)
        .expect("frame-local module dispatch program should compile");
    assert!(
        compiled.locals <= 100,
        "aggregate frame locals should stay within per-frame pressure plus callable slots, got {}",
        compiled.locals
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1), Value::Int(32)]);
}

#[test]
fn named_callable_materialization_module_split_same_name_materialization() {
    // Two modules each declare a private `helper` with the same source name.
    // Milestone 5 classification follows the resolved function identity, and
    // milestone 6 lowering keeps every named function's prototype while
    // omitting hidden slots for the direct-only helpers: each module's
    // exported `run` stays materialized, and each module's `run` calls its
    // own helper through the direct script-call path.
    let root = temp_module_root("named_callable_materialization_same_name");
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");
    write_source(
        &a_dir.join("util.rss"),
        "pub fn run() { helper(); }\nfn helper() { 11; }\n",
        "a/util source",
    );
    write_source(
        &b_dir.join("util.rss"),
        "pub fn run() { helper(); }\nfn helper() { 22; }\n",
        "b/util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::run();\nbu::run();\n",
        "main source",
    );

    let compiled =
        compile_source_file(&main_path).expect("same-named module helpers should compile");
    let program = &compiled.program;
    assert_eq!(
        program.callable_prototypes.len(),
        4,
        "each module's run and each module's same-named helper keep a prototype"
    );
    assert_eq!(
        program.root_callable_bindings.len(),
        2,
        "only the exported run functions stay materialized with root bindings"
    );
    assert_eq!(
        program
            .callable_prototypes
            .iter()
            .filter(|prototype| prototype.self_slot.is_some())
            .count(),
        2,
        "only the exported run functions keep their runtime self slot"
    );
    assert_eq!(
        program
            .callable_prototypes
            .iter()
            .filter(|prototype| prototype.self_slot.is_none())
            .count(),
        2,
        "the direct-only same-named helpers keep no runtime self slot"
    );
    assert_eq!(
        program.code.iter().filter(|byte| **byte == 0x1A).count(),
        2,
        "each module's run calls its own helper through CallScript"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(11), Value::Int(22)],
        "each module's run must resolve its own same-named helper"
    );

    remove_module_root(&root);
}

/// Compile an in-memory root source with inline module overrides and return
/// the compiled program. Module overrides map module paths (relative to the
/// root module directory) to source text.
fn compile_with_module_overrides(
    root_source: &str,
    overrides: &[(&str, &str)],
) -> vm::CompiledProgram {
    let mut options = CompileSourceFileOptions::new();
    for (path, source) in overrides {
        options = options.with_module_override_source(*path, *source);
    }
    vm::compile_source_with_flavor_and_options(root_source, SourceFlavor::RustScript, options)
        .expect("root source with module overrides should compile")
}

/// Assert that EVERY callable prototype whose schema parameters equal
/// `params` declares the same callable schema with result
/// `expected_result`, and that at least one such prototype exists. Returns
/// the number of matching prototypes so callers can pin the expected count.
///
/// The assertion deliberately covers all matches instead of picking the
/// first one: a merged module graph can contain several functions with
/// identical parameter schemas (the `call`/`dispatch` fixtures are both
/// `(map, map)`), and a first-match lookup would silently validate only
/// one of them. Script prototypes carry no source name, so the strongest
/// available contract is schema identity across every prototype that
/// shares the same declared parameters.
fn assert_all_prototypes_with_params(
    program: &vm::Program,
    params: &[vm::compiler::TypeSchema],
    expected_result: &vm::compiler::TypeSchema,
) -> usize {
    let matches = program
        .callable_prototypes
        .iter()
        .filter(|prototype| {
            matches!(
                prototype.schema.as_ref(),
                Some(vm::compiler::TypeSchema::Callable { params: candidate, .. })
                    if candidate == params
            )
        })
        .collect::<Vec<_>>();
    assert!(
        !matches.is_empty(),
        "no callable prototype carries schema params {params:?}"
    );
    for prototype in &matches {
        match prototype.schema.as_ref() {
            Some(vm::compiler::TypeSchema::Callable { result, .. }) => {
                assert_eq!(
                    result.as_ref(),
                    expected_result,
                    "every prototype with schema params {params:?} must declare the same result"
                );
            }
            other => panic!("unexpected non-callable schema on prototype: {other:?}"),
        }
    }
    matches.len()
}

fn assert_result_map_kind(vm: &Vm, expected_kind: &str) {
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("kind")),
                Some(&Value::string(expected_kind)),
                "result map must carry kind {expected_kind:?}"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

fn assert_result_map_has_kind(vm: &Vm) {
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert!(
                map.get(&Value::string("kind")).is_some(),
                "result map must carry a kind key, got {map:?}"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1: a cross-module accessor returning an array, passed into a local
/// script function with a declared `fn(string, array) -> string` schema,
/// must keep the callee's prototype schema and execute.
///
/// Ports `root_splice.rss` + `chain_m1.rss` from the A3 provider repro set.
#[test]
fn module_callable_schema_preserves_cross_module_array_argument() {
    let root_source = r#"
        use self::chain_m1 as types;

        pub fn run(context: map) -> map {
            let request: map = context["request"];
            let tools: array = types::request_array(request, "tools");
            let body: string = splice("{ }", tools);
            { kind: "ok", body: body }
        }

        fn splice(body: string, tools: array) -> string {
            body
        }

        let result: map = run({
            request: {
                tools: [
                    { name: "read_file", description: "read", schema_json: "{}" }
                ]
            }
        });
        result;
    "#;
    let chain_m1 = r#"
        pub fn request_array(request: map, key: string) -> array {
            let mut items: array = [];
            if request.has(key) {
                if type(request[key]) == "array" {
                    let coerced: array = request[key];
                    items = coerced;
                }
            }
            items
        }
    "#;
    let compiled = compile_with_module_overrides(root_source, &[("chain_m1.rss", chain_m1)]);
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::String,
                vm::compiler::TypeSchema::Array(Box::new(vm::compiler::TypeSchema::Unknown)),
            ],
            &vm::compiler::TypeSchema::String,
        ),
        1,
        "only splice declares (string, array) and it must keep its string result"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm
        .run()
        .expect("cross-module array argument call should run");
    assert_eq!(status, VmStatus::Halted);
    assert_result_map_kind(&vm, "ok");
}

/// B1 control: the same call with a literal array argument must pass.
/// Ports `root_splice2.rss`.
#[test]
fn module_callable_schema_literal_array_control() {
    let root_source = r#"
        pub fn run(context: map) -> map {
            let body: string = splice("{ }", []);
            { kind: "ok", body: body }
        }

        fn splice(body: string, tools: array) -> string {
            body
        }

        let result: map = run({});
        result;
    "#;
    let compiled = compile_source(root_source).expect("literal array control should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("literal array control should run");
    assert_eq!(status, VmStatus::Halted);
    assert_result_map_kind(&vm, "ok");
}

/// B1: a two-map module function that string-reads its FIRST map parameter
/// and passes the second onward must keep every declared parameter in source
/// order and execute. Ports the `hop4` behavior (`hop4_root.rss` +
/// `hop4_m2.rss` + `chain_m1.rss`).
#[test]
fn module_callable_schema_preserves_first_map_parameter() {
    let root_source = r#"
        use self::hop4_m2 as adapter;

        pub fn run(context: map) -> map {
            let request: map = context["request"];
            let profile: map = context["profile"];
            adapter::call(request, profile)
        }

        let result: map = run({
            request: { model: "m" },
            profile: { base_url: "http://127.0.0.1:1", api_key: "k", provider: "p" }
        });
        result;
    "#;
    let hop4_m2 = r#"
        use self::chain_m1 as types;

        pub fn call(request: map, profile: map) -> map {
            let stream: bool = false;
            if stream => {
                { kind: "stream" }
            } else => {
                dispatch(profile, request)
            }
        }

        fn dispatch(profile: map, request: map) -> map {
            let base_url: string = types::request_string(profile, "base_url");
            let api_key: string = types::request_string(profile, "api_key");
            let provider: string = types::request_string(profile, "provider");
            complete(request, base_url, api_key, provider)
        }

        fn complete(request: map, base_url: string, api_key: string, provider: string) -> map {
            let model: string = types::request_string(request, "model");
            let body_text: string = local_helper(request, model, false);
            if model == "" => {
                { kind: "missing" }
            } else => {
                { kind: "ok", body: body_text, url: base_url, provider: provider }
            }
        }

        fn local_helper(request: map, model: string, stream: bool) -> string {
            "stub"
        }
    "#;
    let chain_m1 = r#"
        pub fn request_string(request: map, key: string) -> string {
            let mut text: string = "";
            if request.has(key) {
                if type(request[key]) == "string" {
                    let coerced: string = request[key];
                    text = coerced;
                }
            }
            text
        }
    "#;
    let compiled = compile_with_module_overrides(
        root_source,
        &[("hop4_m2.rss", hop4_m2), ("chain_m1.rss", chain_m1)],
    );
    // All declared map parameters stay in source order: `dispatch` is
    // (map, map), `complete` is (map, string, string, string). The
    // (map, map) parameter list is shared by `call` and `dispatch`, so the
    // assertion must cover every matching prototype instead of picking the
    // first one — both must keep their `(map, map) -> map` schema.
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
            ],
            &vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
        ),
        2,
        "call and dispatch both declare (map, map) -> map and keep their schemas"
    );
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
                vm::compiler::TypeSchema::String,
                vm::compiler::TypeSchema::String,
                vm::compiler::TypeSchema::String,
            ],
            &vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
        ),
        1,
        "complete must keep its (map, string, string, string) -> map schema"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm
        .run()
        .expect("first-map-parameter module graph should run");
    assert_eq!(status, VmStatus::Halted);
    assert_result_map_kind(&vm, "ok");
}

/// B1 control: the same two-map layout reading the SECOND map parameter
/// passes. Ports the `hop13` behavior (`hop13_root.rss` + `hop13_m2.rss` +
/// `chain_m1.rss`).
#[test]
fn module_callable_schema_second_parameter_control() {
    let root_source = r#"
        use self::hop13_m2 as adapter;

        pub fn run(context: map) -> map {
            let request: map = context["request"];
            let profile: map = context["profile"];
            adapter::call(request, profile)
        }

        let result: map = run({
            request: { model: "m" },
            profile: { base_url: "http://127.0.0.1:1", api_key: "k", provider: "p" }
        });
        result;
    "#;
    let hop13_m2 = r#"
        use self::chain_m1 as types;

        pub fn call(request: map, profile: map) -> map {
            let stream: bool = false;
            if stream => {
                { kind: "stream" }
            } else => {
                dispatch(profile, request)
            }
        }

        fn dispatch(profile: map, request: map) -> map {
            let model: string = types::request_string(request, "model");
            complete(profile, "u", "k", "p")
        }

        fn complete(request: map, base_url: string, api_key: string, provider: string) -> map {
            let model: string = types::request_string(request, "model");
            let result: map = if model == "" => {
                { kind: "missing" }
            } else => {
                { kind: "ok", url: base_url, key: api_key, provider: provider }
            };
            result
        }
    "#;
    let chain_m1 = r#"
        pub fn request_string(request: map, key: string) -> string {
            let mut text: string = "";
            if request.has(key) {
                if type(request[key]) == "string" {
                    let coerced: string = request[key];
                    text = coerced;
                }
            }
            text
        }
    "#;
    let compiled = compile_with_module_overrides(
        root_source,
        &[("hop13_m2.rss", hop13_m2), ("chain_m1.rss", chain_m1)],
    );
    // Same merged-graph schema contract as the first-map fixture: every
    // (map, map) prototype — `call` and `dispatch` — must keep its
    // `(map, map) -> map` schema, and `complete` its four-parameter one.
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
            ],
            &vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
        ),
        2,
        "call and dispatch both declare (map, map) -> map and keep their schemas"
    );
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
                vm::compiler::TypeSchema::String,
                vm::compiler::TypeSchema::String,
                vm::compiler::TypeSchema::String,
            ],
            &vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
        ),
        1,
        "complete must keep its (map, string, string, string) -> map schema"
    );
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("second-map-parameter control should run");
    assert_eq!(status, VmStatus::Halted);
    // The fixture's `complete(profile, ...)` reads `model` from the profile
    // map (absent), so the semantic result is `missing`; the control's
    // contract is that the merged call graph executes without a callable
    // schema mismatch.
    assert_result_map_has_kind(&vm);
}

/// B1: VMBC round-trip must preserve every script prototype's callable
/// schema for a merged module graph, and the decoded program must execute.
#[test]
fn callable_schema_survives_vmbc_round_trip_for_merged_modules() {
    let root_source = r#"
        use self::chain_m1 as types;

        pub fn run(context: map) -> map {
            let request: map = context["request"];
            let tools: array = types::request_array(request, "tools");
            let body: string = splice("{ }", tools);
            { kind: "ok", body: body }
        }

        fn splice(body: string, tools: array) -> string {
            body
        }

        let result: map = run({
            request: {
                tools: [
                    { name: "read_file", description: "read", schema_json: "{}" }
                ]
            }
        });
        result;
    "#;
    let chain_m1 = r#"
        pub fn request_array(request: map, key: string) -> array {
            let mut items: array = [];
            if request.has(key) {
                if type(request[key]) == "array" {
                    let coerced: array = request[key];
                    items = coerced;
                }
            }
            items
        }
    "#;
    let compiled = compile_with_module_overrides(root_source, &[("chain_m1.rss", chain_m1)]);
    let encoded = vm::encode_program(&compiled.program).expect("merged program should encode");
    let decoded = vm::decode_program(&encoded).expect("merged program should decode");
    assert_eq!(
        decoded.callable_prototypes.len(),
        compiled.program.callable_prototypes.len(),
        "round trip must preserve the prototype count"
    );
    for (before, after) in compiled
        .program
        .callable_prototypes
        .iter()
        .zip(&decoded.callable_prototypes)
    {
        assert_eq!(
            before.schema, after.schema,
            "round trip must preserve prototype schemas"
        );
    }
    vm::validate_program(&decoded, 0).expect("decoded merged program should validate");

    let mut vm = Vm::new(decoded);
    let status = vm.run().expect("decoded merged program should run");
    assert_eq!(status, VmStatus::Halted);
    assert_result_map_kind(&vm, "ok");
}

/// B1: a merged module graph must still enforce the callee's callable
/// schema at runtime. The module accessor `request_value` declares no
/// return schema, so the root binding `tools: array` accepts the module's
/// map value statically; the actual runtime value is a map, and the
/// `splice(string, array)` call must fail with the precise
/// `TypeMismatch("callable argument schema")` error instead of passing
/// silently or corrupting operand placement.
#[test]
fn merged_module_graph_wrong_argument_reports_callable_argument_schema_mismatch() {
    let root_source = r#"
        use self::chain_m1 as types;

        pub fn run(context: map) -> map {
            let request: map = context["request"];
            let tools: array = types::request_value(request, "tools");
            let body: string = splice("{ }", tools);
            { kind: "ok", body: body }
        }

        fn splice(body: string, tools: array) -> string {
            body
        }

        let result: map = run({
            request: {
                tools: { name: "read_file", description: "read", schema_json: "{}" }
            }
        });
        result;
    "#;
    let chain_m1 = r#"
        pub fn request_value(request: map, key: string) {
            request[key]
        }
    "#;
    let compiled = compile_with_module_overrides(root_source, &[("chain_m1.rss", chain_m1)]);
    // The merged graph still carries splice's (string, array) -> string schema.
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::String,
                vm::compiler::TypeSchema::Array(Box::new(vm::compiler::TypeSchema::Unknown)),
            ],
            &vm::compiler::TypeSchema::String,
        ),
        1,
        "splice must keep its (string, array) -> string schema in the merged graph"
    );
    let mut vm = Vm::new(compiled.program);
    assert!(matches!(
        vm.run(),
        Err(vm::VmError::TypeMismatch("callable argument schema"))
    ));
}

/// B1: the liveness allocator only compacts once the merged program's
/// local count exceeds `LOCAL_SLOT_ALLOCATOR_COMPAT_THRESHOLD` (8). This
/// fixture proves the frame layout really sits beyond that threshold:
/// `wide` declares ten parameters, and because every parameter stays live
/// for the whole body, compaction must keep ten distinct physical slots
/// (parameter_slots pairwise distinct, compacted frame above 8) and the
/// call site must place each operand in its own slot. The tenth parameter
/// `j` is never used by the body — exactly the dead-parameter shape that
/// used to let the colorer alias two parameters onto one physical slot and
/// corrupt operand placement at the call site.
#[test]
fn wide_frame_exceeds_liveness_compaction_threshold() {
    let root_source = r#"
        pub fn run(context: map) -> map {
            let text: string = wide(
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j"
            );
            { kind: "ok", text: text }
        }

        fn wide(
            a: string, b: string, c: string, d: string,
            e: string, f: string, g: string, h: string,
            i: string, j: string
        ) -> string {
            let s1: string = a;
            let s2: string = b;
            let s3: string = c;
            let s4: string = d;
            let s5: string = e;
            let s6: string = f;
            let s7: string = g;
            let s8: string = h;
            let s9: string = i;
            s1 + s2 + s3 + s4 + s5 + s6 + s7 + s8 + s9
        }

        let result: map = run({});
        result;
    "#;
    let compiled = compile_source(root_source).expect("wide-frame fixture should compile");
    let string_params =
        std::iter::repeat_n(vm::compiler::TypeSchema::String, 10).collect::<Vec<_>>();
    let wide_prototypes = compiled
        .program
        .callable_prototypes
        .iter()
        .filter(|prototype| {
            matches!(
                prototype.schema.as_ref(),
                Some(vm::compiler::TypeSchema::Callable { params: candidate, .. })
                    if *candidate == string_params
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        wide_prototypes.len(),
        1,
        "exactly one prototype declares ten string parameters"
    );
    let wide = wide_prototypes[0];
    let distinct_param_slots = wide
        .parameter_slots
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        distinct_param_slots.len(),
        10,
        "every parameter must keep a distinct physical slot, got {:?}",
        wide.parameter_slots
    );
    assert!(
        compiled.program.local_count > 8,
        "the compacted frame ({}) must exceed the liveness compaction threshold of 8",
        compiled.program.local_count
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("wide-frame call should run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(map.get(&Value::string("kind")), Some(&Value::string("ok")));
            assert_eq!(
                map.get(&Value::string("text")),
                Some(&Value::string("abcdefghi")),
                "operand placement must survive compaction: j is unused, a..i must keep their values"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 A/B contract: a root-module function and an imported-module function
/// with identical signatures must carry identical callable schemas in the
/// merged program, and both call sites must execute. The root `root_ident`
/// and the module `ident` both declare `(map, string) -> string`; the
/// merged graph must contain both prototypes with that schema (the root
/// one and the non-root one), each keeping its declared result.
#[test]
fn root_and_module_functions_share_schema_ab_contract() {
    let root_source = r#"
        use self::chain_m1 as types;

        pub fn run(context: map) -> map {
            let local: string = root_ident(context, "local");
            let remote: string = types::ident(context, "remote");
            { kind: "ok", local: local, remote: remote }
        }

        fn root_ident(context: map, key: string) -> string {
            let text: string = context[key];
            text
        }

        let result: map = run({
            local: "L",
            remote: "R"
        });
        result;
    "#;
    let chain_m1 = r#"
        pub fn ident(context: map, key: string) -> string {
            let text: string = context[key];
            text
        }
    "#;
    let compiled = compile_with_module_overrides(root_source, &[("chain_m1.rss", chain_m1)]);
    // Both the root `ident` and the module `ident` share the same
    // (map, string) -> string schema; both must be present and identical.
    assert_eq!(
        assert_all_prototypes_with_params(
            &compiled.program,
            &[
                vm::compiler::TypeSchema::Map(Box::new(vm::compiler::TypeSchema::Unknown)),
                vm::compiler::TypeSchema::String,
            ],
            &vm::compiler::TypeSchema::String,
        ),
        2,
        "root and module ident must both keep their (map, string) -> string schema"
    );
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("root and module ident calls should run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(map.get(&Value::string("local")), Some(&Value::string("L")));
            assert_eq!(map.get(&Value::string("remote")), Some(&Value::string("R")));
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 follow-up: a local defined after body entry must never be colored
/// onto a parameter slot. The five-parameter caller below uses every
/// parameter, defines body locals (`body`, `status`, `tag`, `result`)
/// after entry, and dispatches through a statement-if to the imported
/// parse helper (sibling dispatch between the two imported modules). At
/// `d8cf291` this shape fails the VM callable-schema check with
/// `TypeMismatch("string")` (`type mismatch: expected string`) because a
/// body-defined local aliases a parameter slot, so the callee frame reads
/// the wrong slot while evaluating call arguments even though every value
/// is correctly typed.
///
/// Minimal cross-module repro: root -> adapter (five-parameter caller) ->
/// parse (schema-typed helper). The two-parameter control variant passes
/// at the same revision, isolating the corruption to the caller's
/// parameter-slot layout. The parse module carries no json/bytes/loop
/// machinery and the dispatch if has no else branch; only the minimal
/// strict-typing accessors remain.
#[test]
fn body_defined_local_never_aliases_parameter_slot() {
    let root_source = r#"
        use self::param_aliasing_m2 as adapter;

        pub fn run(context: map) -> map {
            let request: map = context["request"];
            adapter::chat_send_complete(request, "m", "http://127.0.0.1:1", "k", "p")
        }

        let result: map = run({
            request: { model: "m" }
        });
        result;
    "#;
    let m2 = r#"
        use self::param_aliasing_parse as parse;

        pub fn chat_send_complete(
            request: map,
            model: string,
            base_url: string,
            api_key: string,
            provider: string
        ) -> map {
            let body: map = {
                choices: [
                    { message: { role: "assistant", content: "hi" } }
                ],
                usage: { total_tokens: 27 }
            };
            let status: int = 200;
            let tag: string = model + base_url + api_key;
            let mut result: map = {};
            if status >= 200 && status < 300 {
                result = parse::parse_body(body, status, provider);
            }
            result
        }
    "#;
    let parse = r#"
        pub fn parse_body(body: map, status: int, provider: string) -> map {
            let choices: array = request_array(body, "choices");
            let first: map = array_entry(choices, 0);
            let message: map = request_map(first, "message");
            let content: string = request_string(message, "content");
            { ok: true, response: { text: content, provider: provider }, error: {} }
        }

        pub fn request_array(request: map, key: string) -> array {
            let mut items: array = [];
            if request.has(key) {
                if type(request[key]) == "array" {
                    let coerced: array = request[key];
                    items = coerced;
                }
            }
            items
        }

        pub fn request_string(request: map, key: string) -> string {
            let mut text: string = "";
            if request.has(key) {
                if type(request[key]) == "string" {
                    let coerced: string = request[key];
                    text = coerced;
                }
            }
            text
        }

        pub fn request_map(request: map, key: string) -> map {
            let mut items: map = {};
            if request.has(key) {
                if type(request[key]) == "map" {
                    let coerced: map = request[key];
                    items = coerced;
                }
            }
            items
        }

        pub fn array_entry(items: array, index: int) -> map {
            let mut result: map = {};
            if items.has(index) {
                if type(items[index].copy()) == "map" {
                    let coerced: map = items[index].copy();
                    result = coerced;
                }
            }
            result
        }
    "#;
    let compiled = compile_with_module_overrides(
        root_source,
        &[
            ("param_aliasing_m2.rss", m2),
            ("param_aliasing_parse.rss", parse),
        ],
    );
    // The five-parameter caller keeps one distinct physical slot per
    // parameter, and every body-defined local (`body`, `status`, `tag`,
    // `result`) must land on a slot that no parameter uses: the callee
    // frame reads parameter slots while evaluating the parse call's
    // arguments, so a body local sharing a parameter slot corrupts the
    // operand placement even though every value is correctly typed.
    let string_params =
        std::iter::repeat_n(vm::compiler::TypeSchema::String, 4).collect::<Vec<_>>();
    let caller_prototypes = compiled
        .program
        .callable_prototypes
        .iter()
        .filter(|prototype| {
            matches!(
                prototype.schema.as_ref(),
                Some(vm::compiler::TypeSchema::Callable { params: candidate, .. })
                    if candidate.len() == 5
                        && candidate[1..] == string_params[..]
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        caller_prototypes.len(),
        1,
        "exactly one prototype declares the five-parameter caller shape"
    );
    let param_slots = caller_prototypes[0].parameter_slots.clone();
    let distinct_param_slots = param_slots
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        distinct_param_slots.len(),
        5,
        "every parameter must keep a distinct physical slot, got {:?}",
        param_slots
    );
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");
    // Imported-module locals carry module-qualified names in debug info
    // (e.g. `..._param_aliasing_m2_rss__m1::body`); look up the
    // five-parameter caller's body locals by their qualified suffix.
    for local in ["body", "status", "tag", "result"] {
        let slot = debug
            .locals
            .iter()
            .find(|info| {
                info.name.contains("param_aliasing_m2")
                    && info.name.ends_with(&format!("::{local}"))
            })
            .unwrap_or_else(|| panic!("{local} should be in debug info"))
            .index as u16;
        assert!(
            !param_slots.contains(&slot),
            "body-defined local {local} must not share its final slot with a parameter: params {param_slots:?}, {local} at {slot}"
        );
    }

    let mut vm = Vm::new(compiled.program);
    let status = vm
        .run()
        .expect("five-parameter caller with body-defined locals must run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("ok")),
                Some(&Value::Bool(true)),
                "the success path must return ok(...)"
            );
            match map.get(&Value::string("response")) {
                Some(Value::Map(response)) => {
                    assert_eq!(
                        response.get(&Value::string("text")),
                        Some(&Value::string("hi")),
                        "the parsed response text must survive parameter-slot coloring"
                    );
                    assert_eq!(
                        response.get(&Value::string("provider")),
                        Some(&Value::string("p")),
                        "the fifth parameter must survive parameter-slot coloring"
                    );
                }
                other => panic!("expected a response map, got {other:?}"),
            }
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 follow-up smoke guard: parameter interference must be scoped to
/// parameter slots, not a global freeze of slot coloring. `mixed` declares
/// six parameters (each live for the whole body, so each needs its own
/// physical slot) and six body locals whose live ranges overlap at most
/// two deep (`s1` dies when `s2` is defined, and so on). The allocator must
/// still compact the locals onto a shared pair of slots, keeping the
/// compacted frame strictly below the twelve slots `mixed` alone declares.
///
/// Smoke only: on the base compiler (no full-body parameter rule) the
/// locals compact at least as well, so this fixture cannot be RED there —
/// it guards against future over-conservatism (an all-interfere coloring or
/// a disabled allocator) rather than pinning a base defect.
#[test]
fn parameter_interference_preserves_local_slot_compaction_smoke() {
    let source = r#"
        pub fn run(context: map) -> map {
            let text: string = mixed("a", "b", "c", "d", "e", "f");
            { kind: "ok", text: text }
        }

        fn mixed(
            a: string, b: string, c: string,
            d: string, e: string, f: string
        ) -> string {
            let s1: string = a + "1";
            let s2: string = s1 + b;
            let s3: string = s2 + c;
            let s4: string = s3 + d;
            let s5: string = s4 + e;
            let s6: string = s5 + f;
            s6
        }

        let result: map = run({});
        result;
    "#;
    let compiled = compile_source(source).expect("boundary fixture should compile");
    let string_params =
        std::iter::repeat_n(vm::compiler::TypeSchema::String, 6).collect::<Vec<_>>();
    let mixed_prototypes = compiled
        .program
        .callable_prototypes
        .iter()
        .filter(|prototype| {
            matches!(
                prototype.schema.as_ref(),
                Some(vm::compiler::TypeSchema::Callable { params: candidate, .. })
                    if *candidate == string_params
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mixed_prototypes.len(),
        1,
        "exactly one prototype declares six string parameters"
    );
    let distinct_param_slots = mixed_prototypes[0]
        .parameter_slots
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        distinct_param_slots.len(),
        6,
        "every parameter must keep a distinct physical slot, got {:?}",
        mixed_prototypes[0].parameter_slots
    );
    // `mixed` alone declares twelve pre-compaction slots (six parameters +
    // six locals). Locals with two-deep overlap must share physical slots,
    // so the compacted program stays well below twelve; an all-interfere
    // coloring or a disabled allocator would exceed it.
    assert!(
        compiled.program.local_count < 12,
        "non-parameter locals must still be compacted: compacted frame {} must stay below the twelve slots mixed declares",
        compiled.program.local_count
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("boundary fixture should run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(map.get(&Value::string("kind")), Some(&Value::string("ok")));
            assert_eq!(
                map.get(&Value::string("text")),
                Some(&Value::string("a1bcdef")),
                "chained local values must survive compaction: {:?}",
                map
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 follow-up: closure parameters must stay live for the whole closure
/// body, exactly like named-function parameters. The closure below declares
/// one parameter (`x`), defines a body local (`local`) before reading the
/// parameter, and returns the concatenation. If the body local were colored
/// onto the parameter's physical slot, invoking the closure would read the
/// local's value instead of the argument, so the returned string would be
/// wrong.
///
/// The closure is deliberately never invoked from the script: source-level
/// closure invocation lowers to a dynamic `LocalCall`, whose conservative
/// liveness fill (keep every slot live across a dynamic call) masks the
/// aliasing defect on the pre-fix compiler by making every slot interfere
/// with every other slot. `run` takes no parameters and defines its own
/// locals only *after* the closure, so nothing is live at the closure's
/// definition site on the pre-fix compiler: the closure's parameter and its
/// body local receive no interference edges at all and are colored onto the
/// same physical slot. The slot-level assertion pins the compile-time
/// invariant directly: the closure's parameter slot and its body local's
/// final slot stay distinct.
#[test]
fn closure_parameter_stays_live_for_whole_closure_body() {
    let source = r#"
        pub fn run() -> map {
            let f = |x| if true => {
                let local: string = "zz";
                local + x
            } else => {
                "?"
            };
            let a: string = "a";
            let b: string = a + "b";
            let c: string = b + "c";
            let d: string = c + "d";
            let out: string = d;
            { ok: out }
        }
        let result: map = run();
        result;
    "#;
    let compiled = compile_source(source).expect("closure fixture should compile");
    let closure_prototypes = compiled
        .program
        .callable_prototypes
        .iter()
        .filter(|prototype| prototype.kind == vm::CallableKind::Closure)
        .collect::<Vec<_>>();
    assert_eq!(
        closure_prototypes.len(),
        1,
        "exactly one closure prototype should be emitted"
    );
    let param_slots = closure_prototypes[0].parameter_slots.clone();
    assert_eq!(param_slots.len(), 1);
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");
    let local_slot = debug
        .locals
        .iter()
        .find(|info| info.name == "local")
        .expect("body local should be in debug info")
        .index as u16;
    assert_ne!(
        param_slots[0], local_slot,
        "the closure body local must not share the parameter's physical slot: param {param_slots:?}, local at {local_slot}"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("closure fixture should run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("ok")),
                Some(&Value::string("abcd")),
                "the enclosing frame must stay correct beside the closure"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 follow-up: nested closures keep their parameter protection scoped to
/// their own bodies. The outer closure (`f`, parameter `x`) creates the
/// inner closure (`g`, parameters `y`, `z`) inside its body; `g`'s body
/// writes its own local before reading its parameters. The inner closure's
/// parameters must stay distinct from its own body local, and the outer
/// closure's protection must never leak into the inner closure's frame (and
/// vice versa).
///
/// Neither closure is invoked from the script (see
/// `closure_parameter_stays_live_for_whole_closure_body` for why
/// source-level invocation would mask the aliasing defect on the pre-fix
/// compiler). The inner closure captures nothing and the outer body's tail
/// is a constant, so on the pre-fix compiler nothing is live at the inner
/// closure's definition site: `g`'s parameters and `inner_local` receive no
/// interference edges and are colored onto the same physical slots. The
/// slot-level assertion pins the invariant directly: each of `g`'s
/// parameter slots stays distinct from `inner_local`'s final slot.
#[test]
fn nested_closure_parameters_stay_scoped_to_own_bodies() {
    let source = r#"
        pub fn run() -> map {
            let f = |x| if true => {
                let outer_local: string = "O";
                let g = |y, z| if true => {
                    let inner_local: string = "I";
                    inner_local + y + z
                } else => {
                    "?"
                };
                "done"
            } else => {
                "?"
            };
            let a: string = "a";
            let b: string = a + "b";
            let c: string = b + "c";
            let d: string = c + "d";
            let out: string = d;
            { ok: out }
        }
        let result: map = run();
        result;
    "#;
    let compiled = compile_source(source).expect("nested closure fixture should compile");
    let closure_prototypes = compiled
        .program
        .callable_prototypes
        .iter()
        .filter(|prototype| prototype.kind == vm::CallableKind::Closure)
        .collect::<Vec<_>>();
    assert_eq!(
        closure_prototypes.len(),
        2,
        "exactly two closure prototypes should be emitted"
    );
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");
    // The inner closure declares two parameters; the outer closure declares
    // one. Assert the inner closure's parameters stay distinct from its body
    // local.
    let inner = closure_prototypes
        .iter()
        .find(|prototype| prototype.parameter_slots.len() == 2)
        .expect("inner closure should declare two parameters");
    let inner_param_slots = inner.parameter_slots.clone();
    let inner_local_slot = debug
        .locals
        .iter()
        .find(|info| info.name == "inner_local")
        .expect("inner_local should be in debug info")
        .index as u16;
    assert!(
        !inner_param_slots.contains(&inner_local_slot),
        "the inner closure body local must not share a parameter's physical slot: params {inner_param_slots:?}, inner_local at {inner_local_slot}"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("nested closure fixture should run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("ok")),
                Some(&Value::string("abcd")),
                "the enclosing frame must stay correct beside nested closures"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 follow-up smoke guard: the full-body parameter rule must survive
/// `Assign` statements that target a parameter slot. `mixed` defines a body
/// local before reassigning its parameter, and reads the local afterwards.
/// The allocator keeps parameter slots live for the whole body as a
/// conservative safety rule for caller-written frame-entry state, so the
/// local and the parameter stay distinct and the result survives.
///
/// Smoke only: on the base compiler the reassignment's def-edge already
/// separates the parameter from anything live after the assignment, so this
/// fixture cannot be RED there — it guards the full-body rule against future
/// regressions rather than pinning a base defect.
#[test]
fn assign_to_parameter_keeps_full_body_interference_smoke() {
    let source = r#"
        fn mixed(a: string) -> string {
            let c: string = "x";
            a = "fixed";
            c + "!"
        }
        pub fn run(context: map) -> map {
            let out: string = mixed("orig");
            let tag: string = "t";
            let t2: string = tag + "!";
            let u1: string = t2 + "u";
            let u2: string = u1 + "v";
            { ok: out, t: u2 }
        }
        let result: map = run({});
        result;
    "#;
    let compiled = compile_source(source).expect("assign-to-param fixture should compile");
    // The (string) -> string prototype is uniquely `mixed` (run takes a map).
    let string_params =
        std::iter::repeat_n(vm::compiler::TypeSchema::String, 1).collect::<Vec<_>>();
    let mixed_prototypes = compiled
        .program
        .callable_prototypes
        .iter()
        .filter(|prototype| {
            matches!(
                prototype.schema.as_ref(),
                Some(vm::compiler::TypeSchema::Callable { params: candidate, .. })
                    if *candidate == string_params
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mixed_prototypes.len(),
        1,
        "exactly one prototype declares the single-string-parameter shape"
    );
    let param_slots = mixed_prototypes[0].parameter_slots.clone();
    assert_eq!(param_slots.len(), 1);
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");
    let local_slot = debug
        .locals
        .iter()
        .find(|info| info.name == "c")
        .expect("body local c should be in debug info")
        .index as u16;
    assert_ne!(
        param_slots[0], local_slot,
        "the body local must not share the parameter's physical slot even after an assign-to-param: param {param_slots:?}, c at {local_slot}"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("assign-to-param fixture should run");
    assert_eq!(status, VmStatus::Halted);
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("ok")),
                Some(&Value::string("x!")),
                "the pre-assign local must survive the parameter reassignment"
            );
            assert_eq!(
                map.get(&Value::string("t")),
                Some(&Value::string("t!uv")),
                "the enclosing frame must stay correct beside the assign-to-param"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// B1 follow-up boundary: parameter-heavy frames near the 256-slot limit
/// must fail with the existing typed compile error when coloring cannot
/// proceed — never a panic and never a miscompiled program. 250
/// parameters (each kept live for the whole body by the full-body
/// parameter rule) plus seven simultaneously live body locals exceed the
/// 256 physical slots the allocator can color, so compilation reports the
/// typed "too many simultaneously live locals" error.
#[test]
fn parameter_heavy_frame_near_boundary_returns_typed_error() {
    let param_count = 250;
    let local_count = 7;
    let mut source = String::from("fn crowded(");
    for idx in 0..param_count {
        if idx > 0 {
            source.push_str(", ");
        }
        source.push_str(&format!("p{idx}"));
    }
    source.push_str(") -> int {\n");
    for idx in 0..local_count {
        source.push_str(&format!("    let v{idx} = {idx};\n"));
    }
    source.push_str("    ");
    for idx in 0..param_count.min(8) {
        if idx > 0 {
            source.push_str(" + ");
        }
        source.push_str(&format!("p{idx}"));
    }
    for idx in 0..local_count {
        source.push_str(&format!(" + v{idx}"));
    }
    source.push_str(";\n}\ncrowded(");
    for idx in 0..param_count {
        if idx > 0 {
            source.push_str(", ");
        }
        source.push_str(&format!("{idx}"));
    }
    source.push_str(");\n");

    let err = match compile_source(&source) {
        Ok(_) => panic!("compile should fail with the frame-local limit"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse_err) => {
            assert!(
                parse_err
                    .message
                    .contains("too many simultaneously live locals"),
                "unexpected parse error: {parse_err:?}"
            );
        }
        other => panic!("expected parse error, got {other:?}"),
    }
}

/// B1 follow-up boundary: near the 256-slot limit, non-parameter locals
/// must still compact. `spacious` declares 200 parameters (each keeping
/// its own physical slot under the full-body rule) plus 100 chained body
/// locals whose live ranges overlap at most two deep, and the top level
/// additionally calls a closure through a local binding (a dynamic
/// `LocalCall`). The compacted frame must stay below the 300 declared
/// slots, must fit within the 256-slot frame limit, and the chained values
/// must survive.
///
/// RED at base: without the full-body parameter rule *and* with the
/// dynamic-local-call liveness fill, every slot in the program interferes
/// with every other slot, so the 300-slot frame cannot color at all and
/// compilation fails with a spurious "too many simultaneously live locals"
/// error even though no frame needs more than ~205 slots.
#[test]
fn non_param_locals_still_compact_beside_wide_parameter_frames() {
    let param_count = 200;
    let local_count = 100;
    let mut source = String::from("fn spacious(");
    for idx in 0..param_count {
        if idx > 0 {
            source.push_str(", ");
        }
        source.push_str(&format!("p{idx}"));
    }
    source.push_str(") -> int {\n");
    for idx in 0..local_count {
        source.push_str(&format!("    let s{idx} = "));
        if idx == 0 {
            source.push_str("p0");
        } else {
            source.push_str(&format!("s{} + p{idx}", idx - 1));
        }
        source.push_str(";\n");
    }
    source.push_str(&format!(
        "    s{} + p{};\n}}\nspacious(",
        local_count - 1,
        param_count - 1
    ));
    for idx in 0..param_count {
        if idx > 0 {
            source.push_str(", ");
        }
        source.push_str(&format!("{idx}"));
    }
    // A dynamic closure call: without the precise allocator liveness this
    // one `LocalCall` fills every slot live and turns the 300-slot program
    // into one interference clique, failing the 256-slot limit spuriously.
    source.push_str(");\nlet f = |q| q;\nlet g: int = f(1);\n");

    let compiled = compile_source(&source).expect("wide-parameter program should compile");
    assert!(
        compiled.locals < param_count + local_count,
        "chained non-parameter locals must still compact: frame {} must stay below the {} declared slots",
        compiled.locals,
        param_count + local_count
    );
    assert!(
        compiled.locals <= (u8::MAX as usize) + 1,
        "compacted frame {} must fit within the 256-slot frame limit",
        compiled.locals
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("wide-parameter program should run");
    assert_eq!(status, VmStatus::Halted);
    let expected: i64 = (0..local_count as i64).sum::<i64>() + (param_count as i64 - 1);
    assert_eq!(vm.stack(), &[Value::Int(expected)]);
}

/// P2-2 regression: a closure whose body invokes another closure through a
/// dynamic `LocalCall` must not turn the whole program into one interference
/// clique. `run` declares 250 chained locals whose live ranges overlap at
/// most two deep, then calls the closure `f`, whose body creates and calls
/// the inner closure `g` through a local binding. Before the fix the
/// closure-body live-out was seeded with every slot used in the body, and
/// the dynamic-local-call liveness fill kept every slot live across the
/// call, so the whole program became one clique: the frame could not
/// compact and a spurious "too many simultaneously live locals" error fired
/// even though no frame needs more than a handful of slots. The compacted
/// frame must stay well below the declared slots, must fit within the
/// 256-slot frame limit, and the chained values must survive.
#[test]
fn closure_local_call_keeps_unrelated_locals_compact() {
    let local_count = 250;
    let mut source = String::from(
        "pub fn run(context: map) -> map {\n\
         let f = |x| if true => {\n\
             let g = |y| if true => {\n\
                 y + \"?\"\n\
             } else => {\n\
                 \"?\"\n\
             };\n\
             g(x) + \"!\"\n\
         } else => {\n\
             \"?\"\n\
         };\n",
    );
    source.push_str("    let s0: string = \"a\";\n");
    for idx in 1..local_count {
        source.push_str(&format!("    let s{idx}: string = s{} + \"b\";\n", idx - 1));
    }
    source.push_str(&format!(
        "    let out: string = f(s{});\n    {{ ok: out }}\n}}\nlet result: map = run({{}});\nresult;\n",
        local_count - 1
    ));
    let compiled = compile_source(&source).expect("closure LocalCall program should compile");
    assert!(
        compiled.locals < local_count,
        "unrelated chained locals must still compact beside a closure LocalCall: frame {} must stay well below the {} declared slots",
        compiled.locals,
        local_count + 8
    );
    assert!(
        compiled.locals <= (u8::MAX as usize) + 1,
        "compacted frame {} must fit within the 256-slot frame limit",
        compiled.locals
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("closure LocalCall program should run");
    assert_eq!(status, VmStatus::Halted);
    let mut expected = String::from("a");
    for _ in 1..local_count {
        expected.push('b');
    }
    expected.push_str("?!");
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("ok")),
                Some(&Value::string(expected.as_str())),
                "chained local values must survive the closure LocalCall"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}

/// P2 regression: a dynamic `LocalCall` nested inside a plain named-call
/// argument, an optional-access key (`?.[...]`), or an `unwrap_or`
/// fallback must not leak the conservative dynamic-call liveness fill into
/// the allocator's precise path. `run` binds the closure `f` and calls it
/// from exactly those three positions (`helper(f(10))`, `?.[f(20)]`,
/// `.unwrap_or(f(30))`) while 250 chained locals whose live ranges overlap
/// at most two deep are alive. Before the fix `add_expr_uses_impl`
/// descended into `OptionalGet` container/key, `OptionUnwrapOr`
/// value/fallback, and `Expr::Call`/`Expr::ModuleCall` args through the
/// conservative wrapper `add_expr_uses`, so the nested `LocalCall` filled
/// every slot live: the whole program became one interference clique, the
/// chained locals lost compaction, and the frame could not color (a
/// spurious "too many simultaneously live locals" error near the 256-slot
/// limit) even though no frame needs more than a handful of slots. The
/// compacted frame must stay well below the declared slots, must fit
/// within the 256-slot frame limit, and every nested-call result must
/// survive.
#[test]
fn nested_local_call_in_call_arg_optional_key_and_unwrap_fallback_stays_compact() {
    let local_count = 250;
    let mut source = String::from(
        "struct Payload { values: [int] }\n\
         fn helper(x: int) -> int { x + 1 }\n\
         pub fn run(context: map) -> map {\n\
             let f = |x| x + 1;\n\
             let payload: Payload = { values: [1, 2, 3] };\n\
             let a: int = helper(f(10));\n\
             let b: int = payload?.values?.[f(20)].unwrap_or(-1);\n\
             let c: int = payload?.values?.[1].unwrap_or(f(30));\n\
             let s0: string = \"a\";\n",
    );
    for idx in 1..local_count {
        source.push_str(&format!("    let s{idx}: string = s{} + \"b\";\n", idx - 1));
    }
    source.push_str(&format!(
        "    {{ a: a, b: b, c: c, tail: s{} }}\n}}\nlet result: map = run({{}});\nresult;\n",
        local_count - 1
    ));
    let compiled = compile_source(&source).expect("nested-LocalCall program should compile");
    assert!(
        compiled.locals < local_count,
        "unrelated chained locals must still compact beside nested LocalCalls: frame {} must stay well below the {} declared slots",
        compiled.locals,
        local_count
    );
    assert!(
        compiled.locals <= (u8::MAX as usize) + 1,
        "compacted frame {} must fit within the 256-slot frame limit",
        compiled.locals
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("nested-LocalCall program should run");
    assert_eq!(status, VmStatus::Halted);
    let mut expected_tail = String::from("a");
    for _ in 1..local_count {
        expected_tail.push('b');
    }
    match vm.stack().last() {
        Some(Value::Map(map)) => {
            assert_eq!(
                map.get(&Value::string("a")),
                Some(&Value::Int(12)),
                "named-call argument LocalCall must evaluate"
            );
            assert_eq!(
                map.get(&Value::string("b")),
                Some(&Value::Int(-1)),
                "optional-access key LocalCall must evaluate (out-of-range key unwraps to the fallback)"
            );
            assert_eq!(
                map.get(&Value::string("c")),
                Some(&Value::Int(2)),
                "unwrap_or fallback LocalCall must keep the present value"
            );
            assert_eq!(
                map.get(&Value::string("tail")),
                Some(&Value::string(expected_tail.as_str())),
                "chained local values must survive the nested LocalCalls"
            );
        }
        other => panic!("expected a result map on the stack, got {other:?}"),
    }
}
