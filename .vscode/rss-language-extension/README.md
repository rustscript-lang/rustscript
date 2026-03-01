# RustScript Syntax Extension

Local VS Code extension for syntax highlighting of RustScript `.rss` files.

## Covered syntax

- Keywords: `pub`, `fn`, `let`, `for`, `if`, `else`, `match`, `while`, `break`, `continue`, `use`, `as`
- Module use: `use foo::bar;`; `use foo::bar as ns;`; `use foo::bar::{a, b as c}`
- Namespaced calls: `ns::function(...)`, `ns::sub::function(...)`
- Literals: `true`, `false`, `null`, integers, floats, strings
- Declarations and calls: functions, variables, macros (for example `print!(...)`)
- Expressions: operators (including `=>`, `!=`, `&&`, `||`, `%`), closure pipes (`|...|` and `| | ...`), wildcard `_`, member access (`obj.field`, `obj?.field`)
- Delimiters: `()`, `{}`, `[]`, `,`, `:`, `;`

## Install locally

1. Open VS Code.
2. Open the Command Palette.
3. Run `Extensions: Install from VSIX...` after packaging, or run the extension in an Extension Development Host.

## Development Host

1. Open this folder in VS Code.
2. Open `.vscode/rss-language-extension`.
3. Press `F5` to launch an Extension Development Host.
4. Open an `.rss` file in the development host.
