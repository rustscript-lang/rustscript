use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use vm::{HostFunctionRegistry, HttpConfig, Program, Value, Vm, VmStatus, compile_source};

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
