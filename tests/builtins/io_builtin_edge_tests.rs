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
fn io_open_read_mode_reports_missing_file() {
    let err = run_source_host_error(
        r#"
        io::open("__pd_vm_missing_file_for_test__.txt", "r");
    "#,
    );
    assert!(
        err.contains("io_open failed"),
        "unexpected error message: {err}"
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
        let handle = io::popen("echo hello", "w");
        io::read_all(handle);
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
        let handle = io::popen("echo hello", "r");
        io::write(handle, "payload");
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
fn io_flush_on_read_handle_is_a_noop_true() {
    let stack = run_source(
        r#"
        let handle = io::popen("echo hello", "r");
        io::flush(handle);
    "#,
    )
    .expect("program should execute");
    assert_eq!(stack.last(), Some(&Value::Bool(true)));
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
        let handle = io::open("{}", "w");
        io::write(handle, "four");
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

    assert!(matches!(
        vm.run().expect("open should start"),
        VmStatus::Waiting(_)
    ));
    vm.wait_for_host_op_blocking()
        .expect("open should complete");
    let error = vm.resume().expect_err("oversized write should be denied");
    assert!(matches!(error, VmError::HostError(message) if message.contains("write limit")));
    let _ = std::fs::remove_file(path);
}
