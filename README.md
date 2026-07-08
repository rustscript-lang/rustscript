# RustScript

RustScript is the language, VM, Lua/JavaScript frontends, standard library, examples, browser playground, bytecode/AOT tooling, and debugger-facing runtime contract from the original `project-d` history.

## Repository split

- RustScript core: https://github.com/rustscript-lang/rustscript
- CLR VM: https://github.com/rustscript-lang/rustscript-clr-vm
- Edge runtime and ABI: https://github.com/rustscript-lang/pd-edge
- Controller: https://github.com/rustscript-lang/pd-controller

## Local crates

Consumers can refer to the VM crate from the split repository:

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
