//! Milestone 5 of the semantic module system: source-owned spans and
//! diagnostics through merge.
//!
//! Every span produced during load/parse/typing/merge references the semantic
//! module graph's `SourceId` space, and the compilation-wide `SourceMap`
//! travels with module-compile errors (`SourcePathError::SourceWithMap`), so
//! rendered diagnostics always read from the owning source. Merging units can
//! never reinterpret one module's offsets or lines against another file.

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

/// Render a module-compile error against the compilation-wide source map it
/// carries (milestone 5): the rendered diagnostic shows the owning file name,
/// line, and code frame.
fn render_path_error(err: &vm::SourcePathError) -> String {
    match err {
        vm::SourcePathError::SourceWithMap { error, sources } => match error {
            vm::SourceError::Parse(parse) => vm::render_source_error(sources, parse, false),
            vm::SourceError::Compile(compile) => vm::render_compile_error(sources, compile, false),
        },
        other => vm::render_source_path_error(Path::new("<source>"), other, false),
    }
}

#[test]
fn root_parse_error_renders_root_path_and_frame() {
    let root = temp_module_root("semantic_m5_root_parse");
    let main_path = root.join("main.rss");
    write_source(&main_path, "fn run() {\nlet x = ;\n}\n", "main source");

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("invalid root source should fail"),
        Err(err) => err,
    };
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains(&main_path.display().to_string()),
        "root diagnostic must name the root path, got:\n{rendered}"
    );
    assert!(
        rendered.contains("let x = ;"),
        "root diagnostic must show the root code frame, got:\n{rendered}"
    );
    assert!(
        rendered.contains("--> "),
        "root diagnostic must include a source frame, got:\n{rendered}"
    );

    remove_module_root(&root);
}

#[test]
fn nested_module_parse_error_renders_from_owning_source() {
    let root = temp_module_root("semantic_m5_nested_parse");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::nested as nested;\nnested::run();\n",
        "main source",
    );
    let nested_path = root.join("nested.rss");
    write_source(&nested_path, "pub fn run( {\n", "nested source");

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("malformed nested module should fail"),
        Err(err) => err,
    };
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains(&nested_path.display().to_string()),
        "nested parse diagnostic must name the nested path, got:\n{rendered}"
    );
    assert!(
        rendered.contains("pub fn run( {"),
        "nested parse diagnostic must show the nested code frame, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("nested::run();"),
        "nested parse diagnostic must not show the root frame, got:\n{rendered}"
    );

    remove_module_root(&root);
}

#[test]
fn nested_module_typing_error_renders_from_owning_source() {
    let root = temp_module_root("semantic_m5_nested_typing");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::broken as broken;\nbroken::run();\n",
        "main source",
    );
    let broken_path = root.join("broken.rss");
    write_source(
        &broken_path,
        "pub fn run() {\nlet cond = 1 == 1;\nlet value = if cond => {\n    1\n} else => {\n    \"x\"\n};\nvalue\n}\n",
        "broken source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("typed mismatched nested module should fail"),
        Err(err) => err,
    };
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains("compile error"),
        "typing diagnostic must render as a compile error, got:\n{rendered}"
    );
    assert!(
        rendered.contains(&broken_path.display().to_string()),
        "typing diagnostic must name the nested module, got:\n{rendered}"
    );
    assert!(
        rendered.contains("let value = if cond => {"),
        "typing diagnostic must show the nested module's code frame, got:\n{rendered}"
    );
    assert!(
        rendered.contains("int vs string") || rendered.contains("incompatible"),
        "typing diagnostic must keep its detail message, got:\n{rendered}"
    );

    remove_module_root(&root);
}

#[test]
fn duplicate_function_error_renders_from_owning_source() {
    let root = temp_module_root("semantic_m5_duplicate");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::dupmod as d;\nd::run();\n",
        "main source",
    );
    let dup_path = root.join("dupmod.rss");
    write_source(
        &dup_path,
        "fn run() { 1; }\nfn run() { 2; }\n",
        "duplicate source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("duplicate declarations should fail"),
        Err(err) => err,
    };
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains("duplicate"),
        "duplicate diagnostic must say 'duplicate', got:\n{rendered}"
    );
    assert!(
        rendered.contains(&dup_path.display().to_string()),
        "duplicate diagnostic must name the owning module, got:\n{rendered}"
    );
    assert!(
        rendered.contains("fn run() { 2; }"),
        "duplicate diagnostic must point at the redeclaration's frame, got:\n{rendered}"
    );

    remove_module_root(&root);
}

#[test]
fn private_export_visibility_failure_renders_from_importing_source() {
    // `hidden` is private in the module; the named import in main must fail
    // and the diagnostic must render from main's own `use` line.
    let root = temp_module_root("semantic_m5_visibility");
    let module_path = root.join("module.rss");
    write_source(&module_path, "fn hidden() { 1; }\n", "module source");
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use module::{hidden};\nhidden();\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("importing a private function should fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("has no public function 'hidden'"),
        "visibility diagnostic must name the missing public function, got: {err}"
    );
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains(&main_path.display().to_string()),
        "visibility diagnostic must render from the importing module, got:\n{rendered}"
    );
    assert!(
        rendered.contains("use module::{hidden};"),
        "visibility diagnostic must show the importing module's use line, got:\n{rendered}"
    );

    remove_module_root(&root);
}

#[test]
fn unresolved_module_call_renders_from_owning_source() {
    // `nested` imports `sibling` privately; main cannot call `leaf` through
    // `nested` (no implicit transitive re-export). The unresolved call must
    // render from main's own source.
    let root = temp_module_root("semantic_m5_unresolved_call");
    let sibling_path = root.join("sibling.rss");
    write_source(
        &sibling_path,
        "pub fn leaf() -> int { 19 }\n",
        "sibling source",
    );
    let nested_path = root.join("nested.rss");
    write_source(
        &nested_path,
        "use self::sibling as sibling;\npub fn run() -> int { sibling::leaf() }\n",
        "nested source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use self::nested as nested;\nnested::leaf();\n",
        "main source",
    );

    let err = match compile_source_file(&main_path) {
        Ok(_) => panic!("calling a non re-exported function should fail"),
        Err(err) => err,
    };
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains(&main_path.display().to_string()),
        "unresolved call diagnostic must name the calling module, got:\n{rendered}"
    );
    assert!(
        rendered.contains("nested::leaf();"),
        "unresolved call diagnostic must show the call site frame, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("pub fn run() -> int"),
        "unresolved call diagnostic must not show the nested module's frame, got:\n{rendered}"
    );

    remove_module_root(&root);
}

#[test]
fn same_line_number_in_different_modules_renders_each_owning_source() {
    // Both modules fail strict typing on line 2, but each compilation must
    // render its own file's line 2 text.
    let root = temp_module_root("semantic_m5_same_line");
    let a_module = root.join("a.rss");
    write_source(
        &a_module,
        "pub fn run() {\nlet a: unknown = 1;\na\n}\n",
        "a module source",
    );
    let b_module = root.join("b.rss");
    write_source(
        &b_module,
        "pub fn run() {\nlet b: unknown = 2;\nb\n}\n",
        "b module source",
    );

    let compile_entry = |entry_name: &str, module: &Path| {
        let entry = root.join(entry_name);
        let module_name = module
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("stem");
        write_source(
            &entry,
            &format!("use self::{module_name} as m;\nm::run();\n"),
            "entry source",
        );
        compile_source_file(&entry)
    };

    let err_a = match compile_entry("main_a.rss", &a_module) {
        Ok(_) => panic!("a module should fail strict typing"),
        Err(err) => err,
    };
    let rendered_a = render_path_error(&err_a);
    assert!(
        rendered_a.contains(&a_module.display().to_string()),
        "a diagnostic must name a.rss, got:\n{rendered_a}"
    );
    assert!(
        rendered_a.contains("let a: unknown = 1;"),
        "a diagnostic must show a.rss line 2, got:\n{rendered_a}"
    );

    let err_b = match compile_entry("main_b.rss", &b_module) {
        Ok(_) => panic!("b module should fail strict typing"),
        Err(err) => err,
    };
    let rendered_b = render_path_error(&err_b);
    assert!(
        rendered_b.contains(&b_module.display().to_string()),
        "b diagnostic must name b.rss, got:\n{rendered_b}"
    );
    assert!(
        rendered_b.contains("let b: unknown = 2;"),
        "b diagnostic must show b.rss line 2, got:\n{rendered_b}"
    );
    assert!(
        !rendered_b.contains("let a: unknown = 1;"),
        "b diagnostic must never show a.rss's line 2 text, got:\n{rendered_b}"
    );

    remove_module_root(&root);
}

#[test]
fn in_memory_override_and_disk_modules_render_their_own_sources() {
    // Disk module a/util and in-memory override b/util both fail strict
    // typing on line 2 of their own text. The disk failure must render the
    // file that exists on disk; the override failure must render the override
    // text even though b/util.rss does not exist on disk.
    let root = temp_module_root("semantic_m5_override_ownership");
    let a_dir = root.join("a");
    std::fs::create_dir_all(&a_dir).expect("a dir should be created");
    let a_module = a_dir.join("util.rss");
    write_source(
        &a_module,
        "pub fn alpha() {\nlet a: unknown = 1;\na\n}\n",
        "a/util source",
    );
    let main_path = root.join("main.rss");
    write_source(
        &main_path,
        "use a::util as au;\nuse b::util as bu;\nau::alpha();\nbu::beta();\n",
        "main source",
    );

    // Phase 1: a/util (disk) is the first failing module; the diagnostic must
    // render the disk text of a/util.rss.
    let options_phase1 = vm::CompileSourceFileOptions::new()
        .with_module_override_source("b/util.rss", "pub fn beta() {\nlet b: unknown = 2;\nb\n}\n");
    let err_phase1 = match compile_source_file_with_options(&main_path, options_phase1) {
        Ok(_) => panic!("disk module should fail strict typing"),
        Err(err) => err,
    };
    let rendered_phase1 = render_path_error(&err_phase1);
    assert!(
        rendered_phase1.contains(&a_module.display().to_string()),
        "disk diagnostic must name a/util.rss, got:\n{rendered_phase1}"
    );
    assert!(
        rendered_phase1.contains("let a: unknown = 1;"),
        "disk diagnostic must show the disk code frame, got:\n{rendered_phase1}"
    );

    // Phase 2: a/util (disk) is valid; the in-memory override for b/util is
    // the failing module. The diagnostic must render the override text even
    // though no b/util.rss exists on disk.
    write_source(&a_module, "pub fn alpha() { 1; }\n", "a/util fixed source");
    let options_phase2 = vm::CompileSourceFileOptions::new()
        .with_module_override_source("b/util.rss", "pub fn beta() {\nlet b: unknown = 2;\nb\n}\n");
    let err_phase2 = match compile_source_file_with_options(&main_path, options_phase2) {
        Ok(_) => panic!("override module should fail strict typing"),
        Err(err) => err,
    };
    let rendered_phase2 = render_path_error(&err_phase2);
    assert!(
        rendered_phase2.contains("__pd_vm_inmemory__/b/util.rss")
            || rendered_phase2.contains(&root.join("b/util.rss").display().to_string()),
        "override diagnostic must name the virtual b/util identity, got:\n{rendered_phase2}"
    );
    assert!(
        rendered_phase2.contains("let b: unknown = 2;"),
        "override diagnostic must show the override text frame, got:\n{rendered_phase2}"
    );
    assert!(
        !rendered_phase2.contains("let a: unknown = 1;"),
        "override diagnostic must not show a/util's frame, got:\n{rendered_phase2}"
    );

    remove_module_root(&root);
}

#[test]
fn in_memory_root_error_renders_virtual_path_and_frame() {
    // `compile_source_with_flavor_and_options` compiles a virtual root; its
    // parse error must render the virtual path and the in-memory frame.
    let source = "use self::nested as nested;\nnested::run();\n";
    let options = vm::CompileSourceFileOptions::new()
        .with_module_override_source("nested.rss", "pub fn run( {\n");

    let err = match vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        options,
    ) {
        Ok(_) => panic!("malformed override module should fail"),
        Err(err) => err,
    };
    let rendered = render_path_error(&err);
    assert!(
        rendered.contains("__pd_vm_inmemory__/nested.rss"),
        "in-memory diagnostic must name the virtual nested path, got:\n{rendered}"
    );
    assert!(
        rendered.contains("pub fn run( {"),
        "in-memory diagnostic must show the override code frame, got:\n{rendered}"
    );

    // The same fixture through the at-path entry point renders the disk-path
    // identity of the override instead.
    let root = temp_module_root("semantic_m5_virtual_at_path");
    let main_path = root.join("main.rss");
    let at_path_err = match vm::compile_source_at_path_with_flavor_and_options(
        &main_path,
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::new()
            .with_module_override_source("nested.rss", "pub fn run( {\n"),
    ) {
        Ok(_) => panic!("malformed override module should fail"),
        Err(err) => err,
    };
    let rendered_at_path = render_path_error(&at_path_err);
    assert!(
        rendered_at_path.contains(&root.join("nested.rss").display().to_string()),
        "at-path diagnostic must name the disk-path override identity, got:\n{rendered_at_path}"
    );
    assert!(
        rendered_at_path.contains("pub fn run( {"),
        "at-path diagnostic must show the override code frame, got:\n{rendered_at_path}"
    );

    remove_module_root(&root);
}
