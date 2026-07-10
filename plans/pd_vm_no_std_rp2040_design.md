# pd-vm no_std Runtime and RP2040 Integration Design

## Summary

Add a deliberately small `no_std + alloc` execution surface to the existing `pd-vm` package on the dedicated `feat/no-std-runtime` branch, then consume that surface from an RP2040 Raspberry Pi Pico example built by PlatformIO with the Arduino-Pico framework.

The embedded target executes host-produced VMBC. Parsing RustScript source, type checking, optimization, JIT/AOT, debugging, asynchronous host calls, filesystem access, and CLI behavior remain on `std` targets.

## Why WASM and RP2040 Need Different Runtime Boundaries

The existing `pd-vm-wasm` package targets `wasm32-unknown-unknown`. Rust supplies a target-specific `std` subset and allocator-backed linear memory there, so the WASM package can compile source and use the existing full VM.

The RP2040 target is `thumbv6m-none-eabi`. Rust supplies `core` and `alloc`, but no operating-system `std` implementation and no allocator selected by default. The RP2040 firmware therefore needs an explicit `no_std` library boundary and a final application-provided allocator.

## Approaches Considered

### 1. Feature-gated embedded module in `pd-vm` — selected

Add `pd-vm/embedded` types and an interpreter that compile when `default-features = false` and `features = ["embedded-runtime"]` are selected. Gate the existing compiler/tooling/runtime surface behind a new `std` feature while preserving all default behavior.

Advantages:

- the published package remains `pd-vm`
- current desktop, server, and WASM APIs remain unchanged under default features
- the no-std branch can land incrementally without first rewriting the optimized VM
- the embedded interpreter can consume the same VMBC version and opcode values

Trade-off:

- the compact interpreter initially has an independent implementation of the small 25-opcode dispatch loop

### 2. Extract a shared `pd-vm-core` crate

Move values, VMBC, and the interpreter into a new workspace package, then make `pd-vm` wrap it.

Advantages:

- one interpreter implementation across all targets
- clean long-term layering

Trade-off:

- large migration touching optimized dispatch, JIT/AOT integration, builtins, debugger state, and public types before the first RP2040 result

This remains a possible later refactor after the embedded contract is proven.

### 3. Run the WASM package through an MCU WASM engine

Advantages:

- preserves the existing WASM artifact boundary

Trade-off:

- adds another VM and substantial flash/RAM overhead on a 264 KiB SRAM device
- still needs a hardware host ABI and does not remove the allocator problem

This approach is rejected for the first RP2040 target.

## `pd-vm` Feature Boundary

The branch adds:

```toml
[features]
default = ["std", "runtime", "cli", "cranelift-jit", "http", "tls", "websocket"]
std = [/* current std-only dependencies */]
embedded-runtime = []
runtime = ["std"]
```

The crate root uses:

```rust
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
```

Existing compiler, assembler, debugger, optimized VM, CLI, builtins, VMBC tooling, and diagnostics remain available with `std`. Default builds retain their current API and behavior. `pd-vm-wasm` explicitly enables `std`; its `runtime` feature continues to enable the existing optimized VM.

A target check must succeed:

```bash
cargo check -p pd-vm \
  --target thumbv6m-none-eabi \
  --no-default-features \
  --features embedded-runtime
```

## Embedded Runtime Surface

The new `vm::embedded` module contains focused files:

- `value.rs`: allocator-backed `Value` variants using `alloc`; `Rc` replaces `Arc`
- `program.rs`: compact `Program`, `HostImport`, `ValueType`, and the existing opcode numbers
- `vmbc.rs`: VMBC v8 decoder that reads executable fields and safely skips std-only debug/type metadata
- `host.rs`: synchronous static host bindings with caller-owned context
- `vm.rs`: interpreter state and all current bytecode opcodes
- `error.rs`: `core::fmt::Display` errors without `std::error::Error`

The first embedded contract supports:

- VMBC v8 scalar constants: null, integer, float, boolean, string, bytes
- all current bytecode instructions: load, arithmetic, shifts, comparisons, branches, stack operations, locals, synchronous calls, return
- allocator-backed strings and byte buffers
- arrays/maps returned by host functions when needed, represented with embedded-only allocator-backed values
- synchronous named host imports with arity validation
- instruction fuel so firmware can bound script execution

The first contract excludes:

- source compilation on the MCU
- JIT and AOT
- async host operations
- debugger recording and source maps
- filesystem/network protocol builtins
- default host registry and global locks

## Host Function Contract

The Rust API owns a host context and resolves VMBC imports before execution:

```rust
pub struct HostBinding<C> {
    pub name: &'static str,
    pub arity: u8,
    pub function: fn(&mut C, &[Value]) -> Result<Option<Value>, HostError>,
}
```

The interpreter copies the resolved function pointer before borrowing the context, avoiding self-borrow conflicts. Missing functions and arity mismatches fail during VM construction.

RP2040 bindings expose GPIO and timing through the Arduino host, initially:

- `gpio::set(pin, high)`
- `time::delay_ms(milliseconds)`
- `serial::write(text)`

No SoC registers are implemented inside `pd-vm`.

## PlatformIO Arduino-Pico Integration

`rustscript-embedded` gains an RP2040 example with this build shape:

1. PlatformIO selects Raspberry Pi Pico and the Arduino-Pico core.
2. A PlatformIO pre-build script invokes Cargo for `thumbv6m-none-eabi` and builds a Rust `staticlib`.
3. The same script runs the host `pd-vm-run --emit-vmbc` command when the `.rss` source changes and emits a generated C header containing VMBC bytes.
4. PlatformIO links the Rust archive into the normal Arduino C++ firmware.
5. The Arduino sketch supplies allocator symbols, host callbacks, GPIO/UART/timing operations, and calls the Rust C ABI.

The final Rust static library uses `panic = "abort"`. Its global allocator delegates to allocator functions exported by the Arduino C++ side, so the firmware has one heap owner. The allocator adapter preserves requested alignment rather than assuming plain `malloc` alignment is always sufficient.

The C ABI uses tagged scalar values and byte/string slices. Arrays/maps remain internal to Rust for the first integration surface.

## Repository and Branch Layout

- `rustscript` branch: `feat/no-std-runtime`
- `rustscript-embedded` branch: `feat/rp2040-platformio`

The existing dirty `rustscript-embedded` checkout is left untouched. RP2040 work starts from a clean `origin/master` worktree so previous uncommitted Raspberry Pi Zero correction files cannot be overwritten.

## Testing

### `pd-vm`

- RED/GREEN host tests for VMBC decode, each opcode group, branch/local behavior, host calls, fuel, and errors
- compatibility test: compile `.rss` with the std compiler, encode VMBC v8, decode and execute with `vm::embedded`
- `thumbv6m-none-eabi` no-std compile check
- default workspace format, Clippy, and test suite to prove no default regression
- WASM package build to prove explicit `std` feature wiring

### `rustscript-embedded`

- host tests for C value conversion and callback dispatch
- Cargo static-library build for `thumbv6m-none-eabi`
- real `pio run` using Raspberry Pi Pico plus Arduino-Pico
- inspect the final ELF/map to prove the Rust VM symbols and VMBC blob are linked
- assert the firmware size is plausible and report exact ELF/UF2 sizes
- serial-runtime verification through hardware or an available RP2040 simulator; if no simulator credential/device is available, report the build/link proof separately from runtime proof

## Acceptance Criteria

- `pd-vm` builds without `std` for `thumbv6m-none-eabi` on its dedicated branch
- the no-std runtime executes host-generated VMBC, including control flow and a GPIO-oriented host call sequence
- the default `pd-vm` and `pd-vm-wasm` checks remain green
- PlatformIO builds a Raspberry Pi Pico Arduino firmware that links the Rust static library
- the final firmware contains real interpreter and VMBC symbols; no placeholder C interpreter is used
- size reporting names the exact ELF/UF2 artifacts and never labels a frozen-program stub as the full desktop VM
