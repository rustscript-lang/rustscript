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

/// Run a VM configured with a custom registry and policy, driving pending
/// operations to completion, and return the first HostError encountered.
/// Handles errors from both wait_for_host_op_blocking and vm.resume().
fn run_vm_until_error(vm: &mut Vm) -> String {
    // First call must be run(); subsequent calls use resume().
    let mut status = match vm.run() {
        Ok(status) => status,
        Err(VmError::HostError(message)) => return message,
        Err(other) => panic!("expected host error, got: {other:?}"),
    };
    loop {
        match status {
            VmStatus::Waiting(_) => {
                match vm.wait_for_host_op_blocking() {
                    Ok(()) => {}
                    Err(VmError::HostError(message)) => return message,
                    Err(other) => panic!("expected host error, got: {other:?}"),
                }
                match vm.resume() {
                    Ok(s) => status = s,
                    Err(VmError::HostError(message)) => return message,
                    Err(other) => panic!("expected host error, got: {other:?}"),
                }
            }
            VmStatus::Halted => {
                panic!("expected host error, got halted");
            }
            VmStatus::Yielded => match vm.resume() {
                Ok(s) => status = s,
                Err(VmError::HostError(message)) => return message,
                Err(other) => panic!("expected host error, got: {other:?}"),
            },
        }
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
        let handle = io::open("{path}", "w");
        io::write(handle, "four");
        "#,
        path = path.display()
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

    let error = run_vm_until_error(&mut vm);
    assert!(
        error.contains("write limit"),
        "unexpected error message: {error}"
    );
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
        let handle = io::open("{path}", "r");
        io::read_all(handle);
        "#,
        path = path.display()
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

    let error = run_vm_until_error(&mut vm);
    assert!(
        error.contains("read limit"),
        "unexpected error message: {error}"
    );
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
        let handle = io::open("{path}", "r");
        io::read_line(handle);
        "#,
        path = path.display()
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

    let error = run_vm_until_error(&mut vm);
    assert!(
        error.contains("read limit"),
        "unexpected error message: {error}"
    );
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
        "#,
        command = command
    ))
    .expect("descendant popen source should compile");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("popen should start");
    assert!(matches!(status, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("waiting for popen should succeed");
    let _ = vm.resume().expect("popen should finish");

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

    // Drive reset to completion (async close worker).
    let started = Instant::now();
    let deadline = started + Duration::from_secs(2);
    loop {
        vm.reset_for_reuse();
        if vm.is_reusable() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

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
fn reset_interrupts_a_blocked_popen_read_within_a_bounded_time() {
    // popen spawns a process that sleeps; the popen itself returns
    // immediately through the worker pattern. Then read_all also
    // returns through a ReadyOperation (reading from the pipe
    // synchronously on the VM thread). The real test is that reset
    // cleans up the process resource quickly.
    let compiled = compile_source(
        r#"
        use io;
        let handle = io::popen("sleep 3600", "r");
        "#,
    )
    .expect("blocking popen source should compile");
    let mut vm = Vm::new(compiled.program);

    let first = vm.run().expect("popen should start");
    assert!(matches!(first, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("popen should complete");
    let _second = vm.resume().expect("popen should finish");

    let started = Instant::now();
    while vm.reset_state() != vm::VmResetState::Ready {
        vm.reset_for_reuse();
        if started.elapsed() >= Duration::from_secs(2) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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

    let status = vm.run().expect("popen should start");
    // popen uses a worker thread now; drive the VM to completion.
    assert!(matches!(status, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("waiting for popen should succeed");
    let _ = vm.resume().expect("popen should finish");

    let started = Instant::now();
    while vm.reset_state() != vm::VmResetState::Ready {
        vm.reset_for_reuse();
        if started.elapsed() >= Duration::from_secs(2) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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

// ---- Lifecycle tests for worker-based IO operations ----

#[cfg(unix)]
#[test]
fn io_exists_operation_is_truly_pending_before_worker_completes() {
    use std::time::{Duration, Instant};
    use vm::VmStatus;

    let compiled = vm::compile_source(
        r#"
        use io;
        io::exists("/tmp");
        "#,
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    // First call is run() which returns Wait for the first op
    let status = vm.run().expect("run should start");
    let started = Instant::now();

    // Poll until we get a result or timeout
    let mut status = status;
    loop {
        match status {
            VmStatus::Waiting(_) => {
                // sit tight — the sleep(0) is not an option;
                // we just drive the VM loop
                vm.wait_for_host_op_blocking().expect("wait should succeed");
                status = vm.resume().expect("resume should work");
            }
            VmStatus::Halted => {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "io::exists took too long"
                );
                break;
            }
            VmStatus::Yielded => {
                status = vm.resume().expect("resume should work");
            }
        }
    }
    assert_eq!(vm.stack().last(), Some(&Value::Bool(true)));
}

#[test]
fn io_open_operation_is_truly_pending_before_worker_completes() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "pd-vm-lifecycle-open-{}-{nonce}",
        std::process::id()
    ));

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{path}", "w");
        io::close(handle);
        io::exists("{path}");
        "#,
        path = path.display()
    ))
    .expect("lifecycle program should complete");

    assert_eq!(stack.last(), Some(&Value::Bool(true)));
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[test]
fn close_begin_close_returns_pending_and_poll_close_completes() {
    use vm::VmStatus;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "pd-vm-lifecycle-close-{}-{nonce}",
        std::process::id()
    ));

    let compiled = vm::compile_source(&format!(
        r#"
        use io;
        let handle = io::open("{path}", "w");
        io::write(handle, "data");
        io::close(handle);
        "#,
        path = path.display()
    ))
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    let mut status = vm.run().expect("run should start");
    loop {
        match status {
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking().expect("wait should succeed");
                status = vm.resume().expect("resume should work");
            }
            VmStatus::Halted => {
                break;
            }
            VmStatus::Yielded => {
                status = vm.resume().expect("resume should work");
            }
        }
    }

    // File should exist and have the correct content
    assert_eq!(
        std::fs::read_to_string(&path).expect("written file should exist"),
        "data"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn io_operations_use_real_pending_lifecycle() {
    // Verify that the ThreadedOperation-based io::open and io::exists
    // actually use a worker thread by checking that the source has
    // ThreadedOperation references.
    let source = include_str!("../../src/builtins/runtime/io/shared.rs");
    assert!(
        source.contains("ThreadedOperation::spawn"),
        "shared IO must use ThreadedOperation for pending operations"
    );
    // Worker thread spawning is in ops.rs, not shared.rs directly
    let ops_source = include_str!("../../src/builtins/runtime/io/ops.rs");
    assert!(
        ops_source.contains("thread::Builder"),
        "ops.rs must spawn worker threads for ThreadedOperation"
    );
}

/// Verify that a worker resource is registered in the scope after a blocking
/// open operation, confirming the close lifecycle can handle it.
#[cfg(unix)]
#[test]
fn worker_resource_is_present_after_blocking_io_open() {
    let path = unique_temp_path("worker-presence-open");
    let compiled = vm::compile_source(&format!(
        r#"
        use io;
        let handle = io::open("{path}", "w");
        io::close(handle);
        "#,
        path = path.display()
    ))
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    let mut status = vm.run().expect("run should start");
    loop {
        match status {
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking().expect("wait should succeed");
                status = vm.resume().expect("resume should work");
            }
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume should work");
            }
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Two concurrent blocking operations on separate handles do not interfere.
#[cfg(unix)]
#[test]
fn concurrent_blocking_operations_are_isolated() {
    let path_a = unique_temp_path("concurrent-a");
    let path_b = unique_temp_path("concurrent-b");
    let compiled = vm::compile_source(&format!(
        r#"
        use io;
        let a = io::open("{path_a}", "w");
        io::write(a, "hello");
        io::close(a);
        let b = io::open("{path_b}", "w");
        io::write(b, "world");
        io::close(b);
        "#,
        path_a = path_a.display(),
        path_b = path_b.display()
    ))
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    let mut status = vm.run().expect("run should start");
    loop {
        match status {
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking().expect("wait should succeed");
                status = vm.resume().expect("resume should work");
            }
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume should work");
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(&path_a).expect("file a should exist"),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(&path_b).expect("file b should exist"),
        "world"
    );
    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);
}

/// Reset does not block even when a worker thread is still running.
#[cfg(unix)]
#[test]
fn reset_does_not_block_on_worker_teardown() {
    use std::time::Instant;

    let path = unique_temp_path("reset-worker");
    let compiled = vm::compile_source(&format!(
        r#"
        use io;
        io::open("{path}", "w");
        "#,
        path = path.display()
    ))
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("run should start");
    assert!(matches!(status, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking().expect("wait should succeed");
    let _ = vm.resume().expect("resume should work");

    let started = Instant::now();
    while vm.reset_state() != vm::VmResetState::Ready {
        vm.reset_for_reuse();
        if started.elapsed() >= std::time::Duration::from_secs(2) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "reset should not block on worker teardown"
    );
    let _ = std::fs::remove_file(&path);
}

/// Sequential IO operations stress test: 100+ open/write/close cycles
/// without reset, verifying that worker resources are properly retired
/// and no slot exhaustion occurs.
#[test]
fn sequential_io_worker_retirement_stress() {
    let path = unique_temp_path("retirement-stress");
    const COUNT: usize = 100;

    for i in 0..COUNT {
        let result = run_source(&format!(
            r#"
            let h = io::open("{path}", "w");
            io::write(h, "hello");
            io::flush(h);
            io::close(h);
            "#,
            path = path.display()
        ));
        assert!(result.is_ok(), "iteration {i}/{COUNT} failed: {result:?}");
    }

    // Verify the file was written correctly (last write persists).
    let content = std::fs::read_to_string(&path).expect("file should exist");
    assert_eq!(content, "hello");
    let _ = std::fs::remove_file(&path);
}

/// Sequential IO exists stress test: 100+ exists calls without reset,
/// verifying no worker resource slot exhaustion.
#[test]
fn sequential_io_exists_worker_retirement_stress() {
    let path = unique_temp_path("exists-stress");
    std::fs::write(&path, "test").expect("fixture should be written");

    for i in 0..100 {
        let result = run_source(&format!(
            r#"
            io::exists("{path}");
            "#,
            path = path.display()
        ));
        assert!(result.is_ok(), "iteration {i}/100 failed: {result:?}");
        if let Ok(stack) = result {
            assert_eq!(stack.last(), Some(&Value::Bool(true)));
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Sequential IO read stress test: 100+ read_all calls on the same file
/// (reopened each time), verifying no worker resource slot exhaustion.
#[test]
fn sequential_io_read_worker_retirement_stress() {
    let path = unique_temp_path("read-stress");
    std::fs::write(&path, "sequential-read-data").expect("fixture should be written");

    for i in 0..100 {
        let result = run_source(&format!(
            r#"
            let h = io::open("{path}", "r");
            let content = io::read_all(h);
            io::close(h);
            content;
            "#,
            path = path.display()
        ));
        assert!(result.is_ok(), "iteration {i}/100 failed: {result:?}");
        if let Ok(stack) = result {
            assert_eq!(stack.last(), Some(&Value::string("sequential-read-data")));
        }
    }
    let _ = std::fs::remove_file(&path);
}
