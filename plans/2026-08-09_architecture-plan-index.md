# RustScript Architecture Plan Index

**Goal:** Classify the current VM/compiler/runtime findings into independent implementation plans with explicit dependency order.

**Architecture:** Correctness fixes are separated from structural refactors. Each plan owns one architectural surface and defines its own boundary and target criteria. Agent-owned behavior remains in `rustscript-agent` plans.

**Tech Stack:** RustScript compiler, VM, host runtime, interpreter/JIT/AOT/no-std, agent integration contracts.

---

## Classification

| Category | Problem surface | Independent plan | Depends on |
| --- | --- | --- | --- |
| Wire/ABI | Count-derived builtin indices change existing call IDs | `2026-08-09_static-builtin-id.md` | none |
| Compiler correctness | UTF-8 rewrite, parent-path normalization, nested source diagnostics, public entry-point parity | `2026-08-09_nested-module-correctness.md` | none |
| Compiler architecture | Text rewrite, synthetic preludes, flat global symbols, basename identity, source ownership | `2026-08-09_semantic-module-system.md` | nested correctness |
| VM ownership | Monolithic VM mixes engine/program/instance/run/host state | `2026-08-09_vm-runtime-decomposition.md` | static IDs |
| Host lifecycle | Generic resource/operation code unused; IO/HTTP/SQLite duplicate lifecycle/cancellation | `2026-08-09_unified-host-lifecycle.md` | VM decomposition |
| Execution contract | Return/event ambiguity, buffered-only events, string errors, fragmented terminal state | `2026-08-09_run-outcome-event-error-contract.md` | RunContext; host lifecycle for final cancellation integration |
| Authorization | Builtin fast-path bypass, mutable identity/cache complexity, Edge macro leakage | `2026-08-09_capability-profile-host-binding.md` | static IDs |
| Async host transport | Core macro contains Edge scope knowledge; HTTP owns a synchronous scheduler; IO lacks feature-selected blocking/async bindings | `2026-08-09_http-transport-security-executor.md` | capability profile and host lifecycle |
| Structured concurrency | One waiting slot, no generic multi-operation/child-program supervisor | `2026-08-09_structured-task-supervisor.md` | VM decomposition, host lifecycle, capability profile |
| Backend architecture | Repeated semantics across interpreter/JIT/AOT/native/no-std | `2026-08-09_backend-semantic-convergence.md` | static IDs, VM decomposition |

## Agent-owned plans

| Category | Plan |
| --- | --- |
| Canonical product/framework roadmap | `rustscript-agent/plans/2026-07-30_rustscript-agent-gateway-api.md` |
| Run admission, structured input, result/events, timeout/cancellation, live delivery | `rustscript-agent/plans/2026-08-09_agent-run-lifecycle-events.md` |
| Transactional RSS storage, retention, restart, replay, idempotency | `rustscript-agent/plans/2026-08-09_agent-durable-state.md` |

## Implementation route

### Wave 0: Immediate correctness and identity

Can run independently:

1. Static builtin IDs.
2. Nested module correctness.

Exit gate: static IDs no longer depend on catalog length; nested UTF-8/path/diagnostic regressions have executable coverage.

### Wave 1: Foundational ownership and authorization

Can run in parallel after relevant Wave 0 gates:

1. VM runtime decomposition after static IDs.
2. Capability profile/host binding after static IDs.
3. Semantic module system after nested correctness.

Exit gate: module semantics have explicit identities; VM has explicit ownership layers; every privileged call uses one authorization path.

### Wave 2: Unified execution lifecycle

1. Unified host resource/operation/cancellation lifecycle after VM ownership exists.
2. Invocation item stream and typed-error implementation on RunContext, integrating lifecycle cancellation as it becomes available.

Exit gate: production host subsystems use one lifecycle, and every invocation yields bounded `Event` items followed by one `Complete` item or typed error, then ends.

### Wave 3: Specialized consumers

Can run in parallel after their dependencies:

1. Generic host-driven async ABI, IO dual implementation, and async-only HTTP transport security.
2. Structured task supervisor.
3. Backend semantic convergence.
4. Agent run lifecycle and durable state integration.

## Scope boundary

- No plan adds compatibility decoding for pre-static-ID VMBC.
- No plan adds agent/provider/platform policy to `rustscript`.
- No agent plan defines VM internal implementation.
- Correctness plans do not wait for structural refactors.
- Structural plans remove superseded transitional paths after migration; they do not retain dual long-term architectures.
- New generic host functions require their own implementation plans; this index covers the architecture findings already identified.
- Async host futures are driven by the embedding host. VM, HTTP, and IO do not own a private executor or synchronous polling scheduler.
- Core host macros contain no pd-edge scopes, context types, registry generation, or downstream module paths.

## Target criteria

- Every finding from the architecture/current-worktree review maps to one owning plan.
- Cross-plan dependencies are explicit and acyclic.
- Each plan has implementation route, scope boundary, and target criteria.
- Core and agent ownership do not overlap.
- Obsolete review-finding and mixed historical plans are removed after their live requirements are represented here.
