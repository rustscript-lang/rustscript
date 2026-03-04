#![cfg(feature = "runtime")]

use vm::{Value, Vm, VmError, VmStatus, compile_source};

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
