# RustScript

[![rustscript on crates.io](https://img.shields.io/crates/v/rustscript.svg)](https://crates.io/crates/rustscript)

RustScript is a compiled scripting language and runtime built around `pd-vm`: a stack-based VM, compiler toolchain, standard library, bytecode/AOT tooling, WebAssembly runtime support, and debugger-facing runtime contract.

## Documentation

The complete language, runtime, and implementation guides live on the [RustScript documentation site](https://rustscript.org/docs/):

- [RustScript overview](https://rustscript.org/docs/reference/rustscript/)
- [Development and tooling](https://rustscript.org/docs/reference/rustscript/development/)
- [VM and compiler internals](https://rustscript.org/docs/reference/rustscript/internals/)
- [RSS language](https://rustscript.org/docs/reference/rss/)
- [Host functions](https://rustscript.org/docs/reference/host-functions/)
- [Runtime controls and artifacts](https://rustscript.org/docs/reference/runtime-controls/)
- [Compiler frontend syntax and feature support](src/compiler/frontends/README.md)

## Crate usage

Consumers can refer to the VM crates from this repository:

```toml
rustscript = "0.22.2"
pd-vm = { git = "https://github.com/rustscript-lang/rustscript", package = "pd-vm" }
pd-host-function = { git = "https://github.com/rustscript-lang/rustscript", package = "pd-host-function" }
```

## Build and test

```bash
cargo test --workspace
cargo build --workspace --release
```

## Related projects

- RustScript Playground: https://github.com/rustscript-lang/playground
- IronRust: https://github.com/rustscript-lang/IronRust
- Edge runtime and ABI: https://github.com/rustscript-lang/pd-edge
- Controller: https://github.com/rustscript-lang/pd-controller
- Compatibility frontends (Lua, JavaScript): https://github.com/rustscript-lang/rustscript-compat-frontends
