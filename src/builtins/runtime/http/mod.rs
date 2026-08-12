use pd_host_function::pd_host_function;

use crate::builtins::runtime::VmMap;
use crate::vm::{CaptureAsyncHostContext, Vm, VmError, VmResult};

mod config;
pub(super) mod policy;
pub(super) mod request;

pub use config::HttpConfig;
use policy::{ConnectionAdmission, ConnectionPermit};

const DEFAULT_MAX_HTTP_IN_FLIGHT: usize = 64;

struct HttpHostState {
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
    };
    use super::{HttpConfig, HttpHostExt, builtin_http_client_request};
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
    fn scheme_families_are_protocol_specific() {
        let config = HttpConfig {
            allowed_schemes: vec!["http".into(), "https".into(), "ws".into(), "wss".into()],
            allowed_hosts: vec!["example.com".into()],
            allowed_ports: vec![80, 443],
            ..HttpConfig::default()
        };
        let http: url::Url = "https://example.com/".parse().expect("valid URL");
        let ws: url::Url = "wss://example.com/".parse().expect("valid URL");
        assert!(validate_url_policy(&config, SchemeFamily::Http, &http).is_ok());
        assert!(validate_url_policy(&config, SchemeFamily::WebSocket, &ws).is_ok());
        assert!(validate_url_policy(&config, SchemeFamily::Http, &ws).is_err());
        assert!(validate_url_policy(&config, SchemeFamily::WebSocket, &http).is_err());
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
        assert_eq!(vm.host.runtime_operations.active_count(), 0);
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

    fn execute_fixture_response(
        response: &'static [u8],
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
            socket
                .write_all(response)
                .expect("response should be writable");
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
            method: hyper::Method::GET,
            url: format!("http://{address}/").parse().expect("valid URL"),
            headers: Vec::new(),
            body: None,
        };
        let observer = ResponseReadObserver::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime should build");
        let result = runtime.block_on(execute_request_with_observer(
            &config,
            &request,
            observer.clone(),
        ));
        server.join().expect("server should exit");
        (result, observer)
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
    fn websocket_policy_keeps_host_port_and_address_checks() {
        let config = HttpConfig {
            allowed_schemes: vec!["wss".to_string()],
            allowed_hosts: vec!["example.com".to_string()],
            allowed_ports: vec![443],
            ..HttpConfig::default()
        };
        let denied_host = "wss://other.example/".parse().expect("valid URL");
        let denied_port = "wss://example.com:444/".parse().expect("valid URL");
        let private = "wss://127.0.0.1/".parse().expect("valid URL");
        assert!(validate_url(&config, SchemeFamily::WebSocket, &denied_host).is_err());
        assert!(validate_url(&config, SchemeFamily::WebSocket, &denied_port).is_err());
        let private_config = HttpConfig {
            allowed_schemes: vec!["wss".to_string()],
            allowed_hosts: vec!["127.0.0.1".to_string()],
            allowed_ports: vec![443],
            ..HttpConfig::default()
        };
        assert!(validate_url(&private_config, SchemeFamily::WebSocket, &private).is_err());
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
