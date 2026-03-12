# RustScript Design Critique

A critical review of the design philosophy, architecture, and trade-offs of the RustScript language and the pd-vm runtime.

---

## 1. The Core Idea: What Is RustScript Trying to Be?

RustScript occupies a deliberately strange niche: **a scripting language with Rust-like ownership semantics, running on a stack-based VM with no garbage collector, embedded inside an edge proxy runtime**. It also shares its VM and compiler infrastructure with JavaScript, Lua, and Scheme frontends.

This is an ambitious combination. Let me break down where it's clever, where it's contradictory, and where it has structural problems.

---

## 2. What Works Well

### 2.1 The Multi-Frontend Shared IR Is a Genuinely Good Idea

The architecture of lowering four different surface syntaxes (RustScript, JS, Lua, Scheme) into a single [FrontendIr](pd-vm/src/compiler/ir.rs#291-302) before compilation is clean and well-executed. The IR in [ir.rs](pd-vm/src/compiler/ir.rs) is expressive enough to capture ownership semantics (`MoveVar`, `ToOwned`, `Borrow`, `BorrowMut`) while remaining language-agnostic at the bytecode level.

This means the VM doesn't need to know about Rust syntax, and the JS/Lua/Scheme frontends get the performance benefits of the same optimizer pipeline for free. That's a solid engineering decision.

### 2.2 The No-GC Memory Model Is Coherent (For Its Scope)

The choice to use **owned `Value` enums with `Arc`-wrapped heap types** and **destructive `Ldloc` (move-by-default)** semantics is internally consistent. It gives you:
- Deterministic memory reclamation
- No GC pauses (critical for an edge proxy)
- A simple mental model: values are either on the stack, in a local slot, or gone

The `drop_contract_events` instrumentation for tracking value lifecycles is a nice touch for debugging memory behavior.

### 2.3 The Execution Model Is Well-Designed for Embedding

The `ExecOutcome` / `CallOutcome` / `VmYieldReason` layering for suspension is genuinely well thought out. The distinction between:
- **Fuel/Epoch interrupts** (fire before any opcode, always safe)
- **Host yields** (only at call boundaries, with IP rewind)
- **Async pending** (IP advances, result deferred)

...is clear and correct. The fact that instruction fusion explicitly checks `!interruption_enabled()` before entering fused paths shows careful reasoning about atomicity.

### 2.4 The Opcode Set Is Lean and Honest

24 opcodes. CIL/MSIL-style stack machine. No specialized opcodes for strings, arrays, or maps — those go through [Call](pd-vm/src/compiler/typing/state.rs#359-363) to host/builtin functions. This is the right call for a VM that needs to stay simple, JIT-friendly, and maintainable. The type-hint overlay ([operand_type_hints](pd-vm/src/bytecode.rs#528-543)) for specializing arithmetic at the interpreter level is a pragmatic optimization that doesn't pollute the ISA.

---

## 3. Philosophical Tensions

### 3.1 "Rust Ownership for Scripting" — A DX Layer, Not a Safety Guarantee

The borrow/ownership system in RustScript is best understood as a **developer-time lint and diagnostics layer** — analogous to what TypeScript is for JavaScript, rather than what Rust's borrow checker is for native code. The [availability.rs](pd-vm/src/compiler/lifetime/availability.rs) / [liveness.rs](pd-vm/src/compiler/lifetime/liveness.rs) passes catch common mistakes (use-after-move, uninitialized access, alias-while-mutating) at compile time, while the VM runtime remains fully dynamic.

This is a defensible and pragmatic position. The remaining tension is:

- **Cognitive cost vs. lint value:** Users pay for ownership concepts (move semantics, `.copy()`, `&mut` rules) in exchange for compile-time diagnostics. The question is whether the lint catches are frequent and valuable enough to justify the syntax overhead, especially for users who could switch to the JS/Lua frontends and skip ownership entirely.
- **Non-RustScript frontends silently skip it.** JS, Lua, and Scheme degrade to copy-capture semantics. This is fine architecturally, but it means ownership is a per-frontend opt-in, which dilutes the VM-level identity.
- **Positioning matters.** If users expect Rust-grade safety, they'll be disappointed (no lifetime parameters, no generic constraints, no deep borrow tracking into data structures). If they understand it as "smart linting with Rust-flavored syntax that catches the top 80% of foot-guns," it's a strong value proposition.

> [!TIP]
> Make the documentation explicit: this is a **DX-first model**, not a safety-first model. Frame the ownership checks as lints, not guarantees. This sets correct expectations and lets the feature be appreciated for what it actually delivers.

### 3.2 Dynamic Typing Meets Static Inference — Pragmatic but Opaque

The type system serves a dual purpose: **dev-time error catching** and **runtime performance hints**. It does both reasonably well:

- Flow-sensitive `BoundType` inference catches type conflicts at compile time (e.g., [if](pd-vm/src/vm/mod.rs#837-844)/`else` branch mismatches, binary operand mismatches).
- Host function return types are annotated (`return_type: ValueType` on [HostImport](pd-vm/src/bytecode.rs#385-390) / [FunctionDecl](pd-vm/src/compiler/ir.rs#272-280)), feeding back into inference so chains like `host_call().field` preserve type knowledge.
- Operand type hints ([operand_type_hints](pd-vm/src/bytecode.rs#528-543)) give the interpreter and JIT fast paths for typed arithmetic, comparisons, and string ops.

The remaining concern is **opacity**: `BoundType::Unknown` is a valid degradation state, and users can't easily predict when inference will succeed vs. give up. The `TypeSchema` system (struct schemas, `ArrayOf`, `MapOf`) is a step toward richer inference, but it doesn't yet compose well across function boundaries or through closures.

> [!NOTE]
> The lint infrastructure ([lint_unknown_inferred_local_types](pd-vm/src/compiler/pipeline.rs#477-483)) partially addresses opacity by warning when locals degrade to [Unknown](pd-vm/src/compiler/pipeline.rs#24-29). Expanding this to warn about expression-level degradation (not just locals) would further close the gap.

### 3.3 Four Frontends Is a Maintenance Liability

Supporting JS, Lua, Scheme, and RustScript on one VM is impressive, but each frontend has a growing list of "current subset limits" that reveals the cost:

| Frontend | Notable Limitations |
|---|---|
| **RustScript** | No recursion, closures can't be stored in collections, callable-in-closure rejection |
| **JavaScript** | No arrow block bodies, no direct builtins, no `var`/`class` |
| **Lua** | Minimal function bodies, limited multi-return, no pattern APIs |
| **Scheme** | `apply` is crippled, `symbol?`/`procedure?` are `false`, `string->number` is a placeholder |

Each frontend is a partial subset of its host language. This creates a **documentation and expectation problem**: users familiar with JavaScript/Lua/Scheme will constantly bump into unsupported features, and the error messages for these are not always clear.

> [!IMPORTANT]
> The question isn't "can you support 4 languages?" but "should you?" Every hour spent making Lua's `pcall` work is an hour not spent making RustScript's type system stronger. Consider whether JS/Lua/Scheme are strategic priorities or research curiosities.

---

## 4. Structural Criticisms

### 4.1 Functions Are Not First-Class Citizens

This is the single most impactful design limitation. Right now:

- Functions are **inlined at call sites**. There are no real call frames, no recursive calls, no function values.
- Closures exist but **cannot be stored in collections or returned** from functions.
- `CallableUsedAsValue` is a compile error.

This is not a scripting language limitation — it's closer to a macro expansion system. The [real_script_call_frames_plan.md](plans/real_script_call_frames_plan.md) exists, which is encouraging, but until call frames are real, RustScript is structurally less expressive than any mainstream scripting language.

**Impact:** No recursion means no tree traversal, no divide-and-conquer, no expression parsers, nothing with naturally recursive structure. This severely limits what users can write.

### 4.2 The `Value` Enum Has No Function Variant

Looking at [bytecode.rs](pd-vm/src/bytecode.rs):

```rust
pub enum Value {
    Null, Int(i64), Float(f64), Bool(bool),
    String(SharedString), Array(SharedArray), Map(SharedMap),
}
```

No [Function](pd-vm/src/compiler/ir.rs#282-289), [Closure](pd-vm/src/compiler/ir.rs#78-83), [Callable](pd-vm/src/compiler/typing/state.rs#359-363). This is a consequence of 4.1 — functions aren't values, so they can't be in the value type. But this also means:
- No [map(array, fn)](pd-vm/src/bytecode.rs#336-339) where `fn` is a user-defined function
- No callbacks
- No event handlers passed as values

For an edge proxy runtime that wants user-authored request handlers, this seems like a significant gap.

### 4.3 The [VmMap](pd-vm/src/bytecode.rs#26-29) Uses Identity Equality for Composite Keys

Arrays and maps as map keys compare by `Arc::ptr_eq` (heap identity), not structural equality. This is documented and intentional, but it's surprising behavior:

```
let key = [1, 2, 3];
let map = {key: "value"};
let lookup = [1, 2, 3];  // Different allocation, same content
map[lookup]  // -> null (miss!)
```

This is a foot-gun for anyone used to Python dicts or Lua tables. It's the right engineering choice (structural hashing of nested mutable containers is expensive and fragile), but it should probably be a loud warning in the docs or even a compile-time lint.

### 4.4 Setting a Map Entry to [null](pd-vm/src/vm/mod.rs#734-745) Deletes the Key

From the frontends README:
> Setting a map entry to [null](pd-vm/src/vm/mod.rs#734-745)/`None` removes that key, including during map/object literal construction, so `{a: null}` omits `a`.

This is a bold semantic choice. It means:
- You can't have a key that intentionally maps to [null](pd-vm/src/vm/mod.rs#734-745).
- The pattern `if map[key] == null` conflates "key absent" with "key present with null value".
- This breaks JSON round-tripping for objects with explicit [null](pd-vm/src/vm/mod.rs#734-745) fields (despite the JSON builtins being strict about other things like duplicate keys).

This decision saves runtime complexity (no tombstone tracking, simpler [get](pd-vm/src/compiler/typing/state.rs#165-171) semantics) but creates a semantic hole that will surprise users coming from any other language.

### 4.5 No Real Error Handling

There is no [try](pd-vm/src/bytecode.rs#608-638)/`catch`, no [Result](pd-vm/src/compiler/typing/state.rs#353-357) type in the value system (yet — `Option/Result` plan exists), and host errors bubble up as `VmError::HostError(String)`. The Lua `pcall` lowering has "success-only semantics" (always returns `true`). This means scripts cannot handle errors — they can only crash.

For an edge proxy runtime, this is a reliability concern. A malformed request header shouldn't crash the entire request handler.

---

## 5. Architecture-Level Observations

### 5.1 The JIT Is Trace-Based Without Full CFG Coverage

The trace JIT records linear sequences, with backward `Brfalse` as the only loop construct. Forward branches become guard exits back to the interpreter. This is fine for hot loops but:
- **Branchy code** (complex if/else chains, match arms) will never JIT well.
- **The AOT effort** (documented in [aot_whole_program_effort.md](plans/aot_whole_program_effort.md)) correctly identifies this as a ~3 week rewrite to get full-CFG Cranelift emission.

The trace JIT is a reasonable v1, but the architecture should evolve toward method-level compilation.

### 5.2 The Compilation Pipeline Has Too Many Implicit Phases

The pipeline in [pipeline.rs](pd-vm/src/compiler/pipeline.rs) has the following implicit ordering:

```
parse → legalize_builtins → validate_types → enforce_availability → infer_types → codegen
```

But these phases communicate through mutation of the same [FrontendIr](pd-vm/src/compiler/ir.rs#291-302) struct (adding/removing nodes, inserting `Drop` stmts). The data flow between phases is implicit. A more explicit phase architecture (each phase consuming one type and producing another) would make it easier to reason about where things happen and add new passes.

### 5.3 The 256-Local Limit Will Bite

Local slots are [u8](pd-vm/src/vm/mod.rs#860-868) in bytecode (`Ldloc`/`Stloc` take a single byte operand). This caps programs at 256 locals (including hidden slots for closures, temporaries, etc.). For non-trivial programs with many variables and closure captures, this will be exhausted. The IR already uses [u16](pd-vm/src/vm/mod.rs#869-873) (`LocalSlot = u16`), but the bytecode encoding doesn't.

---

## 6. Summary of Recommendations

| Priority | Recommendation |
|---|---|
| 🔴 Critical | Implement real call frames and make functions first-class values |
| 🔴 Critical | Add error handling (at minimum [Result](pd-vm/src/compiler/typing/state.rs#353-357)-like values or [try](pd-vm/src/bytecode.rs#608-638)/`catch`) |
| 🟡 Important | Clarify the ownership model's purpose — is it for safety, pedagogy, or performance? |
| 🟡 Important | Re-evaluate whether 4 frontends is worth the maintenance cost |
| 🟡 Important | Reconsider null-deletes-key semantics for maps |
| 🟢 Nice-to-have | Move toward method-level JIT compilation (whole-program AOT) |
| 🟢 Nice-to-have | Explicit pipeline phase types instead of mutating [FrontendIr](pd-vm/src/compiler/ir.rs#291-302) in place |
| 🟢 Nice-to-have | Widen bytecode local encoding to u16 |

---

## 7. Final Take

RustScript is an interesting experiment in **bringing systems-language discipline to scripting**. The VM engineering is solid — the execution model, suspension semantics, and JIT infrastructure are well-built for the edge-proxy use case. The multi-frontend architecture is genuinely clever.

But the language design is caught between identities. It's not safe enough to replace Rust, not convenient enough to replace Lua, and not expressive enough (no recursion, no first-class functions) to be a general-purpose scripting language. The ownership model adds complexity without delivering the full value that Rust users expect.

The most impactful work would be: **make functions real** (call frames + first-class values), **add error handling**, and **decide what RustScript's identity is** — then cut what doesn't serve that identity.
