use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::http::HeaderMap;
use edge::{
    ActiveControlPlaneConfig, ProxyVmContext, SharedProxyVmContext, SharedState, SharedVmAsyncOps,
    VmAsyncOpBridge, apply_program_from_bytes, compile_edge_source_file, init_logging,
    new_shared_vm_async_ops, register_host_module, spawn_active_control_plane_client,
};
use tracing::info;
use uuid::Uuid;
use vm::{CallOutcome, HostFunction, Value, Vm, VmError, VmStatus, encode_program};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = match parse_cli_args() {
        Ok(CliAction::Run(cli)) => *cli,
        Ok(CliAction::Help) => {
            print_cli_help();
            return Ok(());
        }
        Ok(CliAction::Version) => {
            println!("{}", binary_version_text());
            return Ok(());
        }
        Err(err) => {
            eprintln!("error: {err}\n");
            print_cli_help();
            return Err(err.into());
        }
    };

    init_logging()?;
    info!("{}", binary_version_text());

    let max_program_bytes = cli.max_program_bytes.unwrap_or(1024 * 1024);
    let state = SharedState::new(max_program_bytes);

    let active_control_url = cli.control_plane_url.clone();
    let edge_id_path = cli
        .edge_id_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(".pd-edge/edge-id"));
    let has_partial_control_plane_flags = cli.edge_id.is_some()
        || cli.edge_id_path.is_some()
        || cli.control_plane_poll_interval_ms.is_some()
        || cli.control_plane_rpc_timeout_ms.is_some();
    if active_control_url.is_none() && has_partial_control_plane_flags {
        let err = "active control-plane flags require --control-plane-url".to_string();
        eprintln!("error: {err}\n");
        print_cli_help();
        return Err(err.into());
    }

    let poll_interval_ms = cli.control_plane_poll_interval_ms.unwrap_or(1_000);
    let request_timeout_ms = cli.control_plane_rpc_timeout_ms.unwrap_or(5_000);
    if let Some(control_plane_url) = active_control_url {
        let edge_name = cli.edge_name.clone().unwrap_or_else(default_edge_name);
        let edge_id = resolve_edge_id(cli.edge_id.as_deref(), edge_id_path.as_path())?;
        let config = ActiveControlPlaneConfig {
            control_plane_url,
            edge_id: edge_id.clone(),
            edge_name: edge_name.clone(),
            poll_interval_ms,
            request_timeout_ms,
        };
        spawn_active_control_plane_client(state.clone(), config);
        info!("active control-plane rpc enabled edge_id={edge_id} edge_name={edge_name}");
    }

    if let Some(program_path) = cli.program_path.as_ref() {
        load_program_from_path(&state, program_path).await?;
    }

    run_console_loop(&state).await
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CliArgs {
    program_path: Option<PathBuf>,
    max_program_bytes: Option<usize>,
    control_plane_url: Option<String>,
    edge_id: Option<String>,
    edge_name: Option<String>,
    edge_id_path: Option<PathBuf>,
    control_plane_poll_interval_ms: Option<u64>,
    control_plane_rpc_timeout_ms: Option<u64>,
}

#[derive(Debug)]
enum CliAction {
    Run(Box<CliArgs>),
    Help,
    Version,
}

fn parse_cli_args() -> Result<CliAction, String> {
    parse_cli_args_from(env::args().skip(1))
}

fn parse_cli_args_from<I>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter().peekable();
    let mut cli = CliArgs::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--program" => {
                let value = next_arg_value("--program", &mut args)?;
                cli.program_path = Some(PathBuf::from(value));
            }
            "--max-program-bytes" => {
                let value = next_arg_value("--max-program-bytes", &mut args)?;
                cli.max_program_bytes = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --max-program-bytes: {value}"))?,
                );
            }
            "--control-plane-url" => {
                cli.control_plane_url = Some(next_arg_value("--control-plane-url", &mut args)?);
            }
            "--edge-id" => {
                cli.edge_id = Some(next_arg_value("--edge-id", &mut args)?);
            }
            "--edge-name" => {
                cli.edge_name = Some(next_arg_value("--edge-name", &mut args)?);
            }
            "--edge-id-path" => {
                cli.edge_id_path =
                    Some(PathBuf::from(next_arg_value("--edge-id-path", &mut args)?));
            }
            "--control-plane-poll-interval-ms" => {
                let value = next_arg_value("--control-plane-poll-interval-ms", &mut args)?;
                cli.control_plane_poll_interval_ms =
                    Some(value.parse::<u64>().map_err(|_| {
                        format!("invalid --control-plane-poll-interval-ms: {value}")
                    })?);
            }
            "--control-plane-rpc-timeout-ms" => {
                let value = next_arg_value("--control-plane-rpc-timeout-ms", &mut args)?;
                cli.control_plane_rpc_timeout_ms = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --control-plane-rpc-timeout-ms: {value}"))?,
                );
            }
            _ => {
                return Err(format!("unknown argument: {arg}"));
            }
        }
    }

    Ok(CliAction::Run(Box::new(cli)))
}

fn next_arg_value(
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("missing value for {flag}"))?;
    if value.trim().is_empty() {
        return Err(format!("value for {flag} cannot be empty"));
    }
    Ok(value)
}

fn print_cli_help() {
    eprintln!(concat!(
        "Usage: pd-edge-console [options]\n\n",
        "Options:\n",
        "  --program <PATH>                          Optional local program source/.vmbc to load at startup\n",
        "  --max-program-bytes <BYTES>               Max upload/program size in bytes (default: 1048576)\n",
        "  --control-plane-url <URL>                 Enable active control-plane RPC client\n",
        "  --edge-id <UUID>                          Explicit edge UUID used by active control-plane client\n",
        "  --edge-name <NAME>                        Friendly edge name (default: hostname)\n",
        "  --edge-id-path <PATH>                     Edge UUID file path (default .pd-edge/edge-id)\n",
        "  --control-plane-poll-interval-ms <MS>     Poll interval for active control-plane client\n",
        "  --control-plane-rpc-timeout-ms <MS>       RPC timeout for active control-plane client\n",
        "  -V, --version                             Show version with git metadata\n",
        "  -h, --help                                Show this help\n\n",
        "Console commands:\n",
        "  .help                                     Show console commands\n",
        "  .status                                   Show whether a program is loaded\n",
        "  .load <PATH>                              Load source or .vmbc program from local path\n",
        "  .run                                      Run currently loaded program once\n",
        "  .quit                                     Exit console\n",
    ));
}

fn binary_version_text() -> String {
    let binary = env!("CARGO_BIN_NAME");
    let git_tag = option_env!("PD_BUILD_GIT_TAG").unwrap_or("untagged");
    let git_commit = option_env!("PD_BUILD_GIT_COMMIT").unwrap_or("unknown");
    let git_dirty = option_env!("PD_BUILD_GIT_DIRTY").unwrap_or("false");
    let dirty = matches!(git_dirty, "true" | "1" | "yes" | "dirty");

    if dirty {
        format!("{binary} {git_tag} (dirty commit: {git_commit})")
    } else {
        format!("{binary} {git_tag}")
    }
}

async fn run_console_loop(state: &SharedState) -> Result<(), Box<dyn std::error::Error>> {
    println!("pd-edge-console interactive mode");
    println!("commands: .help, .status, .load <path>, .run, .quit");

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        print!("pd-edge-console> ");
        io::stdout().flush()?;
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            println!("bye");
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == ".quit" || input == ".exit" {
            println!("bye");
            break;
        }
        if input == ".help" {
            println!("commands: .help, .status, .load <path>, .run, .quit");
            println!(
                "host APIs: console::stdin::read_line/read_all, console::stdout::write/flush, console::stderr::write/flush"
            );
            continue;
        }
        if input == ".status" {
            let has_program = state.active_program.read().await.is_some();
            println!("program_loaded={has_program}");
            continue;
        }
        if let Some(path) = input.strip_prefix(".load ").map(str::trim) {
            if path.is_empty() {
                eprintln!("error: .load requires a path");
                continue;
            }
            if let Err(err) = load_program_from_path(state, Path::new(path)).await {
                eprintln!("error: {err}");
            }
            continue;
        }
        if input == ".run" {
            if let Err(err) = run_loaded_program_once(state).await {
                eprintln!("error: {err}");
            }
            continue;
        }

        eprintln!("unknown command: {input}");
    }

    Ok(())
}

async fn load_program_from_path(
    state: &SharedState,
    input_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = resolve_program_path(input_path)?;
    let bytes = if path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("vmbc"))
        .unwrap_or(false)
    {
        fs::read(&path)?
    } else {
        let compiled = compile_edge_source_file(&path)?;
        encode_program(&compiled.program)?
    };

    let report = apply_program_from_bytes(state, &bytes).await;
    if !report.applied {
        let message = report
            .message
            .unwrap_or_else(|| "failed to apply program".to_string());
        return Err(message.into());
    }

    println!(
        "program loaded from {} (constants={}, code_bytes={}, locals={})",
        path.display(),
        report.constants.unwrap_or(0),
        report.code_bytes.unwrap_or(0),
        report.local_count.unwrap_or(0)
    );
    Ok(())
}

fn resolve_program_path(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if path.is_absolute() && path.exists() {
        return Ok(path.to_path_buf());
    }

    let cwd_candidate = std::env::current_dir()?.join(path);
    if cwd_candidate.exists() {
        return Ok(cwd_candidate);
    }

    let manifest_candidate = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    if manifest_candidate.exists() {
        return Ok(manifest_candidate);
    }

    Err(format!("program path not found: {}", path.display()).into())
}

async fn run_loaded_program_once(state: &SharedState) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = {
        let guard = state.active_program.read().await;
        guard.clone()
    };
    let Some(loaded) = loaded else {
        return Err("no program loaded".into());
    };

    let context: SharedProxyVmContext = Arc::new(Mutex::new(ProxyVmContext::from_request_headers(
        HeaderMap::new(),
        state.rate_limiter.clone(),
    )));
    let async_ops = new_shared_vm_async_ops(); 
    let mut vm = Vm::new_shared(loaded.program.clone());
    vm.set_async_bridge(Box::new(VmAsyncOpBridge::new(async_ops.clone())));

    register_host_module(&mut vm, context, async_ops.clone())?;
    register_console_host_module(&mut vm, async_ops)?;

    loop {
        match vm.run() {
            Ok(VmStatus::Halted) => {
                println!("vm halted; stack={:?}", vm.stack());
                break;
            }
            Ok(VmStatus::Yielded) => {
                tokio::task::yield_now().await;
            }
            Ok(VmStatus::Waiting(_op_id)) => {
                vm.await_waiting_host_op().await?;
            }
            Err(err) => {
                return Err(format!("vm execution failed: {err}").into());
            }
        }
    }

    Ok(())
}

fn register_console_host_module(vm: &mut Vm, async_ops: SharedVmAsyncOps) -> Result<(), VmError> {
    vm.bind_function(
        "console::stdin::read_line",
        Box::new(ConsoleStdinReadLineFunction::new(async_ops.clone())),
    );
    vm.bind_function(
        "console::stdin::read_all",
        Box::new(ConsoleStdinReadAllFunction::new(async_ops.clone())),
    );
    vm.bind_function(
        "console::stdout::write",
        Box::new(ConsoleStdoutWriteFunction::new(async_ops.clone())),
    );
    vm.bind_function(
        "console::stdout::flush",
        Box::new(ConsoleStdoutFlushFunction::new(async_ops.clone())),
    );
    vm.bind_function(
        "console::stderr::write",
        Box::new(ConsoleStderrWriteFunction::new(async_ops.clone())),
    );
    vm.bind_function(
        "console::stderr::flush",
        Box::new(ConsoleStderrFlushFunction::new(async_ops)),
    );
    Ok(())
}

struct ConsoleStdinReadLineFunction {
    async_ops: SharedVmAsyncOps,
}

impl ConsoleStdinReadLineFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for ConsoleStdinReadLineFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            let line = tokio::task::spawn_blocking(move || {
                let mut input = String::new();
                io::stdin()
                    .read_line(&mut input)
                    .map_err(|err| VmError::HostError(format!("stdin read_line failed: {err}")))?;
                Ok::<String, VmError>(input)
            })
            .await
            .map_err(|err| VmError::HostError(format!("stdin read_line task failed: {err}")))??;
            Ok(vec![Value::String(line)])
        })
    }
}

struct ConsoleStdinReadAllFunction {
    async_ops: SharedVmAsyncOps,
}

impl ConsoleStdinReadAllFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for ConsoleStdinReadAllFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            let text = tokio::task::spawn_blocking(move || {
                let mut input = String::new();
                io::stdin()
                    .read_to_string(&mut input)
                    .map_err(|err| VmError::HostError(format!("stdin read_all failed: {err}")))?;
                Ok::<String, VmError>(input)
            })
            .await
            .map_err(|err| VmError::HostError(format!("stdin read_all task failed: {err}")))??;
            Ok(vec![Value::String(text)])
        })
    }
}

struct ConsoleStdoutWriteFunction {
    async_ops: SharedVmAsyncOps,
}

impl ConsoleStdoutWriteFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for ConsoleStdoutWriteFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let text = expect_string(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            let written = tokio::task::spawn_blocking(move || {
                let mut out = io::stdout().lock();
                out.write_all(text.as_bytes())
                    .and_then(|_| out.flush())
                    .map_err(|err| VmError::HostError(format!("stdout write failed: {err}")))?;
                Ok::<i64, VmError>(text.len() as i64)
            })
            .await
            .map_err(|err| VmError::HostError(format!("stdout write task failed: {err}")))??;
            Ok(vec![Value::Int(written)])
        })
    }
}

struct ConsoleStdoutFlushFunction {
    async_ops: SharedVmAsyncOps,
}

impl ConsoleStdoutFlushFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for ConsoleStdoutFlushFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::spawn_blocking(move || {
                io::stdout()
                    .flush()
                    .map_err(|err| VmError::HostError(format!("stdout flush failed: {err}")))?;
                Ok::<(), VmError>(())
            })
            .await
            .map_err(|err| VmError::HostError(format!("stdout flush task failed: {err}")))??;
            Ok(vec![Value::Bool(true)])
        })
    }
}

struct ConsoleStderrWriteFunction {
    async_ops: SharedVmAsyncOps,
}

impl ConsoleStderrWriteFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for ConsoleStderrWriteFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 1)?;
        let text = expect_string(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            let written = tokio::task::spawn_blocking(move || {
                let mut out = io::stderr().lock();
                out.write_all(text.as_bytes())
                    .and_then(|_| out.flush())
                    .map_err(|err| VmError::HostError(format!("stderr write failed: {err}")))?;
                Ok::<i64, VmError>(text.len() as i64)
            })
            .await
            .map_err(|err| VmError::HostError(format!("stderr write task failed: {err}")))??;
            Ok(vec![Value::Int(written)])
        })
    }
}

struct ConsoleStderrFlushFunction {
    async_ops: SharedVmAsyncOps,
}

impl ConsoleStderrFlushFunction {
    fn new(async_ops: SharedVmAsyncOps) -> Self {
        Self { async_ops }
    }
}

impl HostFunction for ConsoleStderrFlushFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        expect_arg_count(args, 0)?;
        schedule_future_call(vm, &self.async_ops, async move {
            tokio::task::spawn_blocking(move || {
                io::stderr()
                    .flush()
                    .map_err(|err| VmError::HostError(format!("stderr flush failed: {err}")))?;
                Ok::<(), VmError>(())
            })
            .await
            .map_err(|err| VmError::HostError(format!("stderr flush task failed: {err}")))??;
            Ok(vec![Value::Bool(true)])
        })
    }
}

fn schedule_future_call<F>(
    vm: &mut Vm,
    async_ops: &SharedVmAsyncOps,
    future: F,
) -> Result<CallOutcome, VmError>
where
    F: std::future::Future<Output = Result<Vec<Value>, VmError>> + Send + 'static,
{
    let mut ops = async_ops.lock().expect("vm async ops lock poisoned");
    let op_id = ops.schedule_future(vm, future)?;
    Ok(CallOutcome::Pending(op_id))
}

fn expect_arg_count(args: &[Value], expected: usize) -> Result<(), VmError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(VmError::HostError(format!(
            "expected {expected} arguments, got {}",
            args.len()
        )))
    }
}

fn expect_string(args: &[Value], index: usize) -> Result<String, VmError> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(VmError::TypeMismatch("string")),
    }
}

fn resolve_edge_id(
    explicit: Option<&str>,
    id_path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(value) = explicit {
        let parsed = Uuid::parse_str(value)
            .map_err(|_| format!("--edge-id must be a valid UUID, got: {value}"))?;
        let id = parsed.to_string();
        persist_edge_id(id_path, &id)?;
        return Ok(id);
    }

    if id_path.exists() {
        let raw = fs::read_to_string(id_path)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "edge id file '{}' is empty; remove it or provide --edge-id",
                id_path.display()
            )
            .into());
        }
        if let Ok(parsed) = Uuid::parse_str(trimmed) {
            let normalized = parsed.to_string();
            if normalized != trimmed {
                persist_edge_id(id_path, &normalized)?;
            }
            return Ok(normalized);
        }
    }

    let generated = Uuid::new_v4().to_string();
    persist_edge_id(id_path, &generated)?;
    Ok(generated)
}

fn persist_edge_id(path: &Path, edge_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{edge_id}\n"))?;
    Ok(())
}

fn default_edge_name() -> String {
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "unknown-host".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(prefix: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("{}-{}", prefix, Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    #[test]
    fn parse_cli_args_from_handles_help_and_version() {
        assert!(matches!(
            parse_cli_args_from(["--help".to_string()]).expect("parse should succeed"),
            CliAction::Help
        ));
        assert!(matches!(
            parse_cli_args_from(["-V".to_string()]).expect("parse should succeed"),
            CliAction::Version
        ));
    }

    #[test]
    fn parse_cli_args_from_parses_run_options() {
        let action = parse_cli_args_from([
            "--program".to_string(),
            "examples/demo.rss".to_string(),
            "--max-program-bytes".to_string(),
            "4096".to_string(),
            "--control-plane-url".to_string(),
            "http://127.0.0.1:9100".to_string(),
            "--edge-id".to_string(),
            "123e4567-e89b-12d3-a456-426614174000".to_string(),
            "--edge-name".to_string(),
            "console-edge".to_string(),
            "--edge-id-path".to_string(),
            ".pd-edge/console-id".to_string(),
            "--control-plane-poll-interval-ms".to_string(),
            "120".to_string(),
            "--control-plane-rpc-timeout-ms".to_string(),
            "3400".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(
            *cli,
            CliArgs {
                program_path: Some(PathBuf::from("examples/demo.rss")),
                max_program_bytes: Some(4096),
                control_plane_url: Some("http://127.0.0.1:9100".to_string()),
                edge_id: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
                edge_name: Some("console-edge".to_string()),
                edge_id_path: Some(PathBuf::from(".pd-edge/console-id")),
                control_plane_poll_interval_ms: Some(120),
                control_plane_rpc_timeout_ms: Some(3400),
            }
        );
    }

    #[test]
    fn parse_cli_args_from_rejects_unknown_argument() {
        let err = parse_cli_args_from(["--bad".to_string()]).expect_err("unknown should fail");
        assert!(err.contains("unknown argument: --bad"));
    }

    #[test]
    fn parse_cli_args_from_rejects_missing_value() {
        let err =
            parse_cli_args_from(["--program".to_string()]).expect_err("missing value should fail");
        assert!(err.contains("missing value for --program"));
    }

    #[test]
    fn resolve_edge_id_explicit_value_is_persisted() {
        let dir = temp_test_dir("pd-edge-console-explicit-id");
        let path = dir.join("edge-id");
        let edge_id = resolve_edge_id(Some("123e4567-e89b-12d3-a456-426614174000"), path.as_path())
            .expect("explicit id should resolve");
        assert_eq!(edge_id, "123e4567-e89b-12d3-a456-426614174000");
        let on_disk = fs::read_to_string(&path).expect("id file should exist");
        assert_eq!(on_disk, "123e4567-e89b-12d3-a456-426614174000\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_edge_id_existing_invalid_file_is_replaced() {
        let dir = temp_test_dir("pd-edge-console-invalid-id");
        let path = dir.join("edge-id");
        fs::write(&path, "invalid-uuid\n").expect("seed invalid id");

        let edge_id = resolve_edge_id(None, path.as_path()).expect("id should be generated");
        assert!(
            Uuid::parse_str(&edge_id).is_ok(),
            "generated id should be uuid"
        );
        let on_disk = fs::read_to_string(&path).expect("id file should exist");
        assert_eq!(on_disk, format!("{edge_id}\n"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_program_path_returns_error_for_missing_path() {
        let missing = PathBuf::from("path/that/does/not/exist.rss");
        let err = resolve_program_path(missing.as_path()).expect_err("missing path should fail");
        assert!(err.to_string().contains("program path not found"));
    }

    #[test]
    fn resolve_program_path_falls_back_to_manifest_dir() {
        let relative = PathBuf::from(format!("target/console-test-{}.rss", Uuid::new_v4()));
        let absolute = Path::new(env!("CARGO_MANIFEST_DIR")).join(&relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).expect("parent should be created");
        }
        fs::write(&absolute, "vm::http::response::set_body(\"ok\");")
            .expect("fixture file should be written");

        let resolved = resolve_program_path(relative.as_path()).expect("path should resolve");
        assert_eq!(resolved, absolute);
        let _ = fs::remove_file(absolute);
    }
}
