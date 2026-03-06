# pd-vm webui playground

Browser playground for `pd-vm` with Monaco editor support for all four frontends:

- RustScript (`.rss`)
- JavaScript (`.js`)
- Lua (`.lua`)
- Scheme (`.scm`)

Features:

- live wasm lint diagnostics (Monaco markers + diagnostics panel)
- wasm runtime execution via **Run** button
- autocomplete catalog sourced from wasm (embedded RSS stdlib + playground host functions)
- print output + final VM stack preview

## Run

```powershell
cd pd-vm/webui
bun install
bun run dev
```

The dev command builds `pd-vm-runtime-wasm` for `wasm32-unknown-unknown` and copies:

- `target/wasm32-unknown-unknown/release/pd_vm_runtime_wasm.wasm` -> `public/wasm/`
- RustScript Monaco grammar assets from `.vscode/rss-language-extension` -> `src/monaco/`
