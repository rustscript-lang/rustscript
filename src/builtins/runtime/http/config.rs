use std::time::Duration;

use crate::vm::{VmError, VmResult};

/// Bounded network policy for the built-in HTTP client and future streaming adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    pub allowed_schemes: Vec<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_ports: Vec<u16>,
    pub max_redirects: usize,
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub allow_private_ips: bool,
    pub max_stream_item_bytes: usize,
    pub max_stream_total_bytes: usize,
    pub max_sse_line_bytes: usize,
    pub max_websocket_frame_bytes: usize,
    pub max_websocket_send_bytes: usize,
    pub stream_idle_timeout: Duration,
    pub websocket_close_timeout: Duration,
}

impl HttpConfig {
    /// Validates limits that must remain positive for every streaming adapter.
    pub fn validate(&self) -> VmResult<()> {
        let positive_limits = [
            ("max_stream_item_bytes", self.max_stream_item_bytes),
            ("max_stream_total_bytes", self.max_stream_total_bytes),
            ("max_sse_line_bytes", self.max_sse_line_bytes),
            ("max_websocket_frame_bytes", self.max_websocket_frame_bytes),
            ("max_websocket_send_bytes", self.max_websocket_send_bytes),
        ];
        if let Some((name, _)) = positive_limits.iter().find(|(_, value)| *value == 0) {
            return Err(VmError::HostError(format!(
                "HTTP configuration field '{name}' must be positive"
            )));
        }
        let positive_timeouts = [
            ("connect_timeout", self.connect_timeout),
            ("request_timeout", self.request_timeout),
            ("stream_idle_timeout", self.stream_idle_timeout),
            ("websocket_close_timeout", self.websocket_close_timeout),
        ];
        if let Some((name, _)) = positive_timeouts
            .iter()
            .find(|(_, timeout)| timeout.is_zero())
        {
            return Err(VmError::HostError(format!(
                "HTTP configuration field '{name}' must be positive"
            )));
        }
        if let Some((name, _)) = positive_timeouts
            .iter()
            .find(|(_, timeout)| std::time::Instant::now().checked_add(*timeout).is_none())
        {
            return Err(VmError::HostError(format!(
                "HTTP configuration field '{name}' is too large"
            )));
        }
        Ok(())
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            allowed_schemes: vec!["https".to_string(), "wss".to_string()],
            allowed_hosts: Vec::new(),
            allowed_ports: Vec::new(),
            max_redirects: 5,
            max_request_body_bytes: 1024 * 1024,
            max_response_body_bytes: 8 * 1024 * 1024,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            allow_private_ips: false,
            max_stream_item_bytes: 1024 * 1024,
            max_stream_total_bytes: 64 * 1024 * 1024,
            max_sse_line_bytes: 64 * 1024,
            max_websocket_frame_bytes: 1024 * 1024,
            max_websocket_send_bytes: 1024 * 1024,
            stream_idle_timeout: Duration::from_secs(30),
            websocket_close_timeout: Duration::from_secs(5),
        }
    }
}
