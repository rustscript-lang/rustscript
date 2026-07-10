# pd-vm-nostd Crate Extraction and Master Merge Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use codex-superpowers-subagent-driven-development (recommended) or codex-superpowers-executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the freestanding VMBC interpreter out of `pd-vm` into a sibling workspace crate named `pd-vm-nostd`, update micro-rustscript to consume it, and merge both completed feature branches into `master` without publishing releases.

**Architecture:** `pd-vm` remains the standard compiler/runtime crate. `pd-vm-nostd` is an independent `#![no_std] + alloc` crate containing the lean value model, VMBC v8 decoder, interpreter, fuel, and synchronous host ABI. `rustscript-embedded` links `pd-vm-nostd` only for the RP2040 feature and keeps `pd-vm` only for host-side source compilation/examples.

**Tech Stack:** Rust 2024 workspace, Cargo, `thumbv6m-none-eabi`, PlatformIO Arduino-Pico, GitHub Actions, git merge commits.

---

### Task 1: Establish the new crate boundary with a failing build

**Files:**
- Create: `pd-vm-nostd/Cargo.toml`
- Move tests to: `pd-vm-nostd/tests/embedded_vmbc.rs`
- Move tests to: `pd-vm-nostd/tests/embedded_vm.rs`
- Move tests to: `pd-vm-nostd/tests/embedded_host.rs`

- [ ] Move the existing embedded tests under `pd-vm-nostd/tests/` and change imports from `vm::embedded` to `pd_vm_nostd`.
- [ ] Run `cargo test --manifest-path pd-vm-nostd/Cargo.toml` before creating the package manifest/library.
- [ ] Confirm failure because package `pd-vm-nostd` or crate `pd_vm_nostd` is missing.

### Task 2: Move the no_std implementation into the sibling crate

**Files:**
- Create: `pd-vm-nostd/Cargo.toml`
- Create: `pd-vm-nostd/README.md`
- Move: `src/embedded/error.rs` → `pd-vm-nostd/src/error.rs`
- Move: `src/embedded/host.rs` → `pd-vm-nostd/src/host.rs`
- Move: `src/embedded/program.rs` → `pd-vm-nostd/src/program.rs`
- Move: `src/embedded/value.rs` → `pd-vm-nostd/src/value.rs`
- Move: `src/embedded/vm.rs` → `pd-vm-nostd/src/vm.rs`
- Move: `src/embedded/vmbc.rs` → `pd-vm-nostd/src/vmbc.rs`
- Move: `src/embedded/mod.rs` → `pd-vm-nostd/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] Declare package `pd-vm-nostd`, library name `pd_vm_nostd`, workspace edition/version, MIT license, RustScript homepage/repository, and no normal dependencies.
- [ ] Add `pd-vm-nostd` beside `pd-vm-wasm` in workspace members.
- [ ] Add `pd-vm` only as a dev-dependency so native compatibility tests can compile/encode VMBC.
- [ ] Add `#![no_std]` and `extern crate alloc` at the new crate root.
- [ ] Run the three migrated test suites and confirm they pass.
- [ ] Run `cargo check -p pd-vm-nostd --target thumbv6m-none-eabi` and confirm it passes.
- [ ] Run `cargo tree -p pd-vm-nostd --target thumbv6m-none-eabi -e normal` and confirm no normal dependency is listed.

### Task 3: Restore pd-vm as the standard crate

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Modify: `pd-vm-wasm/Cargo.toml`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`

- [ ] Remove `embedded-runtime` and the `vm::embedded` module from `pd-vm`.
- [ ] Restore `pd-vm` dependency/feature behavior from `origin/master` so the standard compiler/runtime API does not depend on the new freestanding crate.
- [ ] Point no_std build documentation and CI at `pd-vm-nostd`.
- [ ] Keep host-side differential tests in the new crate and run the full workspace tests, Clippy, format, WASM check, and Windows cross-check already present in CI.

### Task 4: Update micro-rustscript

**Files:**
- Modify: `rustscript-embedded/Cargo.toml`
- Modify: `rustscript-embedded/src/ffi.rs`
- Modify: `rustscript-embedded/tests/ffi.rs`
- Modify: `rustscript-embedded/README.md`
- Modify: `rustscript-embedded/.github/workflows/ci.yml`

- [ ] Make `pd-vm` an optional host-only dependency.
- [ ] Add optional dependency `pd_vm_nostd = { package = "pd-vm-nostd", path = "../rustscript/pd-vm-nostd" }` and enable it only from `rp2040`.
- [ ] Change FFI imports from `vm::embedded::*` to `pd_vm_nostd::*`.
- [ ] Update README and CI commands to build `pd-vm-nostd`.
- [ ] Run host tests, RP2040 FFI tests, `thumbv6m-none-eabi` release staticlib build, and a clean PlatformIO build.

### Task 5: Push and merge rustscript

**Files:**
- Branch: `rustscript:feat/no-std-runtime`
- Target: `rustscript:master`

- [ ] Commit and push the extraction on `feat/no-std-runtime`.
- [ ] Wait for the exact feature-branch CI SHA to succeed.
- [ ] Create a clean master worktree from `origin/master` and merge `origin/feat/no-std-runtime` with a merge commit.
- [ ] Re-run format, Clippy, workspace tests, `thumbv6m-none-eabi`, and WASM checks on merged master.
- [ ] Push master and wait for exact master CI success.

### Task 6: Push and merge micro-rustscript

**Files:**
- Branch: `rustscript-embedded:feat/rp2040-platformio`
- Target: `rustscript-embedded:master`

- [ ] Merge current `origin/master` documentation into `feat/rp2040-platformio`, resolving README in favor of the complete RP2040 documentation.
- [ ] Commit/push dependency migration and wait for feature CI success.
- [ ] Create a clean master worktree, merge the feature branch with a merge commit, and rerun host/RP2040/PlatformIO verification.
- [ ] Push master and wait for exact master CI success.
- [ ] Confirm no publish workflow was dispatched.
