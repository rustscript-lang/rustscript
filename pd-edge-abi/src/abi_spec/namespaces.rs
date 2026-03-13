#[cfg(feature = "http")]
edge_host_namespace!("http", "HTTP request, response, and upstream host namespace.");
edge_host_namespace!("console", "Console stdin/stdout/stderr and argv host namespace.");
edge_host_namespace!("rate_limit", "Rate limiting host namespace.");
edge_host_namespace!("runtime", "Runtime host namespace.");
edge_host_namespace!("tcp", "TCP stream host namespace.");
edge_host_namespace!("udp", "UDP datagram socket host namespace.");
#[cfg(feature = "tls")]
edge_host_namespace!("tls", "TLS session host namespace.");
#[cfg(feature = "websocket")]
edge_host_namespace!("websocket", "WebSocket connection host namespace.");
#[cfg(feature = "webrtc")]
edge_host_namespace!("webrtc", "WebRTC peer connection host namespace.");
edge_host_namespace!("proxy", "Generic proxy byte-stream host namespace.");
