# RustScript Host/HTTP Review Findings

**Review baseline:** `origin/master` (`ddfe64025456dc87dce4c7c7f79311a0b1c47a5a`) → `0f28ee9930890daffae5aa3c0bb70daf9c123997`, plus the current working-tree changes in `pd-host-function`.

**Review mode:** read-only review by `gpt-5.6-sol` with high reasoning. No tests or files were run/changed by the reviewer. The main agent owns the follow-up implementation.

## Blocking findings and directions

1. **SSRF DNS check is detached from the actual connection**
   - `src/builtins/runtime/http.rs:359-464` (`validate_url`, `execute_request`).
   - `ToSocketAddrs` validates one resolution, while Reqwest resolves again during `send()`. DNS rebinding and IPv4-mapped IPv6 can bypass the policy.
   - Use one validated resolver result for the connection; repeat resolve/validate/pin for every redirect; normalize mapped IPv6 and special-use ranges.

2. **HTTP cancellation leaves blocking request threads alive**
   - `src/builtins/runtime/http.rs:102-154,449-504`.
   - Cancellation removes the receiver but does not stop `send()`/`read_to_end()` work.
   - Use cancellable async tasks or explicit abort handles, enforce per-VM/process concurrency limits, and confirm task termination before dropping records.

3. **Host capability binding is fail-open**
   - `src/vm/host.rs:139-158,1795-1835`; `build.rs:997-1052`.
   - Default registry construction and lazy fallback prevent an embedding from declaring an explicit host capability set.
   - Add an explicit empty registry/capability profile, a no-default-fallback mode, and import preflight before side effects.

4. **Blocking DNS runs before the host call becomes pending**
   - `src/builtins/runtime/http.rs:193-216`.
   - Keep synchronous validation structural; move DNS, IP policy checks, and connect into the cancellable task.

5. **Empty port allowlist is interpreted as unrestricted**
   - `HttpConfig::default`, `validate_url`.
   - Fail closed or add an explicit unrestricted-port option.

6. **Timeout and response representation need explicit contracts**
   - Use one absolute deadline across redirects/DNS/connect/body.
   - Preserve duplicate and byte-valued response headers instead of silently dropping them.

7. **`pd_host_function` edge integration is coupled to downstream private layout**
   - `pd-host-function/src/lib.rs`, `pd-host-function/src/edge.rs`.
   - The macro currently emits `crate::abi_impl`, `::vm`, and `::linkme` paths. Introduce a stable support facade or separate edge extension boundary.

8. **Macro diagnostics and public API compatibility**
   - Infer execution type from `ItemFn.sig.asyncness` and return signatures rather than attribute shape.
   - Validate generic arity and reject unsupported native async forms with direct `syn::Error` diagnostics.
   - Assess the public `CallableDef` field addition and provide a compatibility constructor/API.

## Implementation order

1. Define the host macro contract: async from signature, scope only registration metadata.
2. Fix HTTP DNS/cancellation/timeout/port policy.
3. Add explicit host capability binding and remove implicit fallback where requested.
4. Stabilize the proc-macro support boundary and compile-fail coverage.
5. Run focused tests, then workspace gates.
