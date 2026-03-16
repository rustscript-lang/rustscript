use std::{
    collections::BTreeMap,
    env, fs, io,
    net::{SocketAddr, TcpListener, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use edge::HOST_FUNCTION_COUNT;
#[cfg(feature = "http3")]
use futures_util::future::poll_fn;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request as HyperRequest, Response as HyperResponse,
    body::{Bytes, Incoming},
    http::{HeaderMap, HeaderValue, StatusCode as HttpStatusCode, Version as HttpVersion},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
#[cfg(all(feature = "http2", feature = "tls"))]
use rcgen::generate_simple_self_signed;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use socket2::SockRef;
#[cfg(feature = "http3")]
use tokio::net::lookup_host;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, JoinSet},
    time::sleep,
};
#[cfg(all(feature = "http2", feature = "tls", not(feature = "http3")))]
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
#[cfg(all(feature = "http2", feature = "tls"))]
use tokio_rustls::{TlsAcceptor, rustls::ServerConfig};
#[cfg(feature = "http3")]
use url::Url;
use vm::{compile_source, encode_program, validate_program};

#[cfg(feature = "http3")]
use rustls::{
    self, ClientConfig as RustlsClientConfig, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};

const LOAD_REQUEST_BODY: &str = "edge-perf-payload";
const BENCH_TLS_SESSION_REUSE_ENTRIES: usize = 128;
const BENCH_UPSTREAM_HTTP_REUSE_ENTRIES: usize = 128;
const BENCH_DOWNSTREAM_HTTP2_SESSION_ENTRIES: usize = 128;
const BENCH_UPSTREAM_HTTP3_REUSE_ENTRIES: usize = 128;
const BENCH_DOWNSTREAM_HTTP3_SESSION_ENTRIES: usize = 128;
const BENCH_HTTP3_CLIENT_POOL_MAX: usize = 8;
const BENCH_HTTP3_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const BENCH_HTTP3_STREAM_RECEIVE_WINDOW_BYTES: u32 = 8 * 1024 * 1024;
const BENCH_HTTP3_CONNECTION_RECEIVE_WINDOW_BYTES: u32 = 32 * 1024 * 1024;
const BENCH_HTTP3_SEND_WINDOW_BYTES: u64 = 32 * 1024 * 1024;
const BENCH_HTTP3_KEEPALIVE_INTERVAL_MS: u64 = 5_000;
const BENCH_HTTP3_MAX_CONCURRENT_BIDI_STREAMS: u32 = 1024;
const HTTP2_TLS_FEATURE_HINT: &str =
    "HTTP/2 benchmark scenarios require running the example with --features http2,tls";
const HTTP3_TLS_FEATURE_HINT: &str =
    "HTTP/3 benchmark scenarios require running the example with --features http3,tls";

#[cfg(any(all(feature = "http2", feature = "tls"), feature = "http3"))]
fn ensure_rustls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        #[cfg(feature = "http3")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(all(not(feature = "http3"), feature = "http2", feature = "tls"))]
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

fn selected_base_workload_source() -> &'static str {
    if env_flag("PD_EDGE_PERF_SKIP_BASE_WORKLOAD") {
        "let mut acc = 0;"
    } else {
        BASE_WORKLOAD_SOURCE
    }
}

fn no_host_calls_program_source() -> String {
    let workload_source = selected_base_workload_source();
    format!(
        r#"
{workload_source}
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
    let workload_source = selected_base_workload_source();
    format!(
        r#"
{workload_source}

use http;

let method = http::request::get_method();
let path = http::request::get_path();
let client_id = http::request::get_header("x-client-id");
let request_body_mode = http::request::get_header("x-bench-body-mode");

if (acc % 2) == 0 {{
    http::response::set_header("x-perf-acc", "even");
}} else {{
    http::response::set_header("x-perf-acc", "odd");
}}
http::response::set_status(200);
http::response::set_header("x-perf-method", method);
http::response::set_header("x-perf-path", path);
http::response::set_header("x-perf-client-id", client_id);
http::response::set_header("x-perf-request-body-mode", request_body_mode);
http::response::set_body("");
"#
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpstreamProtocol {
    Http1,
    HttpsHttp2,
    HttpsHttp3,
}

impl UpstreamProtocol {
    fn label(self) -> &'static str {
        match self {
            Self::Http1 => "1.1",
            Self::HttpsHttp2 => "2",
            Self::HttpsHttp3 => "3",
        }
    }

    fn requires_http2_tls(self) -> bool {
        matches!(self, Self::HttpsHttp2)
    }

    fn requires_http3_tls(self) -> bool {
        matches!(self, Self::HttpsHttp3)
    }

    fn requires_tls(self) -> bool {
        !matches!(self, Self::Http1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownstreamProtocol {
    Http1,
    HttpsHttp2,
    HttpsHttp3,
}

impl DownstreamProtocol {
    fn label(self) -> &'static str {
        match self {
            Self::Http1 => "1.1",
            Self::HttpsHttp2 => "2",
            Self::HttpsHttp3 => "3",
        }
    }

    fn requires_http2_tls(self) -> bool {
        matches!(self, Self::HttpsHttp2)
    }

    fn requires_http3_tls(self) -> bool {
        matches!(self, Self::HttpsHttp3)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestBodyMode {
    HeadersOnly,
    BodyRead,
}

impl RequestBodyMode {
    fn label(self) -> &'static str {
        match self {
            Self::HeadersOnly => "headers-only",
            Self::BodyRead => "body-read",
        }
    }

    fn request_body(self) -> &'static str {
        match self {
            Self::HeadersOnly => "",
            Self::BodyRead => LOAD_REQUEST_BODY,
        }
    }

    fn reads_body(self) -> bool {
        matches!(self, Self::BodyRead)
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
    let workload_source = selected_base_workload_source();
    let tls_import = if upstream_protocol.requires_tls() {
        "use tls;\n"
    } else {
        ""
    };
    let tls_prelude = match upstream_protocol {
        UpstreamProtocol::Http1 => "",
        UpstreamProtocol::HttpsHttp2 => {
            r#"
let session = tls::session::from_socket(upstream);
tls::session::set_verify(session, false);
tls::session::set_alpn(session, "h2,http/1.1");
"#
        }
        UpstreamProtocol::HttpsHttp3 => {
            r#"
let session = tls::session::from_socket(upstream);
tls::session::set_verify(session, false);
"#
        }
    };
    let version_preference = upstream_protocol.label();
    let batched_upstream_headers = match flavor {
        ProxyProgramFlavor::HeaderTransform => {
            r#"["x-downstream-version", downstream_version, "x-bench-program-header", "program-proxy"]"#
                .to_string()
        }
    };
    let batched_response_headers = match flavor {
        ProxyProgramFlavor::HeaderTransform => r#"[
    "x-downstream-version", downstream_version,
    "x-bench-response-header", "program-proxy"
]"#
        .to_string(),
    };
    let response_header_program = if upstream_protocol.requires_tls() {
        format!(
            r#"
let downstream = proxy::stream::downstream();
let upstream_stream = proxy::stream::exchange(upstream);
proxy::forward(downstream, upstream_stream);
http::response::set_headers({batched_response_headers});
http::response::set_header("x-upstream-alpn", tls::session::get_alpn(session));
"#
        )
    } else {
        format!(
            r#"
let downstream = proxy::stream::downstream();
let upstream_stream = proxy::stream::exchange(upstream);
proxy::forward(downstream, upstream_stream);
http::response::set_headers({batched_response_headers});
"#
        )
    };
    format!(
        r#"
{workload_source}

use http;
use proxy;
{tls_import}

let downstream_version = http::request::get_http_version();

let upstream = http::exchange::prepare_default_upstream(
    "{upstream_origin}",
    "{version_preference}",
    {batched_upstream_headers}
);
{tls_prelude}

{response_header_program}
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
        body_mode: RequestBodyMode,
    },
    ProxyRoundTrip {
        flavor: ProxyProgramFlavor,
        upstream: UpstreamProtocol,
        body_mode: RequestBodyMode,
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
            ProgramVariant::DirectUpstream { upstream, .. } => Some(upstream),
            ProgramVariant::ProxyRoundTrip { upstream, .. } => Some(upstream),
            _ => None,
        }
    }

    fn uses_proxy(self) -> bool {
        !matches!(self.program_variant, ProgramVariant::DirectUpstream { .. })
    }

    fn request_body_mode(self) -> RequestBodyMode {
        match self.program_variant {
            ProgramVariant::None
            | ProgramVariant::NoHostCallsBase
            | ProgramVariant::HostCallsAdditive => RequestBodyMode::HeadersOnly,
            ProgramVariant::DirectUpstream { body_mode, .. }
            | ProgramVariant::ProxyRoundTrip { body_mode, .. } => body_mode,
        }
    }

    fn requires_http2_tls(self) -> bool {
        self.downstream_protocol.requires_http2_tls()
            || self
                .upstream_protocol()
                .is_some_and(UpstreamProtocol::requires_http2_tls)
    }

    fn requires_http3_tls(self) -> bool {
        self.downstream_protocol.requires_http3_tls()
            || self
                .upstream_protocol()
                .is_some_and(UpstreamProtocol::requires_http3_tls)
    }

    fn supports_current_build(self) -> bool {
        (!self.requires_http2_tls() || cfg!(all(feature = "http2", feature = "tls")))
            && (!self.requires_http3_tls() || cfg!(all(feature = "http3", feature = "tls")))
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

const SCENARIOS: [Scenario; 13] = [
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
        description: "pd-edge-http-proxy with additive host calls and terminate (no request-body read, empty response body, no upstream)",
        expected_status: 200,
        program_variant: ProgramVariant::HostCallsAdditive,
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "raw_http_upstream",
        description: "perf client hits hardcoded plaintext HTTP upstream directly (headers only, no body read)",
        expected_status: 200,
        program_variant: ProgramVariant::DirectUpstream {
            upstream: UpstreamProtocol::Http1,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "raw_http_upstream_body_read",
        description: "perf client hits hardcoded plaintext HTTP upstream directly with request body read and echoed response body",
        expected_status: 200,
        program_variant: ProgramVariant::DirectUpstream {
            upstream: UpstreamProtocol::Http1,
            body_mode: RequestBodyMode::BodyRead,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "http_proxy",
        description: "pd-edge-http-proxy with plaintext downstream and plaintext HTTP upstream (header-only upstream response)",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::Http1,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "http_proxy_body_read",
        description: "pd-edge-http-proxy with plaintext downstream and plaintext HTTP upstream with request body read and echoed response body",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::Http1,
            body_mode: RequestBodyMode::BodyRead,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "raw_http2_upstream",
        description: "perf client hits hardcoded HTTPS HTTP/2 upstream directly (headers only, no body read)",
        expected_status: 200,
        program_variant: ProgramVariant::DirectUpstream {
            upstream: UpstreamProtocol::HttpsHttp2,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp2,
    },
    Scenario {
        id: "http->http2",
        description: "pd-edge-http-proxy with plaintext downstream and HTTPS HTTP/2 upstream (header-only upstream response)",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::HttpsHttp2,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::Http1,
    },
    Scenario {
        id: "http2->http",
        description: "pd-edge-http-proxy with downstream HTTPS HTTP/2 and plaintext HTTP upstream (header-only upstream response)",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::Http1,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp2,
    },
    Scenario {
        id: "http2->http2",
        description: "pd-edge-http-proxy with downstream HTTPS HTTP/2 and upstream HTTPS HTTP/2 (header-only upstream response)",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::HttpsHttp2,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp2,
    },
    Scenario {
        id: "raw_http3_upstream",
        description: "perf client hits hardcoded HTTPS HTTP/3 upstream directly (headers only, no body read)",
        expected_status: 200,
        program_variant: ProgramVariant::DirectUpstream {
            upstream: UpstreamProtocol::HttpsHttp3,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp3,
    },
    Scenario {
        id: "http3->http3",
        description: "pd-edge-http-proxy with downstream HTTPS HTTP/3 and upstream HTTPS HTTP/3 (header-only upstream response)",
        expected_status: 200,
        program_variant: ProgramVariant::ProxyRoundTrip {
            flavor: ProxyProgramFlavor::HeaderTransform,
            upstream: UpstreamProtocol::HttpsHttp3,
            body_mode: RequestBodyMode::HeadersOnly,
        },
        downstream_protocol: DownstreamProtocol::HttpsHttp3,
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
    #[serde(default)]
    lock_metrics: Vec<LockMetricSummary>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LockMetricSummary {
    name: String,
    acquisitions_total: u64,
    wait_ns_total: u64,
    hold_ns_total: u64,
    wait_ns_max: u64,
    hold_ns_max: u64,
}

#[derive(Clone)]
struct BenchClients {
    http: Client,
    https: Client,
}

#[derive(Clone)]
struct ReqwestScenarioClient {
    client: Client,
    origin: String,
}

#[cfg(feature = "http3")]
type Http3SendRequest = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
#[cfg(feature = "http3")]
type Http3Driver = h3::client::Connection<h3_quinn::Connection, Bytes>;
#[cfg(feature = "http3")]
type Http3BenchSendStream = h3::server::RequestStream<h3_quinn::SendStream<Bytes>, Bytes>;
#[cfg(feature = "http3")]
type Http3BenchRecvStream = h3::server::RequestStream<h3_quinn::RecvStream, Bytes>;

#[derive(Clone)]
enum ScenarioHttpClient {
    Reqwest(ReqwestScenarioClient),
    #[cfg(feature = "http3")]
    Http3(Arc<Http3BenchClient>),
    #[cfg(feature = "http3")]
    Http3Pool(Vec<Arc<Http3BenchClient>>),
}

#[cfg(feature = "http3")]
struct Http3BenchClient {
    _endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    sender: AsyncMutex<Http3SendRequest>,
    driver: Mutex<Option<JoinHandle<()>>>,
    origin: String,
    authority: String,
    request_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchHttpVersion {
    Http11,
    Http2,
    Http3,
    Unknown,
}

#[derive(Debug)]
struct ProbeResponse {
    status: u16,
    version: BenchHttpVersion,
    headers: HeaderMap,
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
        http3_addr: Option<SocketAddr>,
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
            .arg("--upstream-http3-reuse-entries")
            .arg(BENCH_UPSTREAM_HTTP3_REUSE_ENTRIES.to_string())
            .arg("--downstream-http3-session-entries")
            .arg(BENCH_DOWNSTREAM_HTTP3_SESSION_ENTRIES.to_string())
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
        if let Some(http3_addr) = http3_addr {
            command.arg("--http3-addr").arg(http3_addr.to_string());
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

impl BenchHttpVersion {
    fn label(self) -> &'static str {
        match self {
            Self::Http11 => "1.1",
            Self::Http2 => "2",
            Self::Http3 => "3",
            Self::Unknown => "unknown",
        }
    }

    fn from_http(version: HttpVersion) -> Self {
        match version {
            HttpVersion::HTTP_11 => Self::Http11,
            HttpVersion::HTTP_2 => Self::Http2,
            #[allow(unreachable_patterns)]
            HttpVersion::HTTP_3 => Self::Http3,
            _ => Self::Unknown,
        }
    }
}

impl ScenarioHttpClient {
    fn for_worker(&self, worker_index: usize) -> Self {
        match self {
            Self::Reqwest(client) => Self::Reqwest(client.clone()),
            #[cfg(feature = "http3")]
            Self::Http3(client) => Self::Http3(client.clone()),
            #[cfg(feature = "http3")]
            Self::Http3Pool(pool) => {
                let client = pool[worker_index % pool.len()].clone();
                Self::Http3(client)
            }
        }
    }

    async fn send_request(
        &self,
        path: &str,
        client_id: &str,
        body_mode: RequestBodyMode,
    ) -> Result<ProbeResponse, Box<dyn std::error::Error>> {
        match self {
            Self::Reqwest(client) => client.send_request(path, client_id, body_mode).await,
            #[cfg(feature = "http3")]
            Self::Http3(client) => client.send_request(path, client_id, body_mode).await,
            #[cfg(feature = "http3")]
            Self::Http3Pool(pool) => pool[0].send_request(path, client_id, body_mode).await,
        }
    }
}

impl ReqwestScenarioClient {
    async fn send_request(
        &self,
        path: &str,
        client_id: &str,
        body_mode: RequestBodyMode,
    ) -> Result<ProbeResponse, Box<dyn std::error::Error>> {
        let response = self
            .client
            .post(format!("{}{}", self.origin, path))
            .header("x-client-id", client_id)
            .header("x-bench-body-mode", body_mode.label())
            .header("content-type", "text/plain")
            .body(body_mode.request_body().to_string())
            .send()
            .await?;
        let status = response.status().as_u16();
        let version = BenchHttpVersion::from_http(response.version());
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        Ok(ProbeResponse {
            status,
            version,
            headers,
            body,
        })
    }
}

#[cfg(feature = "http3")]
impl Http3BenchClient {
    async fn connect(
        origin: &str,
        request_timeout: Duration,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        ensure_rustls_provider();
        let url = Url::parse(origin)?;
        let host = url
            .host_str()
            .ok_or_else(|| io::Error::other("http3 origin should include host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| io::Error::other("http3 origin should include port"))?;
        let remotes = lookup_host((host, port)).await?.collect::<Vec<_>>();
        let remote = remotes
            .iter()
            .copied()
            .find(SocketAddr::is_ipv4)
            .or_else(|| remotes.first().copied())
            .ok_or_else(|| io::Error::other("http3 origin should resolve"))?;
        let bind_addr: SocketAddr = if remote.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        }
        .parse()?;
        let socket = std::net::UdpSocket::bind(bind_addr)?;
        socket.set_nonblocking(true)?;
        tune_udp_socket_buffers(&socket)?;
        let mut endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        endpoint.set_default_client_config(build_http3_bench_client_config());
        let connecting = endpoint.connect(remote, host)?;
        let connection = tokio::time::timeout(request_timeout, connecting)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "http3 connect timed out"))??;
        let h3_connection = h3_quinn::Connection::new(connection.clone());
        let (driver, sender) = h3::client::new(h3_connection).await?;
        let driver_handle = tokio::spawn(async move {
            let mut driver: Http3Driver = driver;
            let _ = poll_fn(|cx| driver.poll_close(cx)).await;
        });
        let authority = if let Some(port) = url.port() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };
        Ok(Arc::new(Self {
            _endpoint: endpoint,
            connection,
            sender: AsyncMutex::new(sender),
            driver: Mutex::new(Some(driver_handle)),
            origin: origin.to_string(),
            authority,
            request_timeout,
        }))
    }

    async fn send_request(
        &self,
        path: &str,
        client_id: &str,
        body_mode: RequestBodyMode,
    ) -> Result<ProbeResponse, Box<dyn std::error::Error>> {
        let request = hyper::Request::builder()
            .method("POST")
            .uri(format!("{}{}", self.origin, path))
            .header("host", &self.authority)
            .header("x-client-id", client_id)
            .header("x-bench-body-mode", body_mode.label())
            .header("content-type", "text/plain")
            .body(())
            .map_err(|err| io::Error::other(format!("http3 request should build: {err}")))?;
        tokio::time::timeout(self.request_timeout, async {
            let mut stream = {
                let mut sender = self.sender.lock().await;
                sender.send_request(request).await.map_err(|err| {
                    io::Error::other(format!("http3 request stream should open: {err}"))
                })?
            };
            if !body_mode.request_body().is_empty() {
                stream
                    .send_data(Bytes::copy_from_slice(body_mode.request_body().as_bytes()))
                    .await
                    .map_err(|err| {
                        io::Error::other(format!("http3 request body should send: {err}"))
                    })?;
            }
            stream
                .finish()
                .await
                .map_err(|err| io::Error::other(format!("http3 request should finish: {err}")))?;
            let response = stream.recv_response().await.map_err(|err| {
                io::Error::other(format!("http3 response head should arrive: {err}"))
            })?;
            let status = response.status().as_u16();
            let version = BenchHttpVersion::from_http(response.version());
            let headers = response.headers().clone();
            let mut response_body = Vec::new();
            while let Some(mut chunk) = stream.recv_data().await.map_err(|err| {
                io::Error::other(format!("http3 response body should read: {err}"))
            })? {
                use hyper::body::Buf;
                response_body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
            }
            Ok::<ProbeResponse, Box<dyn std::error::Error>>(ProbeResponse {
                status,
                version,
                headers,
                body: String::from_utf8_lossy(&response_body).to_string(),
            })
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "http3 request timed out"))?
    }
}

#[cfg(feature = "http3")]
impl Drop for Http3BenchClient {
    fn drop(&mut self) {
        self.connection.close(0_u32.into(), b"benchmark-done");
        if let Ok(mut guard) = self.driver.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

async fn build_scenario_client(
    clients: &BenchClients,
    scenario: Scenario,
    request_origin: &str,
    request_timeout_ms: u64,
    http3_pool_size: usize,
) -> Result<ScenarioHttpClient, Box<dyn std::error::Error>> {
    match scenario.downstream_protocol {
        DownstreamProtocol::Http1 => Ok(ScenarioHttpClient::Reqwest(ReqwestScenarioClient {
            client: clients.http.clone(),
            origin: request_origin.to_string(),
        })),
        DownstreamProtocol::HttpsHttp2 => Ok(ScenarioHttpClient::Reqwest(ReqwestScenarioClient {
            client: clients.https.clone(),
            origin: request_origin.to_string(),
        })),
        DownstreamProtocol::HttpsHttp3 => {
            #[cfg(feature = "http3")]
            {
                let pool_size = http3_pool_size.max(1);
                if pool_size == 1 {
                    return Ok(ScenarioHttpClient::Http3(
                        Http3BenchClient::connect(
                            request_origin,
                            Duration::from_millis(request_timeout_ms),
                        )
                        .await?,
                    ));
                }
                let mut pool = Vec::with_capacity(pool_size);
                for _ in 0..pool_size {
                    pool.push(
                        Http3BenchClient::connect(
                            request_origin,
                            Duration::from_millis(request_timeout_ms),
                        )
                        .await?,
                    );
                }
                Ok(ScenarioHttpClient::Http3Pool(pool))
            }
            #[cfg(not(feature = "http3"))]
            {
                let _ = clients;
                let _ = scenario;
                let _ = request_origin;
                let _ = request_timeout_ms;
                let _ = http3_pool_size;
                Err(io::Error::other(HTTP3_TLS_FEATURE_HINT).into())
            }
        }
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
            let hint = if matched.requires_http3_tls() {
                HTTP3_TLS_FEATURE_HINT
            } else {
                HTTP2_TLS_FEATURE_HINT
            };
            return Err(io::Error::other(format!(
                "scenario '{}' requires feature support; {}",
                matched.id, hint
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
                "skipping scenarios requiring protocol feature support: {}",
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
    let needs_http2 = scenarios
        .iter()
        .any(|scenario| scenario.requires_http2_tls());
    let needs_http3 = scenarios
        .iter()
        .any(|scenario| scenario.requires_http3_tls());
    let needs_tls = needs_http2 || needs_http3;
    let mut features = Vec::new();
    if needs_http2 {
        features.push("http2");
    }
    if needs_http3 {
        features.push("http3");
    }
    if needs_tls {
        features.push("tls");
    }
    features
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
        Some(UpstreamProtocol::HttpsHttp3) => Some(spawn_https_http3_upstream_fixture().await?),
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
        ProgramVariant::ProxyRoundTrip {
            flavor, upstream, ..
        } => {
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
        let http3_addr = if scenario.downstream_protocol.requires_http3_tls() {
            Some(reserve_loopback_udp_addr()?)
        } else {
            None
        };
        let admin_addr = reserve_loopback_addr()?;
        let mut proxy = ProxyProcess::spawn(
            binary_path,
            data_addr,
            https_addr,
            http3_addr,
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

        Some((proxy, data_addr, https_addr, http3_addr, admin_addr))
    } else {
        None
    };

    let request_origin = if let Some((_, data_addr, https_addr, http3_addr, _)) = proxy.as_ref() {
        match scenario.downstream_protocol {
            DownstreamProtocol::Http1 => format!("http://{data_addr}"),
            DownstreamProtocol::HttpsHttp2 => {
                let https_addr = https_addr.expect("https addr should exist");
                format!("https://127.0.0.1:{}", https_addr.port())
            }
            DownstreamProtocol::HttpsHttp3 => {
                let http3_addr = http3_addr.expect("http3 addr should exist");
                format!("https://127.0.0.1:{}", http3_addr.port())
            }
        }
    } else {
        upstream_fixture
            .as_ref()
            .expect("direct-upstream scenario should have an upstream fixture")
            .origin()
            .to_string()
    };
    let probe_client = build_scenario_client(
        clients,
        scenario,
        &request_origin,
        config.request_timeout_ms,
        1,
    )
    .await?;
    verify_scenario_probe(&probe_client, scenario, upstream_fixture.as_ref()).await?;
    let load_http3_pool_size = if scenario.downstream_protocol.requires_http3_tls() {
        config.concurrency.clamp(1, BENCH_HTTP3_CLIENT_POOL_MAX)
    } else {
        1
    };
    let request_client = build_scenario_client(
        clients,
        scenario,
        &request_origin,
        config.request_timeout_ms,
        load_http3_pool_size,
    )
    .await?;

    if config.warmup_requests > 0 {
        let warmup = run_load(
            &request_client,
            scenario,
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

    let pid = proxy.as_mut().map(|(proxy, _, _, _, _)| proxy.pid());
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
        &request_client,
        scenario,
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
    let telemetry = if let Some((_, _, _, _, admin_addr)) = proxy.as_ref() {
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
    client: &ScenarioHttpClient,
    scenario: Scenario,
    upstream_fixture: Option<&UpstreamFixture>,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = send_probe_request(client, "/perf", "perf-client", scenario).await?;
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
            if probe_header(&response, "x-perf-request-body-mode")
                != scenario.request_body_mode().label()
            {
                return Err(io::Error::other(format!(
                    "host-call probe missing expected x-perf-request-body-mode '{}': got '{}'",
                    scenario.request_body_mode().label(),
                    probe_header(&response, "x-perf-request-body-mode")
                ))
                .into());
            }
        }
        ProgramVariant::DirectUpstream { upstream, .. } => {
            verify_direct_upstream_probe_response(
                &response,
                scenario,
                upstream,
                "perf-client",
                "/perf",
            )?;
            if upstream.requires_http2_tls() || upstream.requires_http3_tls() {
                let fixture = upstream_fixture
                    .ok_or_else(|| io::Error::other("missing multiplexed upstream fixture"))?;
                verify_upstream_reuse_probe(client, scenario, fixture, upstream.label()).await?;
            }
        }
        ProgramVariant::ProxyRoundTrip { upstream, .. } => {
            verify_proxy_roundtrip_probe_response(&response, scenario, "perf-client", "/perf")?;
            if upstream.requires_http2_tls() || upstream.requires_http3_tls() {
                let fixture = upstream_fixture
                    .ok_or_else(|| io::Error::other("missing multiplexed upstream fixture"))?;
                verify_upstream_reuse_probe(client, scenario, fixture, upstream.label()).await?;
            }
        }
    }

    Ok(())
}

async fn send_probe_request(
    client: &ScenarioHttpClient,
    path: &str,
    client_id: &str,
    scenario: Scenario,
) -> Result<ProbeResponse, Box<dyn std::error::Error>> {
    client
        .send_request(path, client_id, scenario.request_body_mode())
        .await
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
        DownstreamProtocol::Http1 => BenchHttpVersion::Http11,
        DownstreamProtocol::HttpsHttp2 => BenchHttpVersion::Http2,
        DownstreamProtocol::HttpsHttp3 => BenchHttpVersion::Http3,
    };
    if response.version != expected_response_version {
        return Err(io::Error::other(format!(
            "downstream response version mismatch: expected {}, got {}",
            expected_response_version.label(),
            response.version.label()
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
        && probe_header(response, "x-bench-upstream-program-header") != "program-proxy"
    {
        return Err(io::Error::other(format!(
            "header-transform probe missing expected x-bench-upstream-program-header: got '{}'",
            probe_header(response, "x-bench-upstream-program-header")
        ))
        .into());
    }

    let upstream_client_id = probe_header(response, "x-bench-upstream-client-id");
    if upstream_client_id != expected_client_id {
        return Err(io::Error::other(format!(
            "probe missing expected x-bench-upstream-client-id header: got '{}'",
            upstream_client_id
        ))
        .into());
    }

    let upstream_path = probe_header(response, "x-bench-upstream-path");
    if upstream_path != expected_path {
        return Err(io::Error::other(format!(
            "probe missing expected x-bench-upstream-path header: got '{}'",
            upstream_path
        ))
        .into());
    }

    let expected_upstream_version = scenario
        .upstream_protocol()
        .map(UpstreamProtocol::label)
        .unwrap_or_default();
    let upstream_version = probe_header(response, "x-bench-upstream-http-version");
    if !expected_upstream_version.is_empty() && upstream_version != expected_upstream_version {
        return Err(io::Error::other(format!(
            "probe missing expected x-bench-upstream-http-version {}: got '{}'",
            expected_upstream_version, upstream_version
        ))
        .into());
    }

    if let Some(upstream) = scenario.upstream_protocol()
        && upstream.requires_tls()
    {
        let expected_alpn = match upstream {
            UpstreamProtocol::Http1 => "",
            UpstreamProtocol::HttpsHttp2 => "h2",
            UpstreamProtocol::HttpsHttp3 => "h3",
        };
        let actual_alpn = probe_header(response, "x-upstream-alpn");
        if actual_alpn != expected_alpn {
            return Err(io::Error::other(format!(
                "probe missing expected x-upstream-alpn={expected_alpn}: got '{actual_alpn}'"
            ))
            .into());
        }
    }

    let expected_body_mode = scenario.request_body_mode().label();
    let actual_body_mode = probe_header(response, "x-bench-upstream-body-mode");
    if actual_body_mode != expected_body_mode {
        return Err(io::Error::other(format!(
            "proxy round-trip probe missing expected x-bench-upstream-body-mode '{}': got '{}'",
            expected_body_mode, actual_body_mode
        ))
        .into());
    }

    if scenario.request_body_mode().reads_body() {
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
    } else if !response.body.is_empty() {
        return Err(io::Error::other(format!(
            "proxy round-trip probe expected empty body for header-only scenario, got '{}'",
            response.body
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
        UpstreamProtocol::Http1 => BenchHttpVersion::Http11,
        UpstreamProtocol::HttpsHttp2 => BenchHttpVersion::Http2,
        UpstreamProtocol::HttpsHttp3 => BenchHttpVersion::Http3,
    };
    if response.version != expected_response_version {
        return Err(io::Error::other(format!(
            "direct-upstream response version mismatch: expected {}, got {}",
            expected_response_version.label(),
            response.version.label()
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

    let expected_body_mode = scenario.request_body_mode().label();
    let actual_body_mode = probe_header(response, "x-bench-upstream-body-mode");
    if actual_body_mode != expected_body_mode {
        return Err(io::Error::other(format!(
            "direct-upstream probe missing expected x-bench-upstream-body-mode '{}': got '{}'",
            expected_body_mode, actual_body_mode
        ))
        .into());
    }

    if scenario.request_body_mode().reads_body() {
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
    } else if !response.body.is_empty() {
        return Err(io::Error::other(format!(
            "direct-upstream probe expected empty body for header-only scenario, got '{}'",
            response.body
        ))
        .into());
    }

    Ok(())
}

async fn verify_upstream_reuse_probe(
    client: &ScenarioHttpClient,
    scenario: Scenario,
    upstream_fixture: &UpstreamFixture,
    protocol_label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let baseline_connections = upstream_fixture
        .connection_count()
        .ok_or_else(|| io::Error::other("missing upstream connection counter"))?;
    if baseline_connections != 1 {
        return Err(io::Error::other(format!(
            "expected one upstream {protocol_label} TLS connection after baseline probe, got {baseline_connections}",
        ))
        .into());
    }

    let (slow, fast) = tokio::try_join!(
        send_probe_request(client, "/slow", "perf-client-slow", scenario),
        send_probe_request(client, "/fast", "perf-client-fast", scenario),
    )?;

    match scenario.program_variant {
        ProgramVariant::DirectUpstream { upstream, .. } => {
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
                "{protocol_label} reuse probe is only valid for upstream round-trip scenarios, got {}",
                scenario.id
            ))
            .into());
        }
    }

    let after_parallel = upstream_fixture
        .connection_count()
        .ok_or_else(|| io::Error::other("missing upstream connection counter"))?;
    if after_parallel != 1 {
        return Err(io::Error::other(format!(
            "expected {protocol_label} multiplexing over one upstream TLS connection, observed {after_parallel} connections",
        ))
        .into());
    }

    let reused = send_probe_request(client, "/reuse", "perf-client-reuse", scenario).await?;
    match scenario.program_variant {
        ProgramVariant::DirectUpstream { upstream, .. } => {
            verify_direct_upstream_probe_response(
                &reused,
                scenario,
                upstream,
                "perf-client-reuse",
                "/reuse",
            )?;
        }
        ProgramVariant::ProxyRoundTrip { .. } => {
            verify_proxy_roundtrip_probe_response(
                &reused,
                scenario,
                "perf-client-reuse",
                "/reuse",
            )?;
        }
        _ => unreachable!("unsupported scenario variant for HTTP/2 upstream reuse probe"),
    }

    let after_reuse = upstream_fixture
        .connection_count()
        .ok_or_else(|| io::Error::other("missing upstream connection counter"))?;
    if after_reuse != 1 {
        return Err(io::Error::other(format!(
            "expected {protocol_label} connection reuse over one upstream TLS connection, observed {after_reuse} connections",
        ))
        .into());
    }

    Ok(())
}

async fn spawn_plain_http_upstream_fixture() -> Result<UpstreamFixture, Box<dyn std::error::Error>>
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .expect("http upstream benchmark accept should succeed");
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(|request| async move {
                    build_benchmark_upstream_response(request, "1.1").await
                });
                let builder = hyper::server::conn::http1::Builder::new();
                if let Err(err) = builder.serve_connection(io, service).await {
                    eprintln!("http upstream benchmark connection ended: {err}");
                }
            });
        }
    });
    Ok(UpstreamFixture {
        origin: format!("http://{addr}"),
        connection_count: None,
        tasks: vec![task],
    })
}

async fn build_benchmark_upstream_response(
    request: HyperRequest<Incoming>,
    version_label: &'static str,
) -> Result<HyperResponse<Full<Bytes>>, std::convert::Infallible> {
    let (parts, body) = request.into_parts();
    let response =
        prepare_benchmark_upstream_response(parts.uri.path(), &parts.headers, body, version_label)
            .await;
    Ok(response.into_hyper_response())
}

struct BenchmarkUpstreamResponse {
    client_id: String,
    path: String,
    program_header: String,
    body_mode: String,
    version_label: &'static str,
    body: Bytes,
}

impl BenchmarkUpstreamResponse {
    fn response_headers(&self) -> [(&'static str, String); 4] {
        [
            ("x-bench-upstream-client-id", self.client_id.clone()),
            ("x-bench-upstream-path", self.path.clone()),
            (
                "x-bench-upstream-program-header",
                self.program_header.clone(),
            ),
            ("x-bench-upstream-body-mode", self.body_mode.clone()),
        ]
    }

    fn into_hyper_response(self) -> HyperResponse<Full<Bytes>> {
        let BenchmarkUpstreamResponse {
            client_id,
            path,
            program_header,
            body_mode,
            version_label,
            body,
        } = self;
        let mut response = HyperResponse::new(Full::new(body));
        for (name, value) in [
            ("x-bench-upstream-client-id", client_id),
            ("x-bench-upstream-path", path),
            ("x-bench-upstream-program-header", program_header),
            ("x-bench-upstream-body-mode", body_mode),
        ] {
            response.headers_mut().insert(
                name,
                HeaderValue::from_str(&value)
                    .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
            );
        }
        response.headers_mut().insert(
            "x-bench-upstream-http-version",
            HeaderValue::from_static(version_label),
        );
        response
    }

    fn into_http3_head(&self) -> HyperResponse<()> {
        let mut response = HyperResponse::builder()
            .status(HttpStatusCode::OK)
            .body(())
            .expect("http3 benchmark response head should build");
        for (name, value) in self.response_headers() {
            response.headers_mut().insert(
                name,
                HeaderValue::from_str(&value)
                    .unwrap_or_else(|_| HeaderValue::from_static("invalid")),
            );
        }
        response.headers_mut().insert(
            "x-bench-upstream-http-version",
            HeaderValue::from_static(self.version_label),
        );
        response
    }
}

async fn prepare_benchmark_upstream_response(
    path: &str,
    headers: &HeaderMap,
    body: Incoming,
    version_label: &'static str,
) -> BenchmarkUpstreamResponse {
    let client_id = headers
        .get("x-client-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let program_header = headers
        .get("x-bench-program-header")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body_mode = headers
        .get("x-bench-body-mode")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(RequestBodyMode::HeadersOnly.label())
        .to_string();

    if path == "/slow" {
        sleep(Duration::from_millis(75)).await;
    }

    let response_body = if body_mode == RequestBodyMode::BodyRead.label() {
        let body = body
            .collect()
            .await
            .expect("upstream benchmark body should collect")
            .to_bytes();
        Bytes::from(format!(
            "upstream-roundtrip|{path}|{}",
            String::from_utf8_lossy(&body)
        ))
    } else {
        drop(body);
        Bytes::new()
    };

    BenchmarkUpstreamResponse {
        client_id,
        path: path.to_string(),
        program_header,
        body_mode,
        version_label,
        body: response_body,
    }
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
                    let service = service_fn(|request| async move {
                        build_benchmark_upstream_response(request, "2").await
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

#[cfg(feature = "http3")]
fn build_http3_bench_client_config() -> quinn::ClientConfig {
    ensure_rustls_provider();
    let builder = RustlsClientConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("http3 benchmark TLS versions should configure");
    let mut config = builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PermissiveServerCertVerifier::new()))
        .with_no_client_auth();
    config.enable_sni = true;
    config.alpn_protocols = vec![b"h3".to_vec()];

    let mut client = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(config)
            .expect("http3 benchmark QUIC client config should build"),
    ));
    client.transport_config(Arc::new(build_http3_transport_config()));
    client
}

#[cfg(feature = "http3")]
fn build_http3_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(
        quinn::VarInt::from_u32(BENCH_HTTP3_MAX_CONCURRENT_BIDI_STREAMS).into(),
    );
    transport.stream_receive_window(
        quinn::VarInt::from_u32(BENCH_HTTP3_STREAM_RECEIVE_WINDOW_BYTES).into(),
    );
    transport.receive_window(
        quinn::VarInt::from_u32(BENCH_HTTP3_CONNECTION_RECEIVE_WINDOW_BYTES).into(),
    );
    transport.send_window(BENCH_HTTP3_SEND_WINDOW_BYTES);
    transport.keep_alive_interval(Some(Duration::from_millis(
        BENCH_HTTP3_KEEPALIVE_INTERVAL_MS,
    )));
    transport
}

#[cfg(feature = "http3")]
fn tune_udp_socket_buffers(socket: &std::net::UdpSocket) -> io::Result<()> {
    let sock_ref = SockRef::from(socket);
    sock_ref.set_recv_buffer_size(BENCH_HTTP3_SOCKET_BUFFER_BYTES)?;
    sock_ref.set_send_buffer_size(BENCH_HTTP3_SOCKET_BUFFER_BYTES)?;
    Ok(())
}

#[cfg(feature = "http3")]
#[derive(Debug)]
struct PermissiveServerCertVerifier {
    delegate: Arc<dyn ServerCertVerifier>,
}

#[cfg(feature = "http3")]
impl PermissiveServerCertVerifier {
    fn new() -> Self {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let delegate = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("http3 benchmark verifier should build");
        Self { delegate }
    }
}

#[cfg(feature = "http3")]
impl ServerCertVerifier for PermissiveServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}

#[cfg(feature = "http3")]
fn build_http3_upstream_server_config() -> quinn::ServerConfig {
    ensure_rustls_provider();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("http3 upstream benchmark certificate should generate");
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("http3 upstream benchmark TLS versions should configure")
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert.serialize_der().expect(
            "http3 upstream benchmark certificate should serialize",
        ))],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der())),
    )
    .expect("http3 upstream benchmark server config should build");
    server_crypto.alpn_protocols = vec![b"h3".to_vec()];

    let mut server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
            .expect("http3 upstream benchmark QUIC server config should build"),
    ));
    server.transport_config(Arc::new(build_http3_transport_config()));
    server
}

#[cfg(feature = "http3")]
fn prepare_benchmark_upstream_response_from_bytes(
    path: &str,
    headers: &HeaderMap,
    request_body: Bytes,
    version_label: &'static str,
) -> BenchmarkUpstreamResponse {
    let client_id = headers
        .get("x-client-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let program_header = headers
        .get("x-bench-program-header")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body_mode = headers
        .get("x-bench-body-mode")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(RequestBodyMode::HeadersOnly.label())
        .to_string();
    let response_body = if body_mode == RequestBodyMode::BodyRead.label() {
        Bytes::from(format!(
            "upstream-roundtrip|{path}|{}",
            String::from_utf8_lossy(&request_body)
        ))
    } else {
        Bytes::new()
    };
    BenchmarkUpstreamResponse {
        client_id,
        path: path.to_string(),
        program_header,
        body_mode,
        version_label,
        body: response_body,
    }
}

#[cfg(feature = "http3")]
async fn read_http3_bench_request_body(mut stream: Http3BenchRecvStream) -> Bytes {
    use hyper::body::Buf;

    let mut body = Vec::new();
    while let Some(mut chunk) = stream
        .recv_data()
        .await
        .expect("http3 upstream benchmark request body should read")
    {
        body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
    }
    Bytes::from(body)
}

#[cfg(feature = "http3")]
async fn write_http3_bench_response(
    stream: &mut Http3BenchSendStream,
    response: BenchmarkUpstreamResponse,
) {
    let body = response.body.clone();
    stream
        .send_response(response.into_http3_head())
        .await
        .expect("http3 upstream benchmark response head should send");
    if !body.is_empty() {
        stream
            .send_data(body)
            .await
            .expect("http3 upstream benchmark response body should send");
    }
    stream
        .finish()
        .await
        .expect("http3 upstream benchmark response should finish");
}

#[cfg(feature = "http3")]
async fn serve_http3_bench_fixture_connection(connection: quinn::Connection) {
    let mut h3_conn = match h3::server::builder()
        .build(h3_quinn::Connection::new(connection))
        .await
    {
        Ok(connection) => connection,
        Err(err) => {
            let rendered = err.to_string();
            if !err.is_h3_no_error()
                && !rendered.contains("ApplicationClose")
                && !rendered.contains("ConnectionLost")
            {
                eprintln!("http3 upstream benchmark connection init failed: {err}");
            }
            return;
        }
    };

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                tokio::spawn(async move {
                    let (request, stream) = match resolver.resolve_request().await {
                        Ok(value) => value,
                        Err(err) => {
                            let rendered = err.to_string();
                            if !err.is_h3_no_error()
                                && !rendered.contains("ApplicationClose")
                                && !rendered.contains("ConnectionLost")
                            {
                                eprintln!(
                                    "http3 upstream benchmark request resolution failed: {err}"
                                );
                            }
                            return;
                        }
                    };
                    let (parts, _) = request.into_parts();
                    let path = parts.uri.path().to_string();
                    let headers = parts.headers.clone();
                    let (mut send_stream, recv_stream) = stream.split();
                    let request_body = read_http3_bench_request_body(recv_stream).await;
                    if path == "/slow" {
                        sleep(Duration::from_millis(75)).await;
                    }
                    let response = prepare_benchmark_upstream_response_from_bytes(
                        &path,
                        &headers,
                        request_body,
                        "3",
                    );
                    write_http3_bench_response(&mut send_stream, response).await;
                });
            }
            Ok(None) => break,
            Err(err) => {
                let rendered = err.to_string();
                if err.is_h3_no_error()
                    || rendered.contains("ApplicationClose")
                    || rendered.contains("ConnectionLost")
                    || rendered.contains("H3_CLOSED_CRITICAL_STREAM")
                    || rendered.contains("control stream was closed")
                {
                    break;
                }
                panic!("http3 upstream benchmark connection should stay healthy: {err}");
            }
        }
    }
}

#[cfg(feature = "http3")]
async fn spawn_https_http3_upstream_fixture() -> Result<UpstreamFixture, Box<dyn std::error::Error>>
{
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let addr = socket.local_addr()?;
    let std_socket = socket.into_std()?;
    tune_udp_socket_buffers(&std_socket)?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(build_http3_upstream_server_config()),
        std_socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    let connection_count = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn({
        let connection_count = connection_count.clone();
        async move {
            while let Some(incoming) = endpoint.accept().await {
                let connection_count = connection_count.clone();
                tokio::spawn(async move {
                    let connection = incoming
                        .await
                        .expect("http3 upstream benchmark QUIC handshake should succeed");
                    connection_count.fetch_add(1, Ordering::Relaxed);
                    serve_http3_bench_fixture_connection(connection).await;
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

#[cfg(not(feature = "http3"))]
async fn spawn_https_http3_upstream_fixture() -> Result<UpstreamFixture, Box<dyn std::error::Error>>
{
    Err(io::Error::other(HTTP3_TLS_FEATURE_HINT).into())
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
    client: &ScenarioHttpClient,
    scenario: Scenario,
    requests: usize,
    concurrency: usize,
    collect_latencies: bool,
) -> Result<LoadRunResult, Box<dyn std::error::Error>> {
    let shared_counter = Arc::new(AtomicUsize::new(0));
    let worker_count = concurrency.max(1);
    let started = Instant::now();
    let body_mode = scenario.request_body_mode();
    let client = client.clone();
    let mut tasks = JoinSet::new();

    for worker_index in 0..worker_count {
        let shared_counter = shared_counter.clone();
        let client = client.for_worker(worker_index);
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
                match client.send_request("/perf", "perf-client", body_mode).await {
                    Ok(response) => {
                        *worker.status_counts.entry(response.status).or_insert(0) += 1;
                        if collect_latencies {
                            worker
                                .latencies_us
                                .push(request_started.elapsed().as_micros() as u64);
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

fn reserve_loopback_udp_addr() -> Result<SocketAddr, io::Error> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    let addr = socket.local_addr()?;
    drop(socket);
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
  - HTTP/3 scenarios use HTTPS over QUIC with ALPN-negotiated h3 only.\n\
  - {HTTP2_TLS_FEATURE_HINT}\n\
  - {HTTP3_TLS_FEATURE_HINT}\n"
    );
}
