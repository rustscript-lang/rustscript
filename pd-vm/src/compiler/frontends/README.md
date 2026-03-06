# Compiler Frontends: Supported Syntax and Features

This document describes the currently supported source syntax for the four `pd-vm` compiler
frontends:

- RustScript (`.rss`)
- JavaScript subset (`.js`)
- Lua subset (`.lua`)
- Scheme subset (`.scm`)

All frontends lower to the same frontend IR and VM bytecode model.

## Quick Syntax Map

| Feature | RustScript | JavaScript | Lua | Scheme |
| --- | --- | --- | --- | --- |
| Local binding | `let x = 1;` | `let x = 1;` / `const x = 1;` | `local x = 1` | `(define x 1)` |
| Assignment | `x = 2;` | `x = 2;` | `x = 2` | `(set! x 2)` |
| Function declaration | `fn add(x) { x + 1; }` | `function add(x) { x + 1; }` | `local function add(x) return x + 1 end` | `(define (add x) (+ x 1))` |
| Closure literal | `|x| x + 1` | `(x) => x + 1` | `function(x) return x + 1 end` | `(lambda (x) (+ x 1))` |
| If statement | `if cond { ... } else { ... }` | `if (cond) { ... } else { ... }` | `if cond then ... else ... end` | `(if cond then else)` |
| While loop | `while cond { ... }` | `while (cond) { ... }` | `while cond do ... end` | `(while cond ...)` |
| For loop | `for (let i = 0; i < n; i = i + 1) { ... }` | same style | `for i = 0, n, 1 do ... end` / `for k,v in pairs(t) do ... end` | `(for (i 0 n [step]) ...)` / `(do ...)` |
| Collection literals | `[1, 2]`, `{x: 1}` | `[1, 2]`, `{ x: 1 }` | `{1, 2, x = 1}` | `(vector 1 2)`, `(list 1 2)`, `(hash (x 1))` |
| Index/member access | `a[i]`, `m.key` | `a[i]`, `m.key` | `a[i]`, `m.key` | `(vector-ref a i)`, `(hash-ref m k)` |
| Optional chain | `a?.b?.c` | `a?.b?.c` | `a?.b?.c` | `a?.b?.c` |
| Slice | `v[start:end]` | `v[start:end]` | `v[start:end]` | `(slice-range v s e)` / `(slice-to v e)` / `(slice-from v s)` |
| Host import | `use vm::{f};` | `import { f } from "vm";` | `local f = require("vm").f` | `(require (only-in "vm" f))` |
| Print | `print!(x);` / `print(x);` | `console.log(x);` / `print(x);` | `print(x)` | `(print x)` / `(display x)` |

## RustScript (`.rss`)

Supported syntax and features:

- Statements: `use`, `fn`, `pub fn`, `let`, assignment, indexed/member assignment, `if`/`else`,
  `while`, C-style `for`, `break`, `continue`.
- Expressions: literals (`int`, `float`, `bool`, `string`, `null`), arithmetic (`+ - * / %`),
  logical (`! && ||`), comparison (`== != < >`), function calls, closures (`|...| expr`),
  if-expression form (`if cond => { ... } else => { ... }`), match expressions.
- Match patterns: int/string/null literals, wildcard `_`, and type constructors
  `Some(TypeName)`.
- Collections: array literal `[]`, brace literals for arrays/maps, `obj.member`, `obj[key]`,
  optional chaining (`?.` and `?.[key]`), slice syntax (`[a:b]`, `[:b]`, `[a:]`), map key
  literals including `null`.
- Host/runtime calls:
  - `use vm;`, `use vm as alias;`, `use vm::{name, name as local};`, `use vm::*;`
  - builtins via namespaces: `io::...`, `re::...`, `json::...`, `jit::...` (regex supports optional flags arg)
  - additional host namespaces via `use <namespace>;` / `use <namespace> as <alias>;`
- RustScript frontend rewrites:
  - `print!(x)` -> `print(x)`
- Module import syntax (for `.rss` modules): `use module;`, `use module::*;`, `use module::{...}`,
  plus relative paths with `self::` / `super::`.

Current subset limits:

- Function declarations are inlined; recursive declarations are not supported.
- Nested function declarations are not supported.
- `fn` declarations cannot capture outer locals (closures can).
- Callable values cannot currently be stored in arrays/maps or returned.
- Match patterns are limited to the forms listed above.
- `break`/`continue` are only valid inside loops.
- `crate::...` module paths are not supported in RustScript module loading.

## JavaScript Subset (`.js`)

Supported syntax and features:

- Statements: `let`/`const`, assignment, indexed/member assignment, `if`/`else if`/`else`,
  `while`, C-style `for`, `break`, `continue`.
- Functions:
  - `function` declarations are lowered to RustScript-style function declarations.
  - Arrow closures with expression bodies are supported, including empty parameter list (`() => 42`).
- Expressions: literals, arithmetic (`+ - * / %`), logical (`! && ||`), comparison (`== != < >`),
  calls, arrays/objects, index/member access, optional chaining, slice syntax (`[a:b]` family).
- JavaScript frontend rewrites:
  - `console.log(...)` -> `print(...)`
  - `typeof value` -> `type(value)`
  - `const` -> `let`
  - `function` -> `fn`
  - `return <expr>;` -> `<expr>;` (final-expression function body model)
- Imports/host calls:
  - `import * as vm from "vm";` for namespace calls (`vm.add_one(...)`,
    `vm.http.request.get_header(...)`)
  - named imports from `"vm"` (`import { add_one as inc } from "vm";`)
  - `require("vm")` forms for vm namespace aliasing
  - module imports from `.rss` are recognized from `import`/`require` forms
- Semicolons may be omitted at line ends (frontend enables implicit statement terminators).

Current subset limits:

- Arrow closures with block bodies are rejected (`(x) => { ... }`).
- Direct calls to VM helper builtins like `len/get/set/count/...` are rejected; use language syntax
  (`.length`, indexing, assignment, `typeof`, namespace forms).
- Undeclared host calls are rejected (import from `"vm"` first).

## Lua Subset (`.lua`)

Supported syntax and features:

- Statements:
  - `local` bindings, assignment, indexed/member assignment
  - `local function` and `function` declarations
  - `if`/`elseif`/`else`/`end`
  - `while ... do ... end`
  - numeric `for i = start, end [, step] do ... end`
  - generic loops with `pairs(...)` and `ipairs(...)` (single or key/value loop vars)
  - `repeat ... until ...`
  - `do ... end`
  - `break`, `continue`, and `goto continue` compatibility lowering
- Expressions:
  - literals (`nil`, bool, int/float, single/double-quoted strings)
  - operators: `+ - * / %`, `not`, `and`, `or`, `~=` and `..` (lowered)
  - function literals (`function(args) return <expr> end`)
  - table literals (array parts, map parts, and mixed forms)
  - member/index access, optional chaining, slice syntax
  - method-call lowering for `receiver:method(args)`
- Lua-specific lowering helpers:
  - string methods `:sub(...)` and `:len()`
  - length operator `#value` with Lua-style helpers for arrays/maps/strings
- Imports/host calls:
  - `local vm = require("vm")`
  - `local alias = require("vm").name`
  - direct `require("...")` module import forms (including `.rss` modules)

Current subset limits:

- Function literals must use `return <expr>` (single-expression body).
- Lua pattern API string methods (`:find`, `:match`, `:gsub`) are currently rejected.
- Direct VM helper builtin calls are not exposed as frontend functions.

## Scheme Subset (`.scm`)

Supported syntax and features:

- Statements/forms:
  - `(define x expr)` and function form `(define (name args...) body...)`
  - `(set! name expr)`
  - `(if cond then [else])`, `(when ...)`, `(unless ...)`
  - `(cond ...)`, `(case ...)`
  - loops: `(while ...)`, `(do ...)`, `(for (i start end [step]) ...)`
  - `(break)`, `(continue)`
  - `(begin ...)`
  - `(declare (name args...))` external declaration form
  - `(vector-set! ...)`, `(hash-set! ...)`
  - `(display x)`, `(write x)`, `(newline)`, `(for-each proc list)`
- Expression forms:
  - arithmetic: `+ - * / modulo remainder quotient abs min max`
  - comparison/boolean: `= /= < > <= >=`, `and`, `or`, `not`
  - type and predicates:
    `type`, `type-of`, `null?`, `number?`, `integer?`, `string?`, `boolean?`,
    `vector?`, `list?`, `pair?`, `procedure?`, `symbol?`,
    `zero?`, `positive?`, `negative?`, `even?`, `odd?`
  - equality predicates: `eq?`, `eqv?`, `equal?`
  - collections:
    `list`, `cons`, `car`, `cdr`, `cadr`, `caddr`, `length`, `append`, `reverse`,
    `vector`, `vector-ref`, `hash`, `hash-ref`, `keys`,
    `slice-range`, `slice-to`, `slice-from`
  - higher-order forms: `map`, `filter`, `apply`
  - strings: `string-append`, `string-length`, `string-ref`, `substring`,
    `number->string`, `string->number`
  - quoting: `(quote ...)` and `'...`
  - functional forms: `lambda`, `let`, `let*`, `letrec`, named `let`
  - optional-chain symbol lowering (for example `profile?.stats?.score`)
- Imports/module syntax:
  - `(import "...")`, `(require "...")`
  - import sets:
    - `only` / `only-in`
    - `rename` / `rename-in`
    - `prefix` / `prefix-in`
    - `library` / `module`
  - vm-prefixed host calls via forms like `(require (prefix-in vm. "vm"))` and then
    `(vm.http.request.get_header "...")`
- Identifier normalization: Scheme `-` is normalized to `_` for lowered identifiers.

Current subset limits:

- No runtime symbol/procedure type in VM typing model (`symbol?` and `procedure?` lower to `false`).
- `string->number` currently lowers to placeholder behavior (`0`).
- `apply` is currently limited to `(apply func arglist)` and does not implement full spread/varargs.
- Direct VM helper builtin calls (for example raw `len`) are rejected; use Scheme forms instead.

## Frontend-wide Policy Notes

- VM helper builtin names such as `count` are not exposed as direct frontend functions.
- Host calls must be imported/declared in frontend-appropriate syntax.
