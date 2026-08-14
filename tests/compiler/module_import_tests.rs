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
