`pd-edge/examples` is now grouped by runtime scope and behavior:

- `console/`: console-only RSS programs
- `http/proxy/`: HTTP proxy programs that read or shape downstream/upstream HTTP traffic
- `http/downstream/`: downstream protocol detection and listener-facing HTTP examples
- `http/upstream/`: outbound HTTP exchange and session reuse examples
- `proxy/forward/`: CONNECT and forward-proxy flows
- `proxy/tunnel/`: raw downstream to upstream tunnel flows
- `transport/io/`: explicit IO handle and mixed transport/http examples
- `transport/handoff/`: transport-to-HTTP handoff examples
- `transport/tls/`: raw TLS handshake and plaintext transport examples
- `transport/upstream/`: explicit upstream transport setup examples
- `websocket/proxy/`: WebSocket proxy examples
- `websocket/bridge/`: cross-protocol WebSocket bridge examples
- `webrtc/proxy/`: WebRTC proxy examples

The Rust Cargo entrypoints `build_sample_program.rs` and `http_proxy_perf_framework.rs` stay at the
top level so `cargo run -p pd-edge --example ...` keeps the same target names.
