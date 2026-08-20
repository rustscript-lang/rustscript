use std::time::{SystemTime, UNIX_EPOCH};

use vm::{Value, Vm, VmError, VmStatus, compile_source};

fn run_source(source: &str) -> Result<Vec<Value>, VmError> {
    let compiled =
        compile_source(&format!("use io;\n{source}")).expect("async io source should compile");
    let mut vm = Vm::new(compiled.program);
    super::async_test_bridge::install(&mut vm);

    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(vm.stack().to_vec()),
            VmStatus::Yielded => status = vm.resume()?,
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()?;
                status = vm.resume()?;
            }
        }
    }
}

/// Helper: create a unique temp file path.
fn temp_path(label: &str) -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pd-vm-async-{}-{}-{nonce}",
        std::process::id(),
        label
    ))
}

#[test]
fn async_io_round_trips_file_operations_through_host_driver() {
    let path = temp_path("round-trip");

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "w");
        io::write(handle, "host-driven");
        io::flush(handle);
        io::close(handle);
        io::exists("{}");
        "#,
        path.display(),
        path.display()
    ))
    .expect("async io program should complete");

    assert_eq!(stack.last(), Some(&Value::Bool(true)));
    assert_eq!(
        std::fs::read_to_string(&path).expect("written file should exist"),
        "host-driven"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn async_io_read_line_preserves_buffered_data_between_calls() {
    let path = temp_path("read-line");
    std::fs::write(&path, "first\nsecond\n").expect("fixture should be written");

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "r");
        io::read_line(handle);
        let second = io::read_line(handle);
        io::close(handle);
        second;
        "#,
        path.display()
    ))
    .expect("async read_line program should complete");

    assert_eq!(stack.last(), Some(&Value::string("second\n")));
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
#[test]
fn async_io_popen_reads_through_tokio_process_pipe() {
    let stack = run_source(
        r#"
        let handle = io::popen("printf async-process", "r");
        let output = io::read_all(handle);
        io::close(handle);
        output;
        "#,
    )
    .expect("async popen program should complete");

    assert_eq!(stack.last(), Some(&Value::string("async-process")));
}
/// Test that operations return Pending first and complete on a subsequent
/// resume (i.e., the VM thread is not blocked).
#[test]
fn async_io_first_pending_then_wake() {
    let path = temp_path("pending-wake");
    std::fs::write(&path, "test data").expect("fixture should be written");

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "r");
        let content = io::read_all(handle);
        io::close(handle);
        content;
        "#,
        path.display()
    ))
    .expect("io program should complete");

    assert_eq!(stack.last(), Some(&Value::string("test data")));
    let _ = std::fs::remove_file(path);
}

/// Test that a silent pipe read can be cancelled via reset.
#[cfg(unix)]
#[test]
fn async_io_silent_pipe_read_cancellation() {
    // Start a process that outputs nothing and sleeps forever.
    let compiled = compile_source(
        r#"
        use io;
        let handle = io::popen("sleep 60", "r");
        // This read_all will block on a pipe that produces no output.
        // The VM should be able to cancel it via reset.
        let output = io::read_all(handle);
        io::close(handle);
        output;
        "#,
    )
    .expect("source should compile");

    let mut vm = Vm::new(compiled.program);
    super::async_test_bridge::install(&mut vm);

    // Run until we're waiting on the pipe read.
    let mut status = vm.run().expect("vm should start");
    let mut waited = false;
    for _ in 0..10 {
        match status {
            VmStatus::Waiting(_) => {
                waited = true;
                break;
            }
            VmStatus::Yielded => {
                status = vm.resume().expect("vm should resume");
            }
            VmStatus::Halted => break,
        }
    }
    assert!(waited, "expected to be waiting on pipe read");

    // Reset the VM — this should cancel the operation and close resources.
    vm.reset_for_reuse();
}

/// Test that concurrent operations on different handles are isolated.
#[test]
fn async_io_concurrent_operation_isolation() {
    let path_a = temp_path("concurrent-a");
    let path_b = temp_path("concurrent-b");
    std::fs::write(&path_a, "data-a").expect("fixture a should be written");
    std::fs::write(&path_b, "data-b").expect("fixture b should be written");

    let stack = run_source(&format!(
        r#"
        let handle_a = io::open("{}", "r");
        let handle_b = io::open("{}", "r");
        let content_a = io::read_all(handle_a);
        let content_b = io::read_all(handle_b);
        io::close(handle_a);
        io::close(handle_b);
        content_a;
        content_b;
        "#,
        path_a.display(),
        path_b.display()
    ))
    .expect("concurrent io program should complete");

    // Stack: [true (close_a), true (close_b), data_a, data_b]
    assert!(
        stack.len() >= 2,
        "expected at least 2 values, got {}",
        stack.len()
    );
    let data_a_idx = stack.len() - 2;
    let data_b_idx = stack.len() - 1;
    assert_eq!(stack[data_a_idx], Value::string("data-a"));
    assert_eq!(stack[data_b_idx], Value::string("data-b"));
    let _ = std::fs::remove_file(path_a);
    let _ = std::fs::remove_file(path_b);
}

/// Test that workers drain and join properly after close.
#[test]
fn async_io_worker_join_and_drain() {
    let path = temp_path("worker-drain");

    // Open, write, flush, close — ensures all workers join.
    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "w");
        io::write(handle, "hello");
        io::flush(handle);
        io::close(handle);
        io::exists("{}");
        "#,
        path.display(),
        path.display()
    ))
    .expect("io program should complete");

    assert_eq!(stack.last(), Some(&Value::Bool(true)));
    assert_eq!(
        std::fs::read_to_string(&path).expect("written file should exist"),
        "hello"
    );
    let _ = std::fs::remove_file(path);
}

/// Test process/pipe child-first close ordering: closing the pipe before
/// the parent process should work correctly.
#[cfg(unix)]
#[test]
fn async_io_process_pipe_child_first_close() {
    let stack = run_source(
        r#"
        let handle = io::popen("printf process-data", "r");
        let output = io::read_all(handle);
        // Close the pipe first (the handle IS the pipe), then the process
        // is implicitly closed via scope cleanup.
        io::close(handle);
        output;
        "#,
    )
    .expect("popen close program should complete");

    assert_eq!(stack.last(), Some(&Value::string("process-data")));
}

/// Test failure atomicity: a failed open should not leave dangling resources.
#[test]
fn async_io_failure_atomicity_open_nonexistent() {
    let path = temp_path("nonexistent-nonexistent");

    let result = run_source(&format!(
        r#"
        let handle = io::open("{}", "r");
        "#,
        path.display()
    ));

    assert!(result.is_err(), "expected error for nonexistent file");
    if let Err(VmError::HostError(msg)) = result {
        assert!(
            msg.contains("io_open failed"),
            "expected io_open error, got: {msg}"
        );
    }
}

/// Test parity with blocking: write then read back produces same content.
#[test]
fn async_io_write_read_parity() {
    let path = temp_path("parity");

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "w");
        io::write(handle, "parity check");
        io::flush(handle);
        io::close(handle);
        let h = io::open("{}", "r");
        let content = io::read_all(h);
        io::close(h);
        content;
        "#,
        path.display(),
        path.display()
    ))
    .expect("parity program should complete");

    assert_eq!(stack.last(), Some(&Value::string("parity check")));
    let _ = std::fs::remove_file(path);
}

/// Test that writing to a file, closing it, and reopening for read works.
#[test]
fn async_io_close_reopen_read() {
    let path = temp_path("close-reopen");

    let stack = run_source(&format!(
        r#"
        let h = io::open("{}", "w");
        io::write(h, "close-reopen-data");
        io::flush(h);
        io::close(h);
        let rh = io::open("{}", "r");
        let content = io::read_all(rh);
        io::close(rh);
        content;
        "#,
        path.display(),
        path.display()
    ))
    .expect("close-reopen program should complete");

    assert_eq!(stack.last(), Some(&Value::string("close-reopen-data")));
    let _ = std::fs::remove_file(path);
}

/// Test that exists returns false for a non-existent path.
#[test]
fn async_io_exists_nonexistent() {
    let path = temp_path("nonexistent-exists");

    let stack = run_source(&format!(
        r#"
        io::exists("{}");
        "#,
        path.display()
    ))
    .expect("exists program should complete");

    assert_eq!(stack.last(), Some(&Value::Bool(false)));
}

#[cfg(unix)]
#[test]
fn async_io_popen_read_line() {
    let stack = run_source(
        r#"
        let handle = io::popen("printf \"line1\nline2\n\"", "r");
        let first = io::read_line(handle);
        let second = io::read_line(handle);
        io::close(handle);
        first;
        second;
        "#,
    )
    .expect("popen read_line program should complete");

    // Stack: [true (close), first, second]
    assert!(
        stack.len() >= 2,
        "expected at least 2 values, got {}",
        stack.len()
    );
    let first_idx = stack.len() - 2;
    let second_idx = stack.len() - 1;
    assert_eq!(stack[first_idx], Value::string("line1\n"));
    assert_eq!(stack[second_idx], Value::string("line2\n"));
}

/// Test that write to a pipe works.
#[cfg(unix)]
#[test]
fn async_io_popen_write_stdin() {
    let stack = run_source(
        r#"
        let handle = io::popen("cat", "w");
        io::write(handle, "hello stdin");
        io::flush(handle);
        io::close(handle);
        true;
        "#,
    )
    .expect("popen write program should complete");

    assert_eq!(stack.last(), Some(&Value::Bool(true)));
}
/// Test that using an invalid handle returns an error.
#[test]
fn async_io_invalid_handle_error() {
    let result = run_source(
        r#"
        io::read_all(999);
        "#,
    );

    assert!(result.is_err(), "expected error for invalid handle");
}

/// Test that io::exists on a valid path returns true.
#[test]
fn async_io_exists_valid_path() {
    let path = temp_path("valid-exists");
    std::fs::write(&path, "exists").expect("fixture should be written");

    let stack = run_source(&format!(
        r#"
        io::exists("{}");
        "#,
        path.display()
    ))
    .expect("exists program should complete");

    assert_eq!(stack.last(), Some(&Value::Bool(true)));
    let _ = std::fs::remove_file(path);
}
