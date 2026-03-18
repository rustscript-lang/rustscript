# Repo Analysis

- Count basis: physical Rust lines in `src/**/*.rs` plus `build.rs` for each crate.
- Excluded from LOC totals: `tests/`, `examples/`, `docs/`, `target/`, and web assets.
- Feature buckets are source/module areas, not Cargo feature flags. Cargo feature flags are listed separately because they overlap.
- Test counts come from detected `#[test]` and `#[tokio::test]` functions in `src/**/*.rs`, `tests/**/*.rs`, and `build.rs` when present.

Workspace production LOC: **107344**
Detected tests: **842**

## Highlights

- Largest crates by LOC: `pd-vm` (52453 LOC, 509 tests); `pd-edge` (35350 LOC, 239 tests); `pd-controller` (11298 LOC, 32 tests).
- Largest functionality buckets: `pd-edge/abi_impl/http` (10784 LOC); `pd-vm/vm/jit` (8721 LOC); `pd-controller/server/ui_codegen` (7424 LOC); `pd-vm/compiler/parser` (5995 LOC); `pd-vm/compiler/typing` (5506 LOC).
- Heaviest test suites: `pd-vm/src/bin/pd-vm-run.rs` (65 tests); `pd-vm/tests/compiler/compiler_rustscript_tests.rs` (57 tests); `pd-vm/tests/compiler/compiler_common_tests.rs` (54 tests); `pd-vm/tests/vm/vm_runtime_tests.rs` (54 tests); `pd-edge/tests/proxy_tests/http.rs` (51 tests).

## Crate Summary

| Crate | LOC | Cumulative LOC | Tests | Cargo features |
| --- | ---: | ---: | ---: | --- |
| pd-controller | 11298 | 11298 | 32 | - |
| pd-edge | 35350 | 46648 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge-host-function | 1000 | 47648 | 0 | - |
| pd-edge-abi | 1670 | 49318 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-host-function | 386 | 102157 | 0 | - |
| pd-vm-wasm | 5187 | 107344 | 57 | default, runtime |

## Crate Feature Matrix

| Crate | Crate LOC | Crate Cum LOC | Feature / Functionality | Feature LOC | Feature Cum LOC | Tests | Cargo features |
| --- | ---: | ---: | --- | ---: | ---: | ---: | --- |
| pd-controller | 11298 | 11298 | server/ui_codegen | 7424 | 7424 | 32 | - |
| pd-controller | 11298 | 11298 | server | 1923 | 9347 | 32 | - |
| pd-controller | 11298 | 11298 | server/handlers | 1682 | 11029 | 32 | - |
| pd-controller | 11298 | 11298 | root | 185 | 11214 | 32 | - |
| pd-controller | 11298 | 11298 | build | 84 | 11298 | 32 | - |
| pd-edge | 35350 | 46648 | abi_impl/http | 10784 | 10784 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/transport | 4060 | 14844 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | runtime/http_plane | 3394 | 18238 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/http2 | 1894 | 20132 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/websocket | 1806 | 21938 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/http3 | 1313 | 23251 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/proxy | 1130 | 24381 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | sample_echo | 1116 | 25497 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/webrtc | 1048 | 26545 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl | 999 | 27544 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | bin/pd-edge-console | 995 | 28539 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | bin/pd-edge-http-proxy | 863 | 29402 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | runtime | 839 | 30241 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | debug_session | 772 | 31013 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | runtime/vm_runner | 697 | 31710 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | bin/pd-edge-transport-proxy | 617 | 32327 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/io | 369 | 32696 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | bin/pd-edge-sample-echo-server | 362 | 33058 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | lock_metrics | 347 | 33405 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | active_control_plane | 317 | 33722 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/quic | 293 | 34015 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | cache | 291 | 34306 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | control_plane_rpc | 196 | 34502 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | runtime/transport_plane | 147 | 34649 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/registry | 133 | 34782 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | logging | 112 | 34894 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/console | 110 | 35004 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | control_plane_http_client | 110 | 35114 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | build_info | 67 | 35181 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | root | 60 | 35241 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | compile | 41 | 35282 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | abi_impl/runtime | 38 | 35320 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge | 35350 | 46648 | build | 30 | 35350 | 239 | console, default, http, http2, http3, native-jit, tls, webrtc, websocket |
| pd-edge-host-function | 1000 | 47648 | edge | 989 | 989 | 0 | - |
| pd-edge-host-function | 1000 | 47648 | root | 11 | 1000 | 0 | - |
| pd-edge-abi | 1670 | 49318 | build | 802 | 802 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | root | 160 | 962 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/http.exchange | 114 | 1076 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/tls | 76 | 1152 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/websocket | 76 | 1228 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/webrtc | 68 | 1296 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/http.request | 60 | 1356 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/tcp | 60 | 1416 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/udp | 60 | 1476 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/http.response | 56 | 1532 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/proxy | 47 | 1579 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/console | 40 | 1619 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/functions | 20 | 1639 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/namespaces | 15 | 1654 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/runtime | 12 | 1666 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-edge-abi | 1670 | 49318 | abi_spec/http.downstream | 4 | 1670 | 5 | console, default, http, http2, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/jit | 8721 | 8721 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/parser | 5995 | 14716 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/typing | 5506 | 20222 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/frontends | 5142 | 25364 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/lifetime | 4551 | 29915 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | builtins/runtime | 2515 | 32430 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/source_loader | 2329 | 34759 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | bin/pd-vm-run | 2021 | 36780 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/codegen | 1836 | 38616 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | build | 1725 | 40341 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm | 1261 | 41602 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | debugger/tests | 1130 | 42732 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/pipeline | 1008 | 43740 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | debugger | 964 | 44704 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/host | 878 | 45582 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vmbc | 854 | 46436 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | assembler | 785 | 47221 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/tests | 783 | 48004 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | bytecode | 719 | 48723 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/linker | 509 | 49232 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | debugger/replay | 484 | 49716 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/ir | 467 | 50183 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler | 426 | 50609 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | debugger/recording | 329 | 50938 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/superinstructions | 236 | 51174 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/source_map | 211 | 51385 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/epoch | 192 | 51577 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | debug_info | 179 | 51756 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/fuel | 163 | 51919 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/format | 138 | 52057 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/store | 118 | 52175 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | builtins | 80 | 52255 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | root | 70 | 52325 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | builtins/metadata | 57 | 52382 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | compiler/diagnostics | 50 | 52432 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-vm | 52453 | 101771 | vm/diagnostics | 21 | 52453 | 509 | cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket |
| pd-host-function | 386 | 102157 | root | 386 | 386 | 0 | - |
| pd-vm-wasm | 5187 | 107344 | root | 2329 | 2329 | 57 | default, runtime |
| pd-vm-wasm | 5187 | 107344 | completions | 1280 | 3609 | 57 | default, runtime |
| pd-vm-wasm | 5187 | 107344 | runtime | 1252 | 4861 | 57 | default, runtime |
| pd-vm-wasm | 5187 | 107344 | analyzer | 280 | 5141 | 57 | default, runtime |
| pd-vm-wasm | 5187 | 107344 | stdlib | 46 | 5187 | 57 | default, runtime |

## Crate Test Matrix

| Crate | Suite | Kind | Tests |
| --- | --- | --- | ---: |
| pd-controller | tests/controller_tests/ui.rs | integration | 14 |
| pd-controller | src/main.rs | unit | 7 |
| pd-controller | tests/controller_tests/programs.rs | integration | 5 |
| pd-controller | tests/controller_tests/rpc.rs | integration | 4 |
| pd-controller | tests/controller_tests/debug.rs | integration | 1 |
| pd-controller | tests/e2e_demo_tests.rs | integration | 1 |
| pd-edge | tests/proxy_tests/http.rs | integration | 51 |
| pd-edge | tests/proxy_tests/transport.rs | integration | 24 |
| pd-edge | src/bin/pd-edge-http-proxy.rs | unit | 19 |
| pd-edge | src/bin/pd-edge-console.rs | unit | 18 |
| pd-edge | src/abi_impl/transport/state.rs | unit | 14 |
| pd-edge | src/abi_impl/http/state.rs | unit | 11 |
| pd-edge | src/abi_impl/mod.rs | unit | 8 |
| pd-edge | tests/compile_tests.rs | integration | 8 |
| pd-edge | tests/proxy_tests/tls.rs | integration | 7 |
| pd-edge | tests/proxy_tests/websocket.rs | integration | 7 |
| pd-edge | src/bin/pd-edge-transport-proxy.rs | unit | 6 |
| pd-edge | src/debug_session.rs | unit | 5 |
| pd-edge | tests/proxy_tests/io.rs | integration | 5 |
| pd-edge | src/abi_impl/http/exchange.rs | unit | 4 |
| pd-edge | src/abi_impl/http/fast_path.rs | unit | 4 |
| pd-edge | src/abi_impl/http2/model.rs | unit | 4 |
| pd-edge | src/abi_impl/http2/upstream.rs | unit | 4 |
| pd-edge | src/bin/pd-edge-sample-echo-server.rs | unit | 4 |
| pd-edge | src/cache.rs | unit | 4 |
| pd-edge | tests/sample_echo.rs | integration | 4 |
| pd-edge | src/runtime.rs | unit | 3 |
| pd-edge | src/runtime/http_plane/proxy_path.rs | unit | 3 |
| pd-edge | src/runtime/vm_runner.rs | unit | 3 |
| pd-edge | tests/proxy_tests/debug.rs | integration | 3 |
| pd-edge | src/abi_impl/proxy.rs | unit | 2 |
| pd-edge | src/abi_impl/transport/mod.rs | unit | 2 |
| pd-edge | src/abi_impl/websocket/state.rs | unit | 2 |
| pd-edge | src/build_info.rs | unit | 2 |
| pd-edge | tests/proxy_tests/attach_transport.rs | integration | 2 |
| pd-edge | tests/proxy_tests/webrtc.rs | integration | 2 |
| pd-edge | src/abi_impl/http/outbound_http1.rs | unit | 1 |
| pd-edge | src/abi_impl/webrtc/mod.rs | unit | 1 |
| pd-edge | tests/proxy_tests/control_plane.rs | integration | 1 |
| pd-edge | tests/proxy_tests/forward_proxy.rs | integration | 1 |
| pd-edge-host-function | _none_ | - | 0 |
| pd-edge-abi | src/lib.rs | unit | 5 |
| pd-vm | src/bin/pd-vm-run.rs | unit | 65 |
| pd-vm | tests/compiler/compiler_rustscript_tests.rs | integration | 57 |
| pd-vm | tests/compiler/compiler_common_tests.rs | integration | 54 |
| pd-vm | tests/vm/vm_runtime_tests.rs | integration | 54 |
| pd-vm | src/debugger/tests.rs | unit | 24 |
| pd-vm | src/vm/tests.rs | unit | 22 |
| pd-vm | tests/vm/drop_contract_tests.rs | integration | 22 |
| pd-vm | tests/jit/jit_tests.rs | integration | 21 |
| pd-vm | tests/compiler/compiler_javascript_tests.rs | integration | 17 |
| pd-vm | tests/compiler/diagnostics_tests.rs | integration | 16 |
| pd-vm | tests/builtins/stdlib_tests.rs | integration | 12 |
| pd-vm | tests/wire/wire_tests.rs | integration | 12 |
| pd-vm | tests/wire/assembler_vmbc_edge_tests.rs | integration | 11 |
| pd-vm | tests/vm/runtime_state_edge_tests.rs | integration | 8 |
| pd-vm | src/builtins/runtime/core.rs | unit | 7 |
| pd-vm | src/builtins/runtime/host.rs | unit | 7 |
| pd-vm | src/compiler/format.rs | unit | 7 |
| pd-vm | tests/builtins/io_builtin_edge_tests.rs | integration | 7 |
| pd-vm | tests/compiler/module_import_tests.rs | integration | 7 |
| pd-vm | tests/example_tests.rs | integration | 7 |
| pd-vm | tests/jit/jit_nyi_edge_tests.rs | integration | 7 |
| pd-vm | tests/vm/functional_parity_tests.rs | integration | 7 |
| pd-vm | tests/compiler/compiler_lua_tests.rs | integration | 6 |
| pd-vm | tests/compiler/type_inference_tests.rs | integration | 6 |
| pd-vm | tests/jit/perf_tests.rs | integration | 6 |
| pd-vm | src/builtins/runtime/math.rs | unit | 5 |
| pd-vm | tests/compiler/compiler_scheme_tests.rs | integration | 5 |
| pd-vm | tests/compiler/whitespace_resilience_tests.rs | integration | 4 |
| pd-vm | src/builtins/runtime/print.rs | unit | 3 |
| pd-vm | src/builtins/runtime/typed.rs | unit | 3 |
| pd-vm | src/bytecode.rs | unit | 3 |
| pd-vm | src/compiler/source_loader.rs | unit | 3 |
| pd-vm | src/compiler/source_loader/rewrite.rs | unit | 3 |
| pd-vm | src/compiler/parser/format.rs | unit | 2 |
| pd-vm | tests/vm/vm_async_runtime_tests.rs | integration | 2 |
| pd-vm | src/builtins/runtime/mod.rs | unit | 1 |
| pd-vm | src/compiler/frontends/scheme.rs | unit | 1 |
| pd-vm | src/debug_info.rs | unit | 1 |
| pd-vm | src/vm/jit/native/cranelift.rs | unit | 1 |
| pd-vm | src/vm/jit/native/mod.rs | unit | 1 |
| pd-vm | src/vm/jit/trace.rs | unit | 1 |
| pd-vm | tests/common/mod.rs | integration | 1 |
| pd-host-function | _none_ | - | 0 |
| pd-vm-wasm | src/lib.rs | unit | 48 |
| pd-vm-wasm | src/completions.rs | unit | 9 |

## pd-controller

- Manifest: `pd-controller/Cargo.toml`
- LOC: **11298**
- Cumulative workspace LOC at this crate: **11298**
- Cargo features: -
- Tests: **32**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| server/ui_codegen | 7424 | 7424 |
| server | 1923 | 9347 |
| server/handlers | 1682 | 11029 |
| root | 185 | 11214 |
| build | 84 | 11298 |

### Test Suites

| Suite | Kind | Tests |
| --- | --- | ---: |
| tests/controller_tests/ui.rs | integration | 14 |
| src/main.rs | unit | 7 |
| tests/controller_tests/programs.rs | integration | 5 |
| tests/controller_tests/rpc.rs | integration | 4 |
| tests/controller_tests/debug.rs | integration | 1 |
| tests/e2e_demo_tests.rs | integration | 1 |

## pd-edge

- Manifest: `pd-edge/Cargo.toml`
- LOC: **35350**
- Cumulative workspace LOC at this crate: **46648**
- Cargo features: console, default, http, http2, http3, native-jit, tls, webrtc, websocket
- Tests: **239**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| abi_impl/http | 10784 | 10784 |
| abi_impl/transport | 4060 | 14844 |
| runtime/http_plane | 3394 | 18238 |
| abi_impl/http2 | 1894 | 20132 |
| abi_impl/websocket | 1806 | 21938 |
| abi_impl/http3 | 1313 | 23251 |
| abi_impl/proxy | 1130 | 24381 |
| sample_echo | 1116 | 25497 |
| abi_impl/webrtc | 1048 | 26545 |
| abi_impl | 999 | 27544 |
| bin/pd-edge-console | 995 | 28539 |
| bin/pd-edge-http-proxy | 863 | 29402 |
| runtime | 839 | 30241 |
| debug_session | 772 | 31013 |
| runtime/vm_runner | 697 | 31710 |
| bin/pd-edge-transport-proxy | 617 | 32327 |
| abi_impl/io | 369 | 32696 |
| bin/pd-edge-sample-echo-server | 362 | 33058 |
| lock_metrics | 347 | 33405 |
| active_control_plane | 317 | 33722 |
| abi_impl/quic | 293 | 34015 |
| cache | 291 | 34306 |
| control_plane_rpc | 196 | 34502 |
| runtime/transport_plane | 147 | 34649 |
| abi_impl/registry | 133 | 34782 |
| logging | 112 | 34894 |
| abi_impl/console | 110 | 35004 |
| control_plane_http_client | 110 | 35114 |
| build_info | 67 | 35181 |
| root | 60 | 35241 |
| compile | 41 | 35282 |
| abi_impl/runtime | 38 | 35320 |
| build | 30 | 35350 |

### Test Suites

| Suite | Kind | Tests |
| --- | --- | ---: |
| tests/proxy_tests/http.rs | integration | 51 |
| tests/proxy_tests/transport.rs | integration | 24 |
| src/bin/pd-edge-http-proxy.rs | unit | 19 |
| src/bin/pd-edge-console.rs | unit | 18 |
| src/abi_impl/transport/state.rs | unit | 14 |
| src/abi_impl/http/state.rs | unit | 11 |
| src/abi_impl/mod.rs | unit | 8 |
| tests/compile_tests.rs | integration | 8 |
| tests/proxy_tests/tls.rs | integration | 7 |
| tests/proxy_tests/websocket.rs | integration | 7 |
| src/bin/pd-edge-transport-proxy.rs | unit | 6 |
| src/debug_session.rs | unit | 5 |
| tests/proxy_tests/io.rs | integration | 5 |
| src/abi_impl/http/exchange.rs | unit | 4 |
| src/abi_impl/http/fast_path.rs | unit | 4 |
| src/abi_impl/http2/model.rs | unit | 4 |
| src/abi_impl/http2/upstream.rs | unit | 4 |
| src/bin/pd-edge-sample-echo-server.rs | unit | 4 |
| src/cache.rs | unit | 4 |
| tests/sample_echo.rs | integration | 4 |
| src/runtime.rs | unit | 3 |
| src/runtime/http_plane/proxy_path.rs | unit | 3 |
| src/runtime/vm_runner.rs | unit | 3 |
| tests/proxy_tests/debug.rs | integration | 3 |
| src/abi_impl/proxy.rs | unit | 2 |
| src/abi_impl/transport/mod.rs | unit | 2 |
| src/abi_impl/websocket/state.rs | unit | 2 |
| src/build_info.rs | unit | 2 |
| tests/proxy_tests/attach_transport.rs | integration | 2 |
| tests/proxy_tests/webrtc.rs | integration | 2 |
| src/abi_impl/http/outbound_http1.rs | unit | 1 |
| src/abi_impl/webrtc/mod.rs | unit | 1 |
| tests/proxy_tests/control_plane.rs | integration | 1 |
| tests/proxy_tests/forward_proxy.rs | integration | 1 |

## pd-edge-host-function

- Manifest: `pd-edge/pd-edge-host-function/Cargo.toml`
- LOC: **1000**
- Cumulative workspace LOC at this crate: **47648**
- Cargo features: -
- Tests: **0**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| edge | 989 | 989 |
| root | 11 | 1000 |

### Test Suites

_No detected tests._

## pd-edge-abi

- Manifest: `pd-edge-abi/Cargo.toml`
- LOC: **1670**
- Cumulative workspace LOC at this crate: **49318**
- Cargo features: console, default, http, http2, tls, webrtc, websocket
- Tests: **5**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| build | 802 | 802 |
| root | 160 | 962 |
| abi_spec/http.exchange | 114 | 1076 |
| abi_spec/tls | 76 | 1152 |
| abi_spec/websocket | 76 | 1228 |
| abi_spec/webrtc | 68 | 1296 |
| abi_spec/http.request | 60 | 1356 |
| abi_spec/tcp | 60 | 1416 |
| abi_spec/udp | 60 | 1476 |
| abi_spec/http.response | 56 | 1532 |
| abi_spec/proxy | 47 | 1579 |
| abi_spec/console | 40 | 1619 |
| abi_spec/functions | 20 | 1639 |
| abi_spec/namespaces | 15 | 1654 |
| abi_spec/runtime | 12 | 1666 |
| abi_spec/http.downstream | 4 | 1670 |

### Test Suites

| Suite | Kind | Tests |
| --- | --- | ---: |
| src/lib.rs | unit | 5 |

## pd-vm

- Manifest: `pd-vm/Cargo.toml`
- LOC: **52453**
- Cumulative workspace LOC at this crate: **101771**
- Cargo features: cli, cranelift-jit, default, http, http2, runtime, tls, webrtc, websocket
- Tests: **509**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| vm/jit | 8721 | 8721 |
| compiler/parser | 5995 | 14716 |
| compiler/typing | 5506 | 20222 |
| compiler/frontends | 5142 | 25364 |
| compiler/lifetime | 4551 | 29915 |
| builtins/runtime | 2515 | 32430 |
| compiler/source_loader | 2329 | 34759 |
| bin/pd-vm-run | 2021 | 36780 |
| compiler/codegen | 1836 | 38616 |
| build | 1725 | 40341 |
| vm | 1261 | 41602 |
| debugger/tests | 1130 | 42732 |
| compiler/pipeline | 1008 | 43740 |
| debugger | 964 | 44704 |
| vm/host | 878 | 45582 |
| vmbc | 854 | 46436 |
| assembler | 785 | 47221 |
| vm/tests | 783 | 48004 |
| bytecode | 719 | 48723 |
| compiler/linker | 509 | 49232 |
| debugger/replay | 484 | 49716 |
| compiler/ir | 467 | 50183 |
| compiler | 426 | 50609 |
| debugger/recording | 329 | 50938 |
| vm/superinstructions | 236 | 51174 |
| compiler/source_map | 211 | 51385 |
| vm/epoch | 192 | 51577 |
| debug_info | 179 | 51756 |
| vm/fuel | 163 | 51919 |
| compiler/format | 138 | 52057 |
| vm/store | 118 | 52175 |
| builtins | 80 | 52255 |
| root | 70 | 52325 |
| builtins/metadata | 57 | 52382 |
| compiler/diagnostics | 50 | 52432 |
| vm/diagnostics | 21 | 52453 |

### Test Suites

| Suite | Kind | Tests |
| --- | --- | ---: |
| src/bin/pd-vm-run.rs | unit | 65 |
| tests/compiler/compiler_rustscript_tests.rs | integration | 57 |
| tests/compiler/compiler_common_tests.rs | integration | 54 |
| tests/vm/vm_runtime_tests.rs | integration | 54 |
| src/debugger/tests.rs | unit | 24 |
| src/vm/tests.rs | unit | 22 |
| tests/vm/drop_contract_tests.rs | integration | 22 |
| tests/jit/jit_tests.rs | integration | 21 |
| tests/compiler/compiler_javascript_tests.rs | integration | 17 |
| tests/compiler/diagnostics_tests.rs | integration | 16 |
| tests/builtins/stdlib_tests.rs | integration | 12 |
| tests/wire/wire_tests.rs | integration | 12 |
| tests/wire/assembler_vmbc_edge_tests.rs | integration | 11 |
| tests/vm/runtime_state_edge_tests.rs | integration | 8 |
| src/builtins/runtime/core.rs | unit | 7 |
| src/builtins/runtime/host.rs | unit | 7 |
| src/compiler/format.rs | unit | 7 |
| tests/builtins/io_builtin_edge_tests.rs | integration | 7 |
| tests/compiler/module_import_tests.rs | integration | 7 |
| tests/example_tests.rs | integration | 7 |
| tests/jit/jit_nyi_edge_tests.rs | integration | 7 |
| tests/vm/functional_parity_tests.rs | integration | 7 |
| tests/compiler/compiler_lua_tests.rs | integration | 6 |
| tests/compiler/type_inference_tests.rs | integration | 6 |
| tests/jit/perf_tests.rs | integration | 6 |
| src/builtins/runtime/math.rs | unit | 5 |
| tests/compiler/compiler_scheme_tests.rs | integration | 5 |
| tests/compiler/whitespace_resilience_tests.rs | integration | 4 |
| src/builtins/runtime/print.rs | unit | 3 |
| src/builtins/runtime/typed.rs | unit | 3 |
| src/bytecode.rs | unit | 3 |
| src/compiler/source_loader.rs | unit | 3 |
| src/compiler/source_loader/rewrite.rs | unit | 3 |
| src/compiler/parser/format.rs | unit | 2 |
| tests/vm/vm_async_runtime_tests.rs | integration | 2 |
| src/builtins/runtime/mod.rs | unit | 1 |
| src/compiler/frontends/scheme.rs | unit | 1 |
| src/debug_info.rs | unit | 1 |
| src/vm/jit/native/cranelift.rs | unit | 1 |
| src/vm/jit/native/mod.rs | unit | 1 |
| src/vm/jit/trace.rs | unit | 1 |
| tests/common/mod.rs | integration | 1 |

## pd-host-function

- Manifest: `pd-vm/pd-host-function/Cargo.toml`
- LOC: **386**
- Cumulative workspace LOC at this crate: **102157**
- Cargo features: -
- Tests: **0**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| root | 386 | 386 |

### Test Suites

_No detected tests._

## pd-vm-wasm

- Manifest: `pd-vm/pd-vm-wasm/Cargo.toml`
- LOC: **5187**
- Cumulative workspace LOC at this crate: **107344**
- Cargo features: default, runtime
- Tests: **57**

### Feature Areas

| Feature area | LOC | Cumulative in crate |
| --- | ---: | ---: |
| root | 2329 | 2329 |
| completions | 1280 | 3609 |
| runtime | 1252 | 4861 |
| analyzer | 280 | 5141 |
| stdlib | 46 | 5187 |

### Test Suites

| Suite | Kind | Tests |
| --- | --- | ---: |
| src/lib.rs | unit | 48 |
| src/completions.rs | unit | 9 |
