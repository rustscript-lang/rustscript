# Automatic Host Binding Selection Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Select the highest-performance safe VM host binding from every generated `#[pd_host_function]` signature and preserve that capability through cached registry binding.

**Architecture:** Add a non-yielding static args entry to `HostFunctionRegistry`, then classify generated default hosts into VM-aware stack, general args, or non-yielding args bindings. Existing default hosts provide real representatives: `print` remains VM-aware, `runtime::exit` becomes args-only `CallOutcome`, and `runtime::sleep` becomes args-only `VmResult<bool>`.

**Tech Stack:** Rust 2024, `syn` build-time AST parsing, Cargo integration tests, Trace JIT/native JIT feature tests.

---

### Task 1: Preserve non-yielding args bindings in `HostFunctionRegistry`

**Objective:** Make cached registry plans install `ArgsStaticNonYielding` instead of degrading to `ArgsStatic`.

**Files:**
- Modify: `src/vm/host.rs`
- Test: `tests/compiler/compiler_common_tests.rs`

**Step 1: Write the failing registry test**

Add a test that registers `print` and `add_one` through a new `register_static_non_yielding_args` API, prepares one plan, binds two VMs, runs both, and checks the expected final value.

**Step 2: Verify RED**

Run:

```bash
cargo test --test compiler_tests host_function_registry_caches_static_non_yielding_args_function_pointer_plan_across_vms -- --exact
```

Expected: compilation fails because `register_static_non_yielding_args` does not exist.

**Step 3: Implement the minimal registry support**

Add `RegistryEntryKind::ArgsStaticNonYielding`, `HostFunctionRegistry::register_static_non_yielding_args`, and the matching `bind_vm_with_plan` arm that calls `Vm::register_static_non_yielding_args_function`.

**Step 4: Verify GREEN**

Run the exact test, then the surrounding compiler test target.

**Step 5: Commit and push**

Commit only `src/vm/host.rs` and `tests/compiler/compiler_common_tests.rs`, then push to `origin/master`.

### Task 2: Classify generated bindings from signatures

**Objective:** Route generated default hosts to the best compatible static binding.

**Files:**
- Modify: `build.rs`
- Modify: `src/builtins/runtime/host.rs`
- Test: `src/builtins/runtime/host.rs`
- Create: `tests/host_binding_generation_tests.rs`

**Step 1: Write failing behavior tests**

Remove unused `Vm` parameters from `runtime::sleep` and `runtime::exit` test expectations, then add a native-JIT regression proving the generated args-only `runtime::sleep` binding is marked non-yielding and can appear as a host call inside a trace. Keep the existing `runtime::exit` halt test as the general-args contract check.

**Step 2: Verify RED**

Run the narrow host/JIT tests. Expected: generated code still selects `bind_static_args_function`, so the JIT host-call assertion fails.

**Step 3: Implement signature classification**

Add `HostBindingKind::{StaticStack, StaticArgs, StaticNonYieldingArgs}` in `build.rs`. Classify in this order:

1. any `Vm` parameter → `StaticStack`;
2. normalized `CallOutcome`, including `VmResult<CallOutcome>` and `HostResult<CallOutcome>` → `StaticArgs`;
3. all other valid args-only returns → `StaticNonYieldingArgs`.

Use that one classification in generated registry and direct VM binding code. The non-yielding branches must emit `register_static_non_yielding_args` and `bind_static_non_yielding_args_function`.

**Step 4: Verify GREEN**

Run host runtime tests, the exact JIT regression with native JIT features, and generated-code compilation through normal Cargo tests.

**Step 5: Commit and push**

Commit only the generator, host signature, and focused tests, then push to `origin/master`.

### Task 3: Broad verification

**Objective:** Confirm formatting, lint, workspace behavior, and native JIT behavior without modifying unrelated files.

**Files:**
- No intended source changes; apply formatting only to files from Tasks 1–2.

**Step 1: Format check**

```bash
cargo fmt --all -- --check
```

**Step 2: Focused and workspace tests**

```bash
cargo test -p pd-host-function
cargo test --workspace
cargo test --features cranelift-jit --test jit_tests
```

**Step 3: Lint and build**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
```

Use an isolated `CARGO_TARGET_DIR` when necessary so existing concurrent work and large native builds do not interfere.

**Step 4: Inspect final diff and remote state**

Confirm only planned paths plus pre-existing unrelated changes remain, `git diff --check` passes for the implementation commits, and local/remote `master` SHAs match.
