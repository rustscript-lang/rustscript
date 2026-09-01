use std::collections::BTreeMap;
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use vm::{
    BoundedExecError, BoundedProcess, BoundedProcessError, BoundedProcessRequest,
    CancellationToken, IoHostExt, IoPolicy, ProcessStatus, Value, Vm, VmStatus, compile_source,
    exec_bounded, standard_composition,
};
#[cfg(unix)]
use vm::{ConfinedDirectory, ConfinedFsRoot};

#[cfg(unix)]
fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[cfg(unix)]
fn request(parts: &[&str]) -> BoundedProcessRequest {
    BoundedProcessRequest::new(argv(parts)).with_workspace_root(std::env::temp_dir())
}

#[test]
fn request_validation_rejects_unbounded_or_malformed_inputs() {
    let mut request = BoundedProcessRequest::new(Vec::new());
    assert!(request.validate().is_err());

    request.argv = vec!["/bin/echo\0unsafe".to_owned()];
    assert!(request.validate().is_err());

    request.argv = vec!["/bin/echo".to_owned()];
    request.timeout = Some(Duration::ZERO);
    assert!(request.validate().is_err());

    request.argv = (0..=vm::MAX_ARG_COUNT)
        .map(|index| format!("arg-{index}"))
        .collect();
    assert_eq!(
        request.validate(),
        Err(vm::ProcessValidationError::ArgCountExceeded)
    );
    request.argv = vec!["/bin/echo".to_owned()];
    request.timeout = Some(Duration::from_secs(1));
    request.stdout_limit = 0;
    assert!(request.validate().is_err());

    request.stdout_limit = 32;
    request.env = BTreeMap::from([("BAD=KEY".to_owned(), "value".to_owned())]);
    assert!(request.validate().is_err());

    let debug_request = BoundedProcessRequest::new(vec!["/bin/echo secret-argv".to_owned()])
        .with_env("SECRET_KEY", "secret-env-value")
        .with_stdin(b"secret-stdin".to_vec());
    let debug = format!("{debug_request:?}");
    assert!(!debug.contains("secret-argv"));
    assert!(!debug.contains("SECRET_KEY"));
    assert!(!debug.contains("secret-env-value"));
    assert!(!debug.contains("secret-stdin"));
}

#[test]
fn request_validation_rejects_ambient_environment_and_unsafe_cwds() {
    let root = std::env::temp_dir();
    let no_root = BoundedProcessRequest::new(vec!["program".to_owned()]);
    assert_eq!(
        no_root.validate(),
        Err(vm::ProcessValidationError::CwdRequired)
    );

    let relative = BoundedProcessRequest::new(vec!["program".to_owned()])
        .with_cwd("relative")
        .with_workspace_root(root.clone());
    assert_eq!(
        relative.validate(),
        Err(vm::ProcessValidationError::CwdNotAbsolute)
    );

    let inherited = BoundedProcessRequest::new(vec!["program".to_owned()])
        .with_workspace_root(root.clone())
        .with_inherit_env(true);
    assert_eq!(
        inherited.validate(),
        Err(vm::ProcessValidationError::InheritEnvForbidden)
    );

    let safe =
        BoundedProcessRequest::new(vec!["program".to_owned()]).with_workspace_root(root.clone());
    assert!(safe.validate().is_ok());

    let explicit = BoundedProcessRequest::new(vec!["program".to_owned()]).with_cwd(root);
    assert!(explicit.validate().is_ok());
}

#[cfg(unix)]
#[test]
fn argv_execution_treats_metacharacters_as_literal_arguments() {
    let marker =
        std::env::temp_dir().join(format!("rustscript-bounded-marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let result = exec_bounded(request(&[
        "/usr/bin/printf",
        "%s",
        &format!("literal; touch {}", marker.display()),
    ]))
    .expect("printf should execute");
    assert!(result.status.is_success());
    assert_eq!(
        result.stdout,
        format!("literal; touch {}", marker.display()).as_bytes()
    );
    assert!(!marker.exists());
    let _ = std::fs::remove_file(marker);
    assert!(!result.stdout_truncated);
}

#[cfg(unix)]
#[test]
fn both_output_streams_are_drained_with_exact_bounded_snapshots() {
    let request = request(&[
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 20000 ]; do printf o; printf e >&2; i=$((i+1)); done",
    ])
    .with_timeout(Duration::from_secs(5))
    .with_output_limits(128, 96, 160);
    let result = exec_bounded(request).expect("large output should complete");

    assert!(result.status.is_success());
    assert!(result.stdout.len() <= 128);
    assert!(result.stderr.len() <= 96);
    assert!(result.stdout.len() + result.stderr.len() <= 160);
    assert!(result.stdout_truncated);
    assert!(result.stderr_truncated);
    assert_eq!(
        result.stdout_offset + result.stdout.len() as u64,
        result.stdout_next_offset
    );
    assert_eq!(
        result.stderr_offset + result.stderr.len() as u64,
        result.stderr_next_offset
    );
}

#[cfg(unix)]
#[test]
fn background_process_supports_stdin_write_close_and_reap() {
    let request = request(&["/bin/cat"]).with_timeout(Duration::from_secs(5));
    let process = BoundedProcess::spawn(request).expect("cat should spawn");
    let handle = process.handle();
    assert_eq!(handle, process.handle());
    assert_eq!(process.write_stdin(b"hello\n").expect("stdin write"), 6);
    process.close_stdin().expect("stdin close");
    let status = process
        .wait_until(Instant::now() + Duration::from_secs(5))
        .expect("cat should exit");
    assert!(status.is_success());
    assert_eq!(process.reap().expect("reap is idempotent"), status);
    assert_eq!(process.stdout_snapshot().bytes, b"hello\n");
    process.close_stdin().expect("stdin close is idempotent");
}

#[cfg(unix)]
#[test]
fn background_poll_enforces_the_process_deadline_and_cleans_up() {
    let process = BoundedProcess::spawn(
        request(&["/bin/sh", "-c", "sleep 60"]).with_timeout(Duration::from_millis(40)),
    )
    .expect("process should spawn");
    std::thread::sleep(Duration::from_millis(80));
    let error = process
        .poll()
        .expect_err("poll after the deadline must report timeout");
    assert!(matches!(error, vm::BoundedProcessError::DeadlineElapsed));
    assert!(process.terminal_status().is_some());
}

#[cfg(unix)]
#[test]
fn direct_reap_honors_the_process_deadline() {
    let process = BoundedProcess::spawn(
        request(&["/bin/sh", "-c", "sleep 1"]).with_timeout(Duration::from_millis(40)),
    )
    .expect("process should spawn");
    let status = process.reap().expect("reap should force bounded cleanup");
    assert!(matches!(status, ProcessStatus::Signaled { .. }));
}

#[cfg(unix)]
#[test]
fn closing_initial_stdin_interrupts_a_nonreading_writer() {
    let process = BoundedProcess::spawn(
        request(&["/bin/sh", "-c", "sleep 1"])
            .with_stdin(vec![b'x'; 1024 * 1024])
            .with_timeout(Duration::from_millis(200)),
    )
    .expect("bounded process should spawn");
    let started = Instant::now();
    assert!(process.close_stdin().is_ok());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(
        process
            .wait(None)
            .expect_err("child should hit its deadline"),
        BoundedProcessError::DeadlineElapsed
    );
}

#[cfg(unix)]
#[test]
fn concurrent_stdin_write_and_close_complete_within_a_hard_bound() {
    let process = Arc::new(
        BoundedProcess::spawn(
            request(&["/bin/sh", "-c", "sleep 60"]).with_timeout(Duration::from_secs(2)),
        )
        .expect("process should spawn"),
    );
    let barrier = Arc::new(Barrier::new(3));
    let (write_tx, write_rx) = mpsc::channel();
    let (close_tx, close_rx) = mpsc::channel();
    let writer_process = Arc::clone(&process);
    let writer_barrier = Arc::clone(&barrier);
    let writer = std::thread::spawn(move || {
        writer_barrier.wait();
        let result = writer_process.write_stdin(&vec![b'x'; 2 * 1024 * 1024]);
        write_tx.send(result).expect("write result receiver exists");
    });
    let closer_process = Arc::clone(&process);
    let closer_barrier = Arc::clone(&barrier);
    let closer = std::thread::spawn(move || {
        closer_barrier.wait();
        let result = closer_process.close_stdin();
        close_tx.send(result).expect("close result receiver exists");
    });
    barrier.wait();

    assert!(
        write_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "concurrent stdin write must be bounded"
    );
    assert!(
        close_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "concurrent stdin close must be bounded"
    );
    writer.join().expect("writer should finish");
    closer.join().expect("closer should finish");
    process.shutdown().expect("shutdown should reap the child");
}

#[cfg(unix)]
#[test]
fn foreground_exec_writes_initial_stdin_before_closing_it() {
    let result = exec_bounded(
        request(&["/bin/cat"])
            .with_stdin(b"initial stdin".to_vec())
            .with_timeout(Duration::from_secs(2)),
    )
    .expect("cat should receive initial stdin");
    assert_eq!(result.stdout, b"initial stdin");
}

#[cfg(unix)]
#[test]
fn initial_stdin_write_failure_is_reported_after_child_closes_stdin() {
    let error = exec_bounded(
        request(&["/bin/sh", "-c", "exec 0<&-; sleep 1"])
            .with_stdin(vec![b'x'; 1024 * 1024])
            .with_timeout(Duration::from_secs(2)),
    )
    .expect_err("closed child stdin should report the write failure");
    assert!(matches!(
        error,
        BoundedExecError::Failed(vm::BoundedProcessError::StdinWriteFailed { .. })
    ));
}

#[cfg(unix)]
#[test]
fn env_is_cleared_by_default_and_explicit_entries_are_allowlisted() {
    let request = request(&["/usr/bin/env"])
        .with_env("RUSTSCRIPT_BOUNDED_TEST", "literal-value")
        .with_timeout(Duration::from_secs(2));
    let result = exec_bounded(request).expect("env should execute");
    assert_eq!(result.stdout, b"RUSTSCRIPT_BOUNDED_TEST=literal-value\n");
}

#[cfg(unix)]
#[test]
fn dropping_a_background_process_reaps_and_terminates_it() {
    let process = BoundedProcess::spawn(
        request(&["/bin/sh", "-c", "sleep 60"]).with_timeout(Duration::from_secs(5)),
    )
    .expect("sleep should spawn");
    let pid = process.pid();
    drop(process);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("dropped process {pid} is still present");
}

#[cfg(unix)]
#[test]
fn foreground_timeout_kills_process_group_and_retains_terminal_status() {
    let request = request(&["/bin/sh", "-c", "sleep 60 & wait"])
        .with_timeout(Duration::from_millis(80))
        .with_output_limits(64, 64, 128);
    let error = exec_bounded(request).expect_err("sleeping process should time out");
    let output = match error {
        BoundedExecError::TimedOut(output) => output,
        other => panic!("expected timeout, got {other:?}"),
    };
    assert!(matches!(output.status, ProcessStatus::Signaled { .. }));
}

#[cfg(unix)]
#[test]
fn normal_root_exit_terminates_background_descendants() {
    let marker = std::env::temp_dir().join(format!(
        "rustscript-bounded-descendant-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let marker_text = marker.display().to_string();
    let result = exec_bounded(
        request(&[
            "/bin/sh",
            "-c",
            "sleep 60 & echo $! > \"$1\"; exit 0",
            "bounded-tree-test",
            &marker_text,
        ])
        .with_timeout(Duration::from_secs(2)),
    )
    .expect("root process should exit normally");
    assert!(result.status.is_success());
    let descendant_pid = std::fs::read_to_string(&marker)
        .expect("child pid marker should be written")
        .trim()
        .parse::<libc::pid_t>()
        .expect("child pid marker should contain a pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if unsafe { libc::kill(descendant_pid, 0) } != 0 {
            let _ = std::fs::remove_file(marker);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&marker);
    panic!("descendant {descendant_pid} is still present");
}

#[cfg(unix)]
#[test]
fn escaped_descendant_cannot_hold_foreground_cleanup_past_the_drain_deadline() {
    let started = Instant::now();
    let result = exec_bounded(
        request(&[
            "/bin/sh",
            "-c",
            "/usr/bin/setsid /bin/sh -c '/bin/sleep 60' & exit 0",
        ])
        .with_timeout(Duration::from_secs(2)),
    )
    .expect("root process should exit normally");
    assert!(result.status.is_success());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "escaped pipe holders must not defeat bounded cleanup"
    );
}

#[cfg(unix)]
#[test]
fn continuous_escaped_writer_cannot_prolong_close_reap_or_drop() {
    let marker = std::env::temp_dir().join(format!(
        "rustscript-bounded-writer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&marker);
    let marker_text = marker.display().to_string();
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let result = exec_bounded(
            request(&[
                "/bin/sh",
                "-c",
                concat!(
                    "/usr/bin/setsid /bin/sh -c 'echo $$ > \"$1\"; exec /bin/cat /dev/zero' ",
                    "bounded-writer-child \"$1\" & ",
                    "while [ ! -s \"$1\" ]; do :; done; ",
                    "/bin/sleep 0.1; exit 0"
                ),
                "bounded-writer-root",
                &marker_text,
            ])
            .with_timeout(Duration::from_secs(2)),
        );
        let _ = tx.send((result, started.elapsed()));
    });
    let (result, elapsed) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("continuously-writing escaped descendant must not prolong close/reap/drop");
    let _ = std::fs::remove_file(&marker);
    let output = result.expect("root process should exit normally");
    assert!(output.status.is_success());
    assert!(
        elapsed < Duration::from_millis(500),
        "escaped writer held cleanup for {elapsed:?}"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_kills_process_and_returns_typed_error() {
    let token = CancellationToken::new();
    let request = request(&["/bin/sh", "-c", "sleep 60"])
        .with_timeout(Duration::from_secs(5))
        .with_cancellation_token(token.clone());
    let process = BoundedProcess::spawn(request).expect("process should spawn");
    token.cancel();
    let error = process
        .wait_until(Instant::now() + Duration::from_secs(2))
        .expect_err("cancelled process should not report success");
    assert!(matches!(error, vm::BoundedProcessError::Cancelled));
    assert!(process.terminal_status().is_some());
}

#[cfg(unix)]
#[test]
fn explicit_cancel_remains_cancelled_after_the_child_reaches_terminal_state() {
    let process = BoundedProcess::spawn(
        request(&["/bin/sh", "-c", "sleep 60"]).with_timeout(Duration::from_secs(5)),
    )
    .expect("process should spawn");
    process.cancel();
    std::thread::sleep(Duration::from_millis(40));
    let error = process
        .wait_until(Instant::now() + Duration::from_secs(2))
        .expect_err("explicit cancellation must remain observable");
    assert!(matches!(error, vm::BoundedProcessError::Cancelled));
}

#[cfg(unix)]
#[test]
fn nonzero_and_signal_statuses_are_typed_without_losing_logs() {
    let exited = exec_bounded(
        request(&["/bin/sh", "-c", "printf out; printf err >&2; exit 7"])
            .with_timeout(Duration::from_secs(2)),
    )
    .expect("nonzero exit is a process result");
    assert_eq!(exited.status.exit_code(), Some(7));
    assert_eq!(exited.stdout, b"out");
    assert_eq!(exited.stderr, b"err");

    let signaled = exec_bounded(
        request(&["/bin/sh", "-c", "kill -TERM $$"]).with_timeout(Duration::from_secs(2)),
    )
    .expect("signal exit is a process result");
    assert_eq!(signaled.status.signal(), Some(15));
}

#[cfg(unix)]
#[test]
fn log_snapshots_report_monotonic_offsets_and_gaps() {
    let process = BoundedProcess::spawn(
        request(&["/usr/bin/printf", "0123456789"])
            .with_timeout(Duration::from_secs(2))
            .with_output_limits(4, 4, 8),
    )
    .expect("printf should spawn");
    process
        .wait_until(Instant::now() + Duration::from_secs(2))
        .expect("printf should exit");
    let first = process.stdout_snapshot();
    let second = process.stdout_snapshot_from(0);
    assert_eq!(first.bytes, b"6789");
    assert_eq!(first.offset, 6);
    assert_eq!(first.next_offset, 10);
    assert!(first.truncated);
    assert!(second.gap);
}

#[test]
fn spawn_failure_is_typed_without_echoing_argv() {
    let secret = "argv-secret-that-must-not-appear";
    let error = BoundedProcess::spawn(
        BoundedProcessRequest::new(vec![format!("/definitely/missing/{secret}")])
            .with_workspace_root(std::env::temp_dir()),
    )
    .expect_err("missing program should fail at spawn");
    assert!(matches!(
        error,
        vm::BoundedProcessError::Spawn(vm::SpawnError {
            kind: vm::SpawnErrorKind::NotFound,
            ..
        })
    ));
    assert!(!error.to_string().contains(secret));
}

#[test]
fn cancellation_before_spawn_is_typed_as_cancellation() {
    let token = CancellationToken::new();
    token.cancel();
    let error = exec_bounded(
        BoundedProcessRequest::new(vec!["/bin/echo".to_owned()])
            .with_workspace_root(std::env::temp_dir())
            .with_cancellation_token(token),
    )
    .expect_err("pre-cancelled execution should not spawn");
    assert!(matches!(error, BoundedExecError::Cancelled(_)));
}

#[cfg(unix)]
#[test]
fn rss_exec_runs_through_the_pending_host_registration() {
    let compiled = compile_source(
        r#"
        use io;
        let args: [string] = ["/usr/bin/printf", "%s", "rss;literal"];
        io::exec(args, 2000, 64);
        "#,
    )
    .expect("io::exec should compile");
    let mut vm = Vm::try_new(compiled.program).expect("VM construction");
    vm.set_standard_composition(standard_composition());
    vm.configure_io(IoPolicy {
        allow_process: true,
        ..IoPolicy::default()
    });

    let mut status = vm.run().expect("io::exec should start");
    loop {
        status = match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => vm.resume().expect("resume should succeed"),
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()
                    .expect("host op should finish");
                vm.resume().expect("resume should succeed")
            }
        };
    }
    let stack = vm.stack();
    let Value::Map(result) = &stack[0] else {
        panic!("io::exec should return a map: {stack:?}");
    };
    assert_eq!(
        result.get(&Value::string("status")),
        Some(&Value::string("exited"))
    );
    assert_eq!(
        result.get(&Value::string("success")),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        result.get(&Value::string("stdout")),
        Some(&Value::bytes(b"rss;literal".to_vec()))
    );
}

#[test]
fn io_exec_catalog_index_is_stable() {
    assert_eq!(vm::builtin_call_index("io::exec"), Some(0xFFFC));
}

#[cfg(unix)]
#[test]
fn poll_prefers_deadline_over_exited_child() {
    let process =
        BoundedProcess::spawn(request(&["/bin/true"]).with_timeout(Duration::from_millis(40)))
            .expect("true should spawn");
    std::thread::sleep(Duration::from_millis(80));
    let error = process
        .poll()
        .expect_err("deadline must win over a successful wait observation");
    assert!(matches!(error, BoundedProcessError::DeadlineElapsed));
}

#[cfg(unix)]
#[test]
fn rss_exec_preflights_empty_argv_before_spawning() {
    let compiled = compile_source(
        r#"
        use io;
        let args: [string] = [];
        io::exec(args, 2000, 64);
        "#,
    )
    .expect("empty argv should still compile");
    let mut vm = Vm::try_new(compiled.program).expect("VM construction");
    vm.set_standard_composition(standard_composition());
    vm.configure_io(IoPolicy {
        allow_process: true,
        ..IoPolicy::default()
    });
    let error = vm
        .run()
        .expect_err("empty argv must fail before a host worker is spawned");
    assert!(error.to_string().contains("argv"));
}

#[cfg(unix)]
#[test]
fn wait_forever_waits_until_exit_without_caller_deadline() {
    let process = BoundedProcess::spawn(
        request(&["/bin/sleep", "0.05"]).with_timeout(Duration::from_millis(40)),
    )
    .expect("sleep should spawn");
    let started = Instant::now();
    let status = process.wait_forever().expect("wait_forever");
    assert!(started.elapsed() >= Duration::from_millis(40));
    assert!(status.is_success() || matches!(status, ProcessStatus::Exited { .. }));
}

#[cfg(unix)]
#[test]
fn reap_terminates_immediately_instead_of_waiting_for_natural_exit() {
    let process =
        BoundedProcess::spawn(request(&["/bin/sleep", "2"]).with_timeout(Duration::from_secs(5)))
            .expect("sleep should spawn");
    let started = Instant::now();
    let status = process.reap().expect("reap");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(matches!(status, ProcessStatus::Signaled { .. }));
}

#[cfg(unix)]
#[test]
fn try_wait_includes_stdout_tail_once_terminal() {
    let process = BoundedProcess::spawn(
        request(&["/bin/printf", "0123456789"])
            .with_timeout(Duration::from_secs(2))
            .with_output_limits(4, vm::DEFAULT_OUTPUT_BYTES, vm::DEFAULT_OUTPUT_BYTES),
    )
    .expect("printf should spawn");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = process.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "try_wait never observed exit"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.is_success());
    let snapshot = process.stdout_snapshot();
    assert_eq!(snapshot.bytes, b"6789");
}

#[cfg(unix)]
fn confined_temp_dir(label: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after the epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "rustscript-confined-cwd-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&path).expect("temporary test directory should be created");
    path
}

#[cfg(unix)]
fn remove_any(path: &std::path::Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_dir() {
        std::fs::remove_dir_all(path).expect("temporary directory should be removed");
    } else {
        std::fs::remove_file(path).expect("temporary entry should be removed");
    }
}

#[cfg(unix)]
fn confined_exec(
    directory: ConfinedDirectory,
    argv: &[&str],
) -> Result<vm::BoundedExecOutput, BoundedExecError> {
    exec_bounded(
        BoundedProcessRequest::new(argv.iter().map(|part| (*part).to_owned()).collect())
            .with_confined_cwd(directory)
            .with_timeout(Duration::from_secs(5)),
    )
}

#[cfg(target_os = "linux")]
fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("process fd directory should be readable")
        .count()
}

#[cfg(unix)]
#[test]
fn confined_cwd_runs_in_a_nested_retained_directory() {
    let root_path = confined_temp_dir("nested");
    std::fs::create_dir_all(root_path.join("nested/leaf")).expect("nested leaf should be created");
    std::fs::write(root_path.join("root-marker"), b"root").expect("root marker should be written");
    std::fs::write(root_path.join("nested/leaf/marker"), b"nested")
        .expect("nested marker should be written");
    let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
    let nested = root
        .open_directory("nested/leaf")
        .expect("nested directory should open");
    let selected_root = root
        .open_directory("")
        .expect("empty path should select the retained root");

    let nested_result = confined_exec(nested, &["/bin/cat", "marker"]).expect("nested cwd spawn");
    assert!(nested_result.status.is_success());
    assert_eq!(nested_result.stdout, b"nested");

    let root_result =
        confined_exec(selected_root, &["/bin/cat", "root-marker"]).expect("root cwd spawn");
    assert!(root_result.status.is_success());
    assert_eq!(root_result.stdout, b"root");

    remove_any(&root_path);
}

#[cfg(unix)]
#[test]
fn confined_cwd_survives_parent_and_leaf_swaps() {
    let root_path = confined_temp_dir("swap");
    let outside_path = confined_temp_dir("swap-outside");
    std::fs::create_dir_all(root_path.join("parent/leaf")).expect("leaf should be created");
    std::fs::write(root_path.join("parent/leaf/marker"), b"inside")
        .expect("inside marker should be written");
    std::fs::write(outside_path.join("marker"), b"outside")
        .expect("outside marker should be written");
    let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
    let directory = root
        .open_directory("parent/leaf")
        .expect("leaf directory should open");

    std::fs::rename(root_path.join("parent/leaf"), root_path.join("leaf-moved"))
        .expect("leaf should be renamed away");
    std::os::unix::fs::symlink(&outside_path, root_path.join("parent/leaf"))
        .expect("leaf symlink should be installed");
    std::fs::rename(root_path.join("parent"), root_path.join("parent-moved"))
        .expect("parent should be renamed away");
    std::os::unix::fs::symlink(&outside_path, root_path.join("parent"))
        .expect("parent symlink should be installed");

    match confined_exec(directory, &["/bin/cat", "marker"]) {
        Ok(result) => {
            assert_ne!(
                result.stdout, b"outside",
                "retained cwd must not follow a swapped path"
            );
            assert_eq!(result.stdout, b"inside");
            assert!(result.status.is_success());
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                !text.contains("outside")
                    && !text.contains(outside_path.to_string_lossy().as_ref()),
                "fail-closed spawn must stay path-free: {text}"
            );
        }
    }

    remove_any(&root_path);
    remove_any(&outside_path);
}

#[cfg(unix)]
#[test]
fn confined_cwd_rejects_path_cwd_and_capability_together() {
    let root_path = confined_temp_dir("conflict");
    let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
    let directory = root
        .open_directory("")
        .expect("root directory capability should open");

    let with_cwd = BoundedProcessRequest::new(vec!["/bin/true".to_owned()])
        .with_cwd(root_path.clone())
        .with_confined_cwd(directory.clone());
    assert_eq!(
        with_cwd.validate(),
        Err(vm::ProcessValidationError::ConflictingCwd)
    );

    let with_workspace = BoundedProcessRequest::new(vec!["/bin/true".to_owned()])
        .with_workspace_root(root_path.clone())
        .with_confined_cwd(directory);
    assert_eq!(
        with_workspace.validate(),
        Err(vm::ProcessValidationError::ConflictingCwd)
    );

    remove_any(&root_path);
}

#[cfg(target_os = "linux")]
#[test]
fn confined_cwd_does_not_leak_descriptors_across_repeated_spawn() {
    let root_path = confined_temp_dir("fd-leak");
    let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
    let directory = root
        .open_directory("")
        .expect("root directory capability should open");
    let before = open_fd_count();
    for _ in 0..16 {
        let result = confined_exec(directory.clone(), &["/bin/true"]).expect("repeated spawn");
        assert!(result.status.is_success());
    }
    let after = open_fd_count();
    assert!(
        after <= before,
        "repeated confined spawn leaked {} descriptors ({before} -> {after})",
        after.saturating_sub(before)
    );

    remove_any(&root_path);
}

#[cfg(unix)]
#[test]
fn existing_absolute_cwd_path_still_sets_the_working_directory() {
    let root_path = confined_temp_dir("path-cwd");
    std::fs::write(root_path.join("marker"), b"path-cwd").expect("marker should be written");
    let result = exec_bounded(
        BoundedProcessRequest::new(vec!["/bin/cat".to_owned(), "marker".to_owned()])
            .with_cwd(root_path.clone())
            .with_timeout(Duration::from_secs(5)),
    )
    .expect("absolute path cwd should still spawn");
    assert!(result.status.is_success());
    assert_eq!(result.stdout, b"path-cwd");
    remove_any(&root_path);
}

#[cfg(unix)]
#[test]
fn confined_cwd_debug_omits_paths_and_descriptors() {
    let root_path = confined_temp_dir("debug");
    let root = ConfinedFsRoot::new(&root_path).expect("root directory should open");
    let directory = root
        .open_directory("")
        .expect("root directory capability should open");
    let request =
        BoundedProcessRequest::new(vec!["/bin/true".to_owned()]).with_confined_cwd(directory);
    let debug = format!("{request:?}");
    assert!(
        !debug.contains(root_path.to_string_lossy().as_ref()),
        "request debug must not leak the root path: {debug}"
    );
    assert!(
        !debug.contains("fd"),
        "request debug must not leak a descriptor: {debug}"
    );
    remove_any(&root_path);
}
