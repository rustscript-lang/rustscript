use std::time::{SystemTime, UNIX_EPOCH};

use vm::{Value, Vm, VmError, VmStatus, compile_source};

use super::vm_reset::reset_for_reuse_to_ready;

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

#[cfg(unix)]
struct ProcessGroupGuard {
    parent: Option<u32>,
    descendant: Option<u32>,
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(parent) = self.parent
            && let Ok(parent) = libc::pid_t::try_from(parent)
        {
            unsafe {
                libc::kill(-parent, libc::SIGKILL);
            }
        }
        if let Some(descendant) = self.descendant
            && let Ok(descendant) = libc::pid_t::try_from(descendant)
        {
            unsafe {
                libc::kill(descendant, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
fn wait_for_file(path: &std::path::Path) -> String {
    for _ in 0..200 {
        if let Ok(contents) = std::fs::read_to_string(path)
            && !contents.trim().is_empty()
        {
            return contents;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

#[cfg(unix)]
fn pid_is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let stat_path = std::path::PathBuf::from(format!("/proc/{pid}/stat"));
    if std::fs::read_to_string(stat_path).is_err() {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn wait_for_pid_exit(pid: u32) -> bool {
    for _ in 0..200 {
        if !pid_is_running(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[cfg(unix)]
fn process_tree_command(
    parent_path: &std::path::Path,
    descendant_path: &std::path::Path,
    marker_path: &std::path::Path,
) -> String {
    format!(
        "echo $$ > '{}'; sh -c 'while :; do sleep 30; done' '{}-worker' & child=$!; echo $child > '{}'; (sleep 1; echo survived > '{}') & wait",
        parent_path.display(),
        marker_path.display(),
        descendant_path.display(),
        marker_path.display()
    )
}

#[cfg(unix)]
fn guest_popen_program(command: &str, expression: &str) -> String {
    format!(r#"let h = io::popen("{command}", "r"); {expression}"#)
}

#[cfg(unix)]
#[test]
fn async_io_reset_kills_and_reaps_the_entire_popen_process_group() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "pd-vm-async-reset-tree-{}-{nonce}",
        std::process::id()
    ));
    let parent_path = base.with_extension("parent");
    let descendant_path = base.with_extension("descendant");
    let marker_path = base.with_extension("marker");
    let command = process_tree_command(&parent_path, &descendant_path, &marker_path);
    let source = guest_popen_program(&command, "io::read_all(h);");

    let compiled = compile_source(&format!("use io;\n{source}")).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    super::async_test_bridge::install(&mut vm);
    assert!(matches!(
        vm.run().expect("run should start"),
        VmStatus::Waiting(_)
    ));
    vm.wait_for_host_op_blocking()
        .expect("popen should complete before read starts");
    assert!(matches!(
        vm.resume().expect("read should start"),
        VmStatus::Waiting(_)
    ));

    let parent_pid = wait_for_file(&parent_path)
        .trim()
        .parse::<u32>()
        .expect("parent pid");
    let descendant_pid = wait_for_file(&descendant_path)
        .trim()
        .parse::<u32>()
        .expect("descendant pid");
    let _guard = ProcessGroupGuard {
        parent: Some(parent_pid),
        descendant: Some(descendant_pid),
    };

    tokio::runtime::Runtime::new()
        .expect("reset runtime should build")
        .block_on(async {
            reset_for_reuse_to_ready(&mut vm).expect("reset should reach quiescence");
        });
    assert!(vm.execution_scope().resources().is_empty());
    assert!(vm.execution_scope().operations().is_empty());
    assert!(
        !marker_path.exists(),
        "a killed process group must not run descendants"
    );
    assert!(
        wait_for_pid_exit(parent_pid),
        "the popen parent must be gone"
    );
    assert!(
        wait_for_pid_exit(descendant_pid),
        "the popen descendant must be gone"
    );

    let _ = std::fs::remove_file(parent_path);
    let _ = std::fs::remove_file(descendant_path);
    let _ = std::fs::remove_file(marker_path);
}

#[cfg(unix)]
#[test]
fn async_io_failed_resource_handoff_cleans_up_the_popen_process_group() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow Unix epoch")
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "pd-vm-async-handoff-tree-{}-{nonce}",
        std::process::id()
    ));
    let parent_path = base.with_extension("parent");
    let descendant_path = base.with_extension("descendant");
    let marker_path = base.with_extension("marker");
    let command = process_tree_command(&parent_path, &descendant_path, &marker_path);
    let source = guest_popen_program(&command, "h;");

    let compiled = compile_source(&format!("use io;\n{source}")).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    super::async_test_bridge::install(&mut vm);
    assert!(matches!(
        vm.run().expect("run should start"),
        VmStatus::Waiting(_)
    ));
    vm.execution_scope()
        .begin_close(vm::ResourceCloseReason::VmReset)
        .expect("scope close should start");
    let error = vm
        .wait_for_host_op_blocking()
        .expect_err("resource insertion into a closing scope must fail");
    assert!(
        error.to_string().contains("resource insert")
            || error.to_string().contains("scope")
            || error.to_string().contains("closing"),
        "unexpected failed-handoff error: {error}"
    );

    std::thread::sleep(std::time::Duration::from_millis(1_200));
    assert!(
        !marker_path.exists(),
        "failed handoff must not leave a live descendant"
    );

    let _ = std::fs::remove_file(parent_path);
    let _ = std::fs::remove_file(descendant_path);
    let _ = std::fs::remove_file(marker_path);
}
