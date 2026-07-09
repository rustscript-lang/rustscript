# Why We Introduced RustScript

*A Rust-flavored surface language that gives the compiler the right information — and gives AI the right syntax*

---

## The Starting Point

pd-vm already runs four source-language frontends on the same stack-based VM with the same bytecode. All four lower into a shared `FrontendIr` before compilation, and all four run on a runtime that uses no garbage collector and relies on deterministic ownership for memory management.

Given those other frontends, a fair question is: why add another one? What does a Rust-shaped scripting syntax buy a VM that already works?

The short answer is that RustScript exists because its syntax carries **intent that the compiler can act on**. Move-vs-copy distinctions, borrow annotations, and mutability declarations are not runtime features. They are compiler inputs. And they happen to be in a syntax that AI models already know well.

## Move and Copy: Telling the Compiler What You Mean

### The VM Does Not Have Destructive Loads

`ldloc` reads a local slot and places a clone of the value on the stack. For heap-backed values (`String`, `Array`, `Map`), that clone is a cheap shared-ownership bump (`Arc`), not a deep copy. The slot retains its value after the load.

This is important: **the VM instruction set is copy-by-default**. There is no special move opcode.

### Moves Are a Compiler Pattern, Not a VM Primitive

When the compiler decides a value should move rather than copy, it emits the pattern `ldloc; ldc Null; stloc` — load the value, then immediately null out the source slot. That gives the compiler precise control over when shared ownership ends and when backing memory becomes reclaimable, without adding any new opcodes or runtime machinery.

RustScript makes this compiler decision explicit in the source:

```rust
let a = [1, 2, 3];
let b = a;           // move: `a` is consumed, slot is cleared
// a is no longer available here
```

The compiler sees the local-to-local rebind of a non-copyable value, lowers it as a consuming load (`MoveVar` in the IR), and emits the null-clear sequence. Using `a` after this point is a compile-time error, not a runtime surprise.

Compare the same logic in the JavaScript frontend:

```javascript
let a = [1, 2, 3];
let b = a;           // copy: both `a` and `b` share the array
```

Both programs compile to valid bytecode on the same VM. The difference is what the compiler knows at the point of the assignment. RustScript's syntax tells it "this is a transfer of ownership." The JS frontend's syntax tells it nothing, so it defaults to shared copy.

### `.copy()` and Borrows

When a RustScript user wants to keep the source alive, they say so:

```rust
let a = [1, 2, 3];
let b = a.copy();    // explicit clone: `a` stays available
let c = &a;          // borrow: non-consuming access
```

`.copy()` emits a `ToOwned` node in the IR, which the codegen lowers to a deep clone. `&a` and `&mut a` emit `Borrow` / `BorrowMut` nodes, which currently collapse to non-consuming access forms but participate in the availability analysis — the compiler tracks that `a` is borrowed and rejects mutations that would invalidate the alias.

None of these constructs change the VM. They change what the compiler does before code reaches the VM.

## How the Compiler Uses This Information

### The Availability Pass

The core of RustScript's value is the availability analysis in `availability.rs`. This pass walks the IR with a `FlowState` that tracks, for every local slot, whether it is:

- **Definitely available** on every control-flow path
- **Possibly available** on some path but not all
- **Moved** (whole-local or per-field)
- **Aliased** to another collection local

When `enable_local_move_semantics` is active (RustScript only), the pass enforces stricter rules:

| Situation | RustScript behavior | JS / Lua behavior |
|---|---|---|
| Local-to-local rebind of a collection | Move source, reject reuse | Copy (shared ownership) |
| Local read after move | Compile error | N/A (no moves) |
| Field read from a struct-like map | Move by default, clear field | Copy |
| Closure capture of a local | Follows expression semantics (`x` moves, `x.copy()` copies, `&x` borrows) | Always copy |
| Mutation while an alias exists | Rejected unless detached via `.copy()` | Allowed (runtime CoW) |

### Consumed-Parameter Inference

RustScript function calls apply consumed-parameter inference at call sites. If the compiler can determine — by analyzing the function body — that a parameter is consumed (moved into a return value, or rebind-moved), it marks that parameter position as consuming. Callers that reuse the argument local after the call get a compile error.

This is entirely a compiler-side analysis. The runtime function-call contract does not change.

### Liveness and Dead-Local Clearing

After availability, a liveness rewriter pass inserts `Drop` statements to null out dead locals as early as possible. This matters for the GC-free runtime: it turns compiler-known variable lifetimes into runtime memory reclamation points without a collector.

RustScript's ownership annotations make liveness analysis more precise because the compiler knows which reads are consuming and which are not.

## Closure Captures: Copy, Borrow, Move

pd-vm closures capture values into hidden local slots at the time the closure is created. The source-level capture expression determines the `CaptureBindingMode`:

```rust
let data = [1, 2, 3];
let f = |x| data.len() + x;    // `data` is moved into the closure
// data is no longer available here

let shared = [4, 5, 6];
let g = |x| shared.copy().len() + x;  // `shared` is copied
// shared is still available
```

The capture binding modes are:

| Mode | Source syntax | Effect on outer scope |
|---|---|---|
| **Copy** | `x.copy()` in capture position | Outer local stays available |
| **Borrow** | `&x` in capture position | Non-consuming; alias tracked |
| **BorrowMut** | `&mut x` in capture position | Non-consuming; mutation tracked |
| **Move** | `x` (default for non-copyable) | Outer local consumed |

In the JS and Lua frontends, all captures degrade to `Copy` mode. The capture slot still gets a value, but the outer scope keeps its own copy unconditionally.

## Type Inference and the Ownership Surface

RustScript's type rules interact with its ownership model:

- `let` bindings are immutable by default; `let mut` is required for reassignment.
- `if`-expression branches with conflicting concrete types are rejected at compile time.
- Optional chaining (`a?.b?.c`) requires the container to come from a user-declared schema, and the result stays optional until handled with `.unwrap_or(...)`, a `!= null` refinement, or a `match Some(name)` arm.
- `+` with a known-string operand infers string concatenation and treats local reads as copy-like to preserve ergonomics.

These are all DX-layer decisions. They catch mistakes before bytecode is generated without adding runtime cost.

## The AI Angle: Syntax Proximity to Rust

There is a practical benefit that was not part of the original design but became increasingly clear as the project grew: **AI code-generation models produce better RustScript than they produce JavaScript or Lua for this VM**.

The reason is corpus proximity. Large language models trained on significant amounts of Rust code have internalized Rust's ownership patterns, borrow semantics, and idiomatic structures. When asked to write RustScript for pd-vm — which lives in the same repository as the Rust runtime — the model:

1. **Naturally uses ownership-aware patterns.** It writes `let b = a;` as a move and `let b = a.copy();` when it needs the source to survive. It does not try to share mutable state through aliasing.
2. **Avoids GC-dependent patterns.** Rust-trained models do not reach for garbage-collected idioms like long-lived closures over mutable state or circular references. Those patterns would be valid in the JS frontend but counterproductive for a GC-free runtime.
3. **Matches the surrounding code style.** The pd-vm runtime is Rust. The compiler is Rust. The test harness is Rust. When the scripting language is also Rust-shaped, the model does not need to context-switch between two different programming idioms within the same codebase.
4. **Produces more accurate type annotations.** Because the model knows Rust's type annotation syntax, it generates valid RustScript type hints that feed into the compiler's inference pipeline rather than leaving locals at `Unknown`.

This is not a theoretical benefit. In practice, AI-assisted development within the pd-vm repository naturally converges on RustScript because the model sees Rust patterns in scope and extends them into the scripting layer.

## Where This Falls Short

- **The ownership model is a DX layer, not a safety guarantee.** There are no lifetime parameters, no generic constraints, and no deep borrow tracking into nested data structures. Users expecting Rust-grade safety will find this is closer to TypeScript's relationship with JavaScript: useful lints, not formal verification.
- **Functions are currently inlined, not first-class.** RustScript inherits the VM's current limitation: no recursive calls, no storing functions in collections. This restricts what users can write regardless of how good the ownership tracking is.
- **The split identity requires clear documentation.** Users need to understand that RustScript's syntax is Rust-like but its semantics are "ownership-informed scripting." The compile-time checks catch the common mistakes — use-after-move, mutation through aliases, uninitialized access — but they are not exhaustive.

## Conclusion

RustScript exists because the VM's GC-free, ownership-based runtime model benefits from a surface language that makes ownership decisions visible to the compiler. The VM does not care — `ldloc` copies, `stloc` overwrites, and the instruction set stays simple. But the compiler cares deeply, and RustScript gives it the right inputs.

The AI affinity is a bonus that compounds over time. As more of the project is developed with LLM assistance, having a scripting language whose syntax matches the surrounding Rust codebase reduces friction and improves output quality at the same time.

## Related

- [Why pd-vm Uses a Stack + Local-Slots Architecture](./v01-why-stack-and-local-slots.md)
- [GC-Free Scripting](./v04-gc-free-memory.md)
- [Why We Didn't Choose a Functional Language](./v03-why-not-functional.md)
- [Async Suspension Without Coroutines](./v05-async-suspension.md)
