# pd-edge Perf Report (2026-03-19)

This report covers the full same-day Harness A rerun on the optimized proxy path after commit `73d0aef` (`pd-edge: perf: streamline proxy HTTP/1 fast path`).

- Runs were executed sequentially, not in parallel.
- This report covers the full all-features async Harness A matrix.
- `PD_EDGE_PERF_USE_COMBINED_DEFAULT_FORWARD=1` was enabled.
- Config: `requests=120000`, `warmup_requests=20000`, `concurrency=128`, `vm_fuel=disabled`.
- The benchmark upstream fixtures are separate child processes started by the harness.
- HTTP/2 coverage uses TLS + ALPN only.
- HTTP/3 coverage uses HTTPS over QUIC with ALPN-negotiated `h3`.
- Throughput comparisons across scenario groups use matched baselines from the same run.
- Matched baselines: `raw_no_program` for `no_host_calls_program` and `host_calls_terminate`.
- Matched baselines: `raw_http_upstream` for `http_proxy` and `http2->http`.
- Matched baselines: `raw_http_upstream_body_read` for `http_proxy_body_read`.
- Matched baselines: `raw_http2_upstream` for `http->http2` and `http2->http2`.
- Matched baselines: `raw_http3_upstream` for `http3->http3`.

Data sources:

- Baseline worktree before the optimization:
  - `d:/Workspace/project-d-master-before/target/http_proxy_perf_mode_async_2026-03-19_before_perfpass_full_all_features.json`
  - `d:/Workspace/project-d-master-before/target/http_proxy_perf_mode_async_2026-03-19_before_perfpass_run2_full_all_features.json`
  - `d:/Workspace/project-d-master-before/target/http_proxy_perf_mode_async_2026-03-19_before_perfpass_run3_full_all_features.json`
- Current optimized tree rerun:
  - `d:/Workspace/project-d/target/http_proxy_perf_mode_async_2026-03-19_report_run1_full_all_features.json`
  - `d:/Workspace/project-d/target/http_proxy_perf_mode_async_2026-03-19_report_run2_full_all_features.json`
  - `d:/Workspace/project-d/target/http_proxy_perf_mode_async_2026-03-19_report_run3_full_all_features.json`

## 1) Current Tree Raw Results

The table below is the current optimized tree only, aggregated over 3 full reruns.

| Scenario | Mean RPS | RPS Stddev | Mean p50 (ms) | Mean p99 (ms) |
|---|---:|---:|---:|---:|
| `raw_no_program` | 97,533 | 2,373 | 1.269 | 2.470 |
| `no_host_calls_program` | 72,043 | 1,836 | 1.707 | 3.580 |
| `host_calls_terminate` | 93,431 | 3,224 | 1.328 | 2.537 |
| `raw_http_upstream` | 119,556 | 4,132 | 1.036 | 1.940 |
| `http_proxy` | 52,875 | 1,835 | 2.405 | 3.980 |
| `raw_http_upstream_body_read` | 116,909 | 3,860 | 1.056 | 2.053 |
| `http_proxy_body_read` | 43,573 | 867 | 2.925 | 4.802 |
| `raw_http2_upstream` | 72,359 | 2,377 | 1.754 | 2.630 |
| `http->http2` | 50,243 | 605 | 2.514 | 3.903 |
| `http2->http` | 49,160 | 856 | 2.554 | 4.245 |
| `http2->http2` | 38,005 | 270 | 3.284 | 5.576 |
| `raw_http3_upstream` | 211,161 | 2,432 | 0.565 | 1.454 |
| `http3->http3` | 47,031 | 243 | 2.588 | 5.543 |

## 2) Same-Day Before vs After, Matched by Baseline

Each row below compares efficiency against its own direct baseline, not raw cross-group throughput. The `before` side is the clean pre-optimization worktree. The `after` side is the current optimized tree rerun above.

| Scenario | Baseline | Before Ratio | After Ratio | Delta |
|---|---|---:|---:|---:|
| `no_host_calls_program` | `raw_no_program` | `71.55% +/- 3.18` | `73.87% +/- 0.93` | `+2.32 pp` |
| `host_calls_terminate` | `raw_no_program` | `89.36% +/- 7.49` | `95.78% +/- 1.35` | `+6.42 pp` |
| `http_proxy` | `raw_http_upstream` | `41.44% +/- 2.34` | `44.23% +/- 0.07` | `+2.79 pp` |
| `http_proxy_body_read` | `raw_http_upstream_body_read` | `36.22% +/- 0.74` | `37.28% +/- 0.52` | `+1.06 pp` |
| `http->http2` | `raw_http2_upstream` | `65.18% +/- 11.40` | `69.50% +/- 2.89` | `+4.32 pp` |
| `http2->http` | `raw_http_upstream` | `36.41% +/- 2.45` | `41.14% +/- 0.72` | `+4.73 pp` |
| `http2->http2` | `raw_http2_upstream` | `46.83% +/- 6.54` | `52.56% +/- 1.52` | `+5.72 pp` |
| `http3->http3` | `raw_http3_upstream` | `23.10% +/- 1.85` | `22.28% +/- 0.37` | `-0.82 pp` |

## 3) Conclusions

Improvements that look real:

- `http_proxy` improved from `41.44%` to `44.23%` of `raw_http_upstream`, which is the core HTTP/1 proxy target of this change.
- `host_calls_terminate` improved from `89.36%` to `95.78%` of `raw_no_program`, which matches the reduced spawn/race overhead on non-streaming request handling.
- `http2->http` improved from `36.41%` to `41.14%` of `raw_http_upstream`.
- `http2->http2` improved from `46.83%` to `52.56%` of `raw_http2_upstream`.
- `http->http2` also improved, although with more baseline variance than the plaintext HTTP/1 rows.

Small or near-noise improvements:

- `no_host_calls_program` improved modestly.
- `http_proxy_body_read` improved by about one point of matched-baseline efficiency.

Regression to watch:

- `http3->http3` slipped from `23.10%` to `22.28%` of `raw_http3_upstream`.
- That row is outside the direct HTTP/1/native-forward target of this optimization, so it should be treated as a follow-up item rather than a reason to revert this change.

Methodology note:

- Absolute raw throughput moved between same-day batches, so the stable signal here is the matched-baseline ratio, not raw RPS from different runs on different host states.
- On that metric, the optimization is a net win and should be kept.

## 4) HTTP/1 Forward Variant Comparison

This section isolates the plaintext HTTP proxy path and compares three program forms:

- generic `proxy::forward`
- specialized `proxy::forward_default_upstream`
- combined `proxy::prepare_and_forward_default_upstream`

Config:

- default feature build only
- `requests=120000`
- `warmup_requests=20000`
- `concurrency=128`
- `vm_fuel=disabled`
- 3 reruns per variant
- each variant batch includes both `raw_http_upstream` and `http_proxy`, so the ratio is matched against the direct baseline from the same batch

Direct baseline reference across all 9 variant runs:

| Scenario | Mean RPS | RPS Stddev | Mean p50 (ms) | Mean p99 (ms) |
|---|---:|---:|---:|---:|
| `raw_http_upstream` | 128,537 | 3,140 | 0.961 | 1.827 |

Side-by-side comparison for header-only plaintext HTTP proxy traffic:

| Variant | Matched `raw_http_upstream` Mean RPS | `http_proxy` Mean RPS | `http_proxy / raw_http_upstream` | `http_proxy` Mean p50 (ms) | `http_proxy` Mean p99 (ms) | Delta vs `proxy::forward` |
|---|---:|---:|---:|---:|---:|---:|
| `proxy::forward` | 126,274 | 52,294 | `41.42% +/- 0.35` | 2.432 | 4.051 | baseline |
| `proxy::forward_default_upstream` | 128,528 | 54,209 | `42.18% +/- 0.42` | 2.343 | 3.959 | `+0.77 pp` |
| `proxy::prepare_and_forward_default_upstream` | 130,809 | 55,120 | `42.14% +/- 0.10` | 2.301 | 3.909 | `+0.72 pp` |

Readout:

- `proxy::forward_default_upstream` is slightly faster than generic `proxy::forward` on this HTTP/1 proxy row.
- `proxy::prepare_and_forward_default_upstream` is effectively tied on throughput ratio with the specialized forward path, but it has the best p50 and p99 in this comparison.
- The gap is real but small. The specialized forms improve matched-baseline throughput efficiency by about `0.7` to `0.8` percentage points over generic `proxy::forward`.
