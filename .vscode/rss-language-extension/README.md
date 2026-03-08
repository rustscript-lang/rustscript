# RustScript Language Tools

Local VS Code extension for RustScript `.rss` with syntax highlighting plus a language server.

## Covered syntax

- Keywords: `pub`, `fn`, `let`, `for`, `if`, `else`, `match`, `while`, `break`, `continue`, `use`, `as`
- Module use: `use foo::bar;`; `use foo::bar as ns;`; `use foo::bar::{a, b as c}`
- Namespaced calls: `ns::function(...)`, `ns::sub::function(...)`
- Literals: `true`, `false`, `null`, integers, floats, strings
- Declarations and calls: functions and variables
- Expressions: operators (including `=>`, `!=`, `&&`, `||`, `%`), closure pipes (`|...|` and `| | ...`), wildcard `_`, member access (`obj.field`, `obj?.field`)
- Delimiters: `()`, `{}`, `[]`, `,`, `:`, `;`

## Language server features

- Diagnostics from `pd_vm_lint_wasm.wasm` (same lint core used by the controller web UI)
- Completion items sourced from the wasm completion catalog
- Hover docs for known symbols
- Basic document symbols for `fn` and `let` declarations
- Fallback delimiter diagnostics if lint wasm is unavailable

## Install locally

1. Open VS Code.
2. Open the Command Palette.
3. Run `Extensions: Install from VSIX...` after packaging, or run the extension in an Extension Development Host.

## Development Host

1. Open this folder in VS Code.
2. Open `.vscode/rss-language-extension`.
3. Run `npm install`.
4. Run `npm run copy-wasm` (builds `pd-vm-lint-wasm` from source, then bundles it).
5. Press `F5` to launch an Extension Development Host.
6. Open an `.rss` file in the development host.

## Config

- `rustscript.languageServer.wasmPath`: optional absolute path override for `pd_vm_lint_wasm.wasm`.
