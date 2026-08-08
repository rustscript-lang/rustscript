# Capability Profile and Host Binding Contract Plan

**Goal:** Authorize every privileged builtin and host import through one immutable, auditable capability profile keyed by static callable identity.

**Architecture:** Compilation records required callable IDs; binding validates them against a `CapabilityProfile` with a stable fingerprint. Builtin fast paths, registry functions, overrides, cached plans, and cloned registries all use the same authorization decision before dispatch.

**Tech Stack:** Rust 2024, generated builtin catalog, host function registry, proc macro, binding-plan tests.

---

## Independence and dependency

- Depends on static builtin IDs for durable callable identity.
- Can be implemented alongside VM decomposition if the final owner is HostRuntime.
- HTTP policy consumes this profile but transport behavior is handled in a separate plan.

## Scope boundary

### In scope

- One authorization path for privileged builtins and registry host imports.
- Immutable capability profiles and stable fingerprints.
- Parameter-policy attachment points for path/network/database/process limits.
- Binding-plan cache correctness under clone/mutation/generation changes.
- Explicit ordinary versus Edge proc-macro contracts.

### Out of scope

- Adding agent/provider-specific capabilities.
- Implementing filesystem/process/task functions.
- Backward-compatible acceptance of implicit broad profiles.
- HTTP DNS/SSRF implementation details.
- pd-edge release workflow changes.

## Target model

```text
CallableId = static builtin ID or explicit host import identity
CapabilityProfile
  allowed callables
  typed policy objects
  delegation limits
  fingerprint

BindingPlan
  program requirements
  resolved call targets
  capability fingerprint
  registry generation/identity
```

## Implementation route

### Milestone 1: Add authorization regression tests

Required failing cases:

- empty profile rejects privileged builtin fast paths;
- empty profile rejects registry hosts and overrides;
- language-pure builtins remain available under the documented baseline profile;
- cached/uncached plans make identical decisions;
- sibling registry mutation invalidates or rejects stale plans;
- clone identity and structural independence follow one documented rule;
- parameter policy cannot be widened by script input.

### Milestone 2: Define immutable profiles

**Files:**
- Modify: `src/vm/host.rs`
- Create: `src/vm/capability.rs`
- Modify: `src/lib.rs`

1. Separate pure language builtins from privileged host capabilities.
2. Key builtin permissions by explicit static ID.
3. Key external hosts by a stable import identity, not a mutable vector slot alone.
4. Store typed HTTP/SQLite/IO policy references in the profile.
5. Compute a deterministic fingerprint from the effective immutable profile.

### Milestone 3: Put authorization before every dispatch path

**Files:**
- Modify: `src/vm/host.rs`
- Modify builtin dispatch and fast-path entry points

1. Resolve callable identity.
2. Authorize before builtin fast path, override, registry call, or native continuation.
3. Ensure a plan cannot grant a capability absent from the current profile.
4. Reject undeclared imports during preflight before side effects.

### Milestone 4: Simplify plan cache identity

1. Bind plans to program identity, registry identity/generation, and capability fingerprint.
2. Remove Arc-token combinations that encode overlapping identity concepts.
3. Define clone behavior explicitly and test source/sibling mutation.
4. Make stale-plan errors deterministic.

### Milestone 5: Separate proc-macro contracts

**Files:**
- Modify: `pd-host-function/src/lib.rs`
- Modify: `pd-host-function/src/edge.rs`
- Add independent consumer compile fixtures

1. Require an explicit Edge marker/scope for Edge-only expansion.
2. Ordinary async functions must receive an ordinary supported expansion or a direct compile-time diagnostic.
3. Apply one scoped HTTP routing rule to VM-aware and args-only forms.
4. Hide Edge implementation paths from ordinary downstream consumers.
5. Test from a separate crate that has no pd-edge internals.

### Milestone 6: Verification

```bash
cargo fmt --all -- --check
cargo test --locked -p pd-host-function
cargo test --locked --test runtime_host_tests
cargo test --locked --test http_host_tests --features http-client
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
git diff --check
```

## Target criteria

- No privileged builtin bypasses the capability decision.
- Empty means deny for every privileged path.
- Capability identity does not depend on catalog order or registry slot alone.
- Binding-plan cache keys contain one stable capability fingerprint.
- Clones and stale plans have tested deterministic semantics.
- Ordinary proc-macro consumers never receive Edge-private symbols.
- Parameter policies remain native upper bounds that script data cannot widen.
