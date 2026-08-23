//! Milestone 6/7 verification: the semantic module pipeline is the sole
//! file-module path.
//!
//! These tests exercise the end-to-end module behavior that the removed
//! textual rewrite/prelude machinery used to provide: wildcard imports,
//! function values of imported functions, generic calls through every import
//! form, single-segment host-form namespace calls, deterministic output, and
//! import-order independence of behavior.

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

#[test]
fn wildcard_import_exposes_all_public_exports_directly_and_by_namespace() {
    let root = temp_module_root("semantic_m6_wildcard");
    write_source(
        &root.join("util.rss"),
        "pub fn value() -> int { 5 }\npub fn double(x) { x * 2; }\nfn private_helper() { 99; }\n",
        "util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::util::*;\nlet direct = value();\nlet ns = util::double(direct);\nns;\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("wildcard import should compile");
    assert!(
        compiled.functions.is_empty(),
        "wildcard imports must not produce host imports"
    );
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(10)]);

    remove_module_root(&root);
}

#[test]
fn wildcard_import_does_not_expose_private_helpers() {
    let root = temp_module_root("semantic_m6_wildcard_private");
    write_source(
        &root.join("util.rss"),
        "pub fn value() -> int { 5 }\nfn private_helper() { 99; }\n",
        "util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::util::*;\nprivate_helper();\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("wildcard import must not expose private functions"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("unknown function 'private_helper'"),
        "unexpected diagnostic: {err}"
    );

    remove_module_root(&root);
}

#[test]
fn imported_function_values_resolve_to_module_symbols() {
    let root = temp_module_root("semantic_m6_function_values");
    write_source(
        &root.join("util.rss"),
        "pub fn add_one(x) { x + 1; }\n",
        "util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::util;\nlet f = add_one;\nf(41);\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("function value import should compile");
    assert!(
        compiled.functions.is_empty(),
        "imported function values must not produce host imports"
    );
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    remove_module_root(&root);
}

#[test]
fn generic_calls_work_through_named_namespace_and_alias_import_forms() {
    let root = temp_module_root("semantic_m6_generic_forms");
    let a_dir = root.join("a");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    write_source(
        &a_dir.join("util.rss"),
        "pub fn wrap<T>(value: T) { let copied = value; [copied]; }\n",
        "a/util source",
    );
    write_source(
        &root.join("helpers.rss"),
        "pub fn wrap<T>(value: T) { let copied = value; [copied]; }\n",
        "helpers source",
    );

    // Named import (direct), all-public namespace call, and aliased
    // namespace call all carry explicit type arguments through to the
    // exported type parameters.
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        r#"
        use self::helpers::{wrap as direct_wrap};
        use self::helpers;
        use a::util as au;

        let named = direct_wrap::<int>(1);
        let namespace_value = helpers::wrap::<int>(2);
        let aliased = au::wrap::<int>(3);
        named.length + namespace_value.length + aliased.length;
    "#,
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("generic import forms should compile");
    assert!(
        compiled.functions.is_empty(),
        "generic imported calls must not produce host imports"
    );
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(3)]);

    remove_module_root(&root);
}

#[test]
fn single_segment_module_import_namespace_calls_stay_module_calls() {
    // `use module; module::fn()` parses as a host-form call (the parser
    // cannot know `module` is a file module); the loader must fix it up to a
    // module call instead of emitting a host import.
    let root = temp_module_root("semantic_m6_single_segment_ns");
    write_source(
        &root.join("module.rss"),
        "pub fn public_add(x) { x + 1; }\n",
        "module source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use module;\nmodule::public_add(41);\n",
        "main source",
    );

    let compiled =
        compile_source_file(&main_path).expect("single-segment namespace call should compile");
    assert!(
        compiled.functions.is_empty(),
        "file-module namespace calls must not become host imports"
    );
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    remove_module_root(&root);
}

#[test]
fn single_segment_named_import_missing_member_stays_unknown_function() {
    let root = temp_module_root("semantic_m6_single_segment_named");
    write_source(
        &root.join("module.rss"),
        "pub fn add_one(x) { x + 1; }\n",
        "module source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use module::{add_one};\nadd_two(40);\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("unlisted member must fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("unknown function 'add_two'"),
        "unexpected diagnostic: {err}"
    );

    remove_module_root(&root);
}

#[test]
fn same_exported_name_from_two_modules_resolves_per_namespace() {
    // Two modules exporting the same name, imported through aliases; the
    // final flat boundary keeps both addressable with deterministic
    // module-identity mangling for the colliding name.
    let root = temp_module_root("semantic_m6_same_exports");
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");
    write_source(
        &a_dir.join("util.rss"),
        "pub fn helper() { 1; }\n",
        "a/util source",
    );
    write_source(
        &b_dir.join("util.rss"),
        "pub fn helper() { 2; }\n",
        "b/util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::helper();\nbu::helper();\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("same-name exports should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1), Value::Int(2)]);

    remove_module_root(&root);
}

#[test]
fn compiled_output_is_deterministic_across_repeated_compilations() {
    let root = temp_module_root("semantic_m6_deterministic_bytes");
    write_source(
        &root.join("util.rss"),
        "pub fn helper() { 1; }\npub fn other() { helper() + 1; }\n",
        "util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::util;\nhelper();\nother();\n",
        "main source",
    );

    let compile = || {
        let compiled = compile_source_file(&main_path).expect("compile should succeed");
        let function_names = compiled
            .functions
            .iter()
            .map(|func| func.name.clone())
            .collect::<Vec<_>>();
        (compiled.program.code.clone(), function_names)
    };
    let (first_instructions, first_names) = compile();
    let (second_instructions, second_names) = compile();

    assert_eq!(
        first_names, second_names,
        "function table must be identical across compilations"
    );
    assert_eq!(
        first_instructions, second_instructions,
        "bytecode must be identical across compilations"
    );

    remove_module_root(&root);
}

#[test]
fn import_order_swap_produces_identical_behavior() {
    let root = temp_module_root("semantic_m6_import_order");
    write_source(
        &root.join("a.rss"),
        "pub fn value() -> int { 3 }\n",
        "a source",
    );
    write_source(
        &root.join("b.rss"),
        "pub fn value() -> int { 4 }\n",
        "b source",
    );
    let main_ab = root.join("main_ab.rss");
    write_source(
        &main_ab,
        "use self::a as a;\nuse self::b as b;\na::value();\nb::value();\n",
        "main ab source",
    );
    let main_ba = root.join("main_ba.rss");
    write_source(
        &main_ba,
        "use self::b as b;\nuse self::a as a;\nb::value();\na::value();\n",
        "main ba source",
    );

    let run = |path: &Path| -> Vec<Value> {
        let compiled = compile_source_file(path).expect("compile should succeed");
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
        vm.stack().to_vec()
    };
    assert_eq!(run(&main_ab), vec![Value::Int(3), Value::Int(4)]);
    assert_eq!(
        run(&main_ba),
        vec![Value::Int(4), Value::Int(3)],
        "import order must not change which module each call resolves to"
    );

    remove_module_root(&root);
}

#[test]
fn host_namespace_imports_stay_on_the_host_path_without_rewriting() {
    // A virtual host namespace import must compile to host imports even
    // though its single-segment form parses like a file-module candidate.
    let root = temp_module_root("semantic_m6_host_path");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use myhost;\nmyhost::do_thing(81);\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("host namespace import should compile");
    let host_names = compiled
        .functions
        .iter()
        .map(|func| func.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        host_names.contains(&"myhost::do_thing"),
        "host namespace call must remain a host import: {host_names:?}"
    );

    remove_module_root(&root);
}

#[test]
fn namespace_member_arity_mismatch_is_a_diagnostic() {
    let root = temp_module_root("semantic_m6_arity_mismatch");
    write_source(
        &root.join("util.rss"),
        "pub fn add(x, y) { x + y; }\n",
        "util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::util as u;\nu::add(1);\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("arity mismatch must fail"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("function 'u::add' expects 2 arguments"),
        "unexpected diagnostic: {message}"
    );

    remove_module_root(&root);
}
