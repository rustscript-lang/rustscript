use std::{
    collections::BTreeMap,
    env, fs, io,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(all(feature = "http2", feature = "tls"))]
use axum::body::Bytes;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderValue, Response},
    routing::any,
};
use edge::HOST_FUNCTION_COUNT;
#[cfg(all(feature = "http2", feature = "tls"))]
use http_body_util::{BodyExt, Full};
#[cfg(all(feature = "http2", feature = "tls"))]
use hyper::{
    Request as HyperRequest, Response as HyperResponse, body::Incoming, service::service_fn,
};
#[cfg(all(feature = "http2", feature = "tls"))]
use hyper_util::rt::TokioIo;
#[cfg(all(feature = "http2", feature = "tls"))]
use rcgen::generate_simple_self_signed;
use reqwest::{Client, StatusCode, Version as ReqwestVersion};
use serde::{Deserialize, Serialize};
use tokio::{
    task::{JoinHandle, JoinSet},
    time::sleep,
};
#[cfg(all(feature = "http2", feature = "tls"))]
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
use vm::{compile_source, encode_program, validate_program};

const LOAD_REQUEST_BODY: &str = "edge-perf-payload";
const BENCH_TLS_SESSION_REUSE_ENTRIES: usize = 128;
const BENCH_UPSTREAM_HTTP_REUSE_ENTRIES: usize = 128;
const BENCH_DOWNSTREAM_HTTP2_SESSION_ENTRIES: usize = 128;
const HTTP2_TLS_FEATURE_HINT: &str =
    "HTTP/2 benchmark scenarios require running the example with --features http2,tls";

#[cfg(all(feature = "http2", feature = "tls"))]
fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

const BASE_WORKLOAD_SOURCE: &str = r#"
let mut outer = 0;
let mut acc = 1;
while outer < 12 {
    let mut inner = 0;
    while inner < 24 {
        let mixed = (outer * 31 + inner * 17) % 97;
        if (mixed % 2) == 0 {
            acc = acc + mixed;
        } else {
            acc = acc - mixed;
        }
        inner = inner + 1;
    }
    outer = outer + 1;
}
"#;

fn no_host_calls_program_source() -> String {
    format!(
        r#"
{BASE_WORKLOAD_SOURCE}
let parity = acc % 2;
if parity == 0 {{
    acc;
}} else {{
    acc + 1;
}}
"#
    )
}

fn host_calls_terminate_program_source() -> String {
    format!(
        r#"
{BASE_WORKLOAD_SOURCE}

use http;

let method = http::request::get_method();
let path = http::request::get_path();
let client_id = http::request::get_header("x-client-id");
let body = http::request::get_body();

if (acc % 2) == 0 {{
    http::response::set_header("x-perf-acc", "even");
}} else {{
    http::response::set_header("x-perf-acc", "odd");
}}
    http::response::set_header("x-perf-method", method);
    http::response::set_header("x-perf-path", path);
    http::response::set_header("x-perf-client-id", client_id);
    http::response::set_body("host-calls-terminate|" + body);
"#
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpstreamProtocol {
    Http1,
    HttpsHttp2,
}

impl UpstreamProtocol {
    fn label(self) -> &'static str {
        match self {
            Self::Http1 => "1.1",
            Self::HttpsHttp2 => "2",
        }
    }

    fn requires_http2_tls(self) -> bool {
        matches!(self, Self::HttpsHttp2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownstreamProtocol {
    Http1,
    HttpsHttp2,
}

impl DownstreamProtocol {
    fn label(self) -> &'static str {
        match self {
            Self::Http1 => "1.1",
            Self::HttpsHttp2 => "2",
        }
    }

    fn requires_http2_tls(self) -> bool {
        matches!(self, Self::HttpsHttp2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyProgramFlavor {
    HeaderTransform,
}

fn proxy_roundtrip_program_source(
    upstream_origin: &str,
    upstream_protocol: UpstreamProtocol,
    flavor: ProxyProgramFlavor,
) -> String {
    let tls_import = if upstream_protocol.requires_http2_tls() {
        "use tls;\n"
    } else {
        ""
    };
    let tls_prelude = if upstream_protocol.requires_http2_tls() {
        r#"
let session = tls::session::from_socket(upstream);
tls::session::set_verify(session, false);
tls::session::set_alpn(session, "h2,http/1.1");
http::exchange::set_version(upstream, "2");
"#
    } else {
        ""
    };
    let tls_response_headers = if upstream_protocol.requires_http2_tls() {
        r#"
http::response::set_header("x-upstream-alpn", tls::session::get_alpn(session));
"#
    } else {
        ""
    };
    let extra_upstream_header = match flavor {
        ProxyProgramFlavor::HeaderTransform => {
            r#"http::exchange::set_header(upstream, "x-bench-program-header", "program-proxy");"#
                .to_string()
        }
    };
    let extra_response_headers = match flavor {
        ProxyProgramFlavor::HeaderTransform => r#"
http::response::set_header("x-bench-response-header", "program-proxy");
http::response::set_header("x-upstream-program-header", echoed_program_header);
"#
        .to_string(),
    };
    format!(
        r#"
{BASE_WORKLOAD_SOURCE}

use http;
{tls_import}

let method = http::request::get_method();
let path = http::request::get_path();
let client_id = http::request::get_header("x-client-id");
let downstream_version = http::request::get_http_version();
let body = http::request::get_body();

let upstream = http::exchange::default_upstream();
http::exchange::set_target(upstream, "{upstream_origin}");
http::exchange::set_method(upstream, method);
http::exchange::set_path(upstream, path);
http::exchange::set_header(upstream, "x-client-id", client_id);
http::exchange::set_header(upstream, "x-downstream-version", downstream_version);
{extra_upstream_header}
http::exchange::set_body(upstream, body);
{tls_prelude}

let upstream_status = http::exchange::get_status(upstream);
let upstream_version = http::exchange::get_http_version(upstream);
let echoed_client_id = http::exchange::get_header(upstream, "x-bench-upstream-client-id");
let echoed_path = http::exchange::get_header(upstream, "x-bench-upstream-path");
let echoed_program_header = http::exchange::get_header(upstream, "x-bench-upstream-program-header");
let upstream_body = http::exchange::get_body(upstream);

http::response::set_status(upstream_status);
http::response::set_header("x-downstream-version", downstream_version);
http::response::set_header("x-perf-client-id", client_id);
http::response::set_header("x-upstream-version", upstream_version);
http::response::set_header("x-upstream-client-id", echoed_client_id);
http::response::set_header("x-upstream-path", echoed_path);
{extra_response_headers}
{tls_response_headers}
http::response::set_body(upstream_body);
"#
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProgramVariant {
    None,
    NoHostCallsBase,
    HostCallsAdditive,
    DirectUpstream {
        upstream: UpstreamProtocol,
    },
    ProxyRoundTrip {
        flavor: ProxyProgramFlavor,
        upstream: UpstreamProtocol,
    },
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    id: &'static str,
    description: &'static str,
    expected_status: u16,
    program_variant: ProgramVariant,
    downstream_protocol: DownstreamProtocol,
}

impl Scenario {
    fn upstream_protocol(self) -> Option<UpstreamProtocol> {
        match self.program_variant {
            ProgramVariant::DirectUpstream { upstream } => Some(upstream),
            ProgramVariant::ProxyRoundTrip { upstream, .. } => Some(upstream),
            _ => None,
        }
    }

    fn uses_proxy(self) -> bool {
        !matches!(self.program_variant, ProgramVariant::DirectUpstream { .. })
    }

    fn requires_http2_tls(self) -> bool {
        self.downstream_protocol.requires_http2_tls()
            || self
                .upstream_protocol()
                .is_some_and(UpstreamProtocol::requires_http2_tls)
    }

    fn supports_current_build(self) -> bool {
        !self.requires_http2_tls() || cfg!(all(feature = "http2", feature = "tls"))
    }

    fn expects_header_transform(self) -> bool {
        matches!(
            self.program_variant,
            ProgramVariant::ProxyRoundTrip {
                flavor: ProxyProgramFlavor::HeaderTransform,
                ..
            }
        )
    }
}

const SCENARIOS: [Scenario; 8] = [
    Scenario {
        id: "raw_no_program",
        description: "raw pd-edge-http-proxy (no program loaded)",
        expected_status: 404,
        program_variant: ProgramVariant::None,
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "no_host_calls_program",
        description: "pd-edge-http-proxy with no host calls (compute-only program)",
        expected_status: 404,
        program_variant: ProgramVariant::NoHostCallsBase,
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "host_calls_terminate",
        description: "pd-edge-http-proxy with additive host calls and terminate (no upstream)",
        expected_status: 200,
        program_variant: ProgramVariant::HostCallsAdditive,
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "raw_http_upstream",
        description: "perf client hits hardcoded plaintext HTTP upstream directly",
        expected_status: 200,
        program_variant: ProgramVariant::DirectUpstream {
            upstream: UpstreamProtocol::Http1,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "host_calls_upstream_roundtrip",
        description: "pd-edge-http-proxy with program proxying to hardcoded plaintext HTTP upstream and request/response header mutations",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::Http1,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "raw_http2_upstream",
        description: "perf client hits hardcoded HTTPS HTTP/2 upstream directly",
        expected_status: 200,
        program_variant: ProgramVariant::DirectUpstream {
            upstream: UpstreamProtocol::HttpsHttp2,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp2,
    },
    Scenario {
        id: "host_calls_upstream_roundtrip_http2_upstream",
        description: "pd-edge-http-proxy with program proxying to hardcoded HTTPS HTTP/2 upstream while downstream stays plaintext HTTP/1.1",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::HttpsHttp2,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "host_calls_upstream_roundtrip_downstream_http2",
        description: "pd-edge-http-proxy with program proxying to hardcoded plaintext HTTP upstream while downstream uses HTTPS HTTP/2",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::Http1,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp2,
    },
];

struct UpstreamFixture {
    origin: String,
    connection_count: Option<Arc<AtomicUsize>>,
    tasks: Vec<JoinHandle<()>>,
}

impl UpstreamFixture {
    fn origin(&self) -> &str {
        &self.origin
    }

    fn connection_count(&self) -> Option<usize> {
        self.connection_count
            .as_ref()
            .map(|value| value.load(Ordering::Relaxed))
    }
}

impl Drop for UpstreamFixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Debug, Clone)]
struct BenchConfig {
    requests: usize,
    warmup_requests: usize,
    concurrency: usize,
    request_timeout_ms: u64,
    startup_timeout_ms: u64,
    memory_sample_interval_ms: u64,
    vm_fuel: Option<u64>,
    vm_fuel_check_interval: u32,
    vm_execution_mode: VmExecutionModeArg,
    fuel_latency_sweep: bool,
    fuel_latency_fuels: Vec<u64>,
    fuel_latency_check_intervals: Vec<u32>,
    binary_path: Option<PathBuf>,
    auto_build: bool,
    release_build: bool,
    json_out: Option<PathBuf>,
    scenario: Option<String>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            requests: 10_000,
            warmup_requests: 1_000,
            concurrency: 64,
            request_timeout_ms: 10_000,
            startup_timeout_ms: 15_000,
            memory_sample_interval_ms: 100,
            vm_fuel: None,
            vm_fuel_check_interval: 32,
            vm_execution_mode: VmExecutionModeArg::Async,
            fuel_latency_sweep: false,
            fuel_latency_fuels: vec![
                1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192, 16_384, 32_768,
                50_000,
            ],
            fuel_latency_check_intervals: vec![1, 2, 4, 8, 16, 32, 64, 128],
            binary_path: None,
            auto_build: true,
            release_build: true,
            json_out: None,
            scenario: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum VmExecutionModeArg {
    Async,
    Threading,
}

impl VmExecutionModeArg {
    fn as_flag_value(self) -> &'static str {
        match self {
            VmExecutionModeArg::Async => "async",
            VmExecutionModeArg::Threading => "threading",
        }
    }
}

fn parse_vm_execution_mode_arg(value: &str) -> Result<VmExecutionModeArg, String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "async" => Ok(VmExecutionModeArg::Async),
        "threading" | "blocking" | "spawn-blocking" | "spawn_blocking" => {
            Ok(VmExecutionModeArg::Threading)
        }
        _ => Err(format!(
            "invalid --vm-execution-mode: {value} (expected async|threading)"
        )),
    }
}

#[derive(Debug, Serialize)]
struct BenchConfigReport {
    requests: usize,
    warmup_requests: usize,
    concurrency: usize,
    request_timeout_ms: u64,
    startup_timeout_ms: u64,
    memory_sample_interval_ms: u64,
    vm_fuel: Option<u64>,
    vm_fuel_check_interval: u32,
    vm_execution_mode: VmExecutionModeArg,
    fuel_latency_sweep: bool,
    fuel_latency_fuels: Vec<u64>,
    fuel_latency_check_intervals: Vec<u32>,
    binary_path: String,
    auto_build: bool,
    release_build: bool,
    scenario: Option<String>,
}

#[derive(Debug, Serialize)]
struct BenchReport {
    generated_at_unix_ms: u128,
    config: BenchConfigReport,
    scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Serialize)]
struct FuelLatencySweepReport {
    generated_at_unix_ms: u128,
    config: BenchConfigReport,
    fuel_sweep_cases: Vec<FuelSweepCaseReport>,
    fuel_check_interval_sweep_cases: Vec<FuelSweepCaseReport>,
}

#[derive(Debug, Serialize)]
struct FuelSweepCaseReport {
    vm_fuel: Option<u64>,
    vm_fuel_check_interval: u32,
    scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Serialize)]
struct ScenarioReport {
    id: String,
    description: String,
    expected_status: u16,
    requests_sent: usize,
    responses_received: usize,
    request_errors: usize,
    unexpected_status_responses: usize,
    throughput_rps: f64,
    status_counts: Vec<StatusCount>,
    latency_ms: Option<LatencyStats>,
    memory: MemoryStats,
    telemetry: Option<TelemetrySummary>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatusCount {
    status: u16,
    count: usize,
}

#[derive(Debug, Serialize)]
struct LatencyStats {
    min: f64,
    mean: f64,
    median: f64,
    p90: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

#[derive(Debug, Serialize)]
struct MemoryStats {
    samples: usize,
    start_rss_mib: Option<f64>,
    end_rss_mib: Option<f64>,
    min_rss_mib: Option<f64>,
    avg_rss_mib: Option<f64>,
    max_rss_mib: Option<f64>,
    peak_rss_mib: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TelemetrySummary {
    program_loaded: bool,
    data_requests_total: u64,
    vm_execution_errors_total: u64,
}

#[derive(Clone)]
struct BenchClients {
    http: Client,
    https: Client,
}

#[derive(Debug)]
struct ProbeResponse {
    status: u16,
    version: ReqwestVersion,
    headers: reqwest::header::HeaderMap,
    body: String,
}

#[derive(Default)]
struct WorkerRun {
    latencies_us: Vec<u64>,
    status_counts: BTreeMap<u16, usize>,
    request_errors: usize,
}

struct LoadRunResult {
    elapsed: Duration,
    latencies_us: Vec<u64>,
    status_counts: BTreeMap<u16, usize>,
    request_errors: usize,
}

struct ProxyProcess {
    child: Child,
}

impl ProxyProcess {
    fn spawn(
        binary_path: &Path,
        data_addr: SocketAddr,
        https_addr: Option<SocketAddr>,
        admin_addr: SocketAddr,
        vm_fuel: Option<u64>,
        vm_fuel_check_interval: u32,
        vm_execution_mode: VmExecutionModeArg,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let rust_log = env::var("PD_EDGE_PROXY_RUST_LOG").unwrap_or_else(|_| "error".to_string());
        let inherit_stdout = env_flag("PD_EDGE_PROXY_STDOUT_INHERIT");
        let mut command = Command::new(binary_path);
        let stdout = if inherit_stdout {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        command
            .arg("--data-addr")
            .arg(data_addr.to_string())
            .arg("--tls-session-reuse-entries")
            .arg(BENCH_TLS_SESSION_REUSE_ENTRIES.to_string())
            .arg("--upstream-http-reuse-entries")
            .arg(BENCH_UPSTREAM_HTTP_REUSE_ENTRIES.to_string())
            .arg("--downstream-http2-session-entries")
            .arg(BENCH_DOWNSTREAM_HTTP2_SESSION_ENTRIES.to_string())
            .arg("--admin-addr")
            .arg(admin_addr.to_string())
            .arg("--vm-execution-mode")
            .arg(vm_execution_mode.as_flag_value())
            .env("RUST_LOG", rust_log)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::inherit());
        if let Some(https_addr) = https_addr {
            command.arg("--https-addr").arg(https_addr.to_string());
        }
        if let Some(fuel) = vm_fuel {
            command.arg("--vm-fuel").arg(fuel.to_string());
            command
                .arg("--vm-fuel-check-interval")
                .arg(vm_fuel_check_interval.to_string());
        }
        if let Ok(value) = env::var("PD_EDGE_PROFILE_VM_TAIL") {
            command.env("PD_EDGE_PROFILE_VM_TAIL", value);
        }
        if let Ok(value) = env::var("PD_EDGE_PROFILE_VM_TAIL_THRESHOLD_US") {
            command.env("PD_EDGE_PROFILE_VM_TAIL_THRESHOLD_US", value);
        }
        let child = command.spawn()?;
        Ok(Self { child })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, io::Error> {
        self.child.try_wait()
    }
}

fn build_bench_client(
    config: &BenchConfig,
    accept_invalid_certs: bool,
) -> Result<Client, Box<dyn std::error::Error>> {
    let mut builder = Client::builder()
        .pool_max_idle_per_host(config.concurrency.max(1))
        .tcp_nodelay(true)
        .timeout(Duration::from_millis(config.request_timeout_ms));
    if accept_invalid_certs {
        builder = builder.danger_accept_invalid_certs(true);
    }
    #[cfg(feature = "http2")]
    {
        builder = builder.http2_adaptive_window(true);
    }
    Ok(builder.build()?)
}

fn request_client_for_scenario(clients: &BenchClients, scenario: Scenario) -> &Client {
    match scenario.downstream_protocol {
        DownstreamProtocol::Http1 => &clients.http,
        DownstreamProtocol::HttpsHttp2 => &clients.https,
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match parse_args() {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            print_help();
            return Err(io::Error::other("invalid arguments").into());
        }
    };

    let selected_scenarios = if config.fuel_latency_sweep {
        select_sweep_scenarios(&config)?
    } else {
        select_scenarios(&config)?
    };
    let required_features = required_proxy_build_features(&selected_scenarios);
    let binary_path = resolve_proxy_binary_path(&config, &required_features)?;
    println!("proxy binary: {}", binary_path.display());
    println!(
        "requests={}, warmup_requests={}, concurrency={}, request_timeout_ms={}, memory_sample_interval_ms={}, vm_execution_mode={}, vm_fuel={:?}, vm_fuel_check_interval={}, fuel_latency_sweep={}, fuel_latency_fuels={:?}, fuel_latency_check_intervals={:?}",
        config.requests,
        config.warmup_requests,
        config.concurrency,
        config.request_timeout_ms,
        config.memory_sample_interval_ms,
        config.vm_execution_mode.as_flag_value(),
        config.vm_fuel,
        config.vm_fuel_check_interval,
        config.fuel_latency_sweep,
        config.fuel_latency_fuels,
        config.fuel_latency_check_intervals
    );

    let clients = BenchClients {
        http: build_bench_client(&config, false)?,
        https: build_bench_client(&config, true)?,
    };

    if config.fuel_latency_sweep {
        run_fuel_latency_sweep(&config, &binary_path, &clients, &selected_scenarios).await?;
    } else {
        run_standard_bench(&config, &binary_path, &clients, &selected_scenarios).await?;
    }

    Ok(())
}

async fn run_standard_bench(
    config: &BenchConfig,
    binary_path: &Path,
    clients: &BenchClients,
    scenarios: &[Scenario],
) -> Result<(), Box<dyn std::error::Error>> {
    let reports = run_case_scenarios(config, binary_path, clients, scenarios, None).await;
    let report = BenchReport {
        generated_at_unix_ms: generated_at_unix_ms(),
        config: build_bench_config_report(config, binary_path),
        scenarios: reports,
    };
    if let Some(path) = &config.json_out {
        write_json_report(path, &report)?;
    }
    Ok(())
}

async fn run_fuel_latency_sweep(
    config: &BenchConfig,
    binary_path: &Path,
    clients: &BenchClients,
    scenarios: &[Scenario],
) -> Result<(), Box<dyn std::error::Error>> {
    let fixed_interval = 1_u32;
    let fixed_fuel = config.vm_fuel.unwrap_or(50_000);
    println!(
        "fuel latency sweep mode: scenarios={:?}, fixed_interval_for_fuel_sweep={}, fixed_fuel_for_interval_sweep={}",
        scenarios.iter().map(|item| item.id).collect::<Vec<_>>(),
        fixed_interval,
        fixed_fuel
    );

    let mut fuel_sweep_cases = Vec::new();
    for fuel in &config.fuel_latency_fuels {
        let mut case_config = config.clone();
        case_config.vm_fuel = Some(*fuel);
        case_config.vm_fuel_check_interval = fixed_interval;
        let label = format!(
            "fuel sweep case vm_fuel={} vm_fuel_check_interval={fixed_interval}",
            fuel
        );
        let reports =
            run_case_scenarios(&case_config, binary_path, clients, scenarios, Some(&label)).await;
        fuel_sweep_cases.push(FuelSweepCaseReport {
            vm_fuel: Some(*fuel),
            vm_fuel_check_interval: fixed_interval,
            scenarios: reports,
        });
    }

    let mut fuel_check_interval_sweep_cases = Vec::new();
    for interval in &config.fuel_latency_check_intervals {
        let mut case_config = config.clone();
        case_config.vm_fuel = Some(fixed_fuel);
        case_config.vm_fuel_check_interval = *interval;
        let label =
            format!("interval sweep case vm_fuel={fixed_fuel} vm_fuel_check_interval={interval}");
        let reports =
            run_case_scenarios(&case_config, binary_path, clients, scenarios, Some(&label)).await;
        fuel_check_interval_sweep_cases.push(FuelSweepCaseReport {
            vm_fuel: Some(fixed_fuel),
            vm_fuel_check_interval: *interval,
            scenarios: reports,
        });
    }

    print_sweep_summary("fuel sweep", &fuel_sweep_cases);
    print_sweep_summary(
        "fuel check interval sweep",
        &fuel_check_interval_sweep_cases,
    );

    let report = FuelLatencySweepReport {
        generated_at_unix_ms: generated_at_unix_ms(),
        config: build_bench_config_report(config, binary_path),
        fuel_sweep_cases,
        fuel_check_interval_sweep_cases,
    };
    if let Some(path) = &config.json_out {
        write_json_report(path, &report)?;
    }
    Ok(())
}

async fn run_case_scenarios(
    config: &BenchConfig,
    binary_path: &Path,
    clients: &BenchClients,
    scenarios: &[Scenario],
    case_label: Option<&str>,
) -> Vec<ScenarioReport> {
    let mut reports = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        println!();
        if let Some(label) = case_label {
            println!("=== {label} | {} ===", scenario.description);
        } else {
            println!("=== {} ===", scenario.description);
        }
        let report = match run_scenario(config, binary_path, clients, *scenario).await {
            Ok(report) => report,
            Err(err) => scenario_error_report(*scenario, err.to_string()),
        };
        print_scenario_report(&report);
        reports.push(report);
    }
    reports
}

fn select_scenarios(config: &BenchConfig) -> Result<Vec<Scenario>, Box<dyn std::error::Error>> {
    if let Some(filter) = &config.scenario {
        let matched = SCENARIOS
            .iter()
            .copied()
            .find(|scenario| scenario.id == filter)
            .ok_or_else(|| io::Error::other(format!("unknown --scenario: {filter}")))?;
        if !matched.supports_current_build() {
            return Err(io::Error::other(format!(
                "scenario '{}' requires HTTP/2 + TLS support; {}",
                matched.id, HTTP2_TLS_FEATURE_HINT
            ))
            .into());
        }
        Ok(vec![matched])
    } else {
        let mut selected = Vec::new();
        let mut skipped = Vec::new();
        for scenario in SCENARIOS {
            if scenario.supports_current_build() {
                selected.push(scenario);
            } else {
                skipped.push(scenario.id);
            }
        }
        if !skipped.is_empty() {
            println!(
                "skipping scenarios requiring HTTP/2 + TLS support: {}",
                skipped.join(", ")
            );
        }
        Ok(selected)
    }
}

fn select_sweep_scenarios(
    config: &BenchConfig,
) -> Result<Vec<Scenario>, Box<dyn std::error::Error>> {
    if config.scenario.is_some() {
        return select_scenarios(config);
    }
    let scenario = SCENARIOS
        .iter()
        .copied()
        .find(|item| item.id == "no_host_calls_program")
        .ok_or_else(|| io::Error::other("missing no_host_calls_program scenario"))?;
    Ok(vec![scenario])
}

fn scenario_error_report(scenario: Scenario, error: String) -> ScenarioReport {
    ScenarioReport {
        id: scenario.id.to_string(),
        description: scenario.description.to_string(),
        expected_status: scenario.expected_status,
        requests_sent: 0,
        responses_received: 0,
        request_errors: 0,
        unexpected_status_responses: 0,
        throughput_rps: 0.0,
        status_counts: Vec::new(),
        latency_ms: None,
        memory: MemoryStats {
            samples: 0,
            start_rss_mib: None,
            end_rss_mib: None,
            min_rss_mib: None,
            avg_rss_mib: None,
            max_rss_mib: None,
            peak_rss_mib: None,
        },
        telemetry: None,
        error: Some(error),
    }
}

fn required_proxy_build_features(scenarios: &[Scenario]) -> Vec<&'static str> {
    if scenarios
        .iter()
        .any(|scenario| scenario.requires_http2_tls())
    {
        vec!["http2", "tls"]
    } else {
        Vec::new()
    }
}

fn print_sweep_summary(title: &str, cases: &[FuelSweepCaseReport]) {
    if cases.is_empty() {
        return;
    }
    println!();
    println!("=== {title} summary ===");

    let baseline = &cases[0];
    let mut baseline_latency_by_scenario = BTreeMap::<String, (f64, f64, f64, f64)>::new();
    for scenario in &baseline.scenarios {
        if let Some(latency) = &scenario.latency_ms {
            baseline_latency_by_scenario.insert(
                scenario.id.clone(),
                (
                    latency.median,
                    latency.p95,
                    latency.p99,
                    scenario.throughput_rps,
                ),
            );
        }
    }

    for case in cases {
        for scenario in &case.scenarios {
            if let Some(latency) = &scenario.latency_ms {
                let baseline = baseline_latency_by_scenario.get(&scenario.id).copied();
                let median_delta = baseline
                    .and_then(|(median, _, _, _)| percent_delta(latency.median, median))
                    .map(|value| format!("{value:+.2}%"))
                    .unwrap_or_else(|| "n/a".to_string());
                let p99_delta = baseline
                    .and_then(|(_, _, p99, _)| percent_delta(latency.p99, p99))
                    .map(|value| format!("{value:+.2}%"))
                    .unwrap_or_else(|| "n/a".to_string());
                let throughput_delta = baseline
                    .and_then(|(_, _, _, throughput)| {
                        percent_delta(scenario.throughput_rps, throughput)
                    })
                    .map(|value| format!("{value:+.2}%"))
                    .unwrap_or_else(|| "n/a".to_string());
                println!(
                    "scenario={} vm_fuel={:?} vm_fuel_check_interval={} median_ms={:.3} p95_ms={:.3} p99_ms={:.3} throughput_rps={:.2} delta_median={} delta_p99={} delta_throughput={}",
                    scenario.id,
                    case.vm_fuel,
                    case.vm_fuel_check_interval,
                    latency.median,
                    latency.p95,
                    latency.p99,
                    scenario.throughput_rps,
                    median_delta,
                    p99_delta,
                    throughput_delta
                );
            } else {
                println!(
                    "scenario={} vm_fuel={:?} vm_fuel_check_interval={} latency=no_samples throughput_rps={:.2}",
                    scenario.id, case.vm_fuel, case.vm_fuel_check_interval, scenario.throughput_rps
                );
            }
        }
    }
}

fn percent_delta(current: f64, baseline: f64) -> Option<f64> {
    if baseline.abs() < f64::MIN_POSITIVE {
        return None;
    }
    Some(((current - baseline) / baseline) * 100.0)
}

fn generated_at_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0)
}

fn build_bench_config_report(config: &BenchConfig, binary_path: &Path) -> BenchConfigReport {
    BenchConfigReport {
        requests: config.requests,
        warmup_requests: config.warmup_requests,
        concurrency: config.concurrency,
        request_timeout_ms: config.request_timeout_ms,
        startup_timeout_ms: config.startup_timeout_ms,
        memory_sample_interval_ms: config.memory_sample_interval_ms,
        vm_fuel: config.vm_fuel,
        vm_fuel_check_interval: config.vm_fuel_check_interval,
        vm_execution_mode: config.vm_execution_mode,
        fuel_latency_sweep: config.fuel_latency_sweep,
        fuel_latency_fuels: config.fuel_latency_fuels.clone(),
        fuel_latency_check_intervals: config.fuel_latency_check_intervals.clone(),
        binary_path: binary_path.display().to_string(),
        auto_build: config.auto_build,
        release_build: config.release_build,
        scenario: config.scenario.clone(),
    }
}

fn write_json_report<T: Serialize>(
    path: &Path,
    report: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(report)?;
    fs::write(path, json)?;
    println!();
    println!("json report written to {}", path.display());
    Ok(())
}

async fn run_scenario(
    config: &BenchConfig,
    binary_path: &Path,
    clients: &BenchClients,
    scenario: Scenario,
) -> Result<ScenarioReport, Box<dyn std::error::Error>> {
    let upstream_fixture = match scenario.upstream_protocol() {
        Some(UpstreamProtocol::Http1) => Some(spawn_plain_http_upstream_fixture().await?),
        Some(UpstreamProtocol::HttpsHttp2) => Some(spawn_https_http2_upstream_fixture().await?),
        None => None,
    };
    let program_bytes = match scenario.program_variant {
        ProgramVariant::None => None,
        ProgramVariant::NoHostCallsBase => {
            let source = no_host_calls_program_source();
            Some(compile_program_to_vmbc(&source)?)
        }
        ProgramVariant::HostCallsAdditive => {
            let source = host_calls_terminate_program_source();
            Some(compile_program_to_vmbc(&source)?)
        }
        ProgramVariant::DirectUpstream { .. } => None,
        ProgramVariant::ProxyRoundTrip { flavor, upstream } => {
            let upstream_origin = upstream_fixture
                .as_ref()
                .expect("upstream fixture should exist")
                .origin();
            let source = match flavor {
                ProxyProgramFlavor::HeaderTransform => {
                    proxy_roundtrip_program_source(upstream_origin, upstream, flavor)
                }
            };
            Some(compile_program_to_vmbc(&source)?)
        }
    };

    let mut proxy = if scenario.uses_proxy() {
        let data_addr = reserve_loopback_addr()?;
        let https_addr = if scenario.downstream_protocol.requires_http2_tls() {
            Some(reserve_loopback_addr()?)
        } else {
            None
        };
        let admin_addr = reserve_loopback_addr()?;
        let mut proxy = ProxyProcess::spawn(
            binary_path,
            data_addr,
            https_addr,
            admin_addr,
            config.vm_fuel,
            config.vm_fuel_check_interval,
            config.vm_execution_mode,
        )?;

        wait_until_proxy_ready(
            &clients.http,
            admin_addr,
            Duration::from_millis(config.startup_timeout_ms),
            &mut proxy,
        )
        .await?;

        if let Some(bytes) = program_bytes {
            upload_program(&clients.http, admin_addr, bytes).await?;
        }

        Some((proxy, data_addr, https_addr, admin_addr))
    } else {
        None
    };

    let request_origin = if let Some((_, data_addr, https_addr, _)) = proxy.as_ref() {
        match scenario.downstream_protocol {
            DownstreamProtocol::Http1 => format!("http://{data_addr}"),
            DownstreamProtocol::HttpsHttp2 => {
                let https_addr = https_addr.expect("https addr should exist");
                format!("https://127.0.0.1:{}", https_addr.port())
            }
        }
    } else {
        upstream_fixture
            .as_ref()
            .expect("direct-upstream scenario should have an upstream fixture")
            .origin()
            .to_string()
    };
    let request_url = format!("{request_origin}/perf");
    let request_client = request_client_for_scenario(clients, scenario);
    verify_scenario_probe(
        request_client,
        &request_origin,
        scenario,
        upstream_fixture.as_ref(),
    )
    .await?;

    if config.warmup_requests > 0 {
        let warmup = run_load(
            request_client,
            &request_url,
            config.warmup_requests,
            config.concurrency,
            false,
        )
        .await?;
        if warmup.request_errors > 0 {
            println!(
                "warmup had {} request errors (continuing)",
                warmup.request_errors
            );
        }
    }

    let pid = proxy.as_mut().map(|(proxy, _, _, _)| proxy.pid());
    let start_rss_kib = pid.and_then(read_process_rss_kib);
    let stop_sampler = pid.map(|_| Arc::new(AtomicBool::new(false)));
    let rss_samples = pid.map(|_| Arc::new(Mutex::new(Vec::<u64>::new())));
    let sampler_handle = match (pid, stop_sampler.as_ref(), rss_samples.as_ref()) {
        (Some(pid), Some(stop_sampler), Some(rss_samples)) => Some(spawn_memory_sampler(
            pid,
            Duration::from_millis(config.memory_sample_interval_ms.max(10)),
            stop_sampler.clone(),
            rss_samples.clone(),
        )),
        _ => None,
    };

    let run = run_load(
        request_client,
        &request_url,
        config.requests,
        config.concurrency,
        true,
    )
    .await?;
    if let Some(stop_sampler) = stop_sampler.as_ref() {
        stop_sampler.store(true, Ordering::Relaxed);
    }
    if let Some(sampler_handle) = sampler_handle {
        let _ = sampler_handle.await;
    }
    let end_rss_kib = pid.and_then(read_process_rss_kib);

    let memory = if let Some(rss_samples) = rss_samples {
        let samples = {
            let guard = rss_samples
                .lock()
                .map_err(|_| io::Error::other("memory sample lock poisoned"))?;
            guard.clone()
        };
        build_memory_stats(samples, start_rss_kib, end_rss_kib)
    } else {
        empty_memory_stats()
    };
    let telemetry = if let Some((_, _, _, admin_addr)) = proxy.as_ref() {
        fetch_telemetry_summary(&clients.http, *admin_addr).await
    } else {
        None
    };

    let responses_received: usize = run.status_counts.values().copied().sum();
    let unexpected_status_responses = run
        .status_counts
        .iter()
        .filter(|(status, _)| **status != scenario.expected_status)
        .map(|(_, count)| *count)
        .sum();
    let throughput_rps = if run.elapsed.is_zero() {
        0.0
    } else {
        config.requests as f64 / run.elapsed.as_secs_f64()
    };

    Ok(ScenarioReport {
        id: scenario.id.to_string(),
        description: scenario.description.to_string(),
        expected_status: scenario.expected_status,
        requests_sent: config.requests,
        responses_received,
        request_errors: run.request_errors,
        unexpected_status_responses,
        throughput_rps,
        status_counts: run
            .status_counts
            .into_iter()
            .map(|(status, count)| StatusCount { status, count })
            .collect(),
        latency_ms: build_latency_stats(run.latencies_us),
        memory,
        telemetry,
        error: None,
    })
}

fn build_latency_stats(mut latencies_us: Vec<u64>) -> Option<LatencyStats> {
    if latencies_us.is_empty() {
        return None;
    }
    latencies_us.sort_unstable();

    let min = latencies_us[0];
    let max = latencies_us[latencies_us.len() - 1];
    let sum: u128 = latencies_us.iter().map(|value| *value as u128).sum();
    let mean = (sum as f64) / (latencies_us.len() as f64);
    let median = percentile_us(&latencies_us, 50);
    let p90 = percentile_us(&latencies_us, 90);
    let p95 = percentile_us(&latencies_us, 95);
    let p99 = percentile_us(&latencies_us, 99);

    Some(LatencyStats {
        min: us_to_ms(min),
        mean: mean / 1_000.0,
        median: us_to_ms(median),
        p90: us_to_ms(p90),
        p95: us_to_ms(p95),
        p99: us_to_ms(p99),
        max: us_to_ms(max),
    })
}

fn percentile_us(sorted: &[u64], percentile: usize) -> u64 {
    let len = sorted.len();
    if len == 0 {
        return 0;
    }
    let idx = ((len - 1) * percentile) / 100;
    sorted[idx]
}

fn build_memory_stats(
    mut samples_kib: Vec<u64>,
    start_rss_kib: Option<u64>,
    end_rss_kib: Option<u64>,
) -> MemoryStats {
    if let Some(value) = start_rss_kib {
        samples_kib.push(value);
    }
    if let Some(value) = end_rss_kib {
        samples_kib.push(value);
    }

    let (min_rss, avg_rss, max_rss) = if samples_kib.is_empty() {
        (None, None, None)
    } else {
        let min_value = samples_kib.iter().min().copied();
        let max_value = samples_kib.iter().max().copied();
        let sum: u128 = samples_kib.iter().map(|value| *value as u128).sum();
        let avg_value = Some((sum / samples_kib.len() as u128) as u64);
        (min_value, avg_value, max_value)
    };

    MemoryStats {
        samples: samples_kib.len(),
        start_rss_mib: start_rss_kib.map(kib_to_mib),
        end_rss_mib: end_rss_kib.map(kib_to_mib),
        min_rss_mib: min_rss.map(kib_to_mib),
        avg_rss_mib: avg_rss.map(kib_to_mib),
        max_rss_mib: max_rss.map(kib_to_mib),
        peak_rss_mib: max_rss.map(kib_to_mib),
    }
}

fn empty_memory_stats() -> MemoryStats {
    MemoryStats {
        samples: 0,
        start_rss_mib: None,
        end_rss_mib: None,
        min_rss_mib: None,
        avg_rss_mib: None,
        max_rss_mib: None,
        peak_rss_mib: None,
    }
}

fn us_to_ms(value: u64) -> f64 {
    value as f64 / 1_000.0
}

fn kib_to_mib(value: u64) -> f64 {
    value as f64 / 1024.0
}

fn compile_program_to_vmbc(source: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let compiled = compile_source(source).map_err(|err| {
        io::Error::other(format!("failed to compile benchmark program source: {err}"))
    })?;
    validate_program(&compiled.program, HOST_FUNCTION_COUNT)?;
    let bytes = encode_program(&compiled.program)?;
    Ok(bytes)
}

async fn verify_scenario_probe(
    client: &Client,
    request_origin: &str,
    scenario: Scenario,
    upstream_fixture: Option<&UpstreamFixture>,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = send_probe_request(
        client,
        &format!("{request_origin}/perf"),
        "perf-client",
        LOAD_REQUEST_BODY,
    )
    .await?;
    ensure_probe_status(&response, scenario, "baseline probe")?;

    match scenario.program_variant {
        ProgramVariant::None | ProgramVariant::NoHostCallsBase => {}
        ProgramVariant::HostCallsAdditive => {
            if probe_header(&response, "x-perf-client-id") != "perf-client" {
                return Err(io::Error::other(format!(
                    "host-call probe missing expected x-perf-client-id header: got '{}'",
                    probe_header(&response, "x-perf-client-id")
                ))
                .into());
            }
            if !response.body.contains(LOAD_REQUEST_BODY) {
                return Err(io::Error::other(format!(
                    "host-call probe body missing request payload marker '{}': body='{}'",
                    LOAD_REQUEST_BODY, response.body
                ))
                .into());
            }
        }
        ProgramVariant::DirectUpstream { upstream } => {
            verify_direct_upstream_probe_response(
                &response,
                scenario,
                upstream,
                "perf-client",
                "/perf",
            )?;
            if upstream.requires_http2_tls() {
                let fixture = upstream_fixture
                    .ok_or_else(|| io::Error::other("missing HTTP/2 upstream fixture"))?;
                verify_http2_upstream_reuse_probe(client, request_origin, scenario, fixture)
                    .await?;
            }
        }
        ProgramVariant::ProxyRoundTrip { upstream, .. } => {
            verify_proxy_roundtrip_probe_response(&response, scenario, "perf-client", "/perf")?;
            if upstream.requires_http2_tls() {
                let fixture = upstream_fixture
                    .ok_or_else(|| io::Error::other("missing HTTP/2 upstream fixture"))?;
                verify_http2_upstream_reuse_probe(client, request_origin, scenario, fixture)
                    .await?;
            }
        }
    }

    Ok(())
}

async fn send_probe_request(
    client: &Client,
    url: &str,
    client_id: &str,
    body: &str,
) -> Result<ProbeResponse, Box<dyn std::error::Error>> {
    let response = client
        .post(url)
        .header("x-client-id", client_id)
        .header("content-type", "text/plain")
        .body(body.to_string())
        .send()
        .await?;
    let status = response.status().as_u16();
    let version = response.version();
    let headers = response.headers().clone();
    let body = response.text().await.unwrap_or_default();
    Ok(ProbeResponse {
        status,
        version,
        headers,
        body,
    })
}

fn probe_header(response: &ProbeResponse, name: &str) -> String {
    response
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn ensure_probe_status(
    response: &ProbeResponse,
    scenario: Scenario,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if response.status != scenario.expected_status {
        return Err(io::Error::other(format!(
            "{label} status mismatch for {}: expected {}, got {}, body={}",
            scenario.id, scenario.expected_status, response.status, response.body
        ))
        .into());
    }
    Ok(())
}

fn verify_proxy_roundtrip_probe_response(
    response: &ProbeResponse,
    scenario: Scenario,
    expected_client_id: &str,
    expected_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_probe_status(response, scenario, "proxy round-trip probe")?;
    let expected_downstream_version = scenario.downstream_protocol.label();
    let actual_downstream_version = probe_header(response, "x-downstream-version");
    if actual_downstream_version != expected_downstream_version {
        return Err(io::Error::other(format!(
            "probe missing expected downstream version {}: got '{}'",
            expected_downstream_version, actual_downstream_version
        ))
        .into());
    }

    let expected_response_version = match scenario.downstream_protocol {
        DownstreamProtocol::Http1 => ReqwestVersion::HTTP_11,
        DownstreamProtocol::HttpsHttp2 => ReqwestVersion::HTTP_2,
    };
    if response.version != expected_response_version {
        return Err(io::Error::other(format!(
            "downstream response version mismatch: expected {}, got {}",
            downstream_version_label(expected_response_version),
            downstream_version_label(response.version)
        ))
        .into());
    }

    if scenario.expects_header_transform()
        && probe_header(response, "x-bench-response-header") != "program-proxy"
    {
        return Err(io::Error::other(format!(
            "header-transform probe missing expected x-bench-response-header: got '{}'",
            probe_header(response, "x-bench-response-header")
        ))
        .into());
    }
    if scenario.expects_header_transform()
        && probe_header(response, "x-upstream-program-header") != "program-proxy"
    {
        return Err(io::Error::other(format!(
            "header-transform probe missing expected x-upstream-program-header: got '{}'",
            probe_header(response, "x-upstream-program-header")
        ))
        .into());
    }
    if scenario.expects_header_transform()
        && probe_header(response, "x-perf-client-id") != expected_client_id
    {
        return Err(io::Error::other(format!(
            "header-transform probe missing expected x-perf-client-id: got '{}'",
            probe_header(response, "x-perf-client-id")
        ))
        .into());
    }

    let upstream_client_id = probe_header(response, "x-upstream-client-id");
    if upstream_client_id != expected_client_id {
        return Err(io::Error::other(format!(
            "probe missing expected x-upstream-client-id header: got '{}'",
            upstream_client_id
        ))
        .into());
    }

    let upstream_path = probe_header(response, "x-upstream-path");
    if upstream_path != expected_path {
        return Err(io::Error::other(format!(
            "probe missing expected x-upstream-path header: got '{}'",
            upstream_path
        ))
        .into());
    }

    let expected_upstream_version = scenario
        .upstream_protocol()
        .map(UpstreamProtocol::label)
        .unwrap_or_default();
    let upstream_version = probe_header(response, "x-upstream-version");
    if !expected_upstream_version.is_empty() && upstream_version != expected_upstream_version {
        return Err(io::Error::other(format!(
            "probe missing expected x-upstream-version {}: got '{}'",
            expected_upstream_version, upstream_version
        ))
        .into());
    }

    if scenario
        .upstream_protocol()
        .is_some_and(UpstreamProtocol::requires_http2_tls)
        && probe_header(response, "x-upstream-alpn") != "h2"
    {
        return Err(io::Error::other(format!(
            "probe missing expected x-upstream-alpn=h2: got '{}'",
            probe_header(response, "x-upstream-alpn")
        ))
        .into());
    }

    if !response
        .body
        .contains(&format!("upstream-roundtrip|{expected_path}|"))
        || !response.body.contains(LOAD_REQUEST_BODY)
    {
        return Err(io::Error::other(format!(
            "proxy round-trip probe body missing expected markers for path {}: body='{}'",
            expected_path, response.body
        ))
        .into());
    }

    Ok(())
}

fn verify_direct_upstream_probe_response(
    response: &ProbeResponse,
    scenario: Scenario,
    upstream: UpstreamProtocol,
    expected_client_id: &str,
    expected_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_probe_status(response, scenario, "direct-upstream probe")?;

    let expected_response_version = match upstream {
        UpstreamProtocol::Http1 => ReqwestVersion::HTTP_11,
        UpstreamProtocol::HttpsHttp2 => ReqwestVersion::HTTP_2,
    };
    if response.version != expected_response_version {
        return Err(io::Error::other(format!(
            "direct-upstream response version mismatch: expected {}, got {}",
            downstream_version_label(expected_response_version),
            downstream_version_label(response.version)
        ))
        .into());
    }

    let upstream_client_id = probe_header(response, "x-bench-upstream-client-id");
    if upstream_client_id != expected_client_id {
        return Err(io::Error::other(format!(
            "direct-upstream probe missing expected x-bench-upstream-client-id: got '{}'",
            upstream_client_id
        ))
        .into());
    }

    let upstream_path = probe_header(response, "x-bench-upstream-path");
    if upstream_path != expected_path {
        return Err(io::Error::other(format!(
            "direct-upstream probe missing expected x-bench-upstream-path header: got '{}'",
            upstream_path
        ))
        .into());
    }

    let upstream_version = probe_header(response, "x-bench-upstream-http-version");
    if upstream_version != upstream.label() {
        return Err(io::Error::other(format!(
            "direct-upstream probe missing expected x-bench-upstream-http-version {}: got '{}'",
            upstream.label(),
            upstream_version
        ))
        .into());
    }

    if !response
        .body
        .contains(&format!("upstream-roundtrip|{expected_path}|"))
        || !response.body.contains(LOAD_REQUEST_BODY)
    {
        return Err(io::Error::other(format!(
            "direct-upstream probe body missing expected markers for path {}: body='{}'",
            expected_path, response.body
        ))
        .into());
    }

    Ok(())
}

async fn verify_http2_upstream_reuse_probe(
    client: &Client,
    request_origin: &str,
    scenario: Scenario,
    upstream_fixture: &UpstreamFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    let baseline_connections = upstream_fixture
        .connection_count()
        .ok_or_else(|| io::Error::other("missing upstream HTTP/2 connection counter"))?;
    if baseline_connections != 1 {
        return Err(io::Error::other(format!(
            "expected one upstream HTTP/2 TLS connection after baseline probe, got {}",
            baseline_connections
        ))
        .into());
    }

    let slow_url = format!("{request_origin}/slow");
    let fast_url = format!("{request_origin}/fast");
    let (slow, fast) = tokio::try_join!(
        send_probe_request(client, &slow_url, "perf-client-slow", LOAD_REQUEST_BODY),
        send_probe_request(client, &fast_url, "perf-client-fast", LOAD_REQUEST_BODY),
    )?;

    match scenario.program_variant {
        ProgramVariant::DirectUpstream { upstream } => {
            verify_direct_upstream_probe_response(
                &slow,
                scenario,
                upstream,
                "perf-client-slow",
                "/slow",
            )?;
            verify_direct_upstream_probe_response(
                &fast,
                scenario,
                upstream,
                "perf-client-fast",
                "/fast",
            )?;
        }
        ProgramVariant::ProxyRoundTrip { .. } => {
            verify_proxy_roundtrip_probe_response(&slow, scenario, "perf-client-slow", "/slow")?;
            verify_proxy_roundtrip_probe_response(&fast, scenario, "perf-client-fast", "/fast")?;
        }
        _ => {
            return Err(io::Error::other(format!(
                "http2 reuse probe is only valid for upstream round-trip scenarios, got {}",
                scenario.id
            ))
            .into());
        }
    }

    let after_parallel = upstream_fixture
        .connection_count()
        .ok_or_else(|| io::Error::other("missing upstream HTTP/2 connection counter"))?;
    if after_parallel != 1 {
        return Err(io::Error::other(format!(
            "expected HTTP/2 multiplexing over one upstream TLS connection, observed {} connections",
            after_parallel
        ))
        .into());
    }

    let reused = send_probe_request(
        client,
        &format!("{request_origin}/reuse"),
        "perf-client-reuse",
        LOAD_REQUEST_BODY,
    )
    .await?;
    match scenario.program_variant {
        ProgramVariant::DirectUpstream { upstream } => {
            verify_direct_upstream_probe_response(
                &reused,
                scenario,
                upstream,
                "perf-client-reuse",
                "/reuse",
            )?;
        }
        ProgramVariant::ProxyRoundTrip { .. } => {
            verify_proxy_roundtrip_probe_response(&reused, scenario, "perf-client-reuse", "/reuse")?;
        }
        _ => unreachable!("unsupported scenario variant for HTTP/2 upstream reuse probe"),
    }

    let after_reuse = upstream_fixture
        .connection_count()
        .ok_or_else(|| io::Error::other("missing upstream HTTP/2 connection counter"))?;
    if after_reuse != 1 {
        return Err(io::Error::other(format!(
            "expected HTTP/2 connection reuse over one upstream TLS connection, observed {} connections",
            after_reuse
        ))
        .into());
    }

    Ok(())
}

fn downstream_version_label(version: ReqwestVersion) -> &'static str {
    match version {
        ReqwestVersion::HTTP_09 => "0.9",
        ReqwestVersion::HTTP_10 => "1.0",
        ReqwestVersion::HTTP_11 => "1.1",
        ReqwestVersion::HTTP_2 => "2",
        _ => "unknown",
    }
}

async fn spawn_plain_http_upstream_fixture() -> Result<UpstreamFixture, Box<dyn std::error::Error>>
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new().fallback(any(|request: Request<Body>| async move {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().to_string();
        let client_id = parts
            .headers
            .get("x-client-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let program_header = parts
            .headers
            .get("x-bench-program-header")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if path == "/slow" {
            sleep(Duration::from_millis(75)).await;
        }
        let body = to_bytes(body, usize::MAX)
            .await
            .expect("upstream perf server should read body");
        let mut response = Response::new(Body::from(format!(
            "upstream-roundtrip|{path}|{}",
            String::from_utf8_lossy(&body)
        )));
        response.headers_mut().insert(
            "x-bench-upstream-client-id",
            HeaderValue::from_str(&client_id)
                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
        );
        response.headers_mut().insert(
            "x-bench-upstream-path",
            HeaderValue::from_str(&path).unwrap_or_else(|_| HeaderValue::from_static("/invalid")),
        );
        response.headers_mut().insert(
            "x-bench-upstream-program-header",
            HeaderValue::from_str(&program_header)
                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
        );
        response.headers_mut().insert(
            "x-bench-upstream-http-version",
            HeaderValue::from_static("1.1"),
        );
        response
    }));
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("upstream perf server should run");
    });
    Ok(UpstreamFixture {
        origin: format!("http://{addr}"),
        connection_count: None,
        tasks: vec![task],
    })
}

#[cfg(all(feature = "http2", feature = "tls"))]
async fn spawn_https_http2_upstream_fixture() -> Result<UpstreamFixture, Box<dyn std::error::Error>>
{
    ensure_rustls_provider();
    let cert = generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .map_err(|err| io::Error::other(format!("failed to generate benchmark cert: {err}")))?;
    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.serialize_der().map_err(
                |err| io::Error::other(format!("failed to serialize benchmark cert: {err}")),
            )?)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
        )
        .map_err(|err| io::Error::other(format!("failed to build benchmark TLS config: {err}")))?;
    server_config.alpn_protocols = vec![b"h2".to_vec()];

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let connection_count = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn({
        let connection_count = connection_count.clone();
        async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("http2 upstream benchmark accept should succeed");
                connection_count.fetch_add(1, Ordering::Relaxed);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = acceptor
                        .accept(stream)
                        .await
                        .expect("http2 upstream benchmark tls accept should succeed");
                    let service = service_fn(|request: HyperRequest<Incoming>| async move {
                        let (parts, body) = request.into_parts();
                        let path = parts.uri.path().to_string();
                        let client_id = parts
                            .headers
                            .get("x-client-id")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let program_header = parts
                            .headers
                            .get("x-bench-program-header")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        if path == "/slow" {
                            sleep(Duration::from_millis(75)).await;
                        }
                        let body = body
                            .collect()
                            .await
                            .expect("http2 upstream benchmark body should collect")
                            .to_bytes();
                        let mut response = HyperResponse::new(Full::new(Bytes::from(format!(
                            "upstream-roundtrip|{path}|{}",
                            String::from_utf8_lossy(&body)
                        ))));
                        response.headers_mut().insert(
                            "x-bench-upstream-client-id",
                            HeaderValue::from_str(&client_id)
                                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
                        );
                        response.headers_mut().insert(
                            "x-bench-upstream-path",
                            HeaderValue::from_str(&path)
                                .unwrap_or_else(|_| HeaderValue::from_static("/invalid")),
                        );
                        response.headers_mut().insert(
                            "x-bench-upstream-program-header",
                            HeaderValue::from_str(&program_header)
                                .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
                        );
                        response.headers_mut().insert(
                            "x-bench-upstream-http-version",
                            HeaderValue::from_static("2"),
                        );
                        Ok::<_, std::convert::Infallible>(response)
                    });
                    let io = TokioIo::new(tls_stream);
                    let builder = hyper::server::conn::http2::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    );
                    if let Err(err) = builder.serve_connection(io, service).await {
                        eprintln!("http2 upstream benchmark connection ended: {err}");
                    }
                });
            }
        }
    });

    Ok(UpstreamFixture {
        origin: format!("https://127.0.0.1:{}", addr.port()),
        connection_count: Some(connection_count),
        tasks: vec![task],
    })
}

#[cfg(not(all(feature = "http2", feature = "tls")))]
async fn spawn_https_http2_upstream_fixture() -> Result<UpstreamFixture, Box<dyn std::error::Error>>
{
    Err(io::Error::other(HTTP2_TLS_FEATURE_HINT).into())
}

async fn upload_program(
    client: &Client,
    admin_addr: SocketAddr,
    bytes: Vec<u8>,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = client
        .put(format!("http://{admin_addr}/program"))
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await?;
    if response.status() != StatusCode::NO_CONTENT {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(io::Error::other(format!(
            "program upload failed: status={status}, body={body}"
        ))
        .into());
    }
    Ok(())
}

async fn wait_until_proxy_ready(
    client: &Client,
    admin_addr: SocketAddr,
    timeout: Duration,
    proxy: &mut ProxyProcess,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = proxy.try_wait()? {
            return Err(
                io::Error::other(format!("proxy process exited before ready: {status}")).into(),
            );
        }

        if let Ok(response) = client
            .get(format!("http://{admin_addr}/healthz"))
            .send()
            .await
            && response.status().is_success()
        {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for proxy admin endpoint at {admin_addr}"),
            )
            .into());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_memory_sampler(
    pid: u32,
    interval: Duration,
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<u64>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if let Some(rss_kib) = read_process_rss_kib(pid)
                && let Ok(mut guard) = samples.lock()
            {
                guard.push(rss_kib);
            }
            sleep(interval).await;
        }
    })
}

async fn run_load(
    client: &Client,
    url: &str,
    requests: usize,
    concurrency: usize,
    collect_latencies: bool,
) -> Result<LoadRunResult, Box<dyn std::error::Error>> {
    let shared_counter = Arc::new(AtomicUsize::new(0));
    let worker_count = concurrency.max(1);
    let started = Instant::now();
    let mut tasks = JoinSet::new();

    for _ in 0..worker_count {
        let shared_counter = shared_counter.clone();
        let url = url.to_string();
        let client = client.clone();
        tasks.spawn(async move {
            let mut worker = WorkerRun {
                latencies_us: if collect_latencies {
                    Vec::with_capacity(requests / worker_count + 1)
                } else {
                    Vec::new()
                },
                status_counts: BTreeMap::new(),
                request_errors: 0,
            };

            loop {
                let index = shared_counter.fetch_add(1, Ordering::Relaxed);
                if index >= requests {
                    break;
                }

                let request_started = Instant::now();
                match client
                    .post(&url)
                    .header("x-client-id", "perf-client")
                    .header("content-type", "text/plain")
                    .body(LOAD_REQUEST_BODY)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        if response.bytes().await.is_ok() {
                            *worker.status_counts.entry(status).or_insert(0) += 1;
                            if collect_latencies {
                                worker
                                    .latencies_us
                                    .push(request_started.elapsed().as_micros() as u64);
                            }
                        } else {
                            worker.request_errors += 1;
                        }
                    }
                    Err(_) => {
                        worker.request_errors += 1;
                    }
                }
            }

            worker
        });
    }

    let mut merged = WorkerRun::default();
    while let Some(joined) = tasks.join_next().await {
        let worker =
            joined.map_err(|err| io::Error::other(format!("worker task failed: {err}")))?;
        merged.latencies_us.extend(worker.latencies_us);
        merged.request_errors += worker.request_errors;
        for (status, count) in worker.status_counts {
            *merged.status_counts.entry(status).or_insert(0) += count;
        }
    }

    Ok(LoadRunResult {
        elapsed: started.elapsed(),
        latencies_us: merged.latencies_us,
        status_counts: merged.status_counts,
        request_errors: merged.request_errors,
    })
}

async fn fetch_telemetry_summary(
    client: &Client,
    admin_addr: SocketAddr,
) -> Option<TelemetrySummary> {
    let response = client
        .get(format!("http://{admin_addr}/telemetry"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<TelemetrySummary>().await.ok()
}

fn reserve_loopback_addr() -> Result<SocketAddr, io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    Ok(addr)
}

fn read_process_rss_kib(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/status");
        if let Ok(text) = fs::read_to_string(path) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let value = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                    return Some(value);
                }
            }
        }
    }

    let output = Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.split_whitespace().next()?.parse::<u64>().ok()
}

fn resolve_proxy_binary_path(
    config: &BenchConfig,
    required_features: &[&str],
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = &config.binary_path {
        if path.exists() {
            return Ok(path.clone());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("specified --binary does not exist: {}", path.display()),
        )
        .into());
    }

    let workspace_root = workspace_root();
    let profile = if config.release_build {
        "release"
    } else {
        "debug"
    };
    let binary_path = workspace_root
        .join("target")
        .join(profile)
        .join(proxy_binary_name());

    let force_rebuild_for_features = !required_features.is_empty();
    if !binary_path.exists() || (config.auto_build && force_rebuild_for_features) {
        if config.auto_build {
            build_proxy_binary(&workspace_root, config.release_build, required_features)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "proxy binary not found at {} (set --binary or run cargo build)",
                    binary_path.display()
                ),
            )
            .into());
        }
    }

    Ok(binary_path)
}

fn build_proxy_binary(
    workspace_root: &Path,
    release_build: bool,
    required_features: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::new("cargo");
    command
        .current_dir(workspace_root)
        .arg("build")
        .arg("-p")
        .arg("pd-edge")
        .arg("--bin")
        .arg("pd-edge-http-proxy");
    if release_build {
        command.arg("--release");
    }
    if !required_features.is_empty() {
        command.arg("--features").arg(required_features.join(","));
    }

    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other("cargo build failed for pd-edge-http-proxy").into());
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root should exist")
        .to_path_buf()
}

fn proxy_binary_name() -> &'static str {
    if cfg!(windows) {
        "pd-edge-http-proxy.exe"
    } else {
        "pd-edge-http-proxy"
    }
}

fn print_scenario_report(report: &ScenarioReport) {
    if let Some(error) = &report.error {
        println!("error: {error}");
        return;
    }

    println!(
        "requests_sent={}, responses_received={}, request_errors={}, expected_status={}",
        report.requests_sent,
        report.responses_received,
        report.request_errors,
        report.expected_status
    );
    println!(
        "unexpected_status_responses={}, throughput_rps={:.2}",
        report.unexpected_status_responses, report.throughput_rps
    );

    if report.status_counts.is_empty() {
        println!("status_counts: (none)");
    } else {
        let rendered = report
            .status_counts
            .iter()
            .map(|entry| format!("{}={}", entry.status, entry.count))
            .collect::<Vec<_>>()
            .join(", ");
        println!("status_counts: {rendered}");
    }

    if let Some(latency) = &report.latency_ms {
        println!(
            "latency_ms: median={:.3}, p90={:.3}, p95={:.3}, p99={:.3}, mean={:.3}, min={:.3}, max={:.3}",
            latency.median,
            latency.p90,
            latency.p95,
            latency.p99,
            latency.mean,
            latency.min,
            latency.max
        );
    } else {
        println!("latency_ms: (no successful samples)");
    }

    println!(
        "memory_rss_mib: start={}, end={}, min={}, avg={}, max={}, peak={}, samples={}",
        format_optional_f64(report.memory.start_rss_mib),
        format_optional_f64(report.memory.end_rss_mib),
        format_optional_f64(report.memory.min_rss_mib),
        format_optional_f64(report.memory.avg_rss_mib),
        format_optional_f64(report.memory.max_rss_mib),
        format_optional_f64(report.memory.peak_rss_mib),
        report.memory.samples
    );

    if let Some(telemetry) = &report.telemetry {
        println!(
            "telemetry: program_loaded={}, data_requests_total={}, vm_execution_errors_total={}",
            telemetry.program_loaded,
            telemetry.data_requests_total,
            telemetry.vm_execution_errors_total
        );
    }
}

fn format_optional_f64(value: Option<f64>) -> String {
    value
        .map(|number| format!("{number:.2}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn parse_args() -> Result<BenchConfig, String> {
    let mut config = BenchConfig::default();
    let mut args = env::args().skip(1).peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--requests" => {
                config.requests = parse_next_usize("--requests", &mut args)?;
            }
            "--warmup-requests" => {
                config.warmup_requests = parse_next_usize("--warmup-requests", &mut args)?;
            }
            "--concurrency" => {
                config.concurrency = parse_next_usize("--concurrency", &mut args)?;
            }
            "--request-timeout-ms" => {
                config.request_timeout_ms = parse_next_u64("--request-timeout-ms", &mut args)?;
            }
            "--startup-timeout-ms" => {
                config.startup_timeout_ms = parse_next_u64("--startup-timeout-ms", &mut args)?;
            }
            "--memory-sample-interval-ms" => {
                config.memory_sample_interval_ms =
                    parse_next_u64("--memory-sample-interval-ms", &mut args)?;
            }
            "--vm-fuel" => {
                let value = parse_next_u64("--vm-fuel", &mut args)?;
                if value == 0 {
                    return Err("--vm-fuel must be > 0".to_string());
                }
                config.vm_fuel = Some(value);
            }
            "--no-vm-fuel" => {
                config.vm_fuel = None;
            }
            "--vm-fuel-check-interval" => {
                let value = parse_next_u32("--vm-fuel-check-interval", &mut args)?;
                if value == 0 {
                    return Err("--vm-fuel-check-interval must be > 0".to_string());
                }
                config.vm_fuel_check_interval = value;
            }
            "--vm-execution-mode" => {
                let value = parse_next_string("--vm-execution-mode", &mut args)?;
                config.vm_execution_mode = parse_vm_execution_mode_arg(&value)?;
            }
            "--fuel-latency-sweep" => {
                config.fuel_latency_sweep = true;
            }
            "--fuel-latency-fuels" => {
                let raw = parse_next_string("--fuel-latency-fuels", &mut args)?;
                config.fuel_latency_fuels = parse_csv_u64_list("--fuel-latency-fuels", &raw)?;
            }
            "--fuel-latency-check-intervals" => {
                let raw = parse_next_string("--fuel-latency-check-intervals", &mut args)?;
                config.fuel_latency_check_intervals =
                    parse_csv_u32_list("--fuel-latency-check-intervals", &raw)?;
            }
            "--binary" => {
                let path = parse_next_string("--binary", &mut args)?;
                config.binary_path = Some(PathBuf::from(path));
            }
            "--json-out" => {
                let path = parse_next_string("--json-out", &mut args)?;
                config.json_out = Some(PathBuf::from(path));
            }
            "--scenario" => {
                config.scenario = Some(parse_next_string("--scenario", &mut args)?);
            }
            "--skip-build" => {
                config.auto_build = false;
            }
            "--build-debug" => {
                config.release_build = false;
                config.auto_build = true;
            }
            "--build-release" => {
                config.release_build = true;
                config.auto_build = true;
            }
            _ => {
                return Err(format!("unknown argument: {arg}"));
            }
        }
    }

    if config.requests == 0 {
        return Err("--requests must be > 0".to_string());
    }
    if config.concurrency == 0 {
        return Err("--concurrency must be > 0".to_string());
    }
    if config.fuel_latency_sweep {
        if config.fuel_latency_fuels.is_empty() {
            return Err("--fuel-latency-fuels must include at least one value".to_string());
        }
        if config.fuel_latency_check_intervals.is_empty() {
            return Err(
                "--fuel-latency-check-intervals must include at least one value".to_string(),
            );
        }
    }

    Ok(config)
}

fn parse_next_string(
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

fn parse_next_usize(
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<usize, String> {
    let value = parse_next_string(flag, args)?;
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid {flag}: {value}"))
}

fn parse_next_u64(
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<u64, String> {
    let value = parse_next_string(flag, args)?;
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid {flag}: {value}"))
}

fn parse_next_u32(
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<u32, String> {
    let value = parse_next_string(flag, args)?;
    value
        .parse::<u32>()
        .map_err(|_| format!("invalid {flag}: {value}"))
}

fn parse_csv_u64_list(flag: &str, raw: &str) -> Result<Vec<u64>, String> {
    let mut values = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err(format!("{flag} contains an empty list element: '{raw}'"));
        }
        let parsed = trimmed
            .parse::<u64>()
            .map_err(|_| format!("invalid {flag}: {trimmed}"))?;
        if parsed == 0 {
            return Err(format!("{flag} values must be > 0"));
        }
        if !values.contains(&parsed) {
            values.push(parsed);
        }
    }
    if values.is_empty() {
        return Err(format!("{flag} must include at least one value"));
    }
    Ok(values)
}

fn parse_csv_u32_list(flag: &str, raw: &str) -> Result<Vec<u32>, String> {
    let mut values = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err(format!("{flag} contains an empty list element: '{raw}'"));
        }
        let parsed = trimmed
            .parse::<u32>()
            .map_err(|_| format!("invalid {flag}: {trimmed}"))?;
        if parsed == 0 {
            return Err(format!("{flag} values must be > 0"));
        }
        if !values.contains(&parsed) {
            values.push(parsed);
        }
    }
    if values.is_empty() {
        return Err(format!("{flag} must include at least one value"));
    }
    Ok(values)
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn print_help() {
    let scenario_ids = SCENARIOS
        .iter()
        .map(|scenario| scenario.id)
        .collect::<Vec<_>>()
        .join(" | ");
    eprintln!(
        "Usage: cargo run -p pd-edge --example http_proxy_perf_framework -- [options]\n\n\
Options:\n\
  --requests <N>                    Measured requests per scenario (default: 10000)\n\
  --warmup-requests <N>             Warmup requests per scenario (default: 1000)\n\
  --concurrency <N>                 Concurrent workers (default: 64)\n\
  --request-timeout-ms <MS>         Per-request timeout (default: 10000)\n\
  --startup-timeout-ms <MS>         Proxy readiness timeout (default: 15000)\n\
  --memory-sample-interval-ms <MS>  RSS sample interval (default: 100)\n\
  --vm-fuel <UNITS>                 Enable cooperative VM fuel slices (default: disabled)\n\
  --no-vm-fuel                      Disable VM fuel slices\n\
  --vm-fuel-check-interval <OPS>    Fuel check interval for proxy VM (default: 32)\n\
  --vm-execution-mode <MODE>        Proxy VM execution mode: async|threading (default: async)\n\
  --fuel-latency-sweep              Run latency sweeps for fuel and fuel-check-interval (defaults to scenario no_host_calls_program)\n\
  --fuel-latency-fuels <CSV>        CSV list for fuel sweep; must be > 0 (default starts at 1)\n\
  --fuel-latency-check-intervals <CSV> CSV list for check-interval sweep; must be > 0 (default starts at 1)\n\
  --binary <PATH>                   Explicit pd-edge-http-proxy binary path\n\
  --json-out <PATH>                 Write JSON report to path\n\
  --scenario <ID>                   Run a single scenario id ({scenario_ids})\n\
  --skip-build                      Do not auto-build pd-edge-http-proxy\n\
  --build-release                   Auto-build release binary (default)\n\
  --build-debug                     Auto-build debug binary\n\
  -h, --help                        Show help\n\n\
Notes:\n\
  - Plain HTTP scenarios use plaintext HTTP/1.1 only.\n\
  - HTTP/2 scenarios use HTTPS + ALPN-negotiated h2 only; h2c is not used.\n\
  - {HTTP2_TLS_FEATURE_HINT}\n"
    );
}
