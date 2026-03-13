#![cfg(feature = "http")]

#[path = "proxy_tests/attach_transport.rs"]
mod attach_transport;
#[path = "proxy_tests/control_plane.rs"]
mod control_plane;
#[path = "proxy_tests/debug.rs"]
mod debug;
#[cfg(feature = "tls")]
#[path = "proxy_tests/forward_proxy.rs"]
mod forward_proxy;
#[path = "proxy_tests/http.rs"]
mod http;
#[cfg(feature = "http3")]
#[path = "support/http3_support.rs"]
mod http3_support;
#[path = "proxy_tests/io.rs"]
mod io;
#[path = "proxy_tests/support.rs"]
mod support;
#[cfg(feature = "tls")]
#[path = "proxy_tests/tls.rs"]
mod tls;
#[path = "proxy_tests/transport.rs"]
mod transport;
#[cfg(feature = "webrtc")]
#[path = "proxy_tests/webrtc.rs"]
mod webrtc;
#[cfg(feature = "websocket")]
#[path = "proxy_tests/websocket.rs"]
mod websocket;
