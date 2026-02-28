# Stack VM

This crate provides a compact stack-based VM intended as a compilation target for a higher-level language.

## Bytecode format

Bytecode is a stream of opcodes followed by little-endian operands. The VM uses absolute jump targets
and a separate constant table.

- 1 byte: opcode
- Operands: little-endian integers (u8, u16, u32)

### Constant table

Constants are stored in `Program.constants` and accessed by index via `ldc`.

### Instruction set

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

### Host calls and resuming

`call` invokes a host function registered in the VM. The host can return `Yield` to suspend execution.
Calling `vm.resume()` continues execution from the next instruction.

## Assembler and compiler skeleton

`Assembler` emits bytecode with labels and patches jumps when `finish_program()` is called.
`Compiler` is a small AST-to-bytecode skeleton that uses `Assembler` to generate control flow.

### Text assembler

Use `assemble()` to parse a tiny assembly language into a `Program`.

Data section declarations:

- `const NAME VALUE`
- `string NAME "..."`

```
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
- `.label NAME` defines a label for jump targets
- `.local NAME [INDEX]` defines a named local

### Tiny compiler

Use `compile_source()` for RustScript syntax (`.rss`) or `compile_source_file()` to auto-detect
syntax by extension (`.rss`, `.js`, `.lua`, `.scm`). It returns `CompiledProgram`, which includes the
program and required local count.

```
fn print(x);
let x = 2 + 3;
let y = x * 4;
if y > 10 {
	print(y);
} else {
	0;
}
```

Closure subset:

```
let base = 7;
let add = |value| value + base;
add(5);
```

Lua flavor closure syntax is lowered from:

```lua
local add = function(value) return value + base end
```

Built-in print aliases (no declaration needed):
- RustScript: `print!(value);`
- JavaScript subset: `console.log(value);` and `print(value);`
- Lua subset: `print(value)`
- Scheme subset: `(print value)`

Loop control supports `break` and `continue`.
Scheme loop forms include `(while condition body...)` and Guile-style
`(do ((name init [step]) ...) (test expr...) body...)`.

Host calls must be explicitly imported:
- RustScript: `use vm::{...};` / `use vm;`
- JavaScript: `import ... from "vm"` or `require("vm")`
- Lua: `require("vm")`
- Scheme: `(import ...)` / `(require ...)` forms for `"vm"`

## Wasm parser/compiler mode

For browser/editor linting scenarios, `pd-vm` now supports a compiler-only build that excludes VM runtime/JIT/debugger/CLI.

Build the wasm target with compiler/parser APIs only:

```powershell
cargo check -p pd-vm --target wasm32-unknown-unknown --no-default-features
```

Linting entrypoint for editor integrations:
- `lint_source_with_flavor(source, flavor) -> LintReport`
- `lint_source(source) -> LintReport` (RustScript default)

## Trace JIT (x86_64 + aarch64)

The VM includes a trace-based JIT path inspired by LuaJIT's hot-loop tracing model:
- hot bytecode loop heads are detected
- a straight-line trace is recorded from that root
- native machine code is emitted per hot trace and invoked by the VM
- the native bridge executes trace semantics without bytecode re-decoding
- unsupported patterns fall back to interpreter and are tracked as NYI
- native trace emission supports arithmetic/logical opcodes including `mod`, `and`, and `or`

Current NYI in trace compiler:
- backward `brfalse` targets (only forward guard exits are supported)
- traces longer than configured max trace length
- targets outside native-JIT support (`x86_64` Windows/Unix-non-macOS, `aarch64` Linux/macOS)

## Run examples

Use the VM runner binary:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- examples/example.lua
```

Other flavors:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- examples/example.js
cargo run -p pd-vm --bin pd-vm-run -- examples/example.rss
cargo run -p pd-vm --bin pd-vm-run -- examples/example.scm
```

Interactive REPL (RustScript snippets, with up/down history):

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --repl
```

JIT visibility (dump compiled traces + NYI reasons):

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --jit-hot-loop 2 --jit-dump examples/example.rss
```

Emit VMBC wire-format bytecode without running:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --emit-vmbc out/example.vmbc examples/example.rss
```

Disassemble VMBC wire-format binaries:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --disasm-vmbc path/to/program.vmbc
```

Include embedded source from debug info (if present):

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --disasm-vmbc path/to/program.vmbc --show-source
```

The listing uses tab-separated columns for `offset`, `opcode`, and `assembly`.
When `--show-source` is enabled, source is shown on separate `; src ...` lines before the first opcode for that line.

Library API visibility hooks:
- `vm.set_jit_config(...)`
- `vm.jit_snapshot()`
- `vm.dump_jit_info()`
- `vm.jit_native_trace_count()`
- `vm.jit_native_exec_count()`

## Performance tests

Manual perf characterization tests (ignored by default):

```powershell
cargo test -p pd-vm --test perf_tests -- --ignored --nocapture
```

Includes:
- VM creation/cleanup speed and RSS delta
- compiler speed and RSS delta
- JIT native machine code execution verification on supported native targets

### Debugger mode

Run with interactive `pdb` debugger on stdio:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --debug examples/example.lua
```

Run with TCP debugger socket:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --debug --tcp 127.0.0.1:9002 examples/example.lua
```

Useful `pdb` commands: `break`, `break line`, `step`, `next`, `out`, `stack`, `locals`, `where`, `continue`.

### Execution recording and replay

Record a full VM run (captures per-step VM state such as `ip`, `stack`, `locals`, and stores bytecode + debug info):

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --record out/example.pdr examples/example.lua
```

Open a saved recording in replay mode:

```powershell
cargo run -p pd-vm --bin pd-vm-run -- --view-record out/example.pdr
```

Replay mode supports `pdb`-style commands including `break`, `break line`, `continue`, `step`, `next`, `out`, `stack`, `locals`, `print`, `ip`, `where`, and `funcs`.
In replay mode, `break`/`break line` do not create runtime VM breakpoints; they set the next replay pause point used by `continue`.

Try it with integration tests:

```powershell
cargo test -p pd-vm --test example_tests
```
