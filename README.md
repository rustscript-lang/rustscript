# RustScript

RustScript is the language, VM, Lua/JavaScript frontends, standard library, examples, bytecode/AOT tooling, wasm runtime support, and debugger-facing runtime contract for the RustScript family.

## Related projects

- RustScript core: https://github.com/rustscript-lang/rustscript
- RustScript Playground: https://github.com/rustscript-lang/playground
- CLR VM: https://github.com/rustscript-lang/rustscript-clr-vm
- Edge runtime and ABI: https://github.com/rustscript-lang/pd-edge
- Controller: https://github.com/rustscript-lang/pd-controller

## Crate usage

Consumers can refer to the VM crate from this repository:

```toml
pd-vm = { git = "https://github.com/rustscript-lang/rustscript", package = "pd-vm" }
pd-host-function = { git = "https://github.com/rustscript-lang/rustscript", package = "pd-host-function" }
```

The workspace retains `pd-edge-abi` locally because current VM feature gates expose edge-oriented host ABI schemas.

## Test

```bash
cargo test --workspace --jobs 4
cargo build --workspace --release --jobs 4
```
