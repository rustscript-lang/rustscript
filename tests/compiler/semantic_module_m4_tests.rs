//! Milestone 4 of the semantic module system: calls resolve by compiler-owned
//! `SymbolId` before unit merge, the flat linker keys module functions by
//! symbol instead of by source name, and names are deterministically mangled
//! only at the flat bytecode boundary. Same-named declarations in independent
//! modules coexist; local bindings are scoped by full module identity instead
//! of a bare file stem.

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

/// `a/util` and `b/util` both export a public `run` (different bodies) and
/// each keeps a private helper named `helper` that its own `run` calls.
fn write_same_export_fixture(root: &Path) -> PathBuf {
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");

    let a_module = a_dir.join("util.rss");
    write_source(
        &a_module,
        "pub fn run() { helper(); }\nfn helper() { 11; }\n",
        "a/util source",
    );
    let b_module = b_dir.join("util.rss");
    write_source(
        &b_module,
        "pub fn run() { helper(); }\nfn helper() { 22; }\n",
        "b/util source",
    );

    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::run();\nbu::run();\n",
        "main source",
    );
    main_path
}

#[test]
fn same_exported_function_name_in_two_namespaces_calls_separately() {
    let root = temp_module_root("semantic_m4_same_export");
    let main_path = write_same_export_fixture(&root);

    let compiled = compile_source_file(&main_path).expect("same-named exports should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(11), Value::Int(22)],
        "au::run and bu::run must resolve to their own module's export"
    );

    // The flat exported-callable table keeps both exports addressable: the
    // first export keeps its bare name and the collision is deterministically
    // mangled with the module identity.
    let exported_names = vm
        .program()
        .exported_callables
        .iter()
        .map(|exported| exported.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        exported_names.contains(&"run"),
        "one export keeps the bare name: {exported_names:?}"
    );
    assert_eq!(
        exported_names
            .iter()
            .filter(|name| name.starts_with("run__m"))
            .count(),
        1,
        "the colliding export is deterministically mangled: {exported_names:?}"
    );

    remove_module_root(&root);
}

#[test]
fn same_named_private_helpers_are_resolved_within_their_own_module() {
    let root = temp_module_root("semantic_m4_private_helpers");
    let main_path = write_same_export_fixture(&root);

    let compiled = compile_source_file(&main_path).expect("same-named helpers should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(11), Value::Int(22)],
        "each module's private helper must be the one its own run calls"
    );

    remove_module_root(&root);
}

#[test]
fn named_import_aliases_resolve_to_distinct_symbols() {
    let root = temp_module_root("semantic_m4_named_aliases");
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");
    write_source(
        &a_dir.join("util.rss"),
        "pub fn emit(value) { value * 2; }\n",
        "a/util source",
    );
    write_source(
        &b_dir.join("util.rss"),
        "pub fn emit(value) { value * 3; }\n",
        "b/util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util::{emit as twice};\nuse b::util::{emit as thrice};\ntwice(4);\nthrice(4);\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("named alias imports should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(8), Value::Int(12)],
        "each alias must call its own module's emit"
    );

    remove_module_root(&root);
}

#[test]
fn local_functions_resolve_within_their_own_module() {
    // `run` (pub) calls `local` (private) in both modules; the local calls
    // must stay inside their declaring module even though both modules define
    // same-named functions.
    let root = temp_module_root("semantic_m4_local_functions");
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");
    write_source(
        &a_dir.join("util.rss"),
        "pub fn run() { local(); }\nfn local() { 1; }\n",
        "a/util source",
    );
    write_source(
        &b_dir.join("util.rss"),
        "pub fn run() { local(); }\nfn local() { 2; }\n",
        "b/util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::run();\nbu::run();\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("local functions should compile");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(1), Value::Int(2)],
        "each module's local function must resolve within its own module"
    );

    remove_module_root(&root);
}

#[test]
fn ambiguous_direct_call_to_same_name_from_two_modules_is_a_diagnostic() {
    // Both modules export `helper` and the root imports both without aliases:
    // a bare `helper()` call cannot name a single symbol. The legacy pipeline
    // reported a flat merge error; milestone 4 reports the ambiguity and asks
    // for a namespace-qualified or named-import call.
    let root = temp_module_root("semantic_m4_ambiguous_direct");
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
        "use a::util;\nuse b::util;\nhelper();\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("ambiguous direct calls must be rejected"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("ambiguous"),
        "diagnostic should report the ambiguity, got: {message}"
    );
    assert!(
        message.contains("helper"),
        "diagnostic should name the ambiguous function, got: {message}"
    );

    // The same fixture compiles once the calls are namespace-qualified.
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::helper();\nbu::helper();\n",
        "main source",
    );
    let compiled = compile_source_file(&main_path).expect("qualified calls should compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(1), Value::Int(2)]);

    remove_module_root(&root);
}

#[test]
fn internal_lowering_is_deterministic_across_repeated_discovery() {
    // The same fixture compiled twice must produce byte-identical bytecode:
    // module ids, symbol ids, stub names, flat indices, and mangled names are
    // all assigned deterministically from discovery order.
    let root = temp_module_root("semantic_m4_deterministic");
    let main_path = write_same_export_fixture(&root);

    let compile_bytes = || {
        let compiled = compile_source_file(&main_path).expect("compile should succeed");
        vm::encode_program(&compiled.program).expect("program should encode")
    };

    let first = compile_bytes();
    let second = compile_bytes();
    assert_eq!(
        first, second,
        "internal lowering must be deterministic across discovery passes"
    );

    remove_module_root(&root);
}

#[test]
fn same_stem_modules_do_not_collide_local_binding_scope_names() {
    // Two same-stem modules (`a/util`, `b/util`) both declare a local `x`.
    // Milestone 4 scopes non-root locals by full module identity (never a
    // bare file stem), so both survive the flat boundary with distinct names.
    let root = temp_module_root("semantic_m4_no_basename_scope");
    let a_dir = root.join("a");
    let b_dir = root.join("b");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    std::fs::create_dir_all(&b_dir).expect("b dir should be created");
    write_source(
        &a_dir.join("util.rss"),
        "pub fn alpha() { let x = 7; x; }\n",
        "a/util source",
    );
    write_source(
        &b_dir.join("util.rss"),
        "pub fn beta() { let x = 8; x; }\n",
        "b/util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::alpha();\nbu::beta();\n",
        "main source",
    );

    let compiled = compile_source_file(&main_path).expect("same-stem modules should compile");
    let debug = compiled
        .program
        .debug
        .as_ref()
        .expect("compiled program should include debug info");
    let x_names = debug
        .locals
        .iter()
        .filter(|local| local.name.ends_with("::x"))
        .map(|local| local.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        x_names.len(),
        2,
        "both modules' x locals must survive the flat boundary: {x_names:?}"
    );
    assert!(
        x_names.iter().all(|name| *name != "x"),
        "non-root locals must be scoped by module identity, got: {x_names:?}"
    );
    assert!(
        x_names.iter().all(|name| name.contains("__m")),
        "scope identity must encode the compiler-owned module id: {x_names:?}"
    );
    assert!(
        x_names[0] != x_names[1],
        "same-stem modules must not share a scope identity: {x_names:?}"
    );

    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7), Value::Int(8)]);

    remove_module_root(&root);
}
