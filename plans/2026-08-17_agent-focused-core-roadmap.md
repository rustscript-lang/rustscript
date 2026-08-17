# RustScript Agent-Focused Core Roadmap and PR Stack Rewrite Plan

**Goal:** Rebuild the open RustScript core PR stack into seven semantic, single-commit layers; retain only capabilities used by the RustScript Agent production path; and replace the stack's overlapping plan documents with this one canonical roadmap.

**Architecture:** The rewritten stack is ordered by final dependency boundaries rather than historical implementation order. Compiler semantics, host lifecycle, authorization, invocation streaming, local allocation, and HTTP transport remain separate review units. Historical fixups are folded into the owning feature layer. The Agent production path defines retention: buffered HTTP and callable SSE remain; WebSocket, generic RSS task scheduling, wide-local bytecode, and unrelated backend convergence work are excluded from this stack.

**Tech Stack:** Rust 2024, RustScript compiler and VM, `pd-host-function`, Hyper/Tokio/Rustls HTTP, no-std VM, wasm target, Cargo workspace tests, GitHub stacked PRs.

---

## 1. Baseline and scope

### Git baseline

- Stack base: `3affd5214c4f9f6b7710ac86a1f1948487f03a50` (`origin/master`).
- Historical stack tip: `06b37fd155be2b81ba4b41dbb6514e7b283f4f10`.
- Current Agent core pin: `fd4b570d08d7cc90cc29e3b05df59c9e9bf3b88e`.
- The Agent pin descends from the historical stack tip and adds compiler, typing, ownership, and JSON fixes. It does not add a task scheduler or bounded process API.
- Open stack PRs under review: #13-#20 and #22-#26.

### In scope

- Rebuild the open stack into seven one-commit PR layers.
- Consolidate the stack-added plan files into this document.
- Preserve functionality used by the Agent production runner.
- Remove stack-added functionality with no Agent production consumer and no approved Agent adoption path.
- Preserve tests with the feature layer they verify.
- Retarget retained PR bases and close absorbed PRs after remote verification.

### Out of scope

- Changes to pre-existing plans already on `master`.
- Agent repository changes.
- New terminal, filesystem, or task implementation in this rewrite.
- Broader JIT/AOT redesign.
- Removal of legacy core APIs that predate the stack.
- Publishing a crate or release tag.

---

## 2. Agent production capability audit

| Capability | Core/internal use | Agent production use | Decision |
|---|---|---|---|
| Semantic module graph and nested module identity | Compiler-owned | Required to compile Agent RSS modules | Keep |
| Unified `HostRuntime`, `OperationRegistry`, cancellation reasons, resource ownership | VM-owned | Required by Agent invocation lifecycle | Keep |
| Capability profiles and IO policies | Host registry and runtime | Required for restricted Agent tool execution | Keep |
| Generic `HostAsyncBridge` | Async host boundary | Implemented by `AgentAsyncBridge` | Keep |
| Blocking/async IO ownership | Runtime IO implementation | Required by Agent file and host policy paths | Keep |
| `Invocation`, `InvocationItem`, `InvocationPoll` | VM item stream | Used directly by `rss_runner` | Keep |
| Frame-aware local allocation | Compiler/codegen | Required by current Agent callable RSS | Keep |
| Buffered `http::client::request` | HTTP runtime | Used by Anthropic and OpenAI RSS providers | Keep |
| Callable `http::client::sse` | HTTP runtime and callable stream pump | Used by Anthropic and OpenAI streaming providers | Keep |
| Generic callable stream pump | Internal VM/HTTP mechanism | Required indirectly by Agent SSE | Keep internally; do not retain unused public embedding surface |
| `http::client::websocket` | Core implementation and tests only | No Agent source or production call site | Remove from rewritten stack |
| Public `HostStreamPoll/Action/Driver` exports | Public API only; HTTP consumes them internally | No Agent direct use | Narrow to crate-private/internal surface where SSE permits |
| RSS `task::spawn/await/cancel` plan | No implementation in the stack | Native Agent supervisor already owns A6 production scheduling | Remove from active roadmap |
| Wide-local bytecode plan | No implementation in the stack | No Agent requirement after frame-aware allocation | Remove from active roadmap |
| Backend semantic convergence plan | No implementation in the stack | No Agent production dependency | Remove from this stack |
| `io::popen` | Legacy core runtime API | Registered but rejected by Agent terminal adapter due missing bounds | Preserve baseline compatibility; do not extend or use for Agent terminal |

Tests and documentation alone do not count as a production consumer. A stack-added public API with no Agent use is retained only when another retained production mechanism requires it.

---

## 3. Canonical seven-layer stack

Final dependency chain:

```text
origin/master
  -> #14 unified Agent-focused core roadmap
  -> #15 semantic module graph
  -> #16 unified host lifecycle
  -> #18 capability profiles and async host execution
  -> #23 invocation item stream
  -> #24 frame-aware local allocation
  -> #26 buffered HTTP and callable SSE
```

Every retained layer must contain exactly one logical commit relative to its base.

### Layer 1: #14 unified roadmap

**Retained PR:** #14

**Content:**

- This file only.
- Completed, active, inactive, and removed scopes are explicit.
- Historical implementation instructions are summarized rather than copied.

**Absorbs:**

- #22 invocation-stream plan.
- #25 wide-local plan.
- Plan commits embedded in #13, #15, #24, and #26.
- All stack-added `2026-08-09_*`, `2026-08-11_*`, and `2026-08-12_*` plan files.

**Commit subject:**

```text
plan(agent): consolidate core runtime roadmap
```

### Layer 2: #15 semantic module graph

**Retained PR:** #15

**Content:**

- Compiler-owned module identities.
- Nested import/export and source ownership correctness.
- Semantic resolution and diagnostics.
- Tests for UTF-8, nested modules, cycles, imports, exports, and source spans.

**Excluded:**

- The old standalone semantic-module plan file.
- Unrelated future backend convergence work.

**Commit subject:**

```text
refactor(compiler): build semantic module graph
```

### Layer 3: #16 unified host lifecycle

**Retained PR:** #16

**Content:**

- `HostRuntime`, resource arena, operation registry, and cancellation state.
- Typed operation ownership and bounded pending operations.
- Parent cancellation and deadline propagation.
- SQLite/IO migration onto shared lifecycle primitives.

**Excluded:**

- Any second cancellation-token tree.
- HTTP-owned scheduler or pending-operation registry.
- Generic script task scheduling.

**Commit subject:**

```text
refactor(vm): unify host runtime lifecycle
```

### Layer 4: #18 capability profiles and async host execution

**Retained PR:** #18

**Content:**

- Immutable capability profiles.
- Authorization before all host dispatch paths.
- Restricted registry defaults.
- IO policy binding and limits.
- Host plan cache identity required by authorization.
- Generic async host submission, polling, completion, and typed cancellation.
- Host-owned configuration and policy storage.
- Feature-selected blocking/async IO implementation and process resource cleanup.
- Proc-macro support for generic async host functions.

**Absorbs:**

- #19 generic async host drivers.
- #20 host-owned policies and async IO.

**Excluded:**

- HTTP files and HTTP-only dependency changes; those move to #26.
- Generic script task scheduling.

**Commit subject:**

```text
refactor(host): bind capabilities and async execution
```

### Layer 5: #23 invocation item stream

**Retained PR:** #23

**Content:**

- `Invocation`, `InvocationItem`, and `InvocationPoll`.
- Event items followed by one complete item or one typed terminal error.
- Fuel, deadline, capability, host, and cancellation error mapping.
- Consumer Drop retirement of the current waiting host operation.
- Fused terminal behavior.

**Absorbs:**

- #22 plan semantics.
- Invocation Drop and abandonment fixes later developed during #26.

**Excluded:**

- Detached task handles.
- Script-visible HTTP cancellation identifiers.

**Commit subject:**

```text
feat(vm): expose cancellable invocation item streams
```

### Layer 6: #24 final compiler typing and ownership

**Retained PR:** #24

**Content:**

- Frame-aware liveness and local-slot accounting.
- Named-call lowering and callable frame isolation.
- Interpreter, no-std, JIT, native, and AOT parity required by the implemented opcode contract.
- Callable schema preservation across modules and imports.
- Dynamic `map<unknown>` and `array<unknown>` runtime container typing.
- Live callable parameters across native calls.
- Mutable closure-capture sharing and JIT root-callable registration.
- JSON map encode/decode typing compatibility required by the Agent.
- Regression coverage for recursion, captures, ownership, JSON, wire metadata, and local pressure.

**Excluded:**

- Wide-local bytecode and slot widths above the current encoded limit.
- The standalone frame-aware and wide-local plan files.

**Commit subject:**

```text
feat(compiler): finalize frame-local typing and ownership
```

### Layer 7: #26 buffered HTTP and callable SSE

**Retained PR:** #26

**Content:**

- Final bounded buffered HTTP request implementation from #13.
- Address policy, redirect revalidation, response-size bounds, and total request deadline from #17 and later fixes.
- HTTP submission through the generic host async bridge.
- Callable schema support required by SSE callbacks.
- Internal host-to-callable stream pump.
- Callable SSE request, parsing, callback actions, total stream deadline, and transport cleanup.
- Generic stream tests moved under the crate-internal VM test module after public API removal.
- Documentation and tests for buffered HTTP and SSE only.

**Absorbs:**

- #13 bounded cancellable host client.
- #17 address and response deadline hardening.
- HTTP-specific portions of #20.
- Retained portions of #26.

**Removed from this layer:**

- WebSocket builtin registration and metadata.
- WebSocket transport implementation.
- WebSocket-only configuration fields and dependencies.
- WebSocket tests and documentation.
- Public HostStream exports with no Agent consumer.
- Historical fixup commits and unreachable-branch cleanup commits as separate history.

**Commit subject:**

```text
feat(http): add bounded request and callable SSE clients
```

---

## 4. Absorbed PR disposition

| PR | Historical purpose | Final owner | Remote action after verification |
|---|---|---|---|
| #13 | Buffered HTTP client | #26 | Close as superseded by #26 |
| #17 | HTTP address/deadline hardening | #26 | Close as superseded by #26 |
| #19 | Generic async host drivers | #18 | Close as superseded by #18 |
| #20 | Host-owned policies and async IO | #18 | Close as superseded by #18 |
| #22 | Invocation item stream plan | #14/#23 | Close as superseded by #14 and #23 |
| #25 | Wide-local plan | #14 decision log; implementation inactive | Close as superseded by #14 |

Retained PR titles and bodies must be updated to describe final scope and direct reviewers to this roadmap. Absorbed PRs are closed only after retained branch refs, bases, and CI are verified.

---

## 5. Plan consolidation disposition

Only stack-added plans are consolidated. Plans already present on `master` remain unchanged.

| Historical stack plan | Final status in this roadmap |
|---|---|
| Architecture plan index | Replaced by this document |
| Backend semantic convergence | Removed from Agent-focused stack |
| Capability profile host binding | Completed in #18 |
| HTTP transport security executor | Completed in #18/#26 for retained HTTP surfaces |
| Nested module correctness | Completed in #15 |
| Invocation item/error contract | Completed in #23 |
| Semantic module system | Completed in #15 |
| Static builtin ID | Baseline already merged before this stack |
| Structured task supervisor | Inactive; native Agent supervisor remains production path |
| Unified host lifecycle | Completed in #16 |
| VM runtime decomposition | Retained only where delivered by #16/#23 |
| Frame-aware local allocation | Completed in #24 |
| Wide-local bytecode | Inactive; no Agent requirement |
| Callable streaming HTTP client | SSE retained in #26; WebSocket removed |

The historical files remain recoverable from pre-rewrite commit objects and the rollback bundle. They do not remain in the final stack tree.

---

## 6. Cancellation and scheduling decision

The stack already provides the reusable cancellation control plane:

- `OperationRegistry`.
- Bounded pending operations.
- Internal parent/child cancellation tokens.
- Typed `CancellationReason`.
- Resource cleanup.
- Async bridge cancellation.
- Invocation cancellation and Drop retirement.

The rewrite must not introduce:

- A second global cancellation token.
- A second pending-operation registry.
- An HTTP-specific scheduler.
- A HostStream-based task scheduler.
- Agent-side copies of VM cancellation state.

### Agent-focused task decision

No RSS task namespace or generic TaskHost is added in this stack. The Agent native supervisor already provides admit/start/cancel/terminal behavior, durable child links, ownership checks, fanout limits, and restart reconciliation. Reopen a core TaskHost proposal only when an approved Agent RSS workflow requires script-visible task handles.

### Process decision

The current stack does not provide bounded structured process execution. Legacy `io::popen` accepts a shell command string and lacks the Agent terminal contract. It remains a baseline compatibility API, but Agent terminal integration must not use it.

---

## 7. Active follow-up roadmap

Only two active core follow-ups remain after Agent-focused pruning.

### P1 when terminal execution becomes a release requirement: bounded `io::exec`

Required contract:

```text
io::exec(argv, timeout_ms, max_output_bytes)
```

Required semantics:

- Native argv execution with no shell.
- Host policy limit for timeout and total output bytes.
- Concurrent stdout/stderr collection.
- Typed spawn, timeout, cancellation, output-limit, and wait failures.
- Process-tree cleanup on timeout, cancellation, invocation Drop, and VM reset.
- Existing operation registry and internal cancellation token reused.
- Unix process group and Windows Job Object or equivalent tree ownership.
- Agent `terminal.run` continues returning typed unavailable until this contract lands.

### P2 security hardening: root-confined no-follow filesystem opens

Required semantics:

- Explicit symlink policy.
- Symlink metadata and read-link support when approved.
- Unix `openat2` with `RESOLVE_BENEATH`, `RESOLVE_NO_MAGICLINKS`, and optional `RESOLVE_NO_SYMLINKS`.
- Handle-relative `openat` fallback from a trusted root descriptor.
- Windows reparse-point equivalent.
- Deterministic final-component and parent-swap tests.
- Blocking/async parity.

### Inactive follow-ups

- Generic RSS TaskHost.
- WebSocket client.
- Wide-local bytecode.
- Backend-wide semantic convergence.

Inactive work requires a new product requirement and a fresh plan update before implementation.

---

## 8. Rewrite construction rules

1. Create a rollback manifest with every old PR head SHA and base SHA.
2. Create a Git bundle under `/mnt/TEMP/rustscript/` containing the historical stack refs.
3. Rebuild from `origin/master` in an isolated worktree.
4. Construct each retained layer from the final intended tree, not by preserving fixup chronology.
5. Keep tests in the same commit as the feature they verify.
6. Keep this canonical plan only in layer #14.
7. Remove WebSocket and unused public HostStream exposure while constructing #26.
8. Require one commit in each retained PR range.
9. Use explicit paths for staging.
10. Do not modify source worktrees, stashes, or Agent worktrees.
11. Publish with `--force-with-lease=<old-sha>` only after local verification.
12. Retarget PR bases before closing absorbed PRs.

---

## 9. Execution tasks

### Task 1: Freeze rollback data

**Files:**

- Verify: `/mnt/TEMP/rustscript/pr-stack-open-pr-manifest.json`
- Create: `/mnt/TEMP/rustscript/pr-stack-rewrite-manifest.json`
- Create: `/mnt/TEMP/rustscript/pr-stack-before-agent-prune.bundle`

**Steps:**

1. Re-read PR #13-#20 and #22-#26 from the GitHub REST API.
2. Compare every API head SHA with the recorded source SHAs in this file.
3. Create `refs/backup/pr-stack-20260817/pr-<number>` for every open PR head.
4. Bundle the backup refs.
5. Run `git bundle verify` and clone the bundle into a temporary verification directory.
6. Record old base/head refs, titles, bodies, and commit SHAs in the rewrite manifest.

Do not continue if any remote head changed after the approved design snapshot.

### Task 2: Finalize layer #14

**Branch:** `plan/architecture-remediation-roadmap`

**Base:** `origin/master@3affd5214c4f9f6b7710ac86a1f1948487f03a50`

The final #14 SHA is recorded in the rewrite manifest generated after autosquash.

**Verification:**

```bash
git rev-list --count origin/master..HEAD
git diff --name-status origin/master..HEAD
git diff --check origin/master..HEAD
```

Expected: one commit and one added plan file.

### Task 3: Build layer #15

**Branch:** `refactor/semantic-module-graph`

**Source commit:** `e2e554e967a689ed357df1cfafcdafa0d6f62aec`

**Base:** rewritten #14.

**Steps:**

1. Create the layer branch from rewritten #14.
2. Apply the source commit without committing.
3. Remove stack-added plan files from the staged result; this roadmap owns plan history.
4. Preserve compiler, CLI, wasm, no-std, source-map, diagnostic, and test changes from the source layer.
5. Commit with `refactor(compiler): build semantic module graph`.

**Focused gates:** semantic module, nested module, compiler, CLI, wasm, and no-std tests touched by the layer.

### Task 4: Build layer #16

**Branch:** `refactor/unified-host-runtime-lifecycle`

**Source commit:** `c1e90136c76c6d8eda4d58ccb1cda4cabe4f7dd1`

**Base:** rewritten #15.

**Steps:**

1. Apply the source commit without committing.
2. Resolve the missing historical #13 HTTP parent by deferring these paths to #26:
   - `src/builtins/runtime/http.rs` and later `src/builtins/runtime/http/**`.
   - `tests/vm/http_host_tests.rs`.
   - `docs/http-client.md`.
   - HTTP-only Cargo features and dependencies.
3. Keep lifecycle changes in `cancellation`, `resource`, `sqlite`, IO ownership, `HostRuntime`, `RunContext`, `Instance`, VM reset, and their tests.
4. Remove old plan files.
5. Commit with `refactor(vm): unify host runtime lifecycle`.

**Focused gates:** runtime context, runtime host, SQLite host, IO resource, VM reset/drop, and pending-operation-limit tests.

### Task 5: Build layer #18 and absorb #19/#20

**Branch:** `feat/host-capability-profiles`

**Source commits in order:**

1. `2ed098cb2f6d95efa897e1517b0a3032a22ab926`
2. `76f0b8df134fd345339dbf431bc454cee4ec9512`
3. `45fe584a85e525fffb06d265850fe32f5d8e14e6`

**Base:** rewritten #16.

**Steps:**

1. Apply all three source commits without creating intermediate commits.
2. Keep capability profiles, restricted registry semantics, generic async host drivers, proc-macro async support, host-owned policy storage, and blocking/async IO parity.
3. Defer HTTP source, HTTP tests, HTTP docs, and HTTP-only dependency changes to #26.
4. Retain non-HTTP portions of shared files such as `Cargo.toml`, `build.rs`, `src/lib.rs`, `src/vm/*`, and host-binding tests.
5. Remove old plan files.
6. Commit the combined result with `refactor(host): bind capabilities and async execution`.

**Focused gates:** capability profile tests, host binding generation, async bridge tests, blocking/async IO parity, SQLite host tests, no-std checks, and proc-macro tests.

### Task 6: Build layer #23

**Branch:** `feat/invocation-item-stream`

**Primary source commit:** `902efc352b145cb4b966f2332af886b21424eea4`

**Additional source fixes from the old #26 range:**

- Invocation Drop cancellation.
- Abandoned invocation retirement.
- Waiting host-operation cancellation.
- Exact-once retirement after consumer Drop.

**Base:** rewritten #18.

**Steps:**

1. Apply #23 without the old #22 plan.
2. Apply only invocation/host-lifecycle hunks from the later abandonment fixes.
3. Exclude HTTP stream driver code.
4. Collapse the primary implementation and later fixes into one commit.
5. Commit with `feat(vm): expose cancellable invocation item streams`.

**Focused gates:** invocation stream, event/complete ordering, typed terminal errors, Drop cancellation, deadline, fuel, and late-completion rejection.

### Task 7: Build layer #24

**Branch:** `fix/frame-aware-local-allocation`

**Source commit:** `b24400744a9d22366f37ab993b9909f84b095085`

**Base:** rewritten #23.

**Steps:**

1. Apply the source commit without committing.
2. Remove frame-aware and wide-local standalone plan files.
3. Fold the post-#24 compiler, typing, ownership, and JSON fixes through the Agent pin into this layer.
4. Keep implemented compiler, bytecode, interpreter, no-std, JIT, native, AOT, wasm, CLI, and regression-test changes.
5. Keep stream-specific ownership assertions for #26, where the internal stream implementation exists.
6. Commit with `feat(compiler): finalize frame-local typing and ownership`.

**Focused gates:** compiler, VM, recursion/capture, no-std, JIT, AOT, wasm, and true same-frame-pressure controls.

### Task 8: Build layer #26 and remove unused surfaces

**Branch:** `plan/callable-stream-integration`

**Source ranges:**

- Buffered HTTP: `3affd5214c4f9f6b7710ac86a1f1948487f03a50..475e5aa1249ba740b8f71c7cd0699f0bc9dc346a`.
- HTTP hardening: `c1e90136c76c6d8eda4d58ccb1cda4cabe4f7dd1..b73fa5149c26eb38b184b6d8b318fe2a8772f044`.
- Deferred HTTP portions from #16/#18/#19/#20.
- Callable streaming source: `df99a5ac05a9129a34eabd494a2aae49081db0d9..06b37fd155be2b81ba4b41dbb6514e7b283f4f10`.

**Base:** rewritten #24.

**Steps:**

1. Materialize the final buffered-request and callable-SSE implementation.
2. Keep callable schema support and the internal stream pump required by SSE.
3. Delete WebSocket registration, implementation, config, dependencies, tests, and docs.
4. Remove public `HostStreamPoll`, `HostStreamAction`, `HostStreamDriver`, and `Vm::submit_callable_stream` exports; move generic driver tests into the crate-internal VM test module.
5. Remove all historical stack plan files except this roadmap.
6. Compare the candidate tree with the Agent pin and classify every difference. Allowed differences are limited to plan consolidation, WebSocket deletion, HostStream visibility/test relocation, and commit-boundary ownership moves.
7. Commit with `feat(http): add bounded request and callable SSE clients`.

**Focused gates:** buffered request policy, redirects, response bounds, request deadline, SSE parsing, callback actions, callback errors, stream deadline, cancellation, callable schemas, and async bridge integration.

### Task 9: Run structural and unused-surface audit

1. Verify the seven commit ranges each contain one commit.
2. Verify only the canonical roadmap is added under `plans/` relative to master.
3. Search core production source and Agent source for all retained and removed API names.
4. Confirm no WebSocket builtin, dependency feature, configuration field, test fixture, documentation claim, or export remains.
5. Confirm retained internal HostStream machinery has a production SSE caller.
6. Confirm `Invocation`, `HostAsyncBridge`, HTTP request/SSE, capability policy, and frame-aware compiler paths have Agent consumers.
7. Confirm no task namespace or duplicate cancellation registry was introduced.

### Task 10: Run final gates and Agent compatibility

Run the complete core matrix from Section 10 with dedicated temporary Cargo home, target, and TMPDIR paths. Then test the Agent against the rewritten #26 full SHA in a separate worktree without a path override.

### Task 11: Publish with leases

1. Re-read all remote heads and abort if any SHA differs from the manifest.
2. Force-update the seven retained branches with exact `--force-with-lease=<branch>:<old-sha>` leases.
3. Update retained PR base branches, titles, and bodies through the authenticated GitHub API.
4. Verify the resulting head/base chain from GitHub.
5. Monitor retained CI.
6. Close #13, #17, #19, #20, #22, and #25 with replacement links.
7. Verify the final open PR list contains the seven retained layers only.

---

## 10. Verification matrix

### Structural checks

For every retained layer:

```bash
git rev-list --count <base>..<head>
git diff --check <base>..<head>
git log --oneline <base>..<head>
```

Expected:

- Commit count: `1`.
- No conflict markers or whitespace errors.
- Diff scope matches the owning layer.

### Feature audit checks

- Agent source references retained public APIs.
- No Agent source references removed WebSocket APIs.
- No `http::client::websocket` catalog or runtime registration remains.
- Internal callable stream pump remains reachable from SSE.
- Public HostStream exports are absent unless required by retained compilation boundaries.
- No stack-added duplicate cancellation registry remains.
- No `task` namespace is added.
- Only this new canonical file is added under `plans/` relative to `origin/master` by the open stack.

### Focused tests

Run the owning compiler, VM, host binding, invocation, IO, HTTP request, and SSE tests after each related layer is built.

### Final core gates

```bash
cargo test --workspace
cargo test -p pd-vm-nostd
cargo check -p pd-vm --no-default-features --features runtime
cargo build -p pd-vm-wasm --target wasm32-unknown-unknown --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
git diff --check origin/master..HEAD
```

All temporary targets, Cargo home directories, logs, fixtures, and generated artifacts must live under `/mnt/TEMP/rustscript/`.

### Agent compatibility gate

After the rewritten #26 head is available as a full 40-character revision:

- Build and test the Agent against that revision in a separate worktree.
- Run dependency-pin, core-repro, provider, runner, production-loop, gateway, and full workspace suites.
- Preserve official Git HTTPS dependency form.
- Do not use a path override.

---

## 11. Publication and rollback

Before any remote rewrite:

- Save old refs and PR metadata to `/mnt/TEMP/rustscript/pr-stack-rewrite-manifest.json`.
- Save a Git bundle to `/mnt/TEMP/rustscript/pr-stack-before-agent-prune.bundle`.
- Verify the bundle contains every old retained and absorbed head.

Remote sequence:

1. Force-update retained branches with exact leases.
2. Update retained PR bases to the seven-layer chain.
3. Update retained PR titles and bodies.
4. Verify GitHub reports the expected head/base SHAs.
5. Wait for retained PR checks.
6. Close absorbed PRs with explicit replacement links.
7. Re-read the open PR list and verify only the seven intended layers remain.

Rollback uses the manifest and bundle to restore each branch to its recorded SHA with an exact lease. No old commit is discarded before the bundle is verified.

---

## Definition of done

- Seven retained PRs form the documented dependency chain.
- Each retained PR contributes one logical commit.
- Six absorbed PRs are linked to their replacement and closed.
- One canonical stack plan remains.
- Buffered HTTP request and callable SSE remain functional.
- WebSocket and unused public HostStream exposure are absent.
- Agent-required invocation, async bridge, capability, compiler, and IO surfaces remain.
- Generic TaskHost and wide-local work are marked inactive.
- Full core gates pass with zero failures.
- Agent compatibility gates pass against the rewritten full revision.
- Remote head/base/title/body state matches the manifest produced after rewrite.
