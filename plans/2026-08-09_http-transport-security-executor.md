# HTTP Transport Security and Executor Plan

**Goal:** Make the generic HTTP host bounded, cancellable from DNS through body completion, and resistant to special-use address and redirect bypasses.

**Architecture:** Requests run in a shared bounded executor. DNS resolution is asynchronous, included in the total deadline, validated against an explicit address policy, and pinned to the approved destination for connection. Every redirect repeats validation and credential rules.

**Tech Stack:** Rust 2024, Reqwest/Tokio, resolver abstraction, HTTP host tests with local fixtures.

---

## Independence and dependency

- Independent of agent provider protocols.
- Uses the capability profile for configured policy and the unified operation lifecycle for cancellation/cleanup.
- Can develop tests and resolver abstraction before those plans finish.

## Scope boundary

### In scope

- Complete IPv4/IPv6 special-use classification.
- Cancellable bounded DNS and connection scheduling.
- Total/connect/first-byte/idle deadlines and in-flight limits.
- DNS result pinning and redirect revalidation.
- Cancellation/resource reclamation tests.

### Out of scope

- Provider JSON, retries, model selection, SSE semantic parsing, or agent loops.
- Ambient proxy support by default.
- Script-controlled relaxation beyond embedding policy.
- New HTTP source-language syntax.

## Implementation route

### Milestone 1: Freeze the network policy with tests

**Files:**
- Modify: `tests/vm/http_host_tests.rs`
- Add focused policy/resolver test helpers

Cover direct-IP and DNS answers for:

- loopback, private, link-local, multicast, unspecified;
- carrier-grade NAT `100.64.0.0/10`;
- benchmarking `198.18.0.0/15`;
- IPv4 `0.0.0.0/8` and reserved/documentation ranges;
- IPv6 unique-local, link-local, multicast, documentation, IPv4-mapped forms;
- mixed DNS answers containing allowed and denied addresses.

Define whether any denied answer rejects the host or only approved pinned answers may be selected; document and test the chosen fail-closed rule.

### Milestone 2: Introduce a resolver abstraction

**Files:**
- Modify: `src/builtins/runtime/http.rs`
- Create resolver support module if needed

1. Resolve asynchronously inside the request operation.
2. Start the total deadline before DNS.
3. Apply per-VM and process-wide permits before scheduling work.
4. Make cancellation abort or abandon resolver work within a tested grace period.
5. Avoid one OS thread per request.

### Milestone 3: Validate and pin destinations

1. Parse and validate scheme/host/port before resolution.
2. Validate every resolved IP against the configured policy.
3. Connect using an approved pinned address so a second resolver lookup cannot change the target.
4. Preserve TLS hostname/SNI verification against the original host.
5. Repeat resolve/validate/pin for every redirect target.

### Milestone 4: Enforce redirect and credential policy

1. Follow redirects manually or with a policy hook that validates before connection.
2. Strip authorization/cookie credentials on cross-origin redirects.
3. Reject scheme/port/host transitions outside policy.
4. Enforce redirect count and total deadline across all hops.
5. Do not automatically retry unsafe methods.

### Milestone 5: Bound all phases and cleanup

1. Enforce request/header/body limits before scheduling.
2. Enforce connect, first-byte, idle, total, and response-byte limits.
3. Register abort and body/stream resources with the shared operation lifecycle.
4. Verify cancel/reset/drop releases permits and leaves no pending task.
5. Disable ambient environment proxies unless the embedding explicitly supplies a bounded proxy policy.

### Milestone 6: Verification

```bash
cargo fmt --all -- --check
cargo test --locked --test http_host_tests --features http-client
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

Run concurrency tests that exceed configured permits and cancellation tests during DNS, connect, response headers, and body streaming.

## Target criteria

- DNS time counts against the total deadline.
- DNS and connection work obey per-VM and process-wide limits.
- Every connected address was validated and pinned before connection.
- Special-use IPv4/IPv6 ranges are denied by default.
- Redirects repeat destination and credential validation.
- Cancellation during every transport phase completes within the documented bound.
- Reset/drop leave no task, permit, stream, or response body live.
- HTTP host code contains no provider or agent policy.
