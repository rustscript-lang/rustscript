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
    root
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

    let override_module_path = root.join("edge_io_async_override.rss");
    write_source(
        &override_module_path,
        r#"
        pub fn request_body_read() {
            "override-body";
        }
    "#,
        "override module source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use edge::io_async as edge_io;
        edge_io::request_body_read();
    "#,
        "main source",
    );

    let options = CompileSourceFileOptions::new()
        .with_module_override_path("edge/io_async.rss", &override_module_path);
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

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("selective import should not expose unlisted exports"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::Source(vm::SourceError::Parse(vm::ParseError { ref message, .. }))
            if message.contains("unknown function 'add_two'")
        ),
        "expected unknown function error, got {err:?}"
    );

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
    let compiled = compile_source_file(&ok_main_path).expect("compile should succeed");
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
    let err = match compile_source_file(&bad_main_path) {
        Ok(_) => panic!("private import should fail"),
        Err(err) => err,
    };
    assert!(
        matches!(
            err,
            vm::SourcePathError::Source(vm::SourceError::Parse(vm::ParseError { ref message, .. }))
            if message.contains("unknown function 'private_add'")
        ),
        "expected unknown function error, got {err:?}"
    );

    remove_module_root(&root);
}

#[test]
fn rss_function_definition_is_inlined_without_host_imports() {
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
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

    let compiled = compile_source_file(&main_path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(10)]);

    remove_module_root(&root);
}
