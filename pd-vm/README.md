# pd-vm

`pd-vm` is a stack-based virtual machine plus compiler toolchain used as a backend for multiple
source syntaxes (`.rss`, `.js`, `.lua`, `.scm`).

## Contents

- [Overview](#overview)
- [TODO](#todo)
- [How To Use](#how-to-use)
  - [Run Programs](#run-programs)
  - [REPL](#repl)
  - [Debugging](#debugging)
  - [Recording and Replay](#recording-and-replay)
  - [Bytecode and VMBC](#bytecode-and-vmbc)
  - [JIT](#jit)
  - [AOT](#aot)
  - [Fuel Metering](#fuel-metering)
  - [Wasm Lint](#wasm-lint)
  - [Wasm Runtime Playground](#wasm-runtime-playground)
  - [WebUI Playground](#webui-playground)
  - [Test and Perf Commands](#test-and-perf-commands)
- [Internals](#internals)
  - [VM Internals](#vm-internals)
  - [Compiler Internals](#compiler-internals)
    - [Pipeline Layers](#pipeline-layers)
    - [Compiler APIs](#compiler-apis)
    - [Assembler API](#assembler-api)
    - [Builtins and Bridged call Opcode](#builtins-and-bridged-call-opcode)
    - [Current Compiler Subset Limitations](#current-compiler-subset-limitations)
  - [JIT Internals](#jit-internals)
- [Compiler frontend syntax and feature support](src/compiler/frontends/README.md)


## Overview

Executes compiled compact bytecode rather than interpreting source.
Offers consistent runtime semantics for both synchronous and asynchronous execution.
Includes rich debugging and profiling tools: interactive debugger, recording and replay, and JIT trace insights.
## TODO

- [ ] Optional type annotation (rustscript) for function boundaries / host APIs.
- [ ] host call fuel budgeting.
- [ ] Epoch-based interruption API.
- [ ] Callable-as-value support.

## How To Use

### Run Programs

Run with the VM runner binary:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --fuel 100000 examples/example.lua
```

Other supported flavors:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- examples/example.js
cargo run -p pd-vm --bin pd-vm-run -- examples/example.rss
cargo run -p pd-vm --bin pd-vm-run -- examples/example.scm
```

### REPL

RustScript REPL (history + multiline support):

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --repl
```


### Debugging

Run with interactive `pdb` debugger on stdio:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --debug examples/example.lua
```

Run debugger over TCP:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --debug --tcp 127.0.0.1:9002 examples/example.lua
```

Useful commands: `break`, `break line`, `step`, `next`, `out`, `stack`, `locals`, `where`, `continue`, `fuel`.

### Recording and Replay

Record execution:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --record out/example.pdr examples/example.lua
```

Replay execution:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --view-record out/example.pdr
```

Replay supports `break`, `break line`, `continue`, `step`, `next`, `out`, `stack`, `locals`,
`print`, `ip`, `where`, and `funcs`. In replay mode, breakpoints set pause points in the replay
stream instead of runtime VM breakpoints.

### Bytecode and VMBC

Emit VMBC wire-format output without running:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --emit-vmbc out/example.vmbc examples/example.rss
```

Disassemble VMBC:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --disasm-vmbc path/to/program.vmbc
```

Disassemble with embedded source (if present):

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --disasm-vmbc path/to/program.vmbc --show-source
```

### JIT

Dump trace-JIT activity:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --jit-hot-loop 2 --jit-dump examples/example.rss
```

Native JIT codegen uses Cranelift.

- Cranelift is part of the Bytecode Alliance/Wasmtime ecosystem
- NYI behavior is shared by the trace recorder (`TraceJitEngine`) and is backend-independent.
- Some operations may use helper fallback paths internally, but that is not counted as trace-recorder NYI.

Library hooks:

- `vm.set_jit_config(...)`
- `vm.prepare_aot()`
- `vm.emit_aot_bundle()`
- `Vm::from_aot_bundle_bytes(...)`
- `vm.jit_snapshot()`
- `vm.dump_jit_info()`
- `vm.jit_native_trace_count()`
- `vm.jit_native_exec_count()`

### AOT

Ahead-of-time bundles compile whole-program bytecode blocks up front (entry + branch targets),
then execute through the same native backend used by JIT.

Emit an AOT bundle:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --emit-aot out/example.pat examples/example.rss
```

Set a custom AOT fuel checkpoint interval while emitting:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --emit-aot out/example.pat --fuel-check-interval 64 examples/example.rss
```

Run an emitted AOT bundle:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --run-aot out/example.pat --jit-dump
```

- AOT uses the same native backend as JIT (Cranelift).
- Opcode/native support coverage is shared with the JIT backend.
- `.pat` stores native trace machine code, constants/import metadata, local count, and trace
  metadata. Emitted bundles do not persist VM bytecode.
- `--emit-aot` defaults to `--fuel-check-interval 64`.
- `--fuel-check-interval 0` emits native code without inline fuel-check sequences. Such bundles
  cannot be run with `--fuel` because no native checkpoints exist.
- Bundles are validated against target arch/os/pointer width plus a VM-layout fingerprint before
  loading, so they are only portable across compatible builds on the same native support matrix.
- Control-flow targets remain instruction offsets inside a synthetic code space (`ip` labels are
  preserved even though raw bytecode is not), so trace chaining and host-call resume addresses stay
  stable at runtime.
- Native-only AOT bundles support pending host calls, but synchronous host `CallOutcome::Yield`
  requires bytecode replay and is therefore rejected at runtime.

### Fuel Metering

`pd-vm` provides Wasmtime-style fuel controls on both `Vm` and `Store<T>`:

- `set_fuel`
- `set_fuel_check_interval`
- `fuel_check_interval`
- `get_fuel`
- `consume_fuel`
- `consume_fuel_tick`
- `add_fuel` / `recharge_fuel` (`Store::recharge`)
- `fuel_checkpoint` / `checkpoint`
- `restore_fuel` / `restore_checkpoint`

`Store<T>` is a lightweight wrapper around `Vm` plus host context data (`data()` / `data_mut()`),
and forwards `run()` / `resume()`.

`pd-vm-run` supports `--fuel <n>` to set the initial VM fuel budget.

Debugger fuel commands:

- `fuel` (show remaining fuel and check interval)
- `fuel set <n>`
- `fuel add <n>`
- `fuel clear`
- `fuel interval [n]`

Example:

```rust
use vm::{Store, VmStatus};

// ... create vm ...
let mut store = Store::from_vm(vm);
store.set_fuel(10_000);
store.set_fuel_check_interval(1)?; // exact mode: check every instruction/trace step
let checkpoint = store.checkpoint();

loop {
    match store.run()? {
        VmStatus::Halted => break,
        VmStatus::Yielded => continue,
        VmStatus::Waiting(_) => {
            store.vm_mut().wait_for_host_op_blocking()?;
        }
    }
}

store.recharge(1_000)?;
store.restore_checkpoint(checkpoint);
```

Fuel charging semantics:

- Fuel metering is disabled by default (`get_fuel() == None`).
- `set_fuel` sets an explicit budget; `add_fuel` also enables metering if it was disabled.
- Fuel is consumed in chunks at the configured check cadence.
  Chunk size = `fuel_check_interval`.
  Default interval is `1` (exact mode).
- The interpreter applies fuel checks in the VM loop before opcode fetch/execute.
- Trace-JIT execution applies the same cadence before each `TraceStep`.
- When fuel metering is enabled, native JIT execution injects fuel checks in generated machine
  code at the configured check cadence.
- With interval `> 1`, out-of-fuel detection is coarse-grained: execution may run up to
  `interval - 1` extra instructions before the next fuel check.
- If there is not enough fuel, execution returns `VmStatus::Yielded` before the next instruction
  runs (instruction pointer is not advanced). Top up fuel (`set_fuel` / `add_fuel`) and call
  `run()` / `resume()` again.
- `FuelCheckpoint` snapshots only fuel-accounting state (remaining budget, check interval, and
  current check-phase cursor). Restoring a checkpoint does not rewind VM stack, locals, or
  instruction pointer.
- Host-side work is not automatically metered beyond VM instruction execution; host code can call
  `consume_fuel` explicitly for additional charging policy.

### Wasm Lint

Compiler-only wasm build (without runtime/JIT/debugger/CLI):

```powershell
cargo check -p pd-vm --target wasm32-unknown-unknown --no-default-features
```

Browser/editor lint integration is provided by sibling crate `pd-vm-lint-wasm` via
`lint_source_json`.

### Wasm Runtime Playground

Runtime-enabled wasm build (without native JIT backend):

```powershell
cargo check -p pd-vm --target wasm32-unknown-unknown --no-default-features --features runtime
```

Browser playground wasm runtime is provided by sibling crate `pd-vm-runtime-wasm` via:

- `lint_source_json`
- `run_source_json`

### WebUI Playground

Standalone Monaco playground lives in `pd-vm/webui`:

```powershell
cd pd-vm/webui
bun install
bun run dev
```

This runs `scripts/build-wasm-playground.mjs`, which builds `pd-vm-runtime-wasm`, copies wasm
artifacts into `public/wasm`, and syncs RustScript Monaco grammar assets.

### Test and Perf Commands

Integration example tests:

```powershell
cargo test -p pd-vm --test example_tests
```

Manual perf characterization (ignored by default):

```powershell
cargo test -p pd-vm --test perf_tests -- --ignored --nocapture
```

Migration perf record (handwritten vs Cranelift baseline before handwritten backend removal):

- `docs/JIT_BACKEND_MIGRATION_PERF_2026-03-06.md`

## Internals

### VM Internals

`Program` consists of:

- `constants: Vec<Value>`
- `code: Vec<u8>`
- `imports: Vec<HostImport>`
- `debug: Option<DebugInfo>`

Bytecode format:

- 1 byte opcode
- little-endian operands (`u8`, `u16`, `u32`)
- absolute jump targets

Instruction set:

| Opcode | Mnemonic | Operands       | Stack effect                       |
|--------|----------|----------------|------------------------------------|
| 0x00   | `nop`    | -              | no change                          |
| 0x01   | `ret`    | -              | stop execution                     |
| 0x02   | `ldc`    | u32 index      | push constant                      |
| 0x03   | `add`    | -              | (a, b) -> (a + b)                  |
| 0x04   | `sub`    | -              | (a, b) -> (a - b)                  |
| 0x05   | `mul`    | -              | (a, b) -> (a * b)                  |
| 0x06   | `div`    | -              | (a, b) -> (a / b)                  |
| 0x07   | `neg`    | -              | (a) -> (-a)                        |
| 0x08   | `ceq`    | -              | (a, b) -> (a == b)                 |
| 0x09   | `clt`    | -              | (a, b) -> (a < b)                  |
| 0x0A   | `cgt`    | -              | (a, b) -> (a > b)                  |
| 0x0B   | `br`     | u32 target     | ip = target                        |
| 0x0C   | `brfalse`| u32 target     | pop bool; if false jump            |
| 0x0D   | `pop`    | -              | pop value                          |
| 0x0E   | `dup`    | -              | dup top of stack                   |
| 0x0F   | `ldloc`  | u8 index       | push local                         |
| 0x10   | `stloc`  | u8 index       | pop -> local                       |
| 0x11   | `call`   | u16 id, u8 argc| pop args, call host, push returns  |
| 0x12   | `shl`    | -              | (a, b) -> (a << b)                 |
| 0x13   | `shr`    | -              | (a, b) -> (a >> b)                 |
| 0x14   | `mod`    | -              | (a, b) -> (a % b)                  |
| 0x15   | `and`    | -              | (a, b) -> (a && b)                 |
| 0x16   | `or`     | -              | (a, b) -> (a \|\| b)               |

Host calls and resuming:

- `call` dispatches to builtin or bound host function
- host functions can yield
- `vm.resume()` continues from the next instruction

### Compiler Internals

#### Pipeline Layers

The end-to-end stack is split into layers. Not every entrypoint uses every layer (for example,
`compile_source()` skips module loading/linking), but this is the full model:

1. Module/source loading (`compile_source_file()` path)
1. Unit linking (`linker::merge_units`)
1. Frontend lowering (`rustscript`, `javascript`, `lua`, `scheme`)
1. Frontend-independent IR
1. Bytecode backend (`Compiler` + `Assembler` -> `Program`) executed by VM
1. Trace-JIT IR recording (`JitTrace` + `TraceStep`) using TraceStep IR
1. Native machine code emission and execution


#### Compiler APIs

Use `compile_source()` for RustScript, or `compile_source_file()` for extension-based flavor
selection (`.rss`, `.js`, `.lua`, `.scm`).

```text
fn print(x);
let x = 2 + 3;
let y = x * 4;
if y > 10 {
    print(y);
} else {
    0;
}
```

Closure subset example:

```text
let base = 7;
let add = |value| value + base;
add(5);
```

Lua closure equivalent lowered by frontend:

```lua
local add = function(value) return value + base end
```

Built-in print aliases (no declaration needed):

- RustScript: `print(value);`, `print("... {}", a);`, `println(value);`, `println("... {}", a);`
- JavaScript subset: `console.log(value);` and `print(value);`
- Lua subset: `print(value)`
- Scheme subset: `(print value)`

Host calls must be explicitly imported:

- RustScript: `use vm::{...};` / `use vm;`
- JavaScript: `import ... from "vm"` or `require("vm")`
- Lua: `require("vm")`
- Scheme: `(import ...)` / `(require ...)` forms for `"vm"`

#### Assembler API

Use `assemble()` to parse text assembly into a `Program`.

Data declarations:

- `const NAME VALUE`
- `string NAME "..."`

```text
.data
const two 2
string greeting "hello"
.code
.local counter
.label loop
ldc two
stloc counter
ldloc counter
ldc 1
sub
dup
stloc counter
brfalse done
br loop
.label done
ldc greeting
ret
```

Directives:

- `.data` and `.code` switch sections
- `.label NAME` defines a jump label
- `.local NAME [INDEX]` defines a named local


#### Builtins and Bridged `call` Opcode

The compiler uses one call shape (`Expr::Call` -> `OpCode::Call`) and distinguishes targets by call
index.

1. Builtin calls (fixed reserved indices)
   - Builtins use `BuiltinFunction::call_index()`
   - parser lowering emits these for helpers such as `len`, `get`, `set`, `slice`, `count`,
     `type_of`, `assert`, and `io::*`/`re::*`/`json::*`/`jit::*`
2. Runtime host imports (per-program remapped indices)
   - non-inlined runtime imports are remapped to dense import slots (`call_index_remap`)
   - emitted as `call <slot>, <argc>`
   - exposed as `Program.imports` and bound via `HostFunctionRegistry`
   - `runtime::sleep(ms)` is available as a default host import; native runtimes block for the requested duration and wasm runtimes return `true` immediately
3. Inlined RustScript function bodies
   - calls to targets with `FunctionImpl` are inlined (no emitted `call`)

At runtime, `call` is bridged through `Vm::execute_host_call`:

- builtin call indices dispatch to `vm/builtin_runtime.rs`
- non-builtin indices dispatch to bound host imports
- trace-JIT records `TraceStep::Call`, and native traces bridge call steps through runtime helpers
  (with builtin fast paths where available), preserving interpreter semantics

#### Current Compiler Subset Limitations

Core compiler/IR:

- callable locals can be passed and called, but callables are not runtime `Value`s
- callable values cannot currently be stored in arrays/maps or returned from functions
- recursive RustScript function declarations are not supported by current inlining-based lowering
- function declarations can be nested and implicitly capture outer locals (closure-like snapshot at declaration time)
- in RustScript move-semantics mode, implicit captures follow expression semantics (`x` may move, `x.copy()` copies, `&x`/`&mut x` capture borrowed views)
- `match` patterns are limited to int/string/null literals, `_`, and type constructors (`Some(TypeName)`)
- `break` and `continue` are only valid inside loops
- direct host import namespace syntax in the parser is limited to `vm`, but source loading also supports virtual host namespaces such as `runtime` when the module is missing (builtin namespaces are `io::`, `re::`, `json::`, and `jit::`)

Module/source loading:

- `crate::...` module paths are not supported in RustScript source loading; use relative module paths

JavaScript frontend:

- arrow closures with block bodies are not supported (expression-body arrows only)
- `return <expr>;` is lowered to `<expr>;` (function results still follow final-expression semantics)

Lua frontend:

- Lua pattern API string methods (`:match`, `:gsub`, etc.) are not supported
- function literal bodies are limited to `function(...) end`, `function(...) return end`, or `function(...) return <expr[, expr...]> end`
- direct `function`/`local function` bodies are still minimal: empty/fallthrough, `return`, `return <expr[, expr...]>`, or a single return-only `if`/`elseif`/`else` chain
- `pcall(...)` / `xpcall(...)` lower with success-only semantics and always prefix `true` before the callee return values
- multi-return unpacking is currently limited to compiler-known Lua function/closure return shapes; extra return values are dropped, missing locals are filled with `null`, but plain assignment destructuring and arbitrary host-call unpacking are not supported

Scheme frontend:

- no runtime symbol/procedure type support in VM typing model (`procedure?` and `symbol?` lower to `false`)
- `string->number` currently lowers to placeholder behavior (`0`) due to missing parse builtin
- `apply` is limited to `(apply func arglist)` and does not implement full spread/varargs semantics

### JIT Internals

The VM includes a trace-based JIT path inspired by LuaJIT hot-loop tracing:

- hot bytecode loop heads are detected
- a straight-line trace is recorded from each hot root
- native machine code is emitted per compiled trace and invoked by the VM
- the native bridge executes trace semantics without bytecode re-decoding
- unsupported shapes fall back to interpreter and are recorded as NYI
- native trace emission supports arithmetic/logical opcodes including `mod`, `and`, and `or`

Current NYI in trace compiler:

- backward `brfalse` targets (only forward guard exits are supported)
- traces longer than configured max trace length
- unsupported native targets (currently `x86_64` Windows/Unix-non-macOS and `aarch64` Linux/macOS)
