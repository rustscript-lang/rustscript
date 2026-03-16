use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[cfg(feature = "http3")]
use edge::serve_http3_proxy;
use edge::{
    ActiveControlPlaneConfig, RuntimeStoreLimits, SharedState, VM_EPOCH_TICK_INTERVAL_MS,
    VmExecutionConfig, VmExecutionMode, VmInterruptConfig, binary_version_report,
    binary_version_text, build_admin_app, disable_lock_metrics_collection, enabled_feature_line,
    init_logging, serve_http_proxy, serve_https_proxy, spawn_active_control_plane_client,
};
use tracing::{info, warn};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = match parse_cli_args() {
        Ok(CliAction::Run(cli)) => *cli,
        Ok(CliAction::Help) => {
            print_cli_help();
            return Ok(());
        }
        Ok(CliAction::Version) => {
            println!("{}", binary_version_report(env!("CARGO_BIN_NAME")));
            return Ok(());
        }
        Err(err) => {
            eprintln!("error: {err}\n");
            print_cli_help();
            return Err(err.into());
        }
    };

    init_logging(!cli.disable_logging)?;
    info!("{}", binary_version_text(env!("CARGO_BIN_NAME")));
    info!("{}", enabled_feature_line());

    let data_addr = if let Some(value) = cli.proxy_addr {
        value
    } else {
        "0.0.0.0:8080".parse()?
    };
    let admin_addr = if let Some(value) = cli.admin_addr {
        value
    } else {
        "127.0.0.1:8081".parse()?
    };
    let max_program_bytes = cli.max_program_bytes.unwrap_or(1024 * 1024);
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
    let vm_execution = VmExecutionConfig {
        interrupt: cli.vm_interrupt_config()?,
        execution_mode: cli.vm_execution_mode.unwrap_or_default(),
    };
    let store_limits = cli.runtime_store_limits();
    if cli.disable_metrics {
        disable_lock_metrics_collection();
    }
    let state = SharedState::new_with_store_limits(max_program_bytes, store_limits)
        .with_metrics_collection_enabled(!cli.disable_metrics)
        .with_vm_execution_config(vm_execution);
    info!("vm execution mode={}", vm_execution.execution_mode.as_str());
    match vm_execution.interrupt {
        VmInterruptConfig::None => {}
        VmInterruptConfig::Fuel {
            fuel_per_yield,
            check_interval,
        } => {
            info!(
                "vm cooperative scheduling enabled mode=fuel fuel_per_yield={} check_interval={}",
                fuel_per_yield, check_interval
            );
        }
        VmInterruptConfig::Epoch {
            ticks_per_slice,
            check_interval,
        } => {
            info!(
                "vm cooperative scheduling enabled mode=epoch epoch_deadline={} epoch_tick_ms={} check_interval={}",
                ticks_per_slice, VM_EPOCH_TICK_INTERVAL_MS, check_interval
            );
        }
    }
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

    let admin_app = build_admin_app(state.clone());

    let data_listener = tokio::net::TcpListener::bind(data_addr).await?;
    let https_listener = match cli.https_addr {
        Some(addr) => Some(tokio::net::TcpListener::bind(addr).await?),
        None => None,
    };
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
    #[cfg(feature = "http3")]
    let http3_listener = match cli.http3_addr {
        Some(addr) => Some(tokio::net::UdpSocket::bind(addr).await?),
        None => None,
    };

    #[cfg(not(feature = "http3"))]
    if cli.http3_addr.is_some() {
        let err = "--http3-addr requires a build with the `http3` feature".to_string();
        eprintln!("error: {err}\n");
        print_cli_help();
        return Err(err.into());
    }

    info!(
        "proxy/data-plane listening on http://{}",
        data_listener.local_addr()?
    );
    if let Some(listener) = &https_listener {
        info!(
            "proxy/data-plane listening on https://{} (standard HTTP programs terminate TLS directly; downstream transport-prelude programs still use the transport handoff path)",
            listener.local_addr()?
        );
    }
    #[cfg(feature = "http3")]
    if let Some(listener) = &http3_listener {
        info!(
            "proxy/data-plane listening on https://{} over UDP (HTTP/3)",
            listener.local_addr()?
        );
    }
    info!(
        "admin endpoint listening on http://{}",
        admin_listener.local_addr()?
    );

    let data_server = serve_http_proxy(data_listener, state.clone());
    let https_server = async {
        if let Some(listener) = https_listener {
            serve_https_proxy(listener, state.clone()).await
        } else {
            std::future::pending::<std::io::Result<()>>().await
        }
    };
    let http3_server = async {
        #[cfg(feature = "http3")]
        {
            if let Some(listener) = http3_listener {
                serve_http3_proxy(listener, state.clone()).await
            } else {
                std::future::pending::<std::io::Result<()>>().await
            }
        }
        #[cfg(not(feature = "http3"))]
        {
            std::future::pending::<std::io::Result<()>>().await
        }
    };
    let admin_server = axum::serve(admin_listener, admin_app);

    tokio::select! {
        result = data_server => result?,
        result = https_server => result?,
        result = http3_server => result?,
        result = admin_server => result?,
    }

    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CliArgs {
    disable_logging: bool,
    disable_metrics: bool,
    proxy_addr: Option<SocketAddr>,
    https_addr: Option<SocketAddr>,
    http3_addr: Option<SocketAddr>,
    admin_addr: Option<SocketAddr>,
    max_program_bytes: Option<usize>,
    tls_session_reuse_entries: Option<usize>,
    upstream_http_reuse_entries: Option<usize>,
    downstream_http2_session_entries: Option<usize>,
    upstream_http3_reuse_entries: Option<usize>,
    downstream_http3_session_entries: Option<usize>,
    vm_fuel: Option<u64>,
    vm_fuel_check_interval: Option<u32>,
    vm_epoch_deadline: Option<u64>,
    vm_epoch_check_interval: Option<u32>,
    vm_execution_mode: Option<VmExecutionMode>,
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
            "--disable-logging" => {
                cli.disable_logging = true;
            }
            "--disable-metrics" => {
                cli.disable_metrics = true;
            }
            "--control-plane-url" => {
                cli.control_plane_url = Some(next_arg_value("--control-plane-url", &mut args)?);
            }
            "--proxy-addr" | "--data-addr" => {
                let flag = if arg == "--proxy-addr" {
                    "--proxy-addr"
                } else {
                    "--data-addr"
                };
                let value = next_arg_value(flag, &mut args)?;
                cli.proxy_addr = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| format!("invalid {flag}: {value}"))?,
                );
            }
            "--https-addr" => {
                let value = next_arg_value("--https-addr", &mut args)?;
                cli.https_addr = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| format!("invalid --https-addr: {value}"))?,
                );
            }
            "--http3-addr" => {
                let value = next_arg_value("--http3-addr", &mut args)?;
                cli.http3_addr = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| format!("invalid --http3-addr: {value}"))?,
                );
            }
            "--admin-addr" => {
                let value = next_arg_value("--admin-addr", &mut args)?;
                cli.admin_addr = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| format!("invalid --admin-addr: {value}"))?,
                );
            }
            "--max-program-bytes" => {
                let value = next_arg_value("--max-program-bytes", &mut args)?;
                cli.max_program_bytes = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --max-program-bytes: {value}"))?,
                );
            }
            "--tls-session-reuse-entries" => {
                let value = next_arg_value("--tls-session-reuse-entries", &mut args)?;
                cli.tls_session_reuse_entries = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --tls-session-reuse-entries: {value}"))?,
                );
            }
            "--upstream-http-reuse-entries" => {
                let value = next_arg_value("--upstream-http-reuse-entries", &mut args)?;
                cli.upstream_http_reuse_entries = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --upstream-http-reuse-entries: {value}"))?,
                );
            }
            "--downstream-http2-session-entries" => {
                let value = next_arg_value("--downstream-http2-session-entries", &mut args)?;
                cli.downstream_http2_session_entries =
                    Some(value.parse::<usize>().map_err(|_| {
                        format!("invalid --downstream-http2-session-entries: {value}")
                    })?);
            }
            "--upstream-http3-reuse-entries" => {
                let value = next_arg_value("--upstream-http3-reuse-entries", &mut args)?;
                cli.upstream_http3_reuse_entries = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid --upstream-http3-reuse-entries: {value}"))?,
                );
            }
            "--downstream-http3-session-entries" => {
                let value = next_arg_value("--downstream-http3-session-entries", &mut args)?;
                cli.downstream_http3_session_entries =
                    Some(value.parse::<usize>().map_err(|_| {
                        format!("invalid --downstream-http3-session-entries: {value}")
                    })?);
            }
            "--vm-fuel" => {
                let value = next_arg_value("--vm-fuel", &mut args)?;
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --vm-fuel: {value}"))?;
                if parsed == 0 {
                    return Err("--vm-fuel must be > 0".to_string());
                }
                cli.vm_fuel = Some(parsed);
            }
            "--vm-fuel-check-interval" => {
                let value = next_arg_value("--vm-fuel-check-interval", &mut args)?;
                let parsed = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --vm-fuel-check-interval: {value}"))?;
                if parsed == 0 {
                    return Err("--vm-fuel-check-interval must be > 0".to_string());
                }
                cli.vm_fuel_check_interval = Some(parsed);
            }
            "--vm-epoch-deadline" => {
                let value = next_arg_value("--vm-epoch-deadline", &mut args)?;
                let parsed = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --vm-epoch-deadline: {value}"))?;
                if parsed == 0 {
                    return Err("--vm-epoch-deadline must be > 0".to_string());
                }
                cli.vm_epoch_deadline = Some(parsed);
            }
            "--vm-epoch-check-interval" => {
                let value = next_arg_value("--vm-epoch-check-interval", &mut args)?;
                let parsed = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --vm-epoch-check-interval: {value}"))?;
                if parsed == 0 {
                    return Err("--vm-epoch-check-interval must be > 0".to_string());
                }
                cli.vm_epoch_check_interval = Some(parsed);
            }
            "--vm-execution-mode" => {
                let value = next_arg_value("--vm-execution-mode", &mut args)?;
                cli.vm_execution_mode = Some(parse_vm_execution_mode(&value)?);
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
    if cli.vm_fuel.is_some() && cli.vm_epoch_deadline.is_some() {
        return Err("--vm-fuel and --vm-epoch-deadline are mutually exclusive".to_string());
    }
    if cli.vm_fuel_check_interval.is_some() && cli.vm_epoch_check_interval.is_some() {
        return Err(
            "--vm-fuel-check-interval and --vm-epoch-check-interval are mutually exclusive"
                .to_string(),
        );
    }
    if cli.vm_epoch_check_interval.is_some() && cli.vm_epoch_deadline.is_none() {
        return Err("--vm-epoch-check-interval requires --vm-epoch-deadline".to_string());
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

fn parse_vm_execution_mode(value: &str) -> Result<VmExecutionMode, String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "async" => Ok(VmExecutionMode::Async),
        "threading" | "spawn-blocking" => Ok(VmExecutionMode::Threading),
        _ => Err(format!(
            "invalid --vm-execution-mode: {value} (expected async|threading)"
        )),
    }
}

fn print_cli_help() {
    let defaults = RuntimeStoreLimits::default();
    eprintln!(
        concat!(
            "Usage: pd-edge-http-proxy [options]\n\n",
            "Options:\n",
            "  --disable-logging                         Disable tracing/log output\n",
            "  --disable-metrics                         Disable telemetry and Prometheus metric collection\n",
            "  --proxy-addr <ADDR>                       Proxy/data-plane listen address (default: 0.0.0.0:8080)\n",
            "  --data-addr <ADDR>                        Alias for --proxy-addr\n",
            "  --https-addr <ADDR>                       Optional HTTPS/TLS listen address; standard HTTP programs terminate TLS directly, while downstream transport-prelude programs still use the transport handoff path\n",
            "  --http3-addr <ADDR>                       Optional HTTP/3 over UDP listen address\n",
            "  --admin-addr <ADDR>                       Admin endpoint listen address (default: 127.0.0.1:8081)\n",
            "  --max-program-bytes <BYTES>               Max upload/program size in bytes (default: 1048576)\n",
            "  --tls-session-reuse-entries <N>           TLS session reuse store cap (default: {})\n",
            "  --upstream-http-reuse-entries <N>         Upstream HTTP reuse store cap (default: {})\n",
            "  --downstream-http2-session-entries <N>    Downstream HTTP/2 session tracking cap (default: {})\n",
            "  --upstream-http3-reuse-entries <N>        Upstream HTTP/3 reuse store cap (default: {})\n",
            "  --downstream-http3-session-entries <N>    Downstream HTTP/3 session tracking cap (default: {})\n",
            "  --vm-fuel <UNITS>                         Enable cooperative VM fuel slices per request\n",
            "  --vm-fuel-check-interval <OPS>            Fuel check interval when --vm-fuel is enabled (default: 1)\n",
            "  --vm-epoch-deadline <TICKS>               Enable cooperative VM epoch slices per request (1 tick = 1ms wall clock)\n",
            "  --vm-epoch-check-interval <OPS>           Epoch check interval when --vm-epoch-deadline is enabled (default: 1)\n",
            "  --vm-execution-mode <MODE>                VM execution mode: async|threading (default: async)\n",
            "  --control-plane-url <URL>                 Enable active control-plane RPC client\n",
            "  --edge-id <UUID>                          Explicit edge UUID used by active control-plane client\n",
            "  --edge-name <NAME>                        Friendly edge name (default: hostname)\n",
            "  --edge-id-path <PATH>                     Edge UUID file path (default .pd-edge/edge-id)\n",
            "  --control-plane-poll-interval-ms <MS>     Poll interval for active control-plane client\n",
            "  --control-plane-rpc-timeout-ms <MS>       RPC timeout for active control-plane client\n",
            "  -V, --version                             Show version, git metadata, and enabled features\n",
            "  -h, --help                                Show this help\n"
        ),
        defaults.tls_session_reuse_entries,
        defaults.upstream_http_reuse_entries,
        defaults.downstream_http2_session_entries,
        defaults.upstream_http3_reuse_entries,
        defaults.downstream_http3_session_entries,
    );
}

impl CliArgs {
    fn runtime_store_limits(&self) -> RuntimeStoreLimits {
        let mut limits = RuntimeStoreLimits::default();
        if let Some(value) = self.tls_session_reuse_entries {
            limits.tls_session_reuse_entries = value;
        }
        if let Some(value) = self.upstream_http_reuse_entries {
            limits.upstream_http_reuse_entries = value;
        }
        if let Some(value) = self.downstream_http2_session_entries {
            limits.downstream_http2_session_entries = value;
        }
        if let Some(value) = self.upstream_http3_reuse_entries {
            limits.upstream_http3_reuse_entries = value;
        }
        if let Some(value) = self.downstream_http3_session_entries {
            limits.downstream_http3_session_entries = value;
        }
        limits
    }

    fn vm_interrupt_config(&self) -> Result<VmInterruptConfig, Box<dyn std::error::Error>> {
        if let Some(fuel_per_yield) = self.vm_fuel {
            return Ok(VmInterruptConfig::Fuel {
                fuel_per_yield,
                check_interval: self.vm_fuel_check_interval.unwrap_or(1),
            });
        }
        if let Some(ticks_per_slice) = self.vm_epoch_deadline {
            return Ok(VmInterruptConfig::Epoch {
                ticks_per_slice,
                check_interval: self.vm_epoch_check_interval.unwrap_or(1),
            });
        }
        Ok(VmInterruptConfig::None)
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
        match Uuid::parse_str(trimmed) {
            Ok(parsed) => {
                let normalized = parsed.to_string();
                if normalized != trimmed {
                    persist_edge_id(id_path, &normalized)?;
                }
                return Ok(normalized);
            }
            Err(_) => {
                warn!(
                    "invalid UUID in edge id file path={}, generating a new one",
                    id_path.display()
                );
            }
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
            "--data-addr".to_string(),
            "127.0.0.1:7001".to_string(),
            "--admin-addr".to_string(),
            "127.0.0.1:7002".to_string(),
            "--max-program-bytes".to_string(),
            "2048".to_string(),
            "--tls-session-reuse-entries".to_string(),
            "16".to_string(),
            "--upstream-http-reuse-entries".to_string(),
            "24".to_string(),
            "--downstream-http2-session-entries".to_string(),
            "0".to_string(),
            "--control-plane-url".to_string(),
            "http://127.0.0.1:9100".to_string(),
            "--edge-id".to_string(),
            "123e4567-e89b-12d3-a456-426614174000".to_string(),
            "--edge-name".to_string(),
            "test-edge".to_string(),
            "--edge-id-path".to_string(),
            ".pd-edge/custom-id".to_string(),
            "--control-plane-poll-interval-ms".to_string(),
            "150".to_string(),
            "--control-plane-rpc-timeout-ms".to_string(),
            "2500".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(
            *cli,
            CliArgs {
                disable_logging: false,
                disable_metrics: false,
                proxy_addr: Some("127.0.0.1:7001".parse().expect("valid addr")),
                https_addr: None,
                http3_addr: None,
                admin_addr: Some("127.0.0.1:7002".parse().expect("valid addr")),
                max_program_bytes: Some(2048),
                tls_session_reuse_entries: Some(16),
                upstream_http_reuse_entries: Some(24),
                downstream_http2_session_entries: Some(0),
                upstream_http3_reuse_entries: None,
                downstream_http3_session_entries: None,
                vm_fuel: None,
                vm_fuel_check_interval: None,
                vm_epoch_deadline: None,
                vm_epoch_check_interval: None,
                vm_execution_mode: None,
                control_plane_url: Some("http://127.0.0.1:9100".to_string()),
                edge_id: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
                edge_name: Some("test-edge".to_string()),
                edge_id_path: Some(PathBuf::from(".pd-edge/custom-id")),
                control_plane_poll_interval_ms: Some(150),
                control_plane_rpc_timeout_ms: Some(2500),
            }
        );
    }

    #[test]
    fn parse_cli_args_from_parses_disable_flags() {
        let action = parse_cli_args_from([
            "--disable-logging".to_string(),
            "--disable-metrics".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert!(cli.disable_logging);
        assert!(cli.disable_metrics);
    }

    #[test]
    fn parse_cli_args_from_parses_https_addr() {
        let action = parse_cli_args_from([
            "--proxy-addr".to_string(),
            "127.0.0.1:7001".to_string(),
            "--https-addr".to_string(),
            "127.0.0.1:7443".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(
            cli.proxy_addr,
            Some("127.0.0.1:7001".parse().expect("valid addr"))
        );
        assert_eq!(
            cli.https_addr,
            Some("127.0.0.1:7443".parse().expect("valid addr"))
        );
        assert_eq!(cli.http3_addr, None);
    }

    #[test]
    fn parse_cli_args_from_parses_http3_addr() {
        let action = parse_cli_args_from([
            "--proxy-addr".to_string(),
            "127.0.0.1:7001".to_string(),
            "--http3-addr".to_string(),
            "127.0.0.1:7444".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(
            cli.http3_addr,
            Some("127.0.0.1:7444".parse().expect("valid addr"))
        );
    }

    #[test]
    fn parse_cli_args_from_parses_vm_fuel_flags() {
        let action = parse_cli_args_from([
            "--vm-fuel".to_string(),
            "1000".to_string(),
            "--vm-fuel-check-interval".to_string(),
            "8".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(cli.vm_fuel, Some(1000));
        assert_eq!(cli.vm_fuel_check_interval, Some(8));
    }

    #[test]
    fn parse_cli_args_from_parses_vm_epoch_flags() {
        let action = parse_cli_args_from([
            "--vm-epoch-deadline".to_string(),
            "3".to_string(),
            "--vm-epoch-check-interval".to_string(),
            "5".to_string(),
        ])
        .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(cli.vm_epoch_deadline, Some(3));
        assert_eq!(cli.vm_epoch_check_interval, Some(5));
    }

    #[test]
    fn parse_cli_args_from_parses_vm_execution_mode() {
        let action =
            parse_cli_args_from(["--vm-execution-mode".to_string(), "threading".to_string()])
                .expect("parse should succeed");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(cli.vm_execution_mode, Some(VmExecutionMode::Threading));
    }

    #[test]
    fn runtime_store_limits_uses_defaults_and_overrides() {
        let cli = CliArgs {
            tls_session_reuse_entries: Some(8),
            upstream_http_reuse_entries: Some(0),
            upstream_http3_reuse_entries: Some(3),
            downstream_http3_session_entries: Some(5),
            ..CliArgs::default()
        };

        let limits = cli.runtime_store_limits();

        assert_eq!(limits.tls_session_reuse_entries, 8);
        assert_eq!(limits.upstream_http_reuse_entries, 0);
        assert_eq!(limits.upstream_http3_reuse_entries, 3);
        assert_eq!(limits.downstream_http3_session_entries, 5);
        assert_eq!(
            limits.downstream_http2_session_entries,
            RuntimeStoreLimits::default().downstream_http2_session_entries
        );
    }

    #[test]
    fn parse_cli_args_from_rejects_invalid_vm_execution_mode() {
        let err =
            parse_cli_args_from(["--vm-execution-mode".to_string(), "threadpool".to_string()])
                .expect_err("invalid vm execution mode should fail");
        assert!(err.contains("invalid --vm-execution-mode"));
    }

    #[test]
    fn parse_cli_args_from_rejects_zero_vm_fuel() {
        let err = parse_cli_args_from(["--vm-fuel".to_string(), "0".to_string()])
            .expect_err("zero vm fuel should fail");
        assert!(err.contains("--vm-fuel must be > 0"));
    }

    #[test]
    fn parse_cli_args_from_rejects_conflicting_vm_interrupt_flags() {
        let err = parse_cli_args_from([
            "--vm-fuel".to_string(),
            "32".to_string(),
            "--vm-epoch-deadline".to_string(),
            "2".to_string(),
        ])
        .expect_err("conflicting vm interrupt flags should fail");
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn parse_cli_args_from_rejects_missing_value() {
        let err = parse_cli_args_from(["--admin-addr".to_string()])
            .expect_err("missing value should fail");
        assert!(err.contains("missing value for --admin-addr"));
    }

    #[test]
    fn parse_cli_args_from_rejects_unknown_argument() {
        let err = parse_cli_args_from(["--nope".to_string()]).expect_err("unknown should fail");
        assert!(err.contains("unknown argument: --nope"));
    }

    #[test]
    fn resolve_edge_id_explicit_value_is_persisted() {
        let dir = temp_test_dir("pd-edge-http-proxy-explicit-id");
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
        let dir = temp_test_dir("pd-edge-http-proxy-invalid-id");
        let path = dir.join("edge-id");
        fs::write(&path, "not-a-uuid\n").expect("seed invalid id");

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
    fn resolve_edge_id_empty_file_is_rejected() {
        let dir = temp_test_dir("pd-edge-http-proxy-empty-id");
        let path = dir.join("edge-id");
        fs::write(&path, "  \n").expect("seed empty id");

        let err = resolve_edge_id(None, path.as_path()).expect_err("empty file should fail");
        let text = err.to_string();
        assert!(text.contains("is empty"));
        let _ = fs::remove_dir_all(dir);
    }
}
