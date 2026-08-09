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
