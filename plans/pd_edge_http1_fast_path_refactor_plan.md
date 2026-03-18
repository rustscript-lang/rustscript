# PD-Edge HTTP/1 Fast-Path Refactor Plan

## Goal

- Improve the stock Harness A plaintext metric: `http_proxy / raw_http_upstream`
- Target `> 50%` on the stock benchmark row, not on stripped or diagnostic variants
- Remove high-overhead HTTP/1 proxy layers without worrying about backward compatibility

## Current Read

- Generic upstream HTTP still relies on `reqwest::Client` for fallback paths, custom-client TLS paths, and non-HTTP/1 fast-path cases
- A shared raw outbound HTTP/1 engine exists and is already used for eligible default-upstream and exchange forwarding over TCP and TLS
- Downstream HTTP/1 no longer always relies on Hyper: eligible HTTP/1 requests are handled by a lower-level loop, while Hyper remains the fallback for replay-only or non-HTTP/1 cases
- Upstream HTTP/2 and HTTP/3 already use lower-level session-oriented libraries and should not be rewritten as part of this effort
- The main remaining cost is now the generic graph and fallback path that still surrounds non-eligible traffic; the native HTTP/1 path now streams request bodies, can write snapshot-backed upstream responses directly on the low-level downstream loop, and no longer forces `read_all()` for the direct default-upstream reqwest fallback

## Architecture Direction

- Keep TLS on `rustls` / `tokio-rustls`
- Keep upstream HTTP/2 on Hyper conn-level APIs
- Keep upstream HTTP/3 on `h3` / `quinn`
- Remove `reqwest` from the outbound HTTP/1 hot path
- Replace Hyper downstream HTTP/1 handling for fast-path-eligible requests
- Keep the generic path only as fallback for complex or mutating cases
- Keep one shared outbound HTTP/1 engine for both exchange HTTP and default-upstream HTTP
- Allow downstream HTTP handling to remain a separate pipeline from outbound HTTP

## Workstream 1: Remove Proven Wrong Directions

- Review recent perf-related changes and delete anything that was only diagnostic or unstable
- Remove abstractions that force generic request materialization, reparsing, or snapshotting on the hot path
- Keep only the changes that have held up:
- structured targets
- isolated upstream benchmark fixture
- pooled-sender lease fix for non-empty HTTP/1 bodies
- allocator choice if still validated on the target platform
- Explicitly avoid:
- per-request JIT assumptions for fresh VMs
- borrow-heavy lazy request/header tricks that caused intermittent `500`s
- any optimization that only works for empty bodies unless it has a safe fallback

## Workstream 2: Shared Outbound HTTP/1 Engine

- Add a new internal outbound HTTP/1 module tree, separate from the generic HTTP state path
- The outbound engine must be shared by:
- default-upstream HTTP forwarding
- exchange HTTP forwarding
- Build a raw outbound connection type around:
- `TcpStream` or `TlsStream<TcpStream>`
- parser state
- writer state
- keepalive state
- pool lease state
- Use a low-level HTTP/1 parser for heads
- Serialize request heads directly from the prepared request model
- Parse response heads directly from the outbound stream
- Represent response bodies as explicit framed stream state, not as high-level client body wrappers

## Workstream 3: Outbound HTTP/1 Pool Rewrite

- Remove dependence on `reqwest` pooling for outbound HTTP/1 hot traffic
- Pool raw HTTP/1 connections by:
- scheme
- host
- port
- TLS settings / ALPN / SNI shape
- Return connections to the pool only after the full message boundary is proven
- Track dirty vs reusable connections explicitly:
- non-drained body
- parse error
- unexpected close
- upgrade
- mismatched keepalive semantics
- Keep the pool lease alive until the downstream has fully drained the response
- Add pool instrumentation:
- open
- leased
- reused
- dropped dirty
- parse failures
- close-delimited responses

## Workstream 4: Dedicated Downstream HTTP/1 Server Loop

- For plaintext HTTP/1 requests, bypass Hyper on eligible connections
- Keep listener setup and protocol sniffing, but route eligible HTTP/1 traffic into a lower-level loop
- Parse the downstream request head with the low-level parser
- Stream the request body directly into the chosen upstream path
- Write the upstream response head and body directly to the downstream socket
- Own downstream keepalive explicitly:
- one socket
- sequential request loop
- close/reuse decision after every response
- This downstream loop is allowed to stay distinct from the outbound HTTP/1 engine
- Keep Hyper only for:
- H2 downstream
- upgrade-heavy flows
- fallback HTTP/1 cases

## Workstream 5: Define Fast-Path Eligibility

- Only enable the dedicated fast path when all conditions hold:
- downstream is HTTP/1 plaintext
- upstream target is HTTP/1 over TCP or TLS
- no body mutation
- no response-body reads in the VM
- no costly response-header mutation
- no upgrade, websocket, CONNECT, or tunnel semantics
- no custom transport attachment
- Everything else falls back cleanly to the existing generic machinery

## Workstream 6: Reduce Generic HTTP Graph Overhead

- Stop routing fast-path-eligible requests through:
- exchange snapshots
- generic prepared-request cloning
- downstream `Response<Body>` reconstruction
- Keep the generic HTTP graph for mutating and complex cases only
- Target hot-path shape:
- parse downstream head
- acquire upstream lease
- serialize request head
- stream request body
- parse upstream head
- emit response head
- stream response body
- recycle connection

## Workstream 7: Keep H2/H3 Off the Rewrite Path

- Do not rewrite upstream HTTP/2 transport/session logic
- Do not rewrite upstream HTTP/3 transport/session logic
- Only adjust integration boundaries if the generic HTTP graph gets slimmer
- Leave downstream H2/H3 on existing libraries unless plaintext HTTP/1 refactor is complete and stable

## Workstream 8: HTTP/1 Framing Coverage

- Support response framing before broad rollout:
- `Content-Length`
- `Transfer-Encoding: chunked`
- zero-length body
- header-only responses
- `HEAD`
- `204`, `304`, `1xx`
- close-delimited body
- Support request framing:
- fixed-length
- chunked
- empty
- early client close
- Refuse or fall back on unsupported framing instead of guessing

## Workstream 9: Zero-Copy Goals

- First target: no extra user-space body memcpy in `pd-edge`
- True socket splice is a later, narrower optimization
- If pursued, start with CONNECT or raw tunnel flows first
- Do not block the HTTP/1 refactor on kernel-splice support

## Workstream 10: Benchmark and Correctness Discipline

- After each milestone, rerun only the stock rows that matter:
- `raw_http_upstream`
- `http_proxy`
- then `http_proxy_body_read`
- then HTTP/2 rows
- Compare only against matched baselines
- Add regressions for:
- keepalive reuse with non-empty bodies
- chunked bodies
- early body drop
- close-delimited bodies
- fallback activation
- upgrade/tunnel separation
- Keep full workspace tests, fmt, and clippy green after each milestone

## Execution Order

1. Delete wrong directions and simplify the boundary.
2. Introduce the `http1_fast` module and move current sender-pool logic into it.
3. Build the raw HTTP/1 upstream engine and pool.
4. Switch stock plaintext outbound forwarding to the new shared engine.
5. Build the dedicated downstream HTTP/1 server loop.
6. Route stock plaintext `http_proxy` through the new end-to-end path.
7. Expand framing coverage and fallback handling.
8. Re-run full Harness A and workspace verification after each milestone.

## Success Gates

- Phase 1: no correctness regressions and no intermittent `500`s
- Phase 2: stock plaintext proxy row moves materially without hurting baseline stability
- Phase 3: stock `http_proxy / raw_http_upstream > 50%`
- Phase 4: preserve or improve HTTP/2 rows while keeping plaintext gains

## Progress Snapshot

### Completed or Landed

- Workstream 2 is largely in place for outbound HTTP/1:
- `http/outbound_http1.rs` is a shared engine for both default-upstream and exchange HTTP/1 forwarding
- the engine owns raw `TcpStream` / `TlsStream<TcpStream>` connections, request serialization, response-head parsing, keepalive handling, and response-body framing
- outbound HTTP/1 hot traffic no longer requires the high-level reqwest request path when the transport/version fast-path gate is satisfied
- native outbound HTTP/1 can now stream request bodies instead of waiting for the full body before send; the direct default-upstream reqwest fallback and the attached HTTP/1 path now stream pristine inbound bodies too, while the remaining eager materialization is concentrated in HTTP/2, HTTP/3, and other fallback-only paths
- default-upstream HTTP/2 and HTTP/3 request paths now consume the same reusable request-body template instead of forcing an eager `Bytes` buffer before protocol selection and fallback
- Workstream 3 is largely in place:
- outbound HTTP/1 pooling is explicit and lease-based for both plaintext and HTTPS HTTP/1
- pool keys include scheme/authority for plaintext and `TlsSessionCacheKey` for HTTPS, so TLS configuration shape is already part of pooling
- message-boundary reuse rules are explicit for empty, fixed-length, chunked, and close-delimited responses
- dirty vs reusable connection state is tracked explicitly in the lease path
- pool instrumentation now exists behind `PD_EDGE_HTTP1_POOL_METRICS` for `open`, `leased`, `reused`, `dropped_dirty`, `parse_failures`, and `close_delimited`
- Workstream 4 is largely complete for eligible HTTP/1 traffic:
- `proxy_path.rs` has a dedicated downstream HTTP/1 loop for eligible HTTP/1 requests on both plaintext listeners and TLS-terminated HTTP/1 connections that do not negotiate `h2`
- the loop parses request heads directly, supports empty bodies plus bounded fixed-length and bounded chunked request bodies, and handles `Expect: 100-continue`
- unsupported or ambiguous cases replay buffered bytes and fall back to Hyper
- large fixed-length request bodies stay on the low-level loop; the remaining fallback cases are unsupported/ambiguous framing, upgrades/tunnels, and general Hyper-only paths
- Workstream 5 is largely complete on the HTTP/1 response hot path:
- there is a downstream request-shape gate in `http/fast_path.rs`
- there is an outbound/default-upstream transport-version gate in `http/fast_path.rs`
- there is a shared downstream VM touch ledger for request-body reads, downstream response-body/status/header mutations, and exchange/upstream response-body reads
- the low-level downstream HTTP/1 loop now uses `resolve_http1_downstream_response(...)`, and that planner snapshots one shared response-side eligibility state before choosing native-local, native-forward, snapshot, or generic graph resolution
- Workstream 6 is largely complete for the HTTP/1 fast path:
- the downstream low-level HTTP/1 loop can write the scheduled native default-upstream HTTP/1 response directly without reconstructing `Response<Body>`
- non-native default-upstream responses and exchange-applied bodies can now stay snapshot-backed and be written directly on the low-level downstream HTTP/1 loop without eager `read_all`
- the generic default-upstream reqwest fallback can now stream pristine inbound bodies through a custom reqwest body adapter instead of forcing `resolve_outbound_request_body(...).read_all()`
- the generic default-upstream reqwest fallback still forwards inherited lazy downstream headers directly, without first cloning a filtered `HeaderMap`
- a later attempt to carry inherited fallback headers lazily through `PreparedUpstreamRequest` regressed the honest stock rows and was reverted; the safe state that remains is snapshotting exchange request metadata without cloning the full request node
- snapshot-backed downstream HTTP/1 writes now carry `Arc<HeaderMap>` base headers plus overrides and encode them directly, instead of cloning the full upstream header map up front on the low-level writer
- generic graph / fallback paths still pass through `HttpUpstreamResponseSnapshot`, `response_from_upstream_snapshot`, and `Response<Body>` reconstruction; on the low-level HTTP/1 loop those graph results are written by `write_http1_response(...)`, while replay/non-HTTP/1 cases still terminate in the generic server/client stacks
- Workstream 8 is mostly complete for the currently supported HTTP/1 shapes:
- response framing covers fixed-length, chunked, zero-length, header-only, and close-delimited bodies
- request framing on the low-level loop covers empty, bounded fixed-length, bounded chunked, and early-close error cases
- HTTP/1 response trailers are now preserved through the internal streamed-body path and written back out on chunked downstream HTTP/1 responses
- exchange/runtime trailer APIs now exist via `http::exchange::get_trailer(s)` and `http::response::get_trailer(s)`
- truncated downstream fixed-length and chunked requests stay sticky through finalize and are surfaced as downstream protocol errors instead of generic `500`s
- regression coverage includes non-empty upstream reuse, chunked upstream reuse, close-delimited upstream non-reuse, persistent downstream fixed/chunked HTTP/1 requests, oversized chunked `413`s, HTTPS HTTP/1 persistence, and truncated fixed-length downstream `400`s
- Workstream 9 is only partially complete:
- the native HTTP/1 response path can stream `Bytes` chunks without an extra `pd-edge` body buffer on the untouched fast path
- there is still no socket-splice path, and outbound request bodies are still copied into encoded request buffers
- Phase 3 is not yet stably satisfied on the latest repeated honest stock plaintext rerun:
- latest repeated header-only stock row:
- async average-of-runs: `51,099.94 / 122,453.72 = 41.73%`
- threading average-of-runs: `50,278.35 / 123,128.64 = 40.83%`
- latest repeated body-read stock row:
- async average-of-runs: `42,755.22 / 120,388.34 = 35.51%`
- threading average-of-runs: `42,390.89 / 121,740.42 = 34.82%`
- post-refactor protocol spot-checks:
- `http2->http2`: async `38,144.33 / 85,966.52 = 44.37%`, threading `37,523.13 / 85,651.58 = 43.81%`
- `http3->http3`: async `44,383.67 / 175,258.24 = 25.32%`, threading `37,346.22 / 177,248.32 = 21.07%`

### Remaining Gaps

- No remaining actionable structural gap is left on the dedicated HTTP/1 fast path covered by this plan.
- The remaining `Response<Body>` reconstruction now lives in generic graph paths and true fallback-by-design paths:
- `resolve_http_graph_response(...)` still builds `ResolvedHttpGraphResponse { response: Response<Body>, .. }`
- `response_from_upstream_snapshot(...)` and `response_from_upstream_snapshot_with_head(...)` still reconstruct `Response<Body>` from snapshots
- on the low-level HTTP/1 loop, `Http1DownstreamResolution::Graph(...)` writes those graph results directly with `write_http1_response(...)`
- replay fallback and non-HTTP/1 paths still terminate in the generic server/client stacks
- Phase 3 is still open:
- the stock plaintext row improved in absolute proxy throughput and preserved/improved the H2/H3 rows, but the honest stock plaintext ratio remains around `41%`, not `>50%`
- At this point the remaining delta is no longer explained by the HTTP/1 fast-path architecture captured in this plan; the next plan should target stock-benchmark VM/program overhead and any remaining fallback-independent per-request costs.
