//! Focused TDD tests: `io::read_all` must bound the *actual* read at the
//! Reader boundary by the configured [`IoPolicy::max_read_bytes`], in both the
//! blocking and the async backend.
//!
//! The pre-fix implementation read the whole stream into a `String` and only
//! compared the resulting length afterwards, so a 16 MiB policy still allocated
//! the full source size and a producer that never reaches EOF blocked the call
//! forever. These tests prove the bound through real native evidence:
//!
//! * a pipe producer that counts the bytes it could actually write while the
//!   consumer is bounded (the producer is still mid-stream when the limit
//!   error returns);
//! * byte-boundary fixtures for limit-1 / exact / limit+1 and a stream much
//!   larger than the limit;
//! * UTF-8 character boundaries (a truncated multi-byte tail must report the
//!   limit, not a UTF-8 error) and invalid UTF-8 below the limit (the
//!   pre-existing UTF-8 error is preserved);
//! * the no-policy path keeps the unbounded read;
//! * repeated calls on one handle, and a clean VM (no pending operations, no
//!   leftover resources) after both accepted and rejected reads.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use vm::{IoHostExt, IoPolicy, Value, Vm, VmError, VmStatus, compile_source};

use super::vm_reset::reset_for_reuse_to_ready;

static FIXTURE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn fixture_path(label: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    std::fs::create_dir_all(&dir).expect("fixture directory should exist");
    dir.join(format!(
        "pd-vm-read-bound-{label}-{}-{}",
        std::process::id(),
        FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn policy_with_limit(limit: usize) -> IoPolicy {
    IoPolicy {
        allowed_roots: vec![std::env::temp_dir().display().to_string()],
        max_read_bytes: limit,
        ..IoPolicy::default()
    }
}

fn new_vm(source: &str, policy: Option<IoPolicy>) -> Vm {
    let compiled = compile_source(&format!("use io;\n{source}")).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    if let Some(policy) = policy {
        vm.configure_io(policy);
    }
    #[cfg(feature = "async")]
    super::async_test_bridge::install(&mut vm);
    vm
}

fn drive(vm: &mut Vm) -> Result<Vec<Value>, VmError> {
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

fn host_error(error: VmError) -> String {
    match error {
        VmError::HostError(message) => message,
        other => panic!("expected a host error, got: {other:?}"),
    }
}

/// Drives the VM expecting a host error, reporting a bounded description (not
/// the full decoded payload) when the guest unexpectedly succeeds.
fn drive_error(vm: &mut Vm) -> String {
    match drive(vm) {
        Ok(stack) => {
            let shape = stack
                .last()
                .map(|value| match value {
                    Value::String(text) => format!("String(len={})", text.len()),
                    other => format!("{other:?}"),
                })
                .unwrap_or_else(|| "<empty stack>".to_string());
            panic!("expected a read limit error, got stack: [{shape}]");
        }
        Err(error) => host_error(error),
    }
}

/// Drives the generic execution-scope reset to quiescence, closing every
/// remaining IO resource exactly once through the resource protocol.
fn reset(vm: &mut Vm) {
    #[cfg(feature = "async")]
    {
        tokio::runtime::Runtime::new()
            .expect("reset runtime should build")
            .block_on(async {
                reset_for_reuse_to_ready(vm).expect("reset should reach quiescence");
            });
    }
    #[cfg(not(feature = "async"))]
    {
        reset_for_reuse_to_ready(vm).expect("reset should reach quiescence");
    }
}

fn read_all_program(path: &Path) -> String {
    format!(
        r#"let handle = io::open("{}", "r"); io::read_all(handle);"#,
        path.display()
    )
}

#[cfg(unix)]
fn make_fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .expect("fixture path must not contain NUL");
    let status = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        status,
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );
}

// ------------------------------------------------- actual-read budget (RED)

#[cfg(unix)]
#[test]
fn io_read_all_stops_at_budget_without_waiting_for_producer_eof() {
    // A pipe producer streams 8 MiB in 8 KiB writes and reports how many
    // bytes it could actually write. With a 16-byte policy the limit error
    // must return while the producer is still mid-stream: it can only have
    // filled the socket/pipe buffers, so its accepted byte count stays far
    // below the stream total. The pre-fix implementation read to EOF, so the
    // producer finished the whole stream before the error surfaced.
    const LIMIT: usize = 16;
    const TOTAL: usize = 8 * 1024 * 1024;
    const CHUNK: usize = 8 * 1024;

    let path = fixture_path("fifo");
    make_fifo(&path);

    let producer_path = path.clone();
    let producer = std::thread::spawn(move || -> usize {
        let mut writer = match std::fs::OpenOptions::new().write(true).open(&producer_path) {
            Ok(writer) => writer,
            Err(_) => return 0,
        };
        let chunk = [b'a'; CHUNK];
        let mut written = 0usize;
        while written < TOTAL {
            let want = (TOTAL - written).min(CHUNK);
            match writer.write(&chunk[..want]) {
                Ok(0) => break,
                Ok(count) => written += count,
                Err(_) => break,
            }
        }
        written
    });

    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(LIMIT)));
    let message = drive_error(&mut vm);
    assert_eq!(
        vm.execution_scope().operations().len(),
        0,
        "a rejected read must retire its pending operation"
    );

    // Closing the read end lets the blocked producer observe the closed pipe.
    reset(&mut vm);
    let written = producer.join().expect("producer thread should finish");

    assert!(
        message.contains("io_read_all exceeded read limit"),
        "unexpected error: {message}"
    );
    assert!(
        written < TOTAL / 2,
        "read_all must stop at the budget instead of draining to EOF: producer wrote \
         {written} of {TOTAL} bytes before the limit error"
    );
    assert!(
        vm.execution_scope().resources().is_empty(),
        "closing the denied handle must retire its resource"
    );

    let _ = std::fs::remove_file(path);
}

// -------------------------------------------------- ASCII budget boundaries

#[test]
fn io_read_all_ascii_budget_boundaries_keep_the_vm_clean() {
    const LIMIT: usize = 16;
    let under: &[u8] = b"0123456789abcde"; // limit - 1
    let exact: &[u8] = b"0123456789abcdef"; // limit
    let over: &[u8] = b"0123456789abcdefg"; // limit + 1

    for (label, bytes) in [("under", under), ("exact", exact)] {
        let path = fixture_path(label);
        std::fs::write(&path, bytes).expect("fixture should be written");
        let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(LIMIT)));
        let stack = drive(&mut vm).expect("a payload within the limit must read");
        assert_eq!(
            stack.last(),
            Some(&Value::string(
                String::from_utf8(bytes.to_vec()).expect("fixture is ASCII")
            )),
            "{label} payload should be returned intact"
        );
        assert_eq!(
            vm.execution_scope().operations().len(),
            0,
            "{label}: the read must complete its operation"
        );
        assert!(
            !vm.execution_scope().resources().is_empty(),
            "{label}: the borrowed handle must stay live"
        );
        reset(&mut vm);
        assert!(
            vm.execution_scope().resources().is_empty()
                && vm.execution_scope().operations().is_empty(),
            "{label}: reset must retire the handle and its operation"
        );
        let _ = std::fs::remove_file(path);
    }

    let path = fixture_path("over");
    std::fs::write(&path, over).expect("fixture should be written");
    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(LIMIT)));
    let message = drive_error(&mut vm);
    assert!(
        message.contains("io_read_all exceeded read limit"),
        "unexpected error: {message}"
    );
    assert_eq!(
        vm.execution_scope().operations().len(),
        0,
        "the rejected read must not leave a pending operation"
    );
    reset(&mut vm);
    assert!(
        vm.execution_scope().resources().is_empty() && vm.execution_scope().operations().is_empty(),
        "the same VM must close and reach a clean reset after a denied read"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn io_read_all_rejects_streams_much_larger_than_the_limit() {
    let path = fixture_path("large");
    std::fs::write(&path, vec![b'x'; 1024 * 1024]).expect("fixture should be written");

    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(4096)));
    let message = drive_error(&mut vm);
    assert!(
        message.contains("io_read_all exceeded read limit"),
        "unexpected error: {message}"
    );
    assert_eq!(
        vm.execution_scope().operations().len(),
        0,
        "the rejected read must not leave a pending operation"
    );
    reset(&mut vm);
    assert!(
        vm.execution_scope().resources().is_empty() && vm.execution_scope().operations().is_empty(),
        "the same VM must close and reach a clean reset after a denied read"
    );

    let _ = std::fs::remove_file(path);
}

// ------------------------------------------------------------ UTF-8 handling

#[test]
fn io_read_all_utf8_boundaries_and_error_priority() {
    // "é" is two bytes: "ééé" is six bytes. A limit of six is an exact
    // character boundary; a limit of five splits the last character.
    let utf8 = "ééé".as_bytes();

    let path = fixture_path("utf8-exact");
    std::fs::write(&path, utf8).expect("fixture should be written");
    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(6)));
    let stack = drive(&mut vm).expect("a payload within the limit must read");
    assert_eq!(stack.last(), Some(&Value::string("ééé")));
    reset(&mut vm);
    let _ = std::fs::remove_file(path);

    let path = fixture_path("utf8-cross");
    std::fs::write(&path, utf8).expect("fixture should be written");
    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(5)));
    let message = drive_error(&mut vm);
    assert!(
        message.contains("io_read_all exceeded read limit"),
        "a truncated multi-byte tail must report the limit, got: {message}"
    );
    reset(&mut vm);
    let _ = std::fs::remove_file(path);

    // Invalid UTF-8 below the limit keeps the pre-existing decode error.
    let path = fixture_path("utf8-invalid");
    std::fs::write(&path, [0xFFu8, 0xFE, b'A']).expect("fixture should be written");
    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(16)));
    let message = drive_error(&mut vm);
    assert_eq!(
        message, "io_read_all failed: stream did not contain valid UTF-8",
        "invalid UTF-8 below the limit must keep the original error"
    );
    reset(&mut vm);
    let _ = std::fs::remove_file(path);

    // Invalid UTF-8 *beyond* the limit reports the limit, not the truncation:
    // decoding must only happen after the within-limit check.
    let path = fixture_path("utf8-invalid-over");
    std::fs::write(&path, [0xFFu8; 32]).expect("fixture should be written");
    let mut vm = new_vm(&read_all_program(&path), Some(policy_with_limit(4)));
    let message = drive_error(&mut vm);
    assert!(
        message.contains("io_read_all exceeded read limit"),
        "the over-limit stream must report the limit before any UTF-8 decode, got: {message}"
    );
    reset(&mut vm);
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------- unbounded / reuse

#[cfg(unix)]
#[test]
fn io_read_all_applies_the_budget_to_popen_stdout() {
    // The process-backed read branch consults the same policy limit as the
    // file branch: 40 bytes of child output is rejected under a 16-byte
    // policy and returned intact under a 64-byte policy.
    const PAYLOAD: &str = "0123456789abcdefghijklmnopqrstuvwxyz01234567";

    let over = format!(r#"let handle = io::popen("printf {PAYLOAD}", "r"); io::read_all(handle);"#);
    let mut over_policy = policy_with_limit(16);
    over_policy.allow_process = true;
    let mut vm = new_vm(&over, Some(over_policy));
    let message = drive_error(&mut vm);
    assert!(
        message.contains("io_read_all exceeded read limit"),
        "unexpected error: {message}"
    );
    reset(&mut vm);
    assert!(
        vm.execution_scope().resources().is_empty() && vm.execution_scope().operations().is_empty(),
        "the denied popen read must close and leave no resource or operation"
    );

    let within = format!(
        r#"let handle = io::popen("printf {PAYLOAD}", "r");
let text = io::read_all(handle);
io::close(handle);
text;"#
    );
    let mut within_policy = policy_with_limit(64);
    within_policy.allow_process = true;
    let mut vm = new_vm(&within, Some(within_policy));
    let stack = drive(&mut vm).expect("output within the limit must read");
    assert_eq!(stack.last(), Some(&Value::string(PAYLOAD)));
    reset(&mut vm);
    assert!(
        vm.execution_scope().resources().is_empty() && vm.execution_scope().operations().is_empty(),
        "closing the popen handle must leave no resource or operation"
    );
}

#[test]
fn io_read_all_without_policy_reads_to_eof() {
    let path = fixture_path("unlimited");
    let payload = "u".repeat(1024 * 1024 + 7);
    std::fs::write(&path, payload.as_bytes()).expect("fixture should be written");

    let mut vm = new_vm(&read_all_program(&path), None);
    let stack = drive(&mut vm).expect("without a policy read_all keeps reading to EOF");
    assert_eq!(stack.last(), Some(&Value::string(payload)));
    reset(&mut vm);
    let _ = std::fs::remove_file(path);
}

#[test]
fn io_read_all_on_one_handle_returns_empty_after_eof() {
    let path = fixture_path("reuse");
    std::fs::write(&path, b"0123456789").expect("fixture should be written");
    let source = format!(
        r#"let handle = io::open("{}", "r");
let first = io::read_all(handle);
let second = io::read_all(handle);
io::close(handle);
first + "|" + second;"#,
        path.display()
    );

    let mut vm = new_vm(&source, Some(policy_with_limit(16)));
    let stack = drive(&mut vm).expect("repeated reads within the limit must succeed");
    assert_eq!(stack.last(), Some(&Value::string("0123456789|")));
    reset(&mut vm);
    assert!(
        vm.execution_scope().resources().is_empty() && vm.execution_scope().operations().is_empty(),
        "closing the handle must leave no resource or operation behind"
    );
    let _ = std::fs::remove_file(path);
}
