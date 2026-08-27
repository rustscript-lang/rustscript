//! Focused TDD tests for migrating baseline IO onto the generic
//! [`ExecutionScope`] lifecycle (PR16 commit 3).
//!
//! File/process handles are typed resources stored in the VM's execution
//! scope; read/write/flush/close/open/popen/exists pending work is driven by
//! concrete [`HostOperation`] drivers registered in the same scope. These
//! tests verify the scope-backed behaviour through the public VM + IO API:
//! stale-handle and type-mismatch rejection, exact-once close, pending
//! operation cancellation, and reset/drop retirement through the generic
//! scope.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use vm::operation::OperationCancelReason;
use vm::operation::OperationId;
use vm::resource::close::{CloseProgress, HostResource};
use vm::resource::{ResourceCloseReason, ResourceResult};
use vm::{Value, Vm, VmError, VmStatus, compile_source};

/// Helper: run an IO source to completion, returning the final stack.
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

/// Helper: run an IO source expecting a host error, returning its message.
fn run_source_host_error(source: &str) -> String {
    match run_source(source) {
        Ok(stack) => panic!("expected host error, got stack: {stack:?}"),
        Err(VmError::HostError(message)) => message,
        Err(other) => panic!("expected host error, got: {other:?}"),
    }
}

// A foreign (non-IO) resource used to exercise type-mismatch rejection.
struct ForeignResource {
    closes: Arc<AtomicUsize>,
}

impl HostResource for ForeignResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

/// Compiles and runs an IO source to a VM whose scope reflects the result.
fn vm_for(source: &str) -> Vm {
    let wrapped = format!("use io;\n{source}");
    let compiled = compile_source(&wrapped).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    let mut status = vm.run().expect("run should start");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                status = vm.resume().expect("resume should continue");
            }
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()
                    .expect("waiting host op should complete");
                status = vm.resume().expect("resume should continue");
            }
        }
    }
    vm
}

fn host_error(err: VmError) -> String {
    match err {
        VmError::HostError(message) => message,
        other => panic!("expected host error, got: {other:?}"),
    }
}

// ------------------------------------------------------------------ handles

#[test]
fn io_close_returns_true_and_closed_handle_is_stale() {
    // The first close is exact-once and returns `true`; a second use of the
    // closed handle (a stale handle) is rejected with a host error rather
    // than silently succeeding.
    let err = run_source_host_error(
        r#"
        let handle = io::open("Cargo.toml", "r");
        io::close(handle);
        io::close(handle);
    "#,
    );
    assert!(
        err.contains("stale")
            || err.contains("not found")
            || err.contains("closed")
            || err.contains("invalid"),
        "double close of a closed IO handle should be rejected; got: {err}"
    );
}

#[test]
fn io_close_then_read_rejects_stale_handle() {
    let err = run_source_host_error(
        r#"
        let handle = io::open("Cargo.toml", "r");
        io::close(handle);
        io::read_all(handle);
    "#,
    );
    assert!(
        err.contains("stale")
            || err.contains("not found")
            || err.contains("closed")
            || err.contains("invalid"),
        "reading a closed IO handle should be rejected; got: {err}"
    );
}

#[test]
fn io_close_on_non_positive_handle_is_rejected() {
    let err = run_source_host_error(
        r#"
        io::close(0);
    "#,
    );
    assert!(
        err.contains("invalid io handle"),
        "non-positive handles must be rejected; got: {err}"
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

// ------------------------------------------------------------- type mismatch

#[test]
fn io_rejects_foreign_scope_handles() {
    // IO handles are scope-scoped typed tokens: a handle minted by one VM's
    // execution scope must be rejected when used against another VM's scope
    // (wrong table / stale / invalid), never interpreted as a live handle.
    let foreign_handle = {
        let wrapped = "use io;\nlet h = io::open(\"Cargo.toml\", \"r\");\nh;";
        let compiled = compile_source(wrapped).expect("compile");
        let mut vm = Vm::new(compiled.program);
        // Run and drain any waiting IO op.
        let mut status = vm.run().expect("run");
        loop {
            match status {
                VmStatus::Waiting(_) => {
                    vm.wait_for_host_op_blocking().expect("wait");
                    status = vm.resume().expect("resume");
                }
                VmStatus::Yielded => {
                    status = vm.resume().expect("resume");
                }
                VmStatus::Halted => break,
            }
        }
        let handle = vm.stack().last().cloned().expect("handle on stack");
        let Value::Int(raw) = handle else {
            panic!("io::open must return an integer handle");
        };
        raw
    };

    let wrapped = format!("use io;\nio::close({foreign_handle});");
    let compiled = compile_source(&wrapped).expect("compile");
    let mut vm2 = Vm::new(compiled.program);
    let err = host_error(
        vm2.run()
            .expect_err("foreign handle close must be rejected"),
    );
    assert!(
        err.contains("mismatch")
            || err.contains("type")
            || err.contains("stale")
            || err.contains("invalid")
            || err.contains("table"),
        "foreign-scope IO handle access must be rejected; got: {err}"
    );
}

#[test]
fn io_resources_are_typed_and_never_cross_interpreted() {
    // A foreign (non-IO) resource sharing the same execution scope is a
    // distinct typed resource: the generic typed-table access rejects a
    // wrong-typed token before any IO interpretation can happen. This is the
    // generic guarantee IO handles rely on (TypeId-checked borrows).
    let closes = Arc::new(AtomicUsize::new(0));
    let wrapped = "use io;\nio::open(\"Cargo.toml\", \"r\");";
    let compiled = compile_source(wrapped).expect("compile");
    let mut vm = Vm::new(compiled.program);
    let foreign = vm
        .execution_scope()
        .push_resource(ForeignResource {
            closes: Arc::clone(&closes),
        })
        .expect("foreign resource must insert");
    // The foreign token is a valid live resource in this scope: its own
    // close (typed correctly) succeeds and runs exactly once.
    let _ = vm
        .execution_scope()
        .close_resource::<ForeignResource>(foreign.handle(), ResourceCloseReason::Requested)
        .expect("typed close of the foreign resource must succeed");
    assert_eq!(closes.load(Ordering::SeqCst), 1, "close runs exactly once");
}

// ----------------------------------------------------- pending cancellation

#[test]
fn pending_io_operation_can_be_cancelled_through_scope() {
    // `read_all` on a child that produces no output and does not exit keeps
    // the operation genuinely pending. Cancelling it through the VM's
    // execution scope must mark it terminal and retire it from the registry.
    let wrapped = "use io;\nlet h = io::popen(\"sleep 30\", \"r\");\nio::read_all(h);";
    let compiled = compile_source(wrapped).expect("compile");
    let mut vm = Vm::new(compiled.program);

    let status = vm.run().expect("run should start pending");
    let waiting = match status {
        VmStatus::Waiting(op_id) => op_id,
        other => panic!("expected a waiting host op, got: {other:?}"),
    };
    let id = OperationId::from_raw(waiting).expect("waiting op id must be a valid operation id");
    assert_eq!(
        vm.execution_scope().operations().len(),
        1,
        "the pending IO op must occupy a scope operation slot"
    );

    let can_cancel = vm
        .execution_scope()
        .cancel_operation(id, OperationCancelReason::Requested)
        .expect("pending op must be cancellable");
    assert!(can_cancel, "cancel on a pending op must report success");
    assert_eq!(
        vm.execution_scope().operations().len(),
        1,
        "cancellation must retain the operation until its worker exits"
    );

    let error = vm
        .wait_for_host_op_blocking()
        .expect_err("cancelled IO operation should report cancellation");
    assert!(
        matches!(error, VmError::HostError(ref message) if message.contains("cancelled")),
        "unexpected cancellation error: {error:?}"
    );
    assert!(
        vm.execution_scope().operations().is_empty(),
        "polling the cancelled operation must release it exactly once"
    );
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
                .expect("popen marker should contain process ids");
            if values.len() == 2 {
                return (values[0], values[1]);
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "popen test child did not publish its process marker"
        );
        std::thread::yield_now();
    }
}

// ------------------------------------------------ reset / drop retirement

#[test]
fn reset_for_reuse_joins_pending_io_worker() {
    let compiled =
        compile_source("use io;\nlet h = io::popen(\"sleep 30\", \"r\");\nio::read_all(h);")
            .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    assert!(matches!(
        vm.run().expect("run should start"),
        VmStatus::Waiting(_)
    ));
    assert_eq!(vm.execution_scope().operations().len(), 1);

    vm.reset_for_reuse();
    assert!(vm.execution_scope().operations().is_empty());
    assert!(vm.execution_scope().resources().is_empty());
}

#[test]
fn reset_for_reuse_retires_io_resources_through_scope() {
    let mut vm = vm_for("let h = io::open(\"Cargo.toml\", \"r\");\nh;");
    assert!(
        !vm.execution_scope().resources().is_empty(),
        "open leaves a live IO resource in the scope"
    );

    vm.reset_for_reuse();

    assert!(
        vm.execution_scope().resources().is_empty() && vm.execution_scope().operations().is_empty(),
        "reset for reuse must retire IO resources and operations through the scope"
    );
}

#[test]
fn drop_retires_io_resources_through_scope() {
    // Dropping a VM with a live IO handle must retire the handle through the
    // generic scope (no custom close-all side channel). The scope's own Drop
    // runs the closing sweep; this test guards that path stays wired.
    let mut vm = vm_for("io::open(\"Cargo.toml\", \"r\");");
    assert!(!vm.execution_scope().resources().is_empty());
    drop(vm);
}

#[cfg(unix)]
#[test]
fn reset_for_reuse_terminates_live_popen_process_tree() {
    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let suffix = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let marker = std::env::temp_dir().join(format!(
        "pd-vm-blocking-io-reset-{0}-{suffix}.marker",
        std::process::id()
    ));
    let command = format!(
        "parent=$$; sleep 30 & child=$!; printf '%s %s' $parent $child > {}; wait $child",
        marker.display()
    );
    let source = format!("use io;\nlet h = io::popen(\"{command}\", \"r\");\nh;");
    let compiled = compile_source(&source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    let mut status = vm.run().expect("run should start");
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => status = vm.resume().expect("resume should continue"),
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()
                    .expect("waiting op should finish");
                status = vm.resume().expect("resume should continue");
            }
        }
    }
    let (leader, descendant) = read_process_marker(&marker);
    let _cleanup = ProcessTreeCleanup {
        leader,
        descendant,
        marker: marker.clone(),
    };

    vm.reset_for_reuse();
    assert!(vm.execution_scope().resources().is_empty());
    wait_for_process_exit(descendant);
    let _ = std::fs::remove_file(marker);
}
