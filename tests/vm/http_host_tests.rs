use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use vm::{
    CallOutcome, CallReturn, HostFunctionRegistry, HttpConfig, Program, Value, Vm, VmStatus,
    compile_source,
};

fn build_request_program(url: String) -> Program {
    compile_source(&format!(
        r#"
        use http;
        http::client::request({{"method": "GET", "url": "{url}"}});
        "#
    ))
    .expect("HTTP request source should compile")
    .program
}

fn local_http_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        ..HttpConfig::default()
    }
}

fn spawn_test_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("test listener should bind");
    let port = listener
        .local_addr()
        .expect("test listener should have an address")
        .port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("test request should arrive");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream
                .read(&mut buffer)
                .expect("request should be readable");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        assert!(request.starts_with(b"GET / HTTP/1.1"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Test: yes\r\n\r\nok")
            .expect("response should be writable");
    });
    (port, handle)
}

fn response_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = value else {
        panic!("expected response map, got {value:?}");
    };
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("response missing field {key}"))
}

async fn drive_vm_to_halt(vm: &mut Vm) -> Result<(), vm::VmError> {
    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(()),
            VmStatus::Yielded => status = vm.resume()?,
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op().await?;
                status = vm.resume()?;
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_host_executes_a_bounded_request_and_returns_a_response_map() {
    let (port, server) = spawn_test_server();
    let mut vm = Vm::new(build_request_program(format!("http://127.0.0.1:{port}/")));
    vm.configure_http(local_http_config(port));
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");

    drive_vm_to_halt(&mut vm)
        .await
        .expect("http request should complete");
    server.join().expect("test server should finish");

    assert_eq!(response_field(&vm.stack()[0], "status"), &Value::Int(200));
    assert_eq!(
        response_field(&vm.stack()[0], "body"),
        &Value::bytes(b"ok".to_vec())
    );
}

#[test]
fn http_host_rejects_targets_until_an_explicit_policy_allows_them() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    let error = vm
        .run()
        .expect_err("unconfigured HTTP targets must be rejected");
    assert!(
        error.to_string().contains("HTTP host is not configured")
            || error
                .to_string()
                .contains("HTTP target host is not allowed"),
        "unexpected error: {error}"
    );
}

#[test]
fn empty_registry_keeps_language_builtins_but_rejects_http_capability() {
    let mut language_vm = Vm::new(
        vm::compile_source("assert(true);")
            .expect("language builtin program should compile")
            .program,
    );
    HostFunctionRegistry::empty()
        .bind_vm_cached(&mut language_vm)
        .expect("empty registry should bind a program without host imports");
    assert_eq!(
        language_vm.run().expect("language builtin should run"),
        VmStatus::Halted
    );

    let mut http_vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    HostFunctionRegistry::restricted()
        .bind_vm_cached(&mut http_vm)
        .expect("empty registry should prepare the program");
    let error = http_vm
        .run()
        .expect_err("unapproved HTTP capability must be rejected");
    assert!(matches!(error, vm::VmError::UnboundImport(name) if name == "http::client::request"));
}

#[test]
fn restricted_registry_requires_explicit_namespaced_builtin_capability() {
    let compiled = compile_source(
        r#"use io;
io::open("/tmp/rustscript-capability-test", "r");"#,
    )
    .expect("namespaced host builtin should compile");
    let mut vm = Vm::new(compiled.program);
    HostFunctionRegistry::restricted()
        .bind_vm_cached(&mut vm)
        .expect("restricted registry should bind namespaced host builtin");
    let error = vm
        .run()
        .expect_err("restricted registry should reject ungranted namespaced builtin");
    assert!(matches!(error, vm::VmError::UnboundImport(name) if name == "io_open"));
}

#[test]
fn capability_binding_plan_cannot_cross_registry_profiles() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let unrestricted = HostFunctionRegistry::new();
    let plan = unrestricted
        .prepare_plan(&program.imports)
        .expect("unrestricted registry should prepare HTTP plan");
    let mut vm = Vm::new(program);
    let error = HostFunctionRegistry::restricted()
        .bind_vm_with_plan(&mut vm, &plan)
        .expect_err("capability plan must not cross registry profiles");
    assert!(error.to_string().contains("different capability profile"));
}

#[test]
fn capability_binding_plan_cannot_outlive_registry_mutation() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let mut registry = HostFunctionRegistry::new();
    let plan = registry
        .prepare_plan(&program.imports)
        .expect("registry should prepare HTTP plan");
    registry
        .allow_builtin("http::client::request")
        .expect("HTTP capability should be a known host callable");
    let mut vm = Vm::new(program);
    let error = registry
        .bind_vm_with_plan(&mut vm, &plan)
        .expect_err("stale capability plan must not bind");
    assert!(error.to_string().contains("different capability state"));
}

#[test]
fn capability_binding_plan_detects_divergent_registry_clone_mutations() {
    let unchanged_program = build_request_program("http://127.0.0.1:1/".to_string());
    let unchanged_registry = HostFunctionRegistry::restricted();
    let unchanged_plan = unchanged_registry
        .prepare_plan(&unchanged_program.imports)
        .expect("restricted registry should prepare HTTP plan");
    let unchanged_clone = unchanged_registry.clone();
    let mut unchanged_vm = Vm::new(unchanged_program);
    unchanged_clone
        .bind_vm_with_plan(&mut unchanged_vm, &unchanged_plan)
        .expect("an unchanged registry clone should reuse the plan");

    let branch_program = build_request_program("http://127.0.0.1:1/".to_string());
    let branch_registry = HostFunctionRegistry::restricted();
    let mut first_mutation = branch_registry.clone();
    let mut second_mutation = branch_registry;
    first_mutation
        .allow_builtin("http::client::request")
        .expect("HTTP capability should be known");
    second_mutation
        .allow_builtin("io::open")
        .expect("io capability should be known");
    let plan = first_mutation
        .prepare_plan(&branch_program.imports)
        .expect("first capability branch should prepare HTTP plan");
    let mut mutated_vm = Vm::new(branch_program);
    let error = second_mutation
        .bind_vm_with_plan(&mut mutated_vm, &plan)
        .expect_err("divergent capability branches must reject each other's plan");
    assert!(error.to_string().contains("different capability state"));
}

#[test]
fn registry_state_rejects_structural_sibling_mutations() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let registry = HostFunctionRegistry::new();
    let mut source = registry.clone();
    let destination = registry;
    source.register_static_args("test::structural", 0, |_args| {
        Ok(CallOutcome::Return(CallReturn::One(Value::Null)))
    });
    let plan = source
        .prepare_plan(&program.imports)
        .expect("mutated source registry should prepare HTTP plan");
    let mut vm = Vm::new(program);
    let error = destination
        .bind_vm_with_plan(&mut vm, &plan)
        .expect_err("structural sibling mutation must reject the plan");
    assert!(error.to_string().contains("different registry state"));
}

#[test]
fn cached_plan_refreshes_after_a_sibling_registry_mutation() {
    let program = build_request_program("http://127.0.0.1:1/".to_string());
    let registry = HostFunctionRegistry::new();
    let mut mutating_sibling = registry.clone();
    let destination = registry;

    let mut priming_vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    destination
        .bind_vm_cached(&mut priming_vm)
        .expect("destination should prime its plan cache");
    mutating_sibling.register_static_args("test::cache_refresh", 0, |_args| {
        Ok(CallOutcome::Return(CallReturn::One(Value::Null)))
    });

    let mut refreshed_vm = Vm::new(program);
    destination
        .bind_vm_cached(&mut refreshed_vm)
        .expect("destination should rebuild a plan after sibling mutation");
}

#[tokio::test(flavor = "current_thread")]
async fn explicitly_allowed_http_capability_reaches_http_policy() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![1],
        allow_private_ips: true,
        ..HttpConfig::default()
    });
    let mut registry = HostFunctionRegistry::restricted();
    registry
        .allow_builtin("http::client::request")
        .expect("HTTP builtin should be explicitly allowlisted");
    registry
        .bind_vm_cached(&mut vm)
        .expect("explicit capability plan should bind");
    let error = drive_vm_to_halt(&mut vm)
        .await
        .expect_err("connection failure should reach HTTP runtime");
    assert!(!matches!(error, vm::VmError::UnboundImport(_)));
}

#[test]
fn http_in_flight_limit_rejects_before_starting_a_request() {
    let mut vm = Vm::new(build_request_program("http://127.0.0.1:1/".to_string()));
    vm.set_http_max_in_flight(0);
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![1],
        allow_private_ips: true,

        ..HttpConfig::default()
    });
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default host registry should bind HTTP");
    let error = vm
        .run()
        .expect_err("zero in-flight capacity must reject the request");
    assert!(error.to_string().contains("in-flight request limit"));
}
