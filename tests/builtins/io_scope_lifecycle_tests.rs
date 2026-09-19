//! Lifecycle coverage for the inline non-async IO backend.
//!
//! Calls execute synchronously, while file/process handles remain typed
//! execution-scope resources retired by explicit close, reset, or VM drop.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use vm::resource::close::{CloseProgress, HostResource};
use vm::resource::{ResourceCloseReason, ResourceResult};
use vm::{Value, Vm, VmError, VmStatus, compile_source};

use super::vm_reset::reset_for_reuse_to_ready;

fn run_source(source: &str) -> Result<Vec<Value>, VmError> {
    let compiled = compile_source(&format!("use io;\n{source}")).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    match vm.run()? {
        VmStatus::Halted => Ok(vm.stack().to_vec()),
        status => panic!("inline IO must not suspend the VM, got {status:?}"),
    }
}

fn run_source_host_error(source: &str) -> String {
    match run_source(source) {
        Ok(stack) => panic!("expected host error, got stack: {stack:?}"),
        Err(VmError::HostError(message)) => message,
        Err(other) => panic!("expected host error, got: {other:?}"),
    }
}

struct ForeignResource {
    closes: Arc<AtomicUsize>,
}

impl HostResource for ForeignResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

fn vm_for(source: &str) -> Vm {
    let compiled = compile_source(&format!("use io;\n{source}")).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    assert!(matches!(
        vm.run().expect("inline IO program should run"),
        VmStatus::Halted
    ));
    vm
}

#[test]
fn inline_io_calls_complete_without_pending_operations() {
    let mut vm = vm_for("let h = io::open(\"Cargo.toml\", \"r\"); io::close(h);");
    assert!(vm.execution_scope().operations().is_empty());
    assert!(vm.execution_scope().resources().is_empty());
}

#[test]
fn io_close_returns_true_and_closed_handle_is_stale() {
    let error = run_source_host_error(
        r#"
        let handle = io::open("Cargo.toml", "r");
        io::close(handle);
        io::close(handle);
        "#,
    );
    assert!(
        error.contains("stale")
            || error.contains("not found")
            || error.contains("closed")
            || error.contains("invalid"),
        "double close must be rejected: {error}"
    );
}

#[test]
fn io_close_then_read_rejects_stale_handle() {
    let error = run_source_host_error(
        r#"
        let handle = io::open("Cargo.toml", "r");
        io::close(handle);
        io::read_all(handle);
        "#,
    );
    assert!(
        error.contains("stale")
            || error.contains("not found")
            || error.contains("closed")
            || error.contains("invalid"),
        "reading a closed handle must be rejected: {error}"
    );
}

#[test]
fn io_close_on_non_positive_handle_is_rejected() {
    let error = run_source_host_error("io::close(0);");
    assert!(error.contains("invalid io handle"), "{error}");
}

#[test]
fn io_open_read_mode_reports_missing_file() {
    let error = run_source_host_error("io::open(\"__pd_vm_missing_file_for_test__.txt\", \"r\");");
    assert!(error.contains("io_open failed"), "{error}");
}

#[test]
fn io_open_rejects_unsupported_mode() {
    let error = run_source_host_error("io::open(\"Cargo.toml\", \"bad\");");
    assert!(error.contains("unsupported io_open mode"), "{error}");
}

#[test]
fn io_rejects_foreign_scope_handles() {
    let foreign_handle = {
        let vm = vm_for("let h = io::open(\"Cargo.toml\", \"r\"); h;");
        let Value::Int(raw) = vm.stack().last().cloned().expect("handle on stack") else {
            panic!("io::open must return an integer handle");
        };
        raw
    };
    let error = run_source_host_error(&format!("io::close({foreign_handle});"));
    assert!(
        error.contains("mismatch")
            || error.contains("type")
            || error.contains("stale")
            || error.contains("invalid")
            || error.contains("table"),
        "foreign-scope handle must be rejected: {error}"
    );
}

#[test]
fn generic_resource_types_remain_distinct() {
    let closes = Arc::new(AtomicUsize::new(0));
    let mut vm = vm_for("io::open(\"Cargo.toml\", \"r\");");
    let foreign = vm
        .execution_scope()
        .push_resource(ForeignResource {
            closes: Arc::clone(&closes),
        })
        .expect("foreign resource must insert");
    vm.execution_scope()
        .close_resource::<ForeignResource>(foreign.handle(), ResourceCloseReason::Requested)
        .expect("typed close must succeed");
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}

#[test]
fn reset_for_reuse_retires_io_resources_through_scope() {
    let mut vm = vm_for("let h = io::open(\"Cargo.toml\", \"r\"); h;");
    assert!(!vm.execution_scope().resources().is_empty());
    reset_for_reuse_to_ready(&mut vm).expect("reset should reach quiescence");
    assert!(vm.execution_scope().resources().is_empty());
    assert!(vm.execution_scope().operations().is_empty());
}

#[test]
fn drop_retires_io_resources_through_scope() {
    let mut vm = vm_for("io::open(\"Cargo.toml\", \"r\");");
    assert!(!vm.execution_scope().resources().is_empty());
    drop(vm);
}

#[cfg(unix)]
struct ProcessTreeCleanup {
    leader: i32,
    descendant: i32,
    marker: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for ProcessTreeCleanup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.leader, libc::SIGKILL);
            libc::kill(self.descendant, libc::SIGKILL);
        }
        let _ = std::fs::remove_file(&self.marker);
    }
}

#[cfg(unix)]
fn process_is_running(pid: i32) -> bool {
    let path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some((_, state)) = stat.split_once(") ") else {
        return true;
    };
    !state.starts_with('Z')
}

#[cfg(unix)]
fn wait_for_process_exit(pid: i32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while process_is_running(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "popen descendant remained alive after reset"
        );
        std::thread::yield_now();
    }
}

#[cfg(unix)]
fn read_process_marker(path: &std::path::Path) -> (i32, i32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let values = contents
                .split_whitespace()
                .map(str::parse::<i32>)
                .collect::<Result<Vec<_>, _>>()
                .expect("marker should contain process ids");
            if values.len() == 2 {
                return (values[0], values[1]);
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "popen child did not publish its process marker"
        );
        std::thread::yield_now();
    }
}

#[cfg(unix)]
#[test]
fn reset_for_reuse_terminates_live_popen_process_tree() {
    let marker = std::env::temp_dir().join(format!(
        "pd-vm-blocking-io-reset-{}-{}.marker",
        std::process::id(),
        SystemTimeNonce::new()
    ));
    let command = format!(
        "parent=$$; sleep 30 & child=$!; printf '%s %s' $parent $child > {}; wait $child",
        marker.display()
    );
    let mut vm = vm_for(&format!("let h = io::popen(\"{command}\", \"r\"); h;"));
    let (leader, descendant) = read_process_marker(&marker);
    let _cleanup = ProcessTreeCleanup {
        leader,
        descendant,
        marker: marker.clone(),
    };

    reset_for_reuse_to_ready(&mut vm).expect("reset should reach quiescence");
    assert!(vm.execution_scope().resources().is_empty());
    wait_for_process_exit(descendant);
    let _ = std::fs::remove_file(marker);
}

#[cfg(unix)]
struct SystemTimeNonce(u128);

#[cfg(unix)]
impl SystemTimeNonce {
    fn new() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should follow Unix epoch")
                .as_nanos(),
        )
    }
}

#[cfg(unix)]
impl std::fmt::Display for SystemTimeNonce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
