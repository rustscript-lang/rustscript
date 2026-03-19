# HTTP Proxy Perf Methodology

This note records the comparison rules for `pd-edge/examples/http_proxy_perf_framework.rs`.

## Baseline Rule

Baseline-relative percentages must always use the matched baseline from the same run batch.

Do not compare:

- a scenario from one rerun
- against a baseline row copied from a different rerun

even if the config looks identical.

The benchmark upstream, runtime scheduling, warmup state, connection reuse, and transport behavior
all introduce enough drift that cross-run baseline splicing is misleading.

## Correct Comparison Shape

For each mode and rerun:

1. Run the full matrix for that mode.
2. Compute each scenario ratio against its matched baseline from that same JSON report.
3. If repeating runs, aggregate the per-run ratios after step 2.

Do not:

1. average raw scenario throughput across runs,
2. average raw baseline throughput across different runs,
3. divide those two later.

Compute the ratio inside each run first, then aggregate the ratios.

## Matched Baselines

- `raw_no_program` is the baseline for `no_host_calls_program` and `host_calls_terminate`.
- `raw_http_upstream` is the baseline for `http_proxy` and `http2->http`.
- `raw_http_upstream_body_read` is the baseline for `http_proxy_body_read`.
- `raw_http2_upstream` is the baseline for `http->http2` and `http2->http2`.
- `raw_http3_upstream` is the baseline for `http3->http3`.

## Examples

Correct:

- `http_proxy(local, run2) / raw_http_upstream(local, run2)`
- `host_calls_terminate(async, run3) / raw_no_program(async, run3)`

Wrong:

- `http_proxy(local, run2) / raw_http_upstream(async, run1)`
- `host_calls_terminate(local, rerun) / raw_no_program(from older report)`

## Reporting

When presenting side-by-side mode comparisons:

- prefer matched-baseline percentages over raw cross-group throughput
- if there are repeated runs, report mean and spread of the per-run ratios
- keep raw throughput tables as supporting data, not the primary comparison
