#![cfg(all(feature = "cli", feature = "cranelift-jit", unix))]

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("aot-process-{}-{name}", std::process::id()));
        fs::create_dir(&path).expect("create isolated fixture");
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// A defective loader can jump to an unmapped writer-process helper. Execute
// only in a bounded child, never in this test runner, and prohibit core files.
fn run_cli(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pd-vm-run"));
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn Core CLI");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll Core CLI").is_some() {
            break;
        }
        if started.elapsed() > Duration::from_secs(20) {
            child.kill().expect("kill timed out Core CLI");
            let output = child.wait_with_output().expect("reap Core CLI");
            panic!("Core CLI timed out: {args:?}: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("collect Core CLI");
    eprintln!(
        "Core CLI {args:?}: status={}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_native_result(output: &Output, dump: bool, value: i64) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "reader failed: {output:?}");
    assert!(stdout.contains("vm halted"), "{stdout}");
    assert!(
        stdout.contains(&format!("stack: [Int({value})]")),
        "{stdout}"
    );
    if dump {
        assert!(stdout.contains("lowering=ssa"), "{stdout}");
        assert!(!stdout.contains("interpreter-boundary"), "{stdout}");
        let count = stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("aot executions: "))
            .expect("native execution count")
            .parse::<u64>()
            .expect("numeric count");
        assert!(count > 0, "no actual native returns: {stdout}");
    }
}

fn roundtrip(name: &str, source: &str, result: i64) {
    let fixture = Fixture::new(name);
    let source_path = fixture.0.join("program.rss");
    let artifact_path = fixture.0.join("program.pat");
    fs::write(&source_path, source).expect("write source");
    let source_path = source_path.to_str().unwrap();
    let artifact_path = artifact_path.to_str().unwrap();
    assert_native_result(
        &run_cli(&[
            "--aot-save",
            artifact_path,
            "--aot-dump",
            "--fuel",
            "100000",
            source_path,
        ]),
        true,
        result,
    );
    let bytes = fs::read(artifact_path).expect("read baseline writer artifact");
    assert_eq!(&bytes[..10], b"PAT\0\x08\0\x08\0\0\0");
    // Each load has a new address space, including the matching source companion.
    let mut failures = Vec::new();
    for companion in [false, true] {
        for dump in [false, true] {
            let mut args = vec!["--aot-load", artifact_path, "--fuel", "100000"];
            if dump {
                args.push("--aot-dump");
            }
            if companion {
                args.push(source_path);
            }
            let output = run_cli(&args);
            if !output.status.success() {
                failures.push(format!(
                    "companion={companion} dump={dump}: {}",
                    output.status
                ));
            } else {
                assert_native_result(&output, dump, result);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "fresh-process artifact readers failed: {failures:?}"
    );
    // Embedded bytecode suffices; standalone loading must not reopen source.
    fs::remove_file(source_path).expect("remove original source");
    assert_native_result(
        &run_cli(&[
            "--aot-load",
            artifact_path,
            "--aot-dump",
            "--fuel",
            "100000",
        ]),
        true,
        result,
    );
}

#[test]
fn fresh_process_number_factorial_artifact_is_native_with_and_without_source() {
    roundtrip(
        "number-factorial",
        "fn factorial(n: number) -> number { if n < 2 => { 1 } else => { n * factorial(n - 1) } } factorial(6);",
        720,
    );
}

#[test]
fn fresh_process_float_number_factorial_retains_float_result() {
    let fixture = Fixture::new("float-number-factorial");
    let source = fixture.0.join("program.rss");
    let artifact = fixture.0.join("program.pat");
    fs::write(&source, "fn factorial(n: number) -> number { if n < 2 => { 1 } else => { n * factorial(n - 1) } } factorial(6.0);").unwrap();
    let source = source.to_str().unwrap();
    let artifact = artifact.to_str().unwrap();
    let assert_float_native = |output: Output| {
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("vm halted"), "{stdout}");
        assert!(stdout.contains("stack: [Float(720.0)]"), "{stdout}");
        assert!(stdout.contains("lowering=ssa"), "{stdout}");
        assert!(!stdout.contains("interpreter-boundary"), "{stdout}");
        let count = stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("aot executions: "))
            .expect("native execution count")
            .parse::<u64>()
            .unwrap();
        assert!(count > 0, "{stdout}");
    };
    assert_float_native(run_cli(&[
        "--aot-save",
        artifact,
        "--aot-dump",
        "--fuel",
        "100000",
        source,
    ]));
    assert_float_native(run_cli(&[
        "--aot-load",
        artifact,
        "--aot-dump",
        "--fuel",
        "100000",
        source,
    ]));
    assert_float_native(run_cli(&[
        "--aot-load",
        artifact,
        "--aot-dump",
        "--fuel",
        "100000",
    ]));
    fs::remove_file(source).unwrap();
    assert_float_native(run_cli(&[
        "--aot-load",
        artifact,
        "--aot-dump",
        "--fuel",
        "100000",
    ]));
}

#[test]
fn fresh_process_native_artifact_loads_both_entrypoints() {
    roundtrip("neutral", "let value = 1 + 2; value;", 3);
}

#[test]
fn fresh_process_ignores_persisted_instructions_in_both_modes() {
    let fixture = Fixture::new("persisted-instructions");
    let source = fixture.0.join("program.rss");
    let artifact = fixture.0.join("program.pat");
    fs::write(&source, "let value = 1 + 2; value;").unwrap();
    let source = source.to_str().unwrap();
    let artifact = artifact.to_str().unwrap();
    assert_native_result(
        &run_cli(&[
            "--aot-save",
            artifact,
            "--aot-dump",
            "--fuel",
            "100000",
            source,
        ]),
        true,
        3,
    );
    let mut bytes = fs::read(artifact).unwrap();
    fn read_u32(bytes: &[u8], offset: &mut usize) -> usize {
        let value = u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap()) as usize;
        *offset += 4;
        value
    }
    let mut offset = 11;
    for _ in 0..3 {
        let len = read_u32(&bytes, &mut offset);
        offset += len;
    }
    offset += 12 + 8 + 4;
    let len = read_u32(&bytes, &mut offset);
    offset += len;
    let resumes = read_u32(&bytes, &mut offset);
    offset += resumes * 4;
    let len = read_u32(&bytes, &mut offset);
    assert_eq!(offset + len, bytes.len());
    bytes[offset..].fill(0xcc); // Child isolation contains a regressed raw loader.
    for boundary in [false, true] {
        bytes[8..10].copy_from_slice(&u16::from(boundary).to_le_bytes());
        fs::write(artifact, &bytes).unwrap();
        for companion in [false, true] {
            let mut args = vec!["--aot-load", artifact, "--aot-dump", "--fuel", "100000"];
            if companion {
                args.push(source);
            }
            let output = run_cli(&args);
            if !boundary {
                assert_native_result(&output, true, 3);
            } else {
                assert_native_result(&output, false, 3);
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(stdout.contains("interpreter-boundary"), "{stdout}");
                assert!(stdout.contains("aot executions: 1"), "{stdout}");
            }
        }
    }
}

#[test]
fn fresh_process_native_artifact_calls_script_and_host_services() {
    roundtrip(
        "calls",
        "fn bump(value: int) -> int { value + 1 }\nlet printed = print(\"native artifact host call\");\nlet value = bump(2); value;",
        3,
    );
}
