
**Terminate Guide**
The terminate path is no longer bottlenecked by response writeback. `write_native_local_http1_response` in [proxy_path.rs:1110](/home/wow/dev/project-d/pd-edge/src/runtime/http_plane/proxy_path.rs:1110) is only a small slice, and `__send` is about `1%` self. The bigger costs are request bookkeeping and allocation churn.

- Highest impact: kill string formatting in `next_http_request_id` at [proxy_path.rs:92](/home/wow/dev/project-d/pd-edge/src/runtime/http_plane/proxy_path.rs:92). It shows up around `5.1%` cumulative by itself. Turning request IDs into integers or lazily formatting only when actually needed should buy a real win. Let's for now make pd-edge generate request id for debugger session/replay session, but not on other normal requests.
- Highest impact: make downstream request context construction lazier. `build_fast_http_request_context` in [proxy_path.rs:846](/home/wow/dev/project-d/pd-edge/src/runtime/http_plane/proxy_path.rs:846), `build_downstream_http_request_context` in [state.rs:3326](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/state.rs:3326), and `ProxyVmContext::from_http_request_with_services` in [state.rs:1909](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/state.rs:1909) are a noticeable chunk, while the benchmark program only needs method plus response mutation. Computing `scheme`, `host`, `query`, `client_ip`, and version strings on demand would help.
- Highest impact: reduce per-request allocation and drop churn in the fast path. The top self buckets are `memmove`, `malloc`, `cfree`, and `HeaderMap` drops. Reusing request/response scratch structures in [proxy_path.rs:1172](/home/wow/dev/project-d/pd-edge/src/runtime/http_plane/proxy_path.rs:1172) is likely worth more than VM tuning here.
- Medium impact: trim timing/metrics timestamp work around the fast path. `Timespec::now` and `clock_gettime` are visible, tied to the request loop in [proxy_path.rs:1172](/home/wow/dev/project-d/pd-edge/src/runtime/http_plane/proxy_path.rs:1172). If some of that can be skipped when metrics are effectively off, it should help.
- Lower impact: VM execution itself is small. `Vm::run_internal` is only about `2.5%` self, so the interpreter is not the first place to spend effort on this benchmark.

**Proxy Guide**
The proxy case is a different shape. It is dominated by allocation/copy churn around the upstream-forward path, plus task scheduling overhead. The interpreter is again not the main issue.

- Highest impact: reduce clones and heap churn in the default-upstream forward path at [state.rs:4372](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/state.rs:4372). The profile is heavy on `malloc`, `_int_malloc`, `cfree`, `memmove`, and `String::clone`, and this function clones `host`, `authority`, and related request state before forwarding.
- Highest impact: stop rebuilding pool keys and authority strings every request. `target_pool_key` in [outbound_http1.rs:472](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/outbound_http1.rs:472), `format_upstream_authority` in [state.rs:761](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/state.rs:761), and `next_http_request_id` in [proxy_path.rs:92](/home/wow/dev/project-d/pd-edge/src/runtime/http_plane/proxy_path.rs:92) all show up. Caching canonical authority and the pool key on the request/exchange state should help.
- High impact: avoid spawning/scheduling extra async tasks for the hot forward path. `Handle::bind_new_task` is visible, and the async scheduling hook is [mod.rs:762](/home/wow/dev/project-d/pd-edge/src/abi_impl/mod.rs:762). For the simple native HTTP/1 sender-pool path, staying on the current task would likely cut overhead.
- Medium impact: reduce snapshot/serialization churn. `snapshot_default_upstream_request` in [state.rs:3493](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/state.rs:3493), `serialize_request_head` in [outbound_http1.rs:1010](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/outbound_http1.rs:1010), and `send_and_parse_response` in [outbound_http1.rs:1229](/home/wow/dev/project-d/pd-edge/src/abi_impl/http/outbound_http1.rs:1229) are all on the hot path. Reusing a `BytesMut`, prebuilding invariant request-head pieces, and borrowing rather than snapshot-cloning would help.
- Lower impact: downstream request-context building is present but not dominant in proxy. It is much smaller here than in terminate, so I would not start there for the proxy benchmark.
- Lower impact: VM compute is still minor. `Vm::run_internal` is only about `2.1%` self, so this is mostly a runtime/plumbing problem, not a bytecode engine problem.

The big split is:
- Terminate: optimize local request bookkeeping and allocation churn.
- Proxy: optimize upstream-forward plumbing, clone/allocation behavior, and per-request async scheduling.

**Finished Work**

- Implemented the terminate-side highest-impact request ID change. Normal HTTP requests now carry a deferred `LazyRequestId` and do not format `req-...` strings unless the VM or debugger actually reads the request id.
- Implemented lazy downstream request-head fields. `query`, `http_version`, `scheme`, `host`, `client_ip`, and `port` now derive through shared `OnceLock`-backed metadata instead of being eagerly formatted during request-context construction.
- Implemented fast-path scratch-buffer reuse on downstream HTTP/1 response writes. The low-level loop now reuses a per-connection `BytesMut` for response heads and chunk framing instead of allocating fresh buffers for every response/chunk.
- Implemented proxy-side canonical target caching. Default-upstream/exchange request state now caches canonical authority, normalized host, and the plain HTTP/1 pool key, so the hot sender-pool path no longer rebuilds those strings on every request.
- Implemented outbound HTTP/1 request-write scratch reuse. The sender-pool path now reuses the connection-local request-head buffer and chunk-prefix buffer instead of allocating new request-head buffers and formatted chunk-header strings on every send.

Repeated verification was run from the prebuilt release binaries only, using `--skip-build`, with three sequential matched pairs per scenario and mode.

- Terminate baseline pair:
  - Async average: `host_calls_terminate / raw_no_program = 90,820.62 / 117,391.68 = 77.37%`
  - Threading average: `89,828.25 / 119,875.89 = 74.93%`
  - Compared with the pre-change repeated baseline (`76.45%` async, `72.51%` threading), this is a real improvement.
- Proxy stock pair:
  - Async average: `http_proxy / raw_http_upstream = 51,938.27 / 125,067.93 = 41.53%`
  - Threading average: `51,073.46 / 126,135.12 = 40.49%`
  - Compared with the immediately preceding repeated stock proxy baseline on this branch (`41.73%` async, `40.83%` threading), this is effectively flat within noise on matched ratio while absolute proxy throughput improved to about `52k` RPS in both modes. Compared with the older repeated stock baseline (`39.89%` async, `39.31%` threading), it remains ahead.

**Current Code Check**

- Rechecked the highest-impact items in this plan against the current `pd-edge` code.
- No additional highest-impact item remains unimplemented from this plan.
- The still-open ideas in this file are below the highest-impact tier, mainly around async task scheduling on the proxy path and additional snapshot/fallback churn reduction.
