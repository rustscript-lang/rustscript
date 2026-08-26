# Script call frames and callable values

RustScript bytecode format version 11 (VMBC v11) introduces runtime script call frames, first-class callable values, and the static builtin ID catalog.

## Bytecode contract

- `call <import:u16> <argc:u8>` remains the direct host/builtin operation; the `u16` operand is an explicit static builtin call index from the catalog (or a host-import slot) — never a count-derived offset.
- `callvalue <argc:u8>` consumes a stack segment in `callee, arg0, ..., argN` order.
- callable environments are bound through the internal builtin call path; callable creation adds no bytecode opcode.
- `ret` completes the active script frame. A nested frame leaves exactly one result at the caller segment base, using `null` when the body produced no value. Root `ret` keeps the historical program-result stack behavior.

VMBC v11 is a hard format boundary. Decoders reject all earlier versions (v10 and below) with a deterministic unsupported-version error; there is no compatibility decoder and no old-ID alias. The stream includes script-function entry ranges, callable prototypes, function regions, root callable bindings, and call indices drawn from the static builtin catalog. PDRC v6 recordings and AOT artifacts (format 7, ABI 6) use their corresponding bumped versions and include callable metadata in cache identity.

## Static builtin IDs

Every VM-visible builtin (ordinary, internal, and special-call) has one explicit, immutable `u16` call index assigned in `src/builtins/catalog.rs`. `build.rs` parses that catalog and generates the `BuiltinFunction` enum discriminants, `call_index`/`from_call_index`, the `builtin_call_index` reverse lookup, and the `BUILTIN_CATALOG` iteration from the explicit IDs; no ID is derived from catalog length or source order.

- **Immutable explicit IDs.** IDs never change once assigned. Adding or reordering catalog entries never renumbers existing entries; new builtins take the next free ID in their documented block (extension `0x0000..=0xFF8F` for future builtins and host imports, special-call `0xFF90..=0xFFA1`, ordinary `0xFFA2..=0xFFFF`). The reserved sentinel gap `0xFF90..=0xFF92` stays unassigned.
- **Build-time validation.** The build fails on duplicate IDs, duplicate source names, duplicate Rust variants, out-of-block IDs, class/gate inconsistencies, a discovered runtime callable without an explicit ID, or a catalog entry without a runtime callable.
- **Shared std/no-std IDs.** `pd-vm-nostd` dispatches on the same static indices through the checked-in generated mirror `pd-vm-nostd/src/generated_builtin_ids.rs`; the workspace test `static_builtin_ids_are_frozen` fails when the mirror drifts from the catalog.
- **One-time format break.** The static ID migration bumped VMBC to v11 (and the internal bytecode ABI to 11). Older VMBC versions are rejected, never decoded.

## Runtime model

Each script invocation owns:

- a typed continuation (`ResumeBytecode`, `ReturnToHost`, or root `Halt`);
- operand-stack and local-stack bases;
- frame-local count;
- active prototype and callable identity.

Arguments, captures, named callable bindings, and the self binding are installed before control moves to the function entry. Recursive calls therefore allocate independent local storage and are limited to 1,024 script frames.

Branches are restricted to the active function region. Validation rejects cross-region targets before execution, and the interpreter repeats the check at runtime.

## Callable identity and lifetime

A callable contains its prototype ID, kind, and optional environment. The Program/Store owns the callable lifetime. Capture-free function items compare by prototype identity inside that Program; closures compare by runtime environment identity. Callable constants are forbidden; functions are initialized from Program metadata and closures are materialized at their declaration site.

Reset clears Program runtime values and rebinds root function items from Program metadata. Program replacement cancels and releases the old Store callback registry before dropping the old Program. Raw callable values are Program-local and are not portable across Program/Store boundaries.

`Vm::invoke_callable` is the synchronous host-entry API for a callable retained while the current Program is active. For resumable work, `Vm::start_callable` returns `VmStatus`; after `Yielded` or `Waiting`, continue with `Vm::resume` and read the completed value with `Vm::take_callable_result`. `ScriptCallback::start` and `Store::take_callback_result` expose the same flow for typed callbacks. `Store::script_callback` validates Store ownership, arity, and the copied callable schema and returns a typed `ScriptCallback<Args, Ret>` with no stale identity field. A callback can invoke directly, create a `Send` queued invocation on another thread, unsubscribe all aliases, or enter the Store FIFO through `enqueue_callback`; queue errors propagate and no implicit coalescing occurs. `Vm::shutdown` clears queued work, runtime values and host resources before invalidating every exported callback through the Store registry.

PDRC recordings preserve full execution-frame metadata. Callable environments use identity-table encoding, so aliases still share one environment after decode.

## Invocation item stream

`Vm::start_invocation` starts one exported callable with ordinary `Value` arguments and returns an `Invocation` handle that behaves like a fused `Stream<Item = Result<InvocationItem, InvocationError>>`:

- `InvocationItem::Event(value)` items arrive in order for each `stream::emit(value)` call; `stream::emit` still evaluates to `()` inside RSS.
- exactly one `InvocationItem::Complete(value)` carries the callable return value; events never replace it;
- cancellation, fuel exhaustion, epoch deadline expiry, runtime capability failures (including event payload bound violations), and host failures each produce exactly one typed `InvocationError` item;
- every poll after `Complete` or the error item returns `Ready(None)` (fused end of stream);
- `InvocationPoll::Pending` means the VM is paused on an outstanding host operation; drive it through the embedding-owned async bridge and poll again.

Polling drives execution and provides backpressure: at most one event item is buffered between polls, and the VM does not produce items while the consumer is not polling. `stream::emit` validates only the configured per-item value bound (payload bytes and nesting depth); sequence assignment, receipts, persistence, and delivery policy belong to the embedding. At most one invocation is active per VM, `Invocation::cancel(reason)` cancels with a typed `OperationCancelReason`, dropping the handle retires the invocation synchronously for immediate VM reuse, and the low-level `Vm::run` pump is unchanged for custom drivers.

## Optimized backends

Whole-program AOT and Trace JIT use the same builtin call path (static catalog IDs) for environment binding and native frame dispatch for `callvalue`. Script-frame entry and return preserve frame-relative locals and typed continuations.

## Embedded runtime

`pd-vm-nostd` decodes the same VMBC v11 callable metadata and executes callable binding, `callvalue`, recursive frames, captures, and direct host targets using `core` plus `alloc`, dispatching on the identical static builtin IDs via its checked-in generated mirror.
