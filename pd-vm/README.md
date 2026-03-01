# pd-vm

`pd-vm` is a stack-based virtual machine plus compiler toolchain used as a backend for multiple
source syntaxes (`.rss`, `.js`, `.lua`, `.scm`).

## Sections

- [How To Use](#how-to-use)
- [Internals](#internals)

## How To Use

### Run Programs

Run with the VM runner binary:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- examples/example.lua
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

Useful commands: `break`, `break line`, `step`, `next`, `out`, `stack`, `locals`, `where`, `continue`.

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

Library hooks:

- `vm.set_jit_config(...)`
- `vm.jit_snapshot()`
- `vm.dump_jit_info()`
- `vm.jit_native_trace_count()`
- `vm.jit_native_exec_count()`

### Wasm Lint

Compiler-only wasm build (without runtime/JIT/debugger/CLI):

```powershell
cargo check -p pd-vm --target wasm32-unknown-unknown --no-default-features
```

Browser/editor lint integration is provided by sibling crate `pd-vm-lint-wasm` via
`lint_source_json`.

### Test and Perf Commands

Integration example tests:

```powershell
cargo test -p pd-vm --test example_tests
```

Manual perf characterization (ignored by default):

```powershell
cargo test -p pd-vm --test perf_tests -- --ignored --nocapture
```

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

1. Source and flavor selection
2. Module/source loading (`compile_source_file()` path)
3. Unit linking (`linker::merge_units`)
4. Frontend lowering (`rustscript`, `javascript`, `lua`, `scheme`)
5. Frontend-independent IR (`FrontendIr` / `LinkedIr`)
6. Bytecode backend (`Compiler` + `Assembler` -> `Program`)
7. VM interpreter execution
8. Trace-JIT IR recording (`JitTrace` + `TraceStep`)
9. Native machine code emission and execution
10. Optional VMBC wire serialization (`vmbc`)


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

- RustScript: `print!(value);`
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
     `type_of`, `assert`, and `io::*`
2. Runtime host imports (per-program remapped indices)
   - non-inlined runtime imports are remapped to dense import slots (`call_index_remap`)
   - emitted as `call <slot>, <argc>`
   - exposed as `Program.imports` and bound via `HostFunctionRegistry`
3. Inlined RustScript function bodies
   - calls to targets with `FunctionImpl` are inlined (no emitted `call`)

At runtime, `call` is bridged through `Vm::execute_host_call`:

- builtin call indices dispatch to `vm/builtin_runtime.rs`
- non-builtin indices dispatch to bound host imports
- trace-JIT records `TraceStep::Call`, and native traces bridge call steps through runtime helpers
  (with builtin fast paths where available), preserving interpreter semantics

#### Current Compiler Subset Limitations

Core compiler/IR:

- no first-class function values in IR/runtime call path
- calls are lowered as static call indices, not callable values
- closures are not general values in this subset
- higher-order patterns like storing/returning arbitrary callable values are not generally supported
- recursive RustScript function declarations are not supported by current inlining-based lowering
- nested function declarations are not supported
- RustScript function declarations cannot capture outer locals
- `match` patterns are limited to int/string literals and `_`
- `break` and `continue` are only valid inside loops
- host import namespace support in parser is limited to `vm` (plus `io::` builtin namespace calls)

Module/source loading:

- `crate::...` module paths are not supported in RustScript source loading; use relative module paths

JavaScript frontend:

- arrow closures with block bodies are not supported (expression-body arrows only)
- empty arrow parameter lists are not supported

Lua frontend:

- numeric `for` loops with negative step are not supported
- Lua pattern API string methods (`:match`, `:gsub`, etc.) are not supported
- function literals require non-empty parameter list and non-empty return expression

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
