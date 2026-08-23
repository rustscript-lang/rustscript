//! Milestone 3 of the semantic module system: declaration symbols owned by
//! modules, public export tables, imported-vs-local separation, duplicate
//! declaration diagnostics, same-named helpers across modules, and no
//! implicit transitive re-export — with bytecode behavior preserved.

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
    root.canonicalize().unwrap_or(root)
}

fn write_source(path: &Path, source: &str, description: &str) {
    std::fs::write(path, source).unwrap_or_else(|err| panic!("{description} should write: {err}"));
}

fn remove_module_root(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// `a/util` exports `alpha` and keeps `hidden` private; `b/util` exports
/// `beta`. Both modules declare a private helper named `helper`.
fn write_public_private_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");

    let a_module = a_dir.join("util.rss");
    write_source(
        &a_module,
        "pub fn alpha() { helper(); }\nfn helper() { 42; }\nfn hidden() { 7; }\n",
        "a/util source",
    );
    let b_module = b_dir.join("util.rss");
    write_source(
        &b_module,
        "pub fn beta() { helper(); }\nfn helper() { 42; }\n",
        "b/util source",
    );

    let main_path = root.join("main.rss");
    (main_path, a_module, b_module)
}

#[test]
fn same_named_helpers_across_modules_coexist() {
    // Milestone 4 lifts the flat-merge limitation documented by milestone 3:
    // same-named private helpers in independent modules now coexist, each
    // resolved by its compiler-owned symbol. `alpha` and `beta` each call
    // their own module's `helper`.
    let root = temp_module_root("semantic_m3_same_helpers");
    let (main_path, _, _) = write_public_private_fixture(&root);
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::alpha();\nbu::beta();\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("same-named helpers should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(42), Value::Int(42)],
        "each module's helper must resolve within its own module"
    );

    remove_module_root(&root);
}

#[test]
fn public_functions_are_importable_private_functions_are_not() {
    let root = temp_module_root("semantic_m3_visibility");
    let (main_path, _, _) = write_public_private_fixture(&root);

    // Public export: `alpha` resolves through the namespace import.
    write_source(
        &main_path,
        "use a::util as au;\nau::alpha();\n",
        "main source",
    );
    let compiled = compile_source_file(&main_path).expect("public export should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    // Private declaration: `hidden` is not in a/util's export table, so the
    // call cannot resolve through the import.
    write_source(
        &main_path,
        "use a::util as au;\nau::hidden();\n",
        "main source",
    );
    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("private functions must not be importable"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("hidden"),
        "diagnostic should name the private function, got: {message}"
    );

    remove_module_root(&root);
}

#[test]
fn transitive_imports_are_not_reexported() {
    // a imports c and uses c::shared internally; the root imports only a.
    // `shared` must stay out of a's export table: calling it from the root
    // without a direct import is a diagnostic, not a silent re-export.
    let root = temp_module_root("semantic_m3_no_reexport");
    let c_module = root.join("c.rss");
    write_source(&c_module, "pub fn shared() { 100; }\n", "c source");
    let a_module = root.join("a.rss");
    write_source(
        &a_module,
        "use self::c;\npub fn alpha() { c::shared(); }\n",
        "a source",
    );
    let main_path = root.join("main.rss");

    write_source(&main_path, "use a;\nalpha();\nshared();\n", "main source");
    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("transitive imports must not be re-exported implicitly"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("shared"),
        "diagnostic should name the non-reexported function, got: {message}"
    );

    // Positive control: importing c directly makes `shared` resolvable.
    write_source(
        &main_path,
        "use a;\nuse c;\nalpha();\nshared();\n",
        "main source",
    );
    let compiled = compile_source_file(&main_path).expect("direct import should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100), Value::Int(100)]);

    remove_module_root(&root);
}

#[test]
fn imported_name_clashing_with_local_declaration_is_a_diagnostic() {
    // The import prelude declares the imported name, so declaring the same
    // name locally in the importing module is a duplicate diagnostic instead
    // of a silent shadow.
    let root = temp_module_root("semantic_m3_import_clash");
    let a_dir = root.join("a");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    let a_module = a_dir.join("util.rss");
    write_source(&a_module, "pub fn helper() { 1; }\n", "a/util source");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util;\nfn helper() { 2; }\nhelper();\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("imported name clashing with a local declaration must fail"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("conflicts with a local declaration")
            || message.contains("duplicate function 'helper'"),
        "unexpected diagnostic: {message}"
    );

    remove_module_root(&root);
}

#[test]
fn duplicate_local_declaration_in_a_module_is_a_diagnostic() {
    let root = temp_module_root("semantic_m3_dup_local");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "fn dup() { 1; }\nfn dup() { 2; }\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("duplicate local declarations must fail"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("duplicate function 'dup'"),
        "unexpected diagnostic: {message}"
    );

    remove_module_root(&root);
}
