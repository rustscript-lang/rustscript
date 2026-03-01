use edge::{
    EDGE_ASYNC_IO_MODULE_SPEC, compile_edge_source_file, compile_edge_source_file_with_options,
};
use vm::{CompileSourceFileOptions, Value, Vm, VmStatus};

fn unique_temp_root(label: &str) -> std::path::PathBuf {
    let unique = format!(
        "pd_edge_compile_test_{label}_{}_{}",
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

#[test]
fn compile_edge_source_file_resolves_edge_async_io_module() {
    let root = unique_temp_root("default");
    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use edge::io_async as edge_io;
        edge_io::request_body_eof();
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "http::request::body::eof"),
        "edge io_async should resolve to edge ABI host imports"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_allows_user_override_of_async_io_module() {
    let root = unique_temp_root("override");

    let override_module = root.join("custom_edge_io_async.rss");
    std::fs::write(
        &override_module,
        r#"
        pub fn request_body_eof() {
            true;
        }
    "#,
    )
    .expect("override module source should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use edge::io_async as edge_io;
        edge_io::request_body_eof();
    "#,
    )
    .expect("main source should write");

    let options = CompileSourceFileOptions::new()
        .with_module_override_path(EDGE_ASYNC_IO_MODULE_SPEC, &override_module);
    let compiled =
        compile_edge_source_file_with_options(&main_path, options).expect("compile should succeed");
    assert!(
        compiled.program.imports.is_empty(),
        "override module should replace edge ABI host imports"
    );

    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Bool(true)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(override_module);
    let _ = std::fs::remove_dir(root);
}
