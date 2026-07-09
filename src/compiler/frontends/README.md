# Compiler Frontends and Source Plugins

`pd-vm` keeps the built-in frontend surface focused on RustScript (`.rss`). Additional source languages are provided by crates that implement the `SourcePlugin` trait and pass themselves through `CompileSourceFileOptions`.

The compatibility JavaScript and Lua frontends live outside this repository in:

- `pd-vm-compat-frontends`

## Built-in RustScript (`.rss`)

Supported syntax and features:

- Statements: `use`, `fn`, `pub fn`, `let`, assignment, indexed/member assignment, `if`/`else`, `while`, C-style `for`, `break`, `continue`.
- Expressions: literals (`int`, `float`, `bool`, `string`, `null`), arithmetic (`+ - * / %`), logical (`! && ||`), comparison (`== != < >`), function calls, closures (`|...| expr`), if-expression form (`if cond => { ... } else => { ... }`), match expressions.
- Match patterns: int/string/null literals, wildcard `_`, `None`, non-null binding patterns `Some(name)`, and type constructors `Some(TypeName)` / `Option::Some(TypeName)`.
- Collections: array literal `[]`, brace literals for arrays/maps, `obj.member`, `obj[key]`, optional chaining (`?.` and `?.[key]`), slice syntax (`[a:b]`, `[:b]`, `[a:]`), map key literals including `null`.
- Host/runtime calls:
  - builtins via namespaces: `io::...`, `re::...`, `json::...`, `jit::...`, `math::...`
  - host namespaces via `use <namespace>;`, `use <namespace> as <alias>;`, or `use <namespace>::{name as local};`
- RustScript rewrites:
  - `Option::None` -> `null`
  - `Option::Some(expr)` -> `(expr)`
  - `print("...", ...)` and `println("...", ...)` support Rust-style `std::fmt` formatting
- RustScript compile-time type rules:
  - unresolved `unknown` states are compile errors on the RustScript path
  - `+` is inferred as string concatenation when either side is known `string`
  - known mixed numeric arithmetic widens to `float`
  - incompatible known `if`/`else` expression results and branch-local merges are compile errors

## Source Plugin Contract

A source plugin supplies three things:

1. `parse_source(...) -> FrontendIr`
2. `parse_imports(...) -> Vec<ModuleImport>`
3. `strip_imports(...) -> String`

Register the plugin on compile options:

```rust
use pd_vm::{compile_source_file_with_options, CompileSourceFileOptions};

let options = CompileSourceFileOptions::new()
    .with_source_plugin(pd_vm_compat_frontends::plugin());
let compiled = compile_source_file_with_options("examples/example.js", options)?;
```

`compile_source_file()` without options only handles built-in `.rss` files. `.js` and `.lua` paths need a plugin registered for their `SourceFlavor`.

## Frontend-wide Policy Notes

- All accepted sources lower into `FrontendIr`, then use the same linker, type metadata pass, bytecode backend, debugger metadata, and VM runtime.
- Plugin frontends can reuse the shared expression parser through `parse_source_with_dialect(...)`, or build `FrontendIr` directly.
- Plugins own compatibility-language import parsing and import stripping so core module loading does not encode language-specific import syntax.
