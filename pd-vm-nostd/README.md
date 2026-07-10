# pd-vm-nostd

`pd-vm-nostd` is the freestanding RustScript VMBC interpreter for `no_std + alloc` targets.
It lives beside `pd-vm-wasm` in the RustScript workspace and intentionally excludes the source
compiler, parser, CLI, debugger, JIT/AOT backends, filesystem support, and operating-system hosts.

## Runtime surface

- VMBC v8 decoding with type/debug metadata skipped after validation
- stack and local execution for direct bytecode opcodes
- instruction fuel with pause/resume support
- synchronous named host bindings and dynamic host dispatch
- `Rc`-backed strings, bytes, arrays, and maps for single-threaded targets

Compile RustScript source to VMBC with the standard `pd-vm` host tools, then decode and execute it on
the target:

```rust
use pd_vm_nostd::{Vm, decode_program};

let program = decode_program(vmbc_bytes)?;
let mut vm = Vm::new(program);
vm.run()?;
```

## Verification

```bash
rustup target add thumbv6m-none-eabi
cargo test -p pd-vm-nostd
cargo check -p pd-vm-nostd --target thumbv6m-none-eabi
cargo tree -p pd-vm-nostd --target thumbv6m-none-eabi -e normal
```

The target dependency tree must contain only `pd-vm-nostd` itself. `pd-vm` is used solely as a native
dev-dependency for compiler/VMBC compatibility tests.
