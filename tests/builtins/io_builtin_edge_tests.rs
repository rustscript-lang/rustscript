use vm::{
    BuiltinFunction, CapabilityProfile, HostFunctionRegistry, IoHostExt, IoPolicy, Value, Vm,
    VmError, VmStatus, compile_source,
};

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
fn io_policy_denies_process_launch_when_process_capability_is_disabled() {
    let compiled = compile_source(
        r#"
        use io;
        io::popen("exit 0", "r");
        "#,
    )
    .expect("source should compile");
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_builtin(BuiltinFunction::IoPopen)
            .build(),
    );
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy::default());
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
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_builtin(BuiltinFunction::IoExists)
            .build(),
    );
    let mut vm = Vm::new(compiled.program);
    vm.configure_io(IoPolicy::default());
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
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_builtin(BuiltinFunction::IoExists)
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

#[cfg(unix)]
#[test]
fn io_policy_limits_write_size() {
    let path = unique_temp_path("policy-write-limit");
    let compiled = compile_source(&format!(
        r#"
        use io;
        let handle = io::open("{}", "w");
        io::write(handle, "four");
        "#,
        path.display()
    ))
    .expect("source should compile");
    let policy = IoPolicy {
        allowed_roots: vec![std::env::temp_dir().display().to_string()],
        allow_write: true,
        max_write_bytes: 3,
        ..IoPolicy::default()
    };
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_builtin(BuiltinFunction::IoOpen)
            .allow_builtin(BuiltinFunction::IoWrite)
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

#[cfg(unix)]
#[test]
fn io_policy_limits_read_all_size() {
    let path = unique_temp_path("policy-read-limit");
    std::fs::write(&path, "four").expect("fixture should be written");
    let compiled = compile_source(&format!(
        r#"
        use io;
        let handle = io::open("{}", "r");
        io::read_all(handle);
        "#,
        path.display()
    ))
    .expect("source should compile");
    let policy = IoPolicy {
        allowed_roots: vec![std::env::temp_dir().display().to_string()],
        max_read_bytes: 3,
        ..IoPolicy::default()
    };
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_builtin(BuiltinFunction::IoOpen)
            .allow_builtin(BuiltinFunction::IoReadAll)
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
    assert!(matches!(
        vm.resume().expect("read should start"),
        VmStatus::Waiting(_)
    ));
    let error = vm
        .wait_for_host_op_blocking()
        .expect_err("oversized read should be denied");
    assert!(matches!(error, VmError::HostError(message) if message.contains("read limit")));
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
#[test]
fn io_policy_limits_read_line_size() {
    let path = unique_temp_path("policy-read-line-limit");
    std::fs::write(&path, "four\n").expect("fixture should be written");
    let compiled = compile_source(&format!(
        r#"
        use io;
        let handle = io::open("{}", "r");
        io::read_line(handle);
        "#,
        path.display()
    ))
    .expect("source should compile");
    let policy = IoPolicy {
        allowed_roots: vec![std::env::temp_dir().display().to_string()],
        max_read_bytes: 3,
        ..IoPolicy::default()
    };
    let mut registry = HostFunctionRegistry::restricted();
    registry.set_capability_profile(
        CapabilityProfile::builder()
            .allow_builtin(BuiltinFunction::IoOpen)
            .allow_builtin(BuiltinFunction::IoReadLine)
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
    assert!(matches!(
        vm.resume().expect("read should start"),
        VmStatus::Waiting(_)
    ));
    let error = vm
        .wait_for_host_op_blocking()
        .expect_err("oversized line should be denied");
    assert!(matches!(error, VmError::HostError(message) if message.contains("read limit")));
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn unique_temp_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should follow the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("pd-vm-{label}-{}-{nonce}", std::process::id()))
}

#[cfg(unix)]
fn process_exists(process_id: i32) -> bool {
    // SAFETY: signal zero performs existence/permission checking without sending a signal.
    let result = unsafe { libc::kill(process_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[test]
fn blocking_io_runs_after_callback_registration_without_spawning_a_worker() {
    let source = include_str!("../../src/builtins/runtime/io/blocking.rs");
    let schedule = source
        .split_once("fn schedule_io_task(")
        .expect("schedule_io_task should exist")
        .1
        .split_once("fn runtime_host_error(")
        .expect("schedule_io_task should precede runtime_host_error")
        .0;
    let callback_registration = schedule
        .find(".insert(ResourceTypeId::CALLBACK, receiver)")
        .expect("schedule_io_task should register its callback receiver");

    assert!(!schedule.contains(".spawn(move ||"));
    assert!(schedule[callback_registration..].contains("task()"));
}

#[test]
fn popen_teardown_does_not_invoke_external_kill_programs() {
    let source = include_str!("../../src/builtins/runtime/io/blocking.rs");
    assert!(
        !source.contains("Command::new(\"kill\")"),
        "Unix popen teardown must use the platform process API"
    );
    assert!(
        !source.contains("Command::new(\"taskkill\")"),
        "Windows popen teardown must use the platform process API"
    );
}

#[cfg(unix)]
#[test]
fn reset_terminates_popen_descendants() {
    let child_pid_path = unique_temp_path("popen-descendant-pid");
    let command = format!(
        "sleep 3600 & child=$!; echo $child > {}; wait",
        child_pid_path.display()
    );
    let compiled = compile_source(&format!(
        r#"
        use io;
        io::popen("{command}", "r");
        "#
    ))
    .expect("descendant popen source should compile");
    let mut vm = Vm::new(compiled.program);

    let first = vm.run().expect("popen should start");
    assert!(matches!(first, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("popen should complete");

    let pid_deadline = Instant::now() + Duration::from_secs(2);
    while !child_pid_path.exists() && Instant::now() < pid_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let child_pid = std::fs::read_to_string(&child_pid_path)
        .expect("popen command should publish its descendant pid")
        .trim()
        .parse::<i32>()
        .expect("descendant pid should be numeric");
    assert!(process_exists(child_pid), "descendant should be running");

    vm.reset_for_reuse();

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    while process_exists(child_pid) && Instant::now() < exit_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = std::fs::remove_file(&child_pid_path);
    assert!(
        !process_exists(child_pid),
        "popen descendant {child_pid} survived VM reset"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "blocking IO runs the read on the caller thread"]
fn reset_interrupts_a_blocked_popen_read_within_a_bounded_time() {
    let compiled = compile_source(
        r#"
        use io;
        let handle = io::popen("sleep 3600", "r");
        io::read_all(handle);
        "#,
    )
    .expect("blocking popen source should compile");
    let mut vm = Vm::new(compiled.program);

    let first = vm.run().expect("popen should start");
    assert!(matches!(first, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("popen should complete");
    let second = vm.resume().expect("read_all should start");
    assert!(matches!(second, VmStatus::Waiting(_)));
    std::thread::sleep(Duration::from_millis(25));

    let started = Instant::now();
    vm.reset_for_reuse();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reset exceeded bounded I/O teardown window: {:?}",
        started.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn reset_reaps_a_popen_child_before_completion_is_polled() {
    let compiled = compile_source(
        r#"
        use io;
        io::popen("sleep 3599", "r");
        "#,
    )
    .expect("popen source should compile");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("popen should enter waiting state");
    assert!(matches!(status, VmStatus::Waiting(_)));
    std::thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    vm.reset_for_reuse();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reset exceeded queued-completion teardown window: {:?}",
        started.elapsed()
    );
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

#[test]
fn io_close_rejects_a_stale_resource_handle() {
    let err = run_source_host_error(
        r#"
        let handle = io::open("Cargo.toml", "r");
        io::close(handle);
        io::close(handle);
    "#,
    );
    assert!(
        err.contains("resource_already_closed"),
        "unexpected error message: {err}"
    );
}

#[test]
fn io_handles_cannot_cross_vm_resource_arenas() {
    let stack = run_source(r#"io::open("Cargo.toml", "r");"#)
        .expect("first VM should open a file resource");
    let Value::Int(handle) = stack.last().expect("open should return a handle") else {
        panic!("open should return an integer resource handle");
    };

    let err = run_source_host_error(&format!("io::close({handle});"));
    assert!(
        err.contains("resource_handle_wrong_table"),
        "unexpected error message: {err}"
    );
}
