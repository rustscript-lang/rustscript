use vm::{Value, Vm, VmError, VmStatus, compile_source};

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
fn io_callback_resource_is_registered_before_worker_spawn() {
    let source = include_str!("../../src/builtins/runtime/io.rs");
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
    let worker_spawn = schedule
        .find(".spawn(move ||")
        .expect("schedule_io_task should spawn its worker");

    assert!(
        callback_registration < worker_spawn,
        "callback receiver must be registered before the worker can run"
    );
}

#[test]
fn popen_teardown_does_not_invoke_external_kill_programs() {
    let source = include_str!("../../src/builtins/runtime/io.rs");
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
        let handle = io::popen("{command}", "r");
        io::read_all(handle);
        "#
    ))
    .expect("descendant popen source should compile");
    let mut vm = Vm::new(compiled.program);

    let first = vm.run().expect("popen should start");
    assert!(matches!(first, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("popen should complete");
    let second = vm.resume().expect("read_all should start");
    assert!(matches!(second, VmStatus::Waiting(_)));

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
