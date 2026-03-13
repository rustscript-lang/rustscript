use edge::{compile_edge_source_file, compile_edge_source_file_with_options};
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
fn compile_edge_source_file_supports_runtime_namespace_host_import() {
    let root = unique_temp_root("runtime_namespace");
    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use runtime;
        runtime::sleep(1);
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "runtime::sleep"),
        "runtime namespace should map to runtime host import"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_supports_runtime_exit_host_import() {
    let root = unique_temp_root("runtime_exit_namespace");
    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use runtime;
        runtime::exit();
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "runtime::exit"),
        "runtime namespace should map runtime::exit to a host import"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_supports_rate_limit_namespace_host_import() {
    let root = unique_temp_root("rate_limit_namespace");
    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use rate_limit;
        rate_limit::allow("client-1", 3, 60);
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "rate_limit::allow"),
        "rate_limit namespace should map to host import"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_supports_console_namespace_host_import() {
    let root = unique_temp_root("console_namespace");
    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use console;
        let arg0 = console::args::get(0);
        console::args::count() + arg0.length + console::stdout::write("ok");
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "console::args::count"),
        "console namespace should map args::count to a host import"
    );
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "console::args::get"),
        "console namespace should map args::get to a host import"
    );
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "console::stdout::write"),
        "console namespace should map stdout::write to a host import"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_supports_console_http3_client_example() {
    let program_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/console/sample_console_http3_client.rss");

    let compiled = compile_edge_source_file(&program_path).expect("example should compile");
    let import_names = compiled
        .program
        .imports
        .iter()
        .map(|import| import.name.as_str())
        .collect::<Vec<_>>();

    assert!(
        import_names.contains(&"console::args::count"),
        "console sample should import argv count"
    );
    assert!(
        import_names.contains(&"console::stdin::read_all"),
        "console sample should import stdin read_all for body streaming"
    );
    assert!(
        import_names.contains(&"runtime::exit"),
        "console sample should import runtime::exit for usage handling"
    );
    assert!(
        import_names.contains(&"http::exchange::send"),
        "console sample should send an outbound exchange"
    );
    assert!(
        import_names.contains(&"tls::session::from_socket"),
        "console sample should create a TLS session from the exchange"
    );
}

#[test]
fn compile_edge_source_file_prefers_local_module_over_host_namespace_fallback() {
    let root = unique_temp_root("runtime_local_module");

    let runtime_module = root.join("runtime.rss");
    std::fs::write(
        &runtime_module,
        r#"
        pub fn sleep(ms) {
            ms + 1;
        }
    "#,
    )
    .expect("runtime module should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use runtime;
        runtime::sleep(41);
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    assert!(
        compiled.program.imports.is_empty(),
        "local runtime module should win over host namespace fallback"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(runtime_module);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_with_options_can_override_runtime_module() {
    let root = unique_temp_root("runtime_override");

    let override_module = root.join("custom_runtime.rss");
    std::fs::write(
        &override_module,
        r#"
        pub fn sleep(ms) {
            ms + 2;
        }
    "#,
    )
    .expect("override module source should write");

    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use runtime;
        runtime::sleep(40);
    "#,
    )
    .expect("main source should write");

    let options =
        CompileSourceFileOptions::new().with_module_override_path("runtime.rss", &override_module);
    let compiled =
        compile_edge_source_file_with_options(&main_path, options).expect("compile should succeed");
    assert!(
        compiled.program.imports.is_empty(),
        "runtime module override should replace host import fallback"
    );

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_file(override_module);
    let _ = std::fs::remove_dir(root);
}

#[test]
fn compile_edge_source_file_supports_embedded_edge_upstream_wrapper_modules() {
    let root = unique_temp_root("edge_upstream_wrappers");
    let main_path = root.join("main.rss");
    std::fs::write(
        &main_path,
        r#"
        use edge::http::upstream as upstream;
        use edge::http::upstream::request as upstream_request;
        use edge::http::upstream::response as upstream_response;

        upstream_request::set_target("http://example.test");
        upstream_request::set_header("accept", "text/plain");
        let proxy_handle = upstream::as_stream();
        let status = upstream_response::get_status();
        let line = upstream_response::read_line();
        let chunk = upstream_response::next_chunk(8);
        let done = upstream_response::eof();

        [proxy_handle, status, line, chunk, done];
    "#,
    )
    .expect("main source should write");

    let compiled = compile_edge_source_file(&main_path).expect("compile should succeed");
    let import_names = compiled
        .program
        .imports
        .iter()
        .map(|import| import.name.as_str())
        .collect::<Vec<_>>();

    assert!(
        import_names.contains(&"http::exchange::default_upstream"),
        "wrapper should import the canonical default exchange handle"
    );
    assert!(
        import_names.contains(&"http::exchange::set_target"),
        "wrapper should import canonical exchange setters"
    );
    assert!(
        import_names.contains(&"http::exchange::get_status"),
        "wrapper should import canonical exchange response getters"
    );
    assert!(
        import_names.contains(&"proxy::stream::exchange"),
        "wrapper should build proxy streams from explicit exchanges"
    );
    assert!(
        import_names.contains(&"http::exchange::body::next_chunk"),
        "wrapper should import canonical exchange body streaming calls"
    );
    assert!(
        !import_names
            .iter()
            .any(|name| name.starts_with("http::upstream::")
                || *name == "proxy::stream::default_upstream"),
        "wrapper modules should not reintroduce removed alias host calls"
    );

    let _ = std::fs::remove_file(main_path);
    let _ = std::fs::remove_dir(root);
}
