# pd-vm no_std RP2040 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use codex-superpowers-subagent-driven-development (recommended) or codex-superpowers-executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a `no_std + alloc` VMBC interpreter in the existing `pd-vm` package and link it into a real Raspberry Pi Pico firmware built by PlatformIO with Arduino-Pico.

**Architecture:** Preserve the complete current API behind a default `std` feature and add an isolated `vm::embedded` interpreter selected by `embedded-runtime`. RustScript source is compiled into VMBC on the host. The RP2040 firmware links a Rust static archive, provides the allocator and synchronous Arduino host callbacks, and executes the VMBC blob.

**Tech Stack:** Rust 2024, `core`, `alloc`, `thumbv6m-none-eabi`, Cargo staticlib, PlatformIO, Arduino-Pico, C++/C ABI, GitHub Actions.

---

### Task 1: Establish the `std` and `embedded-runtime` feature boundary

**Files:**
- Modify: `Cargo.toml:26-75`
- Modify: `src/lib.rs:1-93`
- Modify: `pd-vm-wasm/Cargo.toml:11-19`
- Modify: `.github/workflows/ci.yml:22-36`
- Test: Cargo target compile command

- [ ] **Step 1: Install the RP2040 Rust target**

Run:

```bash
rustup target add thumbv6m-none-eabi
```

Expected: target component installed successfully.

- [ ] **Step 2: Run the missing-feature RED check**

Run:

```bash
cargo check -p pd-vm --target thumbv6m-none-eabi \
  --no-default-features --features embedded-runtime
```

Expected: FAIL because `embedded-runtime` does not exist and the current crate imports `std` unconditionally.

- [ ] **Step 3: Add explicit feature/dependency gating**

Add these feature relationships in `Cargo.toml`:

```toml
[features]
default = ["std", "runtime", "cli", "cranelift-jit", "http", "tls", "websocket"]
std = [
    "dep:base64",
    "dep:futures-channel",
    "dep:paste",
    "dep:pd-host-function",
    "dep:regex",
    "dep:rt-format",
    "dep:serde",
    "dep:serde_json",
]
embedded-runtime = []
runtime = ["std"]
```

Mark the listed dependencies optional. Keep Cranelift, edge ABI, and rustyline optional under their existing features. Make platform `libc` and `windows-sys` optional and add them to `std` through target-aware optional dependency activation if the target compile shows they remain enabled.

Update `pd-vm-wasm` so its `vm` dependency explicitly selects `features = ["std"]`.

Gate the current modules and exports in `src/lib.rs` with `#[cfg(feature = "std")]`, add:

```rust
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(feature = "embedded-runtime")]
pub mod embedded;
```

Create `src/embedded/mod.rs` with an empty public module surface so the target check can reach GREEN before interpreter implementation.

- [ ] **Step 4: Run feature-boundary GREEN checks**

Run:

```bash
cargo check -p pd-vm --target thumbv6m-none-eabi \
  --no-default-features --features embedded-runtime
cargo check -p pd-vm --no-default-features --features runtime
cargo check -p pd-vm-wasm
```

Expected: all three commands exit 0.

- [ ] **Step 5: Add the RP2040 compile check to CI**

Add a CI step after toolchain setup:

```yaml
- name: Install RP2040 target
  run: rustup target add thumbv6m-none-eabi
- name: no_std RP2040 runtime check
  working-directory: rustscript
  run: cargo check -p pd-vm --target thumbv6m-none-eabi --no-default-features --features embedded-runtime
```

- [ ] **Step 6: Commit the feature boundary**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/embedded/mod.rs pd-vm-wasm/Cargo.toml .github/workflows/ci.yml
git commit -m "feat: add pd-vm no-std feature boundary"
```

### Task 2: Add no-std values, program model, and VMBC v8 decoder

**Files:**
- Create: `src/embedded/error.rs`
- Create: `src/embedded/value.rs`
- Create: `src/embedded/program.rs`
- Create: `src/embedded/vmbc.rs`
- Modify: `src/embedded/mod.rs`
- Test: `tests/embedded_vmbc.rs`

- [ ] **Step 1: Write VMBC decoder RED tests**

Create tests that use the std compiler and encoder to produce VMBC, then invoke the new embedded decoder:

```rust
use vm::{compile_source, encode_program};
use vm::embedded::{Value, decode_program};

#[test]
fn embedded_decoder_reads_host_generated_v8() {
    let compiled = compile_source("let x = 40; x + 2;").unwrap();
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals)).unwrap();
    let program = decode_program(&bytes).unwrap();
    assert!(!program.code().is_empty());
    assert!(program.constants().contains(&Value::Int(40)));
}

#[test]
fn embedded_decoder_rejects_trailing_bytes() {
    let mut bytes = include_bytes!("fixtures/minimal.vmbc").to_vec();
    bytes.push(0xff);
    assert!(matches!(decode_program(&bytes), Err(_)));
}
```

Generate `tests/fixtures/minimal.vmbc` from a committed `.rss` source with the existing `pd-vm-run --emit-vmbc` command, or construct the bytes in the test through the std encoder so the fixture never drifts.

Run:

```bash
cargo test -p pd-vm --features embedded-runtime --test embedded_vmbc -- --nocapture
```

Expected: FAIL because embedded value/program/decoder APIs are absent.

- [ ] **Step 2: Implement allocator-backed embedded values**

Implement these variants in `value.rs` using `alloc::rc::Rc`, `alloc::string::String`, and `alloc::vec::Vec`:

```rust
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(Rc<String>),
    Bytes(Rc<Vec<u8>>),
    Array(Rc<Vec<Value>>),
    Map(Rc<Vec<(Value, Value)>>),
}
```

Add `string`, `bytes`, `array`, and `map` constructors plus content equality.

- [ ] **Step 3: Implement the compact program model**

Keep the opcode values identical to `src/bytecode.rs` and expose read-only program accessors. `Program` owns constants, code, inferred local count, and imports; it omits source maps, type hints, lock-backed caches, and JIT metadata.

- [ ] **Step 4: Implement VMBC v8 decoding**

Decode scalar constants, code, imports, type-map payload length/fields, and debug payload fields with checked cursor reads. Keep executable metadata; parse and discard std-only type/debug data. Reject bad magic, unsupported version/flags, invalid tags, invalid UTF-8, truncated lengths, and trailing bytes.

- [ ] **Step 5: Run decoder and target checks**

```bash
cargo test -p pd-vm --features embedded-runtime --test embedded_vmbc
cargo check -p pd-vm --target thumbv6m-none-eabi \
  --no-default-features --features embedded-runtime
```

Expected: decoder tests pass and the target build exits 0.

- [ ] **Step 6: Commit values and decoder**

```bash
git add src/embedded tests/embedded_vmbc.rs
git commit -m "feat: decode VMBC in no-std pd-vm runtime"
```

### Task 3: Implement the compact interpreter with every current opcode

**Files:**
- Create: `src/embedded/vm.rs`
- Modify: `src/embedded/error.rs`
- Modify: `src/embedded/mod.rs`
- Test: `tests/embedded_vm.rs`

- [ ] **Step 1: Write arithmetic and stack RED tests**

Compile scripts through the std compiler, encode/decode VMBC, run the embedded VM, and assert final values for:

```rust
("1 + 2 * 3;", Value::Int(7))
("(8 << 2) + (32 >> 1);", Value::Int(48))
("17 % 5;", Value::Int(2))
("!(1 < 0);", Value::Bool(true))
```

Also create direct bytecode tests for `Nop`, `Dup`, `Pop`, `Lshr`, and division-by-zero.

Run:

```bash
cargo test -p pd-vm --features embedded-runtime --test embedded_vm arithmetic -- --nocapture
```

Expected: FAIL because `embedded::Vm` is absent.

- [ ] **Step 2: Implement stack, constants, arithmetic, and comparisons**

Implement `Vm::new(program)`, checked operand readers, stack access, numeric operations, string/bytes concatenation, boolean operations, comparisons, and deterministic error variants using only `core` and `alloc`.

- [ ] **Step 3: Write branch/local RED tests**

Use a compiled loop:

```rust
let mut total = 0;
let mut n = 1;
while n < 5 {
    total = total + n;
    n = n + 1;
}
total;
```

Assert `Value::Int(10)`. Add malformed jump and local-index tests.

- [ ] **Step 4: Implement control flow and locals**

Implement `Br`, `Brfalse`, `Ldloc`, `Stloc`, bounds checks, `Ret`, `VmStatus::Halted`, and resumable instruction-pointer state.

- [ ] **Step 5: Run interpreter parity checks**

```bash
cargo test -p pd-vm --features embedded-runtime --test embedded_vm
cargo check -p pd-vm --target thumbv6m-none-eabi \
  --no-default-features --features embedded-runtime
```

Expected: all interpreter tests pass and target build exits 0.

- [ ] **Step 6: Commit the interpreter**

```bash
git add src/embedded tests/embedded_vm.rs
git commit -m "feat: execute pd-vm bytecode without std"
```

### Task 4: Add synchronous host calls and instruction fuel

**Files:**
- Create: `src/embedded/host.rs`
- Modify: `src/embedded/vm.rs`
- Modify: `src/embedded/error.rs`
- Modify: `src/embedded/mod.rs`
- Test: `tests/embedded_host.rs`

- [ ] **Step 1: Write host-binding RED tests**

Define a context and static callbacks:

```rust
#[derive(Default)]
struct BoardState {
    led: bool,
    delay_ms: u64,
}

fn gpio_set(state: &mut BoardState, args: &[Value]) -> Result<Option<Value>, HostError> {
    state.led = matches!(args, [Value::Int(25), Value::Bool(true)]);
    Ok(None)
}
```

Compile a script that calls the registered function. Assert context mutation, missing-import failure, and arity-mismatch failure.

Run:

```bash
cargo test -p pd-vm --features embedded-runtime --test embedded_host host -- --nocapture
```

Expected: FAIL because host binding is absent.

- [ ] **Step 2: Implement generic static host bindings**

Add `HostBinding<C>` with `name`, `arity`, and a function pointer. Resolve VMBC imports at construction into a compact slot vector. `Call` validates the encoded argument count, borrows the stack tail, invokes the callback with `&mut C`, truncates arguments, and pushes an optional return value.

- [ ] **Step 3: Write fuel RED tests**

Run an intentional loop with a fuel limit and assert `VmError::OutOfFuel`; recharge and verify execution can resume.

- [ ] **Step 4: Implement fuel accounting**

Charge one unit per interpreter instruction. Add `set_fuel`, `add_fuel`, `clear_fuel`, `fuel`, and resumable out-of-fuel behavior without epoch threads or atomics.

- [ ] **Step 5: Run host/fuel and target checks**

```bash
cargo test -p pd-vm --features embedded-runtime --test embedded_host
cargo check -p pd-vm --target thumbv6m-none-eabi \
  --no-default-features --features embedded-runtime
```

Expected: all tests pass and target build exits 0.

- [ ] **Step 6: Commit host ABI and fuel**

```bash
git add src/embedded tests/embedded_host.rs
git commit -m "feat: add bounded no-std host execution"
```

### Task 5: Prove default and WASM behavior remains intact

**Files:**
- Modify: `pd-vm-wasm/Cargo.toml`
- Modify: `crates/rustscript/Cargo.toml` only if feature forwarding requires it
- Modify: `.github/workflows/ci.yml`
- Test: existing workspace and WASM tests

- [ ] **Step 1: Run the focused compatibility matrix**

```bash
cargo fmt --all -- --check
cargo check -p pd-vm --no-default-features --features runtime
cargo check -p pd-vm-wasm --target wasm32-unknown-unknown --features runtime
cargo test -p pd-vm --features embedded-runtime \
  --test embedded_vmbc --test embedded_vm --test embedded_host
```

Expected: all commands exit 0.

- [ ] **Step 2: Run full quality gates**

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --jobs 4
```

Expected: all tests pass and Clippy reports no warnings.

- [ ] **Step 3: Commit compatibility wiring if changed**

```bash
git add Cargo.toml Cargo.lock pd-vm-wasm/Cargo.toml crates/rustscript/Cargo.toml .github/workflows/ci.yml
git commit -m "test: verify no-std and wasm feature matrix"
```

Skip this commit only when Task 1 already contains every compatibility/CI change and the worktree is clean.

### Task 6: Create a clean RP2040 branch and Rust static-library C ABI

**Files:**
- Create worktree: `/home/wow/rustscript/rustscript-embedded-rp2040`
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Create: `src/ffi.rs`
- Create: `src/allocator.rs`
- Create: `include/rustscript_embedded.h`
- Test: `tests/ffi.rs`

- [ ] **Step 1: Isolate prior dirty work**

From the existing `rustscript-embedded` repository:

```bash
git fetch origin
git worktree add /home/wow/rustscript/rustscript-embedded-rp2040 \
  -b feat/rp2040-platformio origin/master
```

Verify the original checkout still has exactly its previous status and the new worktree is clean.

- [ ] **Step 2: Write C conversion RED tests**

Define tagged `#[repr(C)]` scalar values and test round trips for null, int, float, bool, string, and bytes. Test invalid tags and invalid pointer/length combinations.

Run:

```bash
cargo test --test ffi -- --nocapture
```

Expected: FAIL because the FFI module is absent.

- [ ] **Step 3: Add no-std staticlib mode**

Configure:

```toml
[lib]
crate-type = ["rlib", "staticlib"]

[features]
default = ["host"]
host = ["pd-vm/std", "pd-vm/runtime"]
rp2040 = ["pd-vm/embedded-runtime"]
```

Use the sibling no-std branch during development. Keep current host examples under `host`; compile the C ABI and allocator only under `rp2040`.

- [ ] **Step 4: Implement C ABI and allocator adapter**

Expose:

```c
int32_t rustscript_run_vmbc(
    const uint8_t *program,
    size_t program_len,
    rustscript_host_callback callback,
    void *context,
    uint64_t fuel
);
```

The Rust side decodes VMBC, resolves imports through one callback trampoline, runs until halt/error, and returns stable numeric status codes. The final staticlib delegates aligned allocation/deallocation to C symbols supplied by Arduino, uses `panic = "abort"`, and defines a target-only panic handler.

- [ ] **Step 5: Verify static archive generation**

```bash
cargo build --release --target thumbv6m-none-eabi \
  --no-default-features --features rp2040
arm-none-eabi-nm -g --defined-only \
  target/thumbv6m-none-eabi/release/librustscript_embedded.a \
  | grep rustscript_run_vmbc
```

Expected: build exits 0 and the exported C symbol is present.

- [ ] **Step 6: Commit C ABI**

```bash
git add Cargo.toml Cargo.lock src include tests
git commit -m "feat: expose RP2040 RustScript static library"
```

### Task 7: Add the real PlatformIO Arduino-Pico firmware

**Files:**
- Create: `platformio/rp2040/platformio.ini`
- Create: `platformio/rp2040/src/main.cpp`
- Create: `platformio/rp2040/programs/blinky.rss`
- Create: `platformio/rp2040/scripts/build_rust.py`
- Create: `platformio/rp2040/.gitignore`
- Modify: `README.md`

- [ ] **Step 1: Install PlatformIO and run an empty-project RED build**

Install PlatformIO in an isolated tool environment:

```bash
uv tool install platformio
```

Run:

```bash
pio run -d platformio/rp2040
```

Expected: FAIL before the project files and Rust archive integration exist.

- [ ] **Step 2: Add Arduino-Pico project configuration**

Use the documented Arduino-Pico PlatformIO integration:

```ini
[env:pico]
platform = https://github.com/maxgerhardt/platform-raspberrypi.git
board = pico
framework = arduino
board_build.core = earlephilhower
board_build.filesystem_size = 0m
extra_scripts = pre:scripts/build_rust.py
monitor_speed = 115200
```

- [ ] **Step 3: Add the pre-build pipeline**

The script must:

1. run `cargo build --release --target thumbv6m-none-eabi --no-default-features --features rp2040`
2. run host `pd-vm-run <source> --emit-vmbc <build-dir>/blinky.vmbc` when source changes
3. generate `<build-dir>/program_vmbc.h` with an exact byte array and length
4. add the Rust archive to PlatformIO's link inputs
5. fail if the archive or VMBC output is absent/empty

Use `subprocess.run(..., check=True)` and derive paths from `PROJECT_DIR`; do not depend on the caller's current directory.

- [ ] **Step 4: Implement the Arduino sketch**

The sketch supplies aligned allocation hooks, maps host names to `digitalWrite`, `delay`, and `Serial.write`, and invokes `rustscript_run_vmbc` from `setup()`. The `.rss` program toggles `LED_BUILTIN`, waits, prints a completion marker, and halts. `loop()` remains idle after completion.

- [ ] **Step 5: Run the real PlatformIO build**

```bash
pio run -d platformio/rp2040
```

Expected: PlatformIO compiles Arduino-Pico C++, Cargo builds the Rust archive, the final link succeeds, and a firmware UF2/ELF is produced.

- [ ] **Step 6: Prove the final ELF contains the real runtime**

```bash
arm-none-eabi-nm -C platformio/rp2040/.pio/build/pico/firmware.elf \
  | grep -E 'rustscript_run_vmbc|embedded::vm|decode_program'
arm-none-eabi-size platformio/rp2040/.pio/build/pico/firmware.elf
stat --format='%n %s bytes' platformio/rp2040/.pio/build/pico/firmware.uf2
```

Expected: symbols are present and exact ELF/UF2 sizes are printed.

- [ ] **Step 7: Commit PlatformIO integration**

```bash
git add platformio/rp2040 README.md
git commit -m "feat: run RustScript on RP2040 with PlatformIO"
```

### Task 8: Add CI, final review, and push both branches

**Files:**
- Modify: `rustscript-embedded/.github/workflows/ci.yml`
- Modify: `rustscript-embedded/README.md`
- Modify: `rustscript/README.md` only for no-std feature documentation

- [ ] **Step 1: Replace the obsolete RP2040-unrelated target job on the clean branch**

Add a target job that checks out `rustscript` at `ref: feat/no-std-runtime`, installs `thumbv6m-none-eabi` and PlatformIO, runs the Rust staticlib build, then runs the complete PlatformIO build. Keep host checks intact.

- [ ] **Step 2: Run final local verification in `rustscript`**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --jobs 4
cargo check -p pd-vm --target thumbv6m-none-eabi \
  --no-default-features --features embedded-runtime
cargo check -p pd-vm-wasm --target wasm32-unknown-unknown --features runtime
git diff --check
git status --short
```

Expected: every command exits 0; only intended committed state remains.

- [ ] **Step 3: Run final local verification in `rustscript-embedded-rp2040`**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --target thumbv6m-none-eabi \
  --no-default-features --features rp2040
pio run -d platformio/rp2040
git diff --check
git status --short
```

Expected: every command exits 0 and the worktree is clean.

- [ ] **Step 4: Perform spec and quality reviews**

Review both branch diffs against `plans/pd_vm_no_std_rp2040_design.md`. Confirm no compiler/JIT/debugger code was forced into no-std, no fake firmware is present, all C pointer validation is explicit, the allocator alignment contract is correct, and PlatformIO links the Rust archive.

- [ ] **Step 5: Push explicit branch refs**

```bash
git -C /home/wow/rustscript/rustscript push -u origin feat/no-std-runtime
git -C /home/wow/rustscript/rustscript-embedded-rp2040 push -u origin feat/rp2040-platformio
```

- [ ] **Step 6: Watch exact-SHA CI runs**

Query each repository's GitHub Actions runs by the pushed head SHA, wait until all runs complete, and fix/re-push any failure. Report branch names, final commits, test/build commands, exact firmware sizes, and CI URLs.
