use std::time::{Duration, Instant};

use pd_host_function::pd_host_function;

use super::{borrow_arg, take_arg};
use crate::builtins::runtime::VmMap;
use crate::vm::{CaptureAsyncHostContext, Vm, VmError, VmResult};

mod config;
pub(super) mod policy;
pub(super) mod request;
pub(super) mod sse;

pub use config::HttpConfig;
use policy::{ConnectionAdmission, ConnectionPermit};

const DEFAULT_MAX_HTTP_IN_FLIGHT: usize = 64;

#[derive(Clone)]
pub(crate) struct HttpHostState {
    config: Option<HttpConfig>,
    admission: ConnectionAdmission,
}

/// HTTP host configuration owned by the HTTP host implementation.
pub trait HttpHostExt {
    fn configure_http(&mut self, config: HttpConfig) -> VmResult<()>;
    fn set_http_max_in_flight(&mut self, max_in_flight: usize);
    fn http_max_in_flight(&self) -> usize;
    fn clear_http_configuration(&mut self);
    fn http_is_configured(&self) -> bool;
}

impl HttpHostExt for Vm {
    fn configure_http(&mut self, config: HttpConfig) -> VmResult<()> {
        config.validate()?;
        let admission = self
            .host
            .host_function_state::<HttpHostState>()
            .map_or_else(
                || ConnectionAdmission::new(DEFAULT_MAX_HTTP_IN_FLIGHT),
                |state| state.admission.clone(),
            );
        self.host.set_host_function_state(HttpHostState {
            config: Some(config),
            admission,
        });
        Ok(())
    }

    fn set_http_max_in_flight(&mut self, max_in_flight: usize) {
        if self.host.host_function_state::<HttpHostState>().is_none() {
            self.host.set_host_function_state(HttpHostState {
                config: None,
                admission: ConnectionAdmission::new(DEFAULT_MAX_HTTP_IN_FLIGHT),
            });
        }
        self.host
            .host_function_state_mut::<HttpHostState>()
            .expect("HTTP host state was inserted")
            .admission
            .set_max_in_flight(max_in_flight);
    }

    fn http_max_in_flight(&self) -> usize {
        self.host
            .host_function_state::<HttpHostState>()
            .map_or(DEFAULT_MAX_HTTP_IN_FLIGHT, |state| {
                state.admission.max_in_flight()
            })
    }

    fn clear_http_configuration(&mut self) {
        if let Some(state) = self.host.host_function_state_mut::<HttpHostState>() {
            state.config = None;
        }
    }

    fn http_is_configured(&self) -> bool {
        self.host
            .host_function_state::<HttpHostState>()
            .and_then(|state| state.config.as_ref())
            .is_some()
    }
}

pub(super) struct HttpRequestContext {
    config: HttpConfig,
    _permit: ConnectionPermit,
}

impl HttpRequestContext {
    fn capture_stream(
        vm: &mut Vm,
        script_timeout: Option<Duration>,
        protocol: &str,
    ) -> VmResult<(Self, Instant)> {
        let state = vm
            .host
            .host_function_state::<HttpHostState>()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let config = state
            .config
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let admitted_at = Instant::now();
        if script_timeout.is_some_and(|timeout| admitted_at.checked_add(timeout).is_none()) {
            return Err(VmError::HostError(format!(
                "{protocol} timeout_ms cannot form a deadline"
            )));
        }
        let duration = script_timeout.map_or(config.max_stream_duration, |timeout| {
            timeout.min(config.max_stream_duration)
        });
        let deadline = admitted_at.checked_add(duration).ok_or_else(|| {
            VmError::HostError("HTTP max_stream_duration cannot form a deadline".to_string())
        })?;
        let permit = state.admission.acquire()?;
        Ok((
            Self {
                config,
                _permit: permit,
            },
            deadline,
        ))
    }
}

impl CaptureAsyncHostContext for HttpRequestContext {
    fn capture(vm: &mut Vm) -> VmResult<Self> {
        let state = vm
            .host
            .host_function_state::<HttpHostState>()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let config = state
            .config
            .clone()
            .ok_or_else(|| VmError::HostError("HTTP host is not configured".to_string()))?;
        let permit = state.admission.acquire()?;
        Ok(Self {
            config,
            _permit: permit,
        })
    }
}

/// Starts an HTTP request under the VM's configured network policy.
///
/// The request map accepts `method`, `url`, optional `headers`, and optional `body`.
/// The response map contains `status`, `headers`, `body`, and the final `url`.
#[pd_host_function(name = "http::client::request")]
pub(super) async fn builtin_http_client_request(
    #[pd_host_context] context: HttpRequestContext,
    request: VmMap,
) -> VmResult<VmMap> {
    request::perform_buffered_request(context, request).await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::policy::{
        SchemeFamily, is_restricted_ip, request_deadline, validate_resolved_addresses,
        validate_url, validate_url_policy,
    };
    use super::request::{
        HttpRequest, ResponseReadObserver, execute_request, execute_request_with_observer,
        execute_request_with_tls_config, pending_connection_test,
    };
    use super::{HttpConfig, HttpHostExt, HttpRequestContext, builtin_http_client_request};
    use crate::builtins::runtime::VmMap;
    use crate::vm::{
        CallOutcome, CallReturn, HostAsyncBridge, HostFuture, HostOpId, Value, VmResult,
    };

    #[test]
    fn default_http_policy_denies_all_hosts() {
        let config = HttpConfig::default();
        assert_eq!(config.allowed_schemes, ["https"]);
        assert!(config.allowed_hosts.is_empty());
        assert!(config.allowed_ports.is_empty());
        assert!(!config.allow_private_ips);
        config.validate().expect("default bounds should be valid");
    }

    #[test]
    fn stream_timeout_validation_precedes_permit_admission() {
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.set_http_max_in_flight(0);
        vm.configure_http(HttpConfig::default())
            .expect("default config should be valid");

        let error = HttpRequestContext::capture_stream(&mut vm, Some(Duration::MAX), "SSE")
            .err()
            .expect("an unrepresentable script timeout should be rejected");
        assert!(error.to_string().contains("timeout_ms"), "{error}");
        assert!(
            !error.to_string().contains("in-flight request limit"),
            "deadline validation must happen before permit admission: {error}"
        );
    }

    #[test]
    fn http_scheme_family_rejects_non_http_schemes() {
        let config = HttpConfig {
            allowed_schemes: vec!["http".into(), "https".into(), "ftp".into()],
            allowed_hosts: vec!["example.com".into()],
            allowed_ports: vec![80, 443],
            ..HttpConfig::default()
        };
        let http: url::Url = "https://example.com/".parse().expect("valid URL");
        let ftp: url::Url = "ftp://example.com/".parse().expect("valid URL");
        assert!(validate_url_policy(&config, SchemeFamily::Http, &http).is_ok());
        assert!(validate_url_policy(&config, SchemeFamily::Http, &ftp).is_err());
    }

    #[test]
    fn request_submits_future_to_host_driver_without_runtime_operation() {
        use std::task::{Context, Poll};

        struct RecordingBridge {
            submitted: Arc<Mutex<Option<(HostOpId, HostFuture)>>>,
        }

        impl HostAsyncBridge for RecordingBridge {
            fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
                *self.submitted.lock().expect("submission lock") = Some((op_id, future));
                Ok(())
            }

            fn poll_op(
                &mut self,
                _op_id: HostOpId,
                _cx: &mut Context<'_>,
            ) -> Poll<VmResult<CallReturn>> {
                Poll::Pending
            }
        }

        let submitted = Arc::new(Mutex::new(None));
        let mut vm = crate::vm::Vm::new(crate::vm::Program::new(Vec::new(), Vec::new()));
        vm.configure_http(HttpConfig::default())
            .expect("default config should be valid");
        vm.set_async_bridge(Box::new(RecordingBridge {
            submitted: Arc::clone(&submitted),
        }));
        let args = [Value::Map(Arc::new(VmMap::default()))];

        let outcome = builtin_http_client_request(&mut vm, &args)
            .expect("HTTP async host call should submit");
        let CallOutcome::Pending(op_id) = outcome else {
            panic!("HTTP async host call should suspend");
        };
        assert_eq!(op_id, 1);
        assert_eq!(
            submitted
                .lock()
                .expect("submission lock")
                .as_ref()
                .map(|(submitted_id, _)| *submitted_id),
            Some(op_id)
        );
        assert_eq!(vm.execution_scope().operations().active_count(), 0);
    }

    #[test]
    fn production_request_timeout_covers_delayed_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().expect("request should connect");
            std::thread::sleep(Duration::from_millis(100));
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(20),
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        let error = runtime
            .block_on(super::policy::with_deadline(
                request_deadline(config.request_timeout).expect("valid request deadline"),
                execute_request(&config, &request),
            ))
            .expect_err("hanging server should time out");
        assert!(error.to_string().contains("deadline exceeded"));
        server.join().expect("server should exit");
    }

    #[test]
    fn response_body_timeout_uses_the_same_total_deadline() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("request should connect");
            let mut request = [0u8; 1024];
            let _ = socket
                .read(&mut request)
                .expect("request should be readable");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                .expect("headers should be written");
            socket.flush().expect("headers should flush");
            std::thread::sleep(Duration::from_millis(100));
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(20),
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        let error = runtime
            .block_on(super::policy::with_deadline(
                request_deadline(config.request_timeout).expect("valid request deadline"),
                execute_request(&config, &request),
            ))
            .expect_err("stalled response body should time out");
        assert!(error.to_string().contains("deadline exceeded"));
        server.join().expect("server should exit");
    }

    #[test]
    fn redirects_revalidate_policy_and_strip_cross_origin_credentials() {
        use std::io::{Read, Write};

        let first = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let first_address = first.local_addr().expect("listener should have address");
        let second = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let second_address = second.local_addr().expect("listener should have address");
        let first_server = std::thread::spawn(move || {
            let (mut socket, _) = first.accept().expect("request should connect");
            let mut bytes = [0_u8; 2048];
            let read = socket.read(&mut bytes).expect("request should be readable");
            let request = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
            assert!(request.contains("authorization: bearer secret"));
            assert!(request.contains("cookie: session=secret"));
            write!(
                socket,
                "HTTP/1.1 302 Found\r\nLocation: http://{second_address}/final\r\nContent-Length: 0\r\n\r\n"
            )
            .expect("redirect should be writable");
        });
        let second_server = std::thread::spawn(move || {
            let (mut socket, _) = second.accept().expect("request should connect");
            let mut bytes = [0_u8; 2048];
            let read = socket.read(&mut bytes).expect("request should be readable");
            let request = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("cookie:"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("response should be writable");
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![first_address.port(), second_address.port()],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("http://{first_address}/")
                .parse()
                .expect("valid URL"),
            headers: vec![
                (
                    hyper::header::AUTHORIZATION,
                    hyper::header::HeaderValue::from_static("Bearer secret"),
                ),
                (
                    hyper::header::COOKIE,
                    hyper::header::HeaderValue::from_static("session=secret"),
                ),
            ],
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let response = runtime
            .block_on(super::policy::with_deadline(
                request_deadline(config.request_timeout).expect("valid request deadline"),
                execute_request(&config, &request),
            ))
            .expect("redirected request should complete");
        assert_eq!(
            response.get(&Value::string("status")),
            Some(&Value::Int(200))
        );
        assert_eq!(
            response.get(&Value::string("url")),
            Some(&Value::string(format!("http://{second_address}/final")))
        );
        first_server.join().expect("first server should exit");
        second_server.join().expect("second server should exit");
    }

    #[test]
    fn redirect_destination_is_revalidated_before_connection() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("request should connect");
            let mut request = [0_u8; 1024];
            let _ = socket
                .read(&mut request)
                .expect("request should be readable");
            write!(
                socket,
                "HTTP/1.1 302 Found\r\nLocation: http://localhost:{}/blocked\r\nContent-Length: 0\r\n\r\n",
                address.port()
            )
            .expect("redirect should be writable");
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let error = runtime
            .block_on(super::policy::with_deadline(
                request_deadline(config.request_timeout).expect("valid request deadline"),
                execute_request(&config, &request),
            ))
            .expect_err("redirect target should be denied");
        assert!(error.to_string().contains("target host is not allowed"));
        server.join().expect("server should exit");
    }

    fn assert_redirect_userinfo_is_rejected(userinfo: &str) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let userinfo = userinfo.to_string();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("first request should connect");
            let mut request = [0_u8; 2048];
            let read = socket
                .read(&mut request)
                .expect("request should be readable");
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains(&userinfo.to_ascii_lowercase()));
            write!(
                socket,
                "HTTP/1.1 302 Found\r\nLocation: http://{userinfo}@{address}/blocked\r\nContent-Length: 0\r\n\r\n"
            )
            .expect("redirect should be writable");
            drop(socket);

            listener
                .set_nonblocking(true)
                .expect("listener should become nonblocking");
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                match listener.accept() {
                    Ok(_) => panic!("redirect userinfo must be rejected before a second request"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("unexpected accept error: {error}"),
                }
            }
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        let error = runtime
            .block_on(super::policy::with_deadline(
                request_deadline(config.request_timeout).expect("valid request deadline"),
                execute_request(&config, &request),
            ))
            .expect_err("redirect userinfo should be denied");
        assert!(error.to_string().contains("URL userinfo is not allowed"));
        server.join().expect("server should exit");
    }

    #[test]
    fn redirect_username_is_rejected_before_a_second_request() {
        assert_redirect_userinfo_is_rejected("redirect-user");
    }

    #[test]
    fn redirect_username_and_password_are_rejected_before_a_second_request() {
        assert_redirect_userinfo_is_rejected("redirect-user:redirect-password");
    }

    fn execute_fixture_response_fragments_for(
        method: hyper::Method,
        response: Vec<&'static [u8]>,
        max_response_body_bytes: usize,
    ) -> (VmResult<VmMap>, ResponseReadObserver) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("request should connect");
            let mut request = [0_u8; 2048];
            let _ = socket
                .read(&mut request)
                .expect("request should be readable");
            for fragment in response {
                socket
                    .write_all(fragment)
                    .expect("response fragment should be writable");
                socket.flush().expect("response fragment should flush");
            }
        });
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            max_response_body_bytes,
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let observer = ResponseReadObserver::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let result = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(500),
                execute_request_with_observer(&config, &request, observer.clone()),
            )
            .await
            .expect("fixture response must make progress without deadline fallback")
        });
        server.join().expect("server should exit");
        (result, observer)
    }

    fn execute_fixture_response_fragments(
        response: Vec<&'static [u8]>,
        max_response_body_bytes: usize,
    ) -> (VmResult<VmMap>, ResponseReadObserver) {
        execute_fixture_response_fragments_for(
            hyper::Method::GET,
            response,
            max_response_body_bytes,
        )
    }

    fn execute_fixture_response(
        response: &'static [u8],
        max_response_body_bytes: usize,
    ) -> (VmResult<VmMap>, ResponseReadObserver) {
        execute_fixture_response_fragments(vec![response], max_response_body_bytes)
    }

    #[test]
    fn continue_then_final_response_in_one_write_reaches_the_final_head() {
        let (result, observer) = execute_fixture_response(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            8,
        );
        let response = result.expect("final response should complete after 100 Continue");
        assert_eq!(
            response.get(&Value::string("status")),
            Some(&Value::Int(200))
        );
        assert_eq!(
            response.get(&Value::string("body")),
            Some(&Value::bytes(b"ok".to_vec()))
        );
        assert!(observer.body_read_calls() > 0);
    }

    #[test]
    fn fragmented_continue_then_final_response_reaches_the_final_head() {
        let (result, _) = execute_fixture_response_fragments(
            vec![
                b"HTTP/1.1 100 Cont",
                b"inue\r\n",
                b"X-Info: yes\r\n\r",
                b"\nHTTP/1.1 200 O",
                b"K\r\nContent-Length: 2\r\n\r\n",
                b"ok",
            ],
            8,
        );
        let response = result.expect("fragmented final response should complete after 100");
        assert_eq!(
            response.get(&Value::string("body")),
            Some(&Value::bytes(b"ok".to_vec()))
        );
    }

    #[test]
    fn early_hints_then_final_response_in_one_write_reaches_the_final_head() {
        let (result, _) = execute_fixture_response(
            b"HTTP/1.1 103 Early Hints\r\nLink: </style.css>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            8,
        );
        let response = result.expect("final response should complete after 103 Early Hints");
        assert_eq!(
            response.get(&Value::string("status")),
            Some(&Value::Int(200))
        );
    }

    #[test]
    fn fragmented_early_hints_then_final_response_reaches_the_final_head() {
        let (result, _) = execute_fixture_response_fragments(
            vec![
                b"HTTP/1.1 103 Early Hints\r\n",
                b"Link: </a>\r\n\r\nHTTP/1.1 ",
                b"200 OK\r\nContent-Length: 2\r\n",
                b"\r\nok",
            ],
            8,
        );
        let response = result.expect("fragmented final response should complete after 103");
        assert_eq!(
            response.get(&Value::string("body")),
            Some(&Value::bytes(b"ok".to_vec()))
        );
    }

    fn tls_fixture_configs() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("test certificate should generate");
        let cert_der = certified.cert.der().clone();
        let key_der =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("test server certificate should configure");
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(cert_der)
            .expect("test certificate should be trusted");
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        assert!(client_config.alpn_protocols.is_empty());
        (Arc::new(server_config), Arc::new(client_config))
    }

    #[test]
    fn https_requires_http11_alpn_and_preserves_sni_host_and_query() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let (server_config, client_config) = tls_fixture_configs();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("TLS listener should bind");
        let address = listener.local_addr().expect("TLS listener address");
        let server = runtime.spawn(async move {
            let (stream, _) = listener.accept().await.expect("TLS request should connect");
            let mut stream = tokio_rustls::TlsAcceptor::from(server_config)
                .accept(stream)
                .await
                .expect("TLS handshake should succeed");
            assert_eq!(
                stream.get_ref().1.alpn_protocol(),
                Some(b"http/1.1".as_slice())
            );
            assert_eq!(
                stream
                    .get_ref()
                    .1
                    .server_name()
                    .expect("client should send SNI"),
                "localhost"
            );
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            loop {
                let read = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer)
                    .await
                    .expect("HTTPS request should be readable");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("request should be ASCII");
            assert!(request.starts_with("GET /resource?q=rust HTTP/1.1\r\n"));
            assert!(request.contains(&format!("host: localhost:{}\r\n", address.port())));
            tokio::io::AsyncWriteExt::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .await
            .expect("HTTPS response should be writable");
        });
        let config = HttpConfig {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: vec!["localhost".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            max_response_body_bytes: 2,
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("https://localhost:{}/resource?q=rust", address.port())
                .parse()
                .expect("valid HTTPS URL"),
            headers: Vec::new(),
            body: None,
        };
        let observer = ResponseReadObserver::default();
        let response = runtime
            .block_on(execute_request_with_tls_config(
                &config,
                &request,
                observer.clone(),
                client_config,
            ))
            .expect("HTTPS request should complete");
        assert_eq!(
            response.get(&Value::string("body")),
            Some(&Value::bytes(b"ok".to_vec()))
        );
        assert!(observer.max_raw_transport_read() > 0);
        assert!(observer.max_raw_transport_read() <= 16_384 + 2_048 + 5);
        runtime
            .block_on(server)
            .expect("TLS server should complete");
    }

    #[test]
    fn accepted_tcp_with_stalled_tls_uses_the_connection_stage_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().expect("TCP client should connect");
            std::thread::sleep(Duration::from_millis(200));
        });
        let config = HttpConfig {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            connect_timeout: Duration::from_millis(30),
            request_timeout: Duration::from_secs(1),
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("https://{address}/")
                .parse()
                .expect("valid HTTPS URL"),
            headers: Vec::new(),
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let started = Instant::now();
        let error = runtime
            .block_on(execute_request(&config, &request))
            .expect_err("stalled TLS must time out");
        assert!(error.to_string().contains("deadline exceeded"));
        assert!(started.elapsed() < Duration::from_millis(150));
        server.join().expect("server should exit");
    }

    #[test]
    fn request_deadline_caps_the_connection_stage_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let address = listener.local_addr().expect("listener address");
        let server = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().expect("TCP client should connect");
            std::thread::sleep(Duration::from_millis(200));
        });
        let config = HttpConfig {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![address.port()],
            allow_private_ips: true,
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_millis(30),
            ..HttpConfig::default()
        };
        let request = HttpRequest {
            method: hyper::Method::GET,
            url: format!("https://{address}/")
                .parse()
                .expect("valid HTTPS URL"),
            headers: Vec::new(),
            body: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let started = Instant::now();
        let error = runtime
            .block_on(execute_request(&config, &request))
            .expect_err("request deadline must cap stalled TLS");
        assert!(error.to_string().contains("deadline exceeded"));
        assert!(started.elapsed() < Duration::from_millis(150));
        server.join().expect("server should exit");
    }

    #[test]
    fn dropping_host_future_aborts_connection_and_closes_peer_promptly() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime should build");
        runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let (response_written, response_ready) = tokio::sync::oneshot::channel();
            let mut pending = pending_connection_test(
                client,
                "http://example.test/pending".parse().expect("valid URL"),
            );
            let task = tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 256];
                loop {
                    let read = tokio::io::AsyncReadExt::read(&mut server, &mut buffer)
                        .await
                        .expect("request should be readable");
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                tokio::io::AsyncWriteExt::write_all(
                    &mut server,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\na",
                )
                .await
                .expect("partial response should be writable");
                response_written
                    .send(())
                    .expect("response readiness should be observed");
                let read = tokio::time::timeout(
                    Duration::from_millis(100),
                    tokio::io::AsyncReadExt::read(&mut server, &mut buffer),
                )
                .await
                .expect("peer EOF should be prompt")
                .expect("peer EOF read should succeed");
                assert_eq!(read, 0);
            });
            assert!(
                futures_util::poll!(&mut pending.future).is_pending(),
                "request should remain pending on the partial body"
            );
            response_ready
                .await
                .expect("partial response should become ready");
            assert!(
                futures_util::poll!(&mut pending.future).is_pending(),
                "request should still await the remaining body"
            );
            drop(pending);
            task.await.expect("peer should observe EOF");
        });
    }

    #[test]
    fn head_and_bodyless_statuses_ignore_declared_body_lengths() {
        for (method, response, expected_status) in [
            (
                hyper::Method::HEAD,
                b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n".as_slice(),
                200,
            ),
            (
                hyper::Method::GET,
                b"HTTP/1.1 204 No Content\r\nContent-Length: 999\r\n\r\n".as_slice(),
                204,
            ),
            (
                hyper::Method::GET,
                b"HTTP/1.1 304 Not Modified\r\nContent-Length: 999\r\n\r\n".as_slice(),
                304,
            ),
        ] {
            let (result, observer) =
                execute_fixture_response_fragments_for(method, vec![response], 1);
            let response = result.expect("bodyless response should succeed");
            assert_eq!(
                response.get(&Value::string("status")),
                Some(&Value::Int(expected_status))
            );
            assert_eq!(
                response.get(&Value::string("body")),
                Some(&Value::bytes(Vec::new()))
            );
            assert_eq!(observer.body_read_calls(), 0);
        }
    }

    #[test]
    fn chunked_response_accepts_trailers_without_adding_them_to_the_body() {
        let (result, _) = execute_fixture_response_fragments(
            vec![
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-Checksum\r\n\r\n",
                b"2\r\nok\r\n0\r\nX-Checksum: yes\r\n\r\n",
            ],
            2,
        );
        let response = result.expect("chunked response with trailers should succeed");
        assert_eq!(
            response.get(&Value::string("body")),
            Some(&Value::bytes(b"ok".to_vec()))
        );
    }

    #[test]
    fn truncated_content_length_propagates_a_body_or_connection_error() {
        let (result, _) = execute_fixture_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nok",
            8,
        );
        let error = result.expect_err("truncated response body must fail");
        let message = error.to_string();
        assert!(
            message.contains("response read failed") || message.contains("connection failed"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn oversized_response_head_is_rejected_by_the_hyper_buffer_bound() {
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nX-Oversized: {}\r\nContent-Length: 0\r\n\r\n",
            "a".repeat(70 * 1024)
        );
        let response: &'static [u8] = Box::leak(oversized.into_bytes().into_boxed_slice());
        let (result, _) = execute_fixture_response(response, 1);
        let error = result.expect_err("oversized response head must fail");
        let message = error.to_string();
        assert!(
            message.contains("HTTP request failed")
                || message.contains("connection failed before the response"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn declared_oversized_body_is_rejected_before_body_transport_polling() {
        let (result, observer) = execute_fixture_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde",
            4,
        );
        let error = result.expect_err("declared oversized body must fail");
        assert!(error.to_string().contains("response body exceeds limit"));
        assert_eq!(observer.body_read_calls(), 0);
        assert_eq!(observer.max_body_transport_read(), 0);
    }

    #[test]
    fn chunked_single_write_is_observed_only_through_remaining_plus_sentinel() {
        let (result, observer) = execute_fixture_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nabcde\r\n0\r\n\r\n",
            4,
        );
        let error = result.expect_err("chunked limit plus one must fail");
        assert!(error.to_string().contains("response body exceeds limit"));
        assert!(observer.body_read_calls() > 0);
        assert!(observer.max_body_transport_read() <= 5);
        assert!(observer.max_application_chunk() <= 5);
    }

    #[test]
    fn unknown_length_body_at_exact_limit_succeeds() {
        let (result, observer) =
            execute_fixture_response(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcd", 4);
        let response = result.expect("exact-limit body should succeed");
        assert_eq!(
            response.get(&Value::string("body")),
            Some(&Value::bytes(b"abcd".to_vec()))
        );
        assert!(observer.max_body_transport_read() <= 5);
        assert!(observer.max_application_chunk() <= 4);
    }

    #[test]
    fn unknown_length_body_at_limit_plus_one_reads_only_the_sentinel() {
        let (result, observer) =
            execute_fixture_response(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcde", 4);
        let error = result.expect_err("limit plus one body must fail");
        assert!(error.to_string().contains("response body exceeds limit"));
        assert!(observer.max_body_transport_read() <= 5);
        assert!(observer.max_application_chunk() <= 5);
    }

    #[test]
    fn empty_port_allowlist_rejects_explicit_and_default_ports() {
        let config = HttpConfig {
            allowed_schemes: vec!["https".to_string()],
            allowed_hosts: vec!["example.com".to_string()],
            ..HttpConfig::default()
        };
        let explicit = "https://example.com:443/".parse().expect("valid URL");
        let default_port = "https://example.com/".parse().expect("valid URL");
        assert!(validate_url(&config, SchemeFamily::Http, &explicit).is_err());
        assert!(validate_url(&config, SchemeFamily::Http, &default_port).is_err());
    }

    #[test]
    fn pinned_resolution_preserves_the_original_host_and_validated_address() {
        let config = HttpConfig {
            allowed_schemes: vec!["http".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![8080],
            allow_private_ips: true,
            ..HttpConfig::default()
        };
        let url = "http://127.0.0.1:8080/".parse().expect("valid pinned URL");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");

        let target = runtime
            .block_on(super::policy::resolve_url(
                &config,
                SchemeFamily::Http,
                &url,
            ))
            .expect("target should resolve under policy");

        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.address, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn special_use_networks_and_mixed_dns_answers_are_restricted() {
        for address in [
            "0.1.2.3",
            "100.64.0.1",
            "192.0.0.8",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.1",
            "192.175.48.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "2620:4f:8000::1",
            "3fff::1",
            "fc00::1",
        ] {
            assert!(
                is_restricted_ip(address.parse().expect("valid IP")),
                "{address} must be restricted"
            );
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                !is_restricted_ip(address.parse().expect("valid IP")),
                "{address} must remain globally routable"
            );
        }

        let config = HttpConfig::default();
        let addresses = [
            "8.8.8.8:443".parse().expect("valid socket address"),
            "100.64.0.1:443".parse().expect("valid socket address"),
        ];
        assert!(validate_resolved_addresses(&config, &addresses).is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_is_restricted() {
        assert!(is_restricted_ip(
            "::ffff:127.0.0.1".parse().expect("valid IP")
        ));
    }
}
