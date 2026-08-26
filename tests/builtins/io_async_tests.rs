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

#[test]
fn async_io_round_trips_file_operations_through_host_driver() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("pd-vm-async-io-{}-{nonce}", std::process::id()));

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "w");
        io::write(handle, "host-driven");
        io::flush(handle);
        io::close(handle);
        io::exists("{}");
        "#,
        path.display(),
        path.display(),
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
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "pd-vm-async-read-line-{}-{nonce}",
        std::process::id()
    ));
    std::fs::write(&path, "first\nsecond\n").expect("fixture should be written");

    let stack = run_source(&format!(
        r#"
        let handle = io::open("{}", "r");
        io::read_line(handle);
        let second = io::read_line(handle);
        io::close(handle);
        second;
        "#,
        path.display(),
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

#[test]
fn io_implementations_do_not_create_private_threads_or_runtimes() {
    let async_source = include_str!("../../src/builtins/runtime/io/async_io.rs");
    let blocking_source = include_str!("../../src/builtins/runtime/io/blocking.rs");

    // The async implementation must run on the bridge's executor: it must
    // not spawn its own threads or build its own tokio runtime.
    assert!(!async_source.contains("thread::Builder"));
    assert!(!async_source.contains("runtime::Builder"));
    assert!(!async_source.contains("spawn_blocking"));
    // The blocking implementation must not create a private runtime either;
    // per-op worker threads are driven by the blocking path itself.
    assert!(!blocking_source.contains("runtime::Builder"));
}
