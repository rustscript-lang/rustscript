use vm::{IoHostExt, Value, Vm, VmError, VmStatus, compile_source};

fn run_source(source: &str) -> Result<Vec<Value>, VmError> {
    let wrapped = format!("use io;\n{source}");
    let compiled = compile_source(&wrapped).expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(vm.stack().to_vec()),
            VmStatus::Yielded => {
                status = vm.resume()?;
            }
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()?;
                status = vm.resume()?;
            }
        }
    }
}

fn run_source_host_error(source: &str) -> String {
    match run_source(source) {
        Ok(stack) => panic!("expected host error, got stack: {stack:?}"),
        Err(VmError::HostError(message)) => message,
        Err(other) => panic!("expected host error, got: {other:?}"),
    }
}

fn run_vm_to_error(vm: &mut Vm) -> VmError {
    let mut status = match vm.run() {
        Ok(status) => status,
        Err(error) => return error,
    };
    loop {
        status = match status {
            VmStatus::Halted => panic!("expected host error, VM halted"),
            VmStatus::Yielded => match vm.resume() {
                Ok(status) => status,
                Err(error) => return error,
            },
            VmStatus::Waiting(_) => {
                if let Err(error) = vm.wait_for_host_op_blocking() {
                    return error;
                }
                match vm.resume() {
                    Ok(status) => status,
                    Err(error) => return error,
                }
            }
        };
    }
}

#[test]
fn io_open_rejects_unsupported_mode() {
    let err = run_source_host_error(
        r#"
        io::open("Cargo.toml", "bad");
    "#,
    );
    assert!(
        err.contains("unsupported io_open mode"),
        "unexpected error message: {err}"
    );
}

#[test]
fn io_open_missing_file_is_a_guest_error() {
    let stack = run_source(
        r#"
        match io::open("__pd_vm_missing_file_for_test__.txt", "r") {
            Result::Ok(handle) => "",
            Result::Err(message) => message,
        };
    "#,
    )
    .expect("a missing file must not abort the VM");
    let Some(Value::String(message)) = stack.last() else {
        panic!("a missing file must report a guest Err string, got {stack:?}");
    };
    assert!(
        message.contains("io_open failed"),
        "the guest error must keep the operating-system text: {message}"
    );
}

#[test]
fn io_popen_rejects_invalid_mode() {
    let err = run_source_host_error(
        r#"
        io::popen("echo hello", "x");
    "#,
    );
    assert!(
        err.contains("unsupported io_popen mode"),
        "unexpected error message: {err}"
    );
}

#[test]
fn io_read_all_rejects_write_only_popen_handle() {
    let err = run_source_host_error(
        r#"
        fn probe() -> Result<int, string> {
            let handle = io::popen("echo hello", "w")?;
            let text = io::read_all(handle)?;
            Result::Ok(text.length)
        }
        probe();
    "#,
    );
    assert!(
        err.contains("requires a readable handle"),
        "unexpected error message: {err}"
    );
}

#[test]
fn io_write_rejects_read_only_popen_handle() {
    let err = run_source_host_error(
        r#"
        fn probe() -> Result<int, string> {
            let handle = io::popen("echo hello", "r")?;
            let written = io::write(handle, "payload")?;
            Result::Ok(written)
        }
        probe();
    "#,
    );
    assert!(
        err.contains("requires a writable handle"),
        "unexpected error message: {err}"
    );
}

#[test]
fn io_close_rejects_non_positive_handle_id() {
    let err = run_source_host_error(
        r#"
        io::close(0);
    "#,
    );
    assert!(
        err.contains("invalid io handle id"),
        "unexpected error message: {err}"
    );
}

#[test]
fn io_flush_on_read_handle_is_a_noop_unit_result() {
    let stack = run_source(
        r#"
        fn flush_read_handle() -> Result<bool, string> {
            let handle = io::popen("echo hello", "r")?;
            let flushed = io::flush(handle)?;
            io::close(handle)?;
            Result::Ok(true)
        }
        match flush_read_handle() {
            Result::Ok(value) => value,
            Result::Err(message) => false,
        };
    "#,
    )
    .expect("program should execute");
    assert_eq!(
        stack.last(),
        Some(&Value::Bool(true)),
        "flushing a read-only handle stays a successful command"
    );
}

#[cfg(unix)]
#[test]
fn io_policy_denies_process_launch_when_process_capability_is_disabled() {
    let compiled = compile_source(
        r#"
        use io;
        io::popen("exit 0", "r");
        "#,
    )
    .expect("source should compile");
    let mut registry = vm::HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        vm::CapabilityProfile::builder()
            .allow_builtin(vm::BuiltinFunction::IoPopen)
            .build(),
    );
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(vm::IoPolicy::default());
    registry
        .bind_vm_cached(&mut vm)
        .expect("profile should bind");

    let error = vm.run().expect_err("process launch should be denied");
    assert!(matches!(error, VmError::HostError(message) if message.contains("process capability")));
}

#[test]
fn io_policy_denies_paths_outside_allowed_roots() {
    let compiled = compile_source(
        r#"
        use io;
        io::exists("Cargo.toml");
        "#,
    )
    .expect("source should compile");
    let mut registry = vm::HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        vm::CapabilityProfile::builder()
            .allow_builtin(vm::BuiltinFunction::IoExists)
            .build(),
    );
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(vm::IoPolicy::default());
    registry
        .bind_vm_cached(&mut vm)
        .expect("profile should bind");

    let error = vm.run().expect_err("path should be denied");
    assert!(matches!(error, VmError::HostError(message) if message.contains("allowed roots")));
}

#[test]
fn restricted_registry_defaults_to_deny_when_io_host_state_is_absent() {
    let compiled = compile_source(
        r#"
        use io;
        io::exists("Cargo.toml");
        "#,
    )
    .expect("source should compile");
    let mut registry = vm::HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        vm::CapabilityProfile::builder()
            .allow_builtin(vm::BuiltinFunction::IoExists)
            .build(),
    );
    let mut vm = Vm::new(compiled.program);
    registry
        .bind_vm_cached(&mut vm)
        .expect("profile should bind");

    let error = vm
        .run()
        .expect_err("missing IO host state should use the deny-by-default policy");
    assert!(matches!(error, VmError::HostError(message) if message.contains("allowed roots")));
}

#[test]
fn io_policy_limits_write_size() {
    let path = std::env::temp_dir().join(format!(
        "pd-vm-policy-write-limit-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should follow Unix epoch")
            .as_nanos()
    ));
    let compiled = compile_source(&format!(
        r#"
        use io;
        fn probe() -> Result<int, string> {{
            let handle = io::open("{}", "w")?;
            let written = io::write(handle, "four")?;
            Result::Ok(written)
        }}
        probe();
        "#,
        path.display()
    ))
    .expect("source should compile");
    let policy = vm::IoPolicy {
        allowed_roots: vec![std::env::temp_dir().display().to_string()],
        allow_write: true,
        max_write_bytes: 3,
        ..vm::IoPolicy::default()
    };
    let mut registry = vm::HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        vm::CapabilityProfile::builder()
            .allow_builtin(vm::BuiltinFunction::IoOpen)
            .allow_builtin(vm::BuiltinFunction::IoWrite)
            .build(),
    );
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(policy);
    registry
        .bind_vm_cached(&mut vm)
        .expect("profile should bind");

    let error = run_vm_to_error(&mut vm);
    assert!(matches!(error, VmError::HostError(message) if message.contains("write limit")));
    let _ = std::fs::remove_file(path);
}

/// A unique scratch file for one test run, written with `contents`.
fn scratch_fixture(label: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pd-vm-io-{label}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should follow Unix epoch")
            .as_nanos()
    ));
    std::fs::write(&path, contents).expect("fixture should be writable");
    path
}

#[test]
fn io_read_line_at_eof_is_ok_none() {
    let path = scratch_fixture("eof", "");
    let stack = run_source(&format!(
        r#"
        fn probe() -> Result<bool, string> {{
            let handle = io::open("{path}", "r")?;
            let line = io::read_line(handle)?;
            io::close(handle)?;
            Result::Ok(match line {{
                Option::Some(_) => false,
                Option::None => true,
            }})
        }}
        match probe() {{
            Result::Ok(value) => value,
            Result::Err(message) => false,
        }};
        "#,
        path = path.display()
    ))
    .expect("EOF must be a guest outcome, not a host error");
    assert_eq!(
        stack.last(),
        Some(&Value::Bool(true)),
        "end of file must be Ok(None)"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn io_read_line_reports_an_empty_line_as_ok_some_empty() {
    let path = scratch_fixture("empty-line", "\nsecond\n");
    let stack = run_source(&format!(
        r#"
        fn probe() -> Result<bool, string> {{
            let handle = io::open("{path}", "r")?;
            let first = io::read_line(handle)?;
            let second = io::read_line(handle)?;
            let third = io::read_line(handle)?;
            io::close(handle)?;
            let empty_line = match first {{
                Option::Some(line) => line.length == 0,
                Option::None => false,
            }};
            let content = match second {{
                Option::Some(line) => line == "second",
                Option::None => false,
            }};
            let eof = match third {{
                Option::Some(_) => false,
                Option::None => true,
            }};
            Result::Ok(empty_line && content && eof)
        }}
        match probe() {{
            Result::Ok(value) => value,
            Result::Err(message) => false,
        }};
        "#,
        path = path.display()
    ))
    .expect("empty lines must be a guest outcome");
    assert_eq!(
        stack.last(),
        Some(&Value::Bool(true)),
        "a real empty line must be Ok(Some(\"\")) and the terminator must not leak"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn io_read_all_on_an_empty_file_is_ok_empty_string() {
    let path = scratch_fixture("empty-read", "");
    let stack = run_source(&format!(
        r#"
        fn probe() -> Result<bool, string> {{
            let handle = io::open("{path}", "r")?;
            let text = io::read_all(handle)?;
            io::close(handle)?;
            Result::Ok(text == "")
        }}
        match probe() {{
            Result::Ok(value) => value,
            Result::Err(message) => false,
        }};
        "#,
        path = path.display()
    ))
    .expect("an empty file must read as an empty string");
    assert_eq!(stack.last(), Some(&Value::Bool(true)));
    let _ = std::fs::remove_file(path);
}

#[test]
fn io_write_reports_the_bytes_it_wrote() {
    let path = scratch_fixture("write-count", "");
    let stack = run_source(&format!(
        r#"
        fn probe() -> Result<bool, string> {{
            let handle = io::open("{path}", "w")?;
            let written = io::write(handle, "abcde")?;
            io::flush(handle)?;
            io::close(handle)?;
            Result::Ok(written == 5)
        }}
        match probe() {{
            Result::Ok(value) => value,
            Result::Err(message) => false,
        }};
        "#,
        path = path.display()
    ))
    .expect("a write must report its byte count");
    assert_eq!(stack.last(), Some(&Value::Bool(true)));
    assert_eq!(
        std::fs::read_to_string(&path).expect("the written file must exist"),
        "abcde"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn io_close_reports_unit_and_retires_the_scope_resource() {
    let compiled = compile_source(
        r#"
        use io;
        fn probe() -> Result<bool, string> {
            let handle = io::open("Cargo.toml", "r")?;
            io::close(handle)?;
            Result::Ok(true)
        }
        match probe() {
            Result::Ok(value) => value,
            Result::Err(message) => false,
        };
        "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    let mut status = vm.run().expect("close must not abort the VM");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => status = vm.resume().expect("resume"),
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking().expect("complete op");
                status = vm.resume().expect("resume");
            }
        }
    }
    assert_eq!(
        vm.stack().last(),
        Some(&Value::Bool(true)),
        "close must report the logical Unit success"
    );
    assert!(
        vm.execution_scope().resources().is_empty(),
        "a successful close must retire its handle"
    );
}
