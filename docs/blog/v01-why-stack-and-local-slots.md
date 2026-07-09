# Why pd-vm Uses a Stack + Local-Slots Architecture

*A design rationale for an embedded proxy VM*

---

## The Problem

pd-vm runs inside `pd-edge`, a proxy runtime that may execute user logic on every request. That environment puts pressure on the VM in a specific way:

1. **Per-request startup must be cheap.**
2. **Teardown must be deterministic and GC-free.**
3. **Execution must suspend and resume cleanly around host I/O.**
4. **Bytecode should stay compact for distribution.**
5. **The same program should scale from interpretation to native compilation.**

Those requirements favor a very small execution contract: instruction pointer, operand stack, and local storage.

## Why Not Register-Based?

A register VM can reduce stack traffic, but it moves complexity into the instruction format and compiler:

- **Instructions get wider.** Register operands consume space in every arithmetic and move instruction.
- **Continuation state gets broader.** Suspension has to preserve a full register file rather than a small stack-and-locals snapshot.
- **Compilation gets heavier.** Good register code wants allocation and rewriting passes that do not help short-lived proxy scripts enough to justify the cost.

For a VM embedded in a proxy runtime, compactness and simple suspension matter more than minimizing dispatch count on arithmetic-heavy code.

## Why Not Pure Stack?

A pure stack machine is compact, but it makes durable program state awkward:

- **Repeated variable access becomes clumsy.** Named state turns into `dup`, `swap`, and stack-shape juggling.
- **Closures need stable storage.** Hidden locals are much simpler than heap-managed environments.
- **Suspension is harder to inspect.** Separating temporaries from durable variables makes pause and resume behavior easier to reason about.

Local slots keep the operand stack short-lived and expression-oriented.

## Why Not a Language-Shaped HIR?

Another option is a single language-shaped core IR, such as an expression-oriented or ANF-style HIR, that every frontend must target.

That approach has a real benefit: it gives the compiler one canonical semantic model. Rewrites, normalization, and some whole-program analyses become more uniform.

The cost is that the HIR stops being a neutral boundary. It becomes another language that every frontend must lower into, including frontends whose natural control flow is imperative. Mutable variables, early returns, and loop-heavy code then have to be translated through an opinionated core representation before they can reach execution.

pd-vm uses a language-agnostic execution IR instead. It gives up the elegance of one canonical core language, but it is a better fit for a VM that serves multiple frontends and treats bytecode, not a language-specific HIR, as the runtime contract.

## The Design

pd-vm uses a compact bytecode with 25 opcodes. The important split is structural:

- **Operand stack** holds transient expression state.
- **Local slots** hold durable per-frame state.
- **Host state** stays outside the VM in the embedding runtime.

`ldloc` reads a local onto the stack. `stloc` writes the top-of-stack value back into a slot. Arithmetic and comparisons operate on stack values. That is enough to keep the interpreter small while still giving the compiler stable storage.

Heap-backed values are shared cheaply. Loading a local is non-destructive, and when the compiler knows a value should move rather than stay shared, it emits the explicit `ldloc; ldc Null; stloc` pattern to release the slot early.

## What This Enables

### 1. Deterministic cleanup

The VM can clear locals and drain the stack without a tracing collector. In `pd-edge`, instances can also be reset and reused rather than rebuilt for every request.

### 2. Clean suspension

When a host call blocks on external work, the continuation is already in the right shape: instruction pointer, stack, and locals. There is no separate coroutine stack and no register remapping step.

### 3. A practical multi-frontend target

Multiple source languages can lower into the same execution format without first agreeing on one language-shaped core runtime.

### 4. Tiered execution

The same bytecode can start in the interpreter, move into a trace JIT for hot paths, and compile ahead of time for stable deployments. The stack effects are simple enough to reconstruct SSA later rather than carrying SSA in the baseline runtime representation.

### 5. Compact program bundles

Small opcodes and small operands keep `.vmbc` bundles compact, which matters when programs are distributed from a control plane to edge nodes.

## Where This Falls Short

- **Baseline interpretation does more dispatch work.** Register VMs can express some local-to-local operations more directly.
- **Local slots are bounded.** The one-byte local operand keeps the encoding compact but imposes a hard slot limit.
- **Dynamic typing still costs at runtime.** The interpreter must inspect value tags unless later tiers specialize the path.
- **The value model favors simplicity over density.** pd-vm does not chase the most compact boxed representation.

## Summary

The stack + local-slots model is not unusual. What makes it the right choice here is the combination of compact bytecode, trivial suspension, predictable teardown, and low frontend complexity. For an embedded proxy VM, those properties are more important than maximizing interpreter throughput on compute-heavy code.

## Related

- [JIT, AOT, and Why We Lower to SSA](./v02-jit-aot-and-ssa.md)
- [GC-Free Scripting](./v04-gc-free-memory.md)
- [Async Suspension Without Coroutines](./v05-async-suspension.md)
