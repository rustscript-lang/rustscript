# TODO

## pd-edge performance regression investigation

Date: 2026-03-13

Context:
- The first `HTTP_PROXY_PERF_REPORT_2026-03-13.md` sample suggested a broad RPS regression versus `HTTP_PROXY_PERF_REPORT_2026-03-11.md`, including `raw_no_program`.
- That conclusion was suspicious because the raw path should not have been affected by the recent `ProxyVmContext` ownership split or the new upstream-roundtrip benchmark coverage.

Investigation completed:
- Re-ran `raw_no_program`, `no_host_calls_program`, and `host_calls_terminate` individually.
- Discarded the first parallel reruns as invalid benchmark data because they contended on the same machine and produced obviously unstable numbers.
- Re-ran the full Harness A benchmark serially via `target/release/examples/http_proxy_perf_framework.exe` to avoid `cargo run` lock contention and keep the scenarios on one clean sample.
- Refreshed `target/http_proxy_perf_mode_async_2026-03-13.json` and `target/http_proxy_perf_mode_threading_2026-03-13.json` from that clean rerun and updated the March 13 report.

Current findings:
- `raw_no_program` is not materially regressed. The refreshed sample came back at `101,123.06` RPS for `async` and `101,584.29` RPS for `threading`.
- The regression remains in program-loaded scenarios:
- `no_host_calls_program`: `31,281.59` async, `30,348.21` threading
- `host_calls_terminate`: `37,303.62` async, `36,570.45` threading
- Because `no_host_calls_program` is already down, the problem is not specific to the new upstream round-trip scenario. The likely fault domain is VM execution, fuel accounting, or program-loaded request plumbing.

Next steps:
1. Bisect between the March 11 perf point and current head with focus on `pd-vm` fuel-budgeting and execution-mode changes.
2. Add an explicit current-head comparison for `no_host_calls_program` with fuel disabled, `fuel=50000 interval=32`, and `fuel=50000 interval=64` to isolate fuel-accounting overhead from general program-load cost.
3. Add a lighter benchmark that loads a trivial program with no compute loop, so VM entry/exit and host setup cost can be measured separately from script execution cost.
4. Re-run the refreshed Harness A comparison at least three times per mode and compare medians before treating small tail-latency deltas as real.
