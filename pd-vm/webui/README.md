# pd-vm Playground

Browser playground for `pd-vm`, backed by the `pd-vm-runtime-wasm` runtime and a Monaco-based editor.

Supported source frontends:

- RustScript (`.rss`)
- JavaScript (`.js`)
- Lua (`.lua`)
- Scheme (`.scm`)

## Features

- Live lint diagnostics from the wasm runtime, surfaced as Monaco markers plus a diagnostics panel
- Compile-time type mismatches such as incompatible `if/else` branches are reported as line-aware
  Monaco markers from the wasm runtime
- Run and debug workflows directly in the browser
- Breakpoints, step/next/out/continue controls, stack/locals views, and hover inspection in the debugger
- Print output and final stack panels
- Fuel and epoch interruption controls for both run sessions and debug sessions
- Epoch state visibility in the UI, including current epoch, deadline, slice, and check interval
- A toolbar action to reset the current editor buffer back to the bundled sample program for the active frontend
- Monaco autocomplete populated from the wasm completion catalog, including embedded RSS stdlib symbols and exposed host namespaces such as `runtime`, `json`, and `re`
- Browser-hosted `runtime::sleep(...)` support in the wasm playground runtime

## Limitations

- The playground uses the wasm runtime. Native JIT is not available there.
- Execution in the browser is effectively interpreter-only today.
- Epoch mode in the browser is driven by a JavaScript timer at 1ms per tick, but those ticks only advance while the main thread can run. A long compute-only wasm call cannot be preempted mid-call.
- The playground is a browser embedding, so behavior can differ from native CLI or server embeddings where threading, timers, and native execution paths are available.

## Development

```powershell
cd pd-vm/webui
bun install
bun run dev
```

Useful commands:

- `bun run dev`
  Starts the Vite dev server after rebuilding the wasm playground runtime.
- `bun run build`
  Rebuilds the wasm runtime, type-checks the web UI, and produces a production bundle.
- `bun run preview`
  Serves the built bundle locally.

The wasm build step compiles `pd-vm-runtime-wasm` for `wasm32-unknown-unknown`, copies the generated `pd_vm_runtime_wasm.wasm` into `public/wasm/`, and syncs the RustScript Monaco grammar assets used by the editor.

## Related docs

- Core VM documentation: [../README.md](../README.md)
- Frontend subset notes: [../src/compiler/frontends/README.md](../src/compiler/frontends/README.md)
