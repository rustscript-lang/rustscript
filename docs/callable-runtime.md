# Script call frames and callable values

RustScript bytecode format version 12 (VMBC v12) carries runtime script call frames, first-class callable values, the static builtin ID catalog, and the direct script-call opcode. Version 11 introduced frames, callable values, and the static catalog; version 12 adds `callscript` for statically resolved named calls.

## Bytecode contract

- `call <import:u16> <argc:u8>` remains the direct host/builtin operation; the `u16` operand is an explicit static builtin call index from the catalog (or a host-import slot) — never a count-derived offset.
- `callvalue <argc:u8>` consumes a stack segment in `callee, arg0, ..., argN` order.
- `callscript <prototype_id:u32> <argc:u8>` calls a statically resolved named script function by prototype ID. It consumes only `argc` arguments; no callable value is taken from the stack, so environment-free named functions can be called without a hidden callable local.
- callable environments are bound through the internal builtin call path; callable creation adds no bytecode opcode.
- `ret` completes the active script frame. A nested frame leaves exactly one result at the caller segment base, using `null` when the body produced no value. Root `ret` keeps the historical program-result stack behavior.

### Call ownership

The three call opcodes differ in who owns the callee and what the frame must provide:

- `call` — the callee is owned by the static builtin catalog (or the host-import slot). The frame contributes only `argc` arguments; there is no callable value anywhere in the program.
- `callvalue` — the callee is a `Value::Callable` owned by the caller operand stack at the call site, and remains the caller's responsibility after the call. This path carries environments, closures, and any callable whose identity or capture state is runtime-valued.
- `callscript` — the callee is owned by program callable metadata (the prototype table). The frame contributes only `argc` arguments and no callable value, but unlike `call` the callee is a script function rather than a builtin, so the call enters a new script frame with its own local base.

VMBC v12 is a hard format boundary. Decoders reject all earlier versions (v11 and below) with a deterministic unsupported-version error; there is no compatibility decoder and no old-ID alias. The stream includes script-function entry ranges, callable prototypes, function regions, root callable bindings, and call indices drawn from the static builtin catalog. PDRC v6 recordings and AOT artifacts (format 8, ABI 8) use their corresponding bumped versions and include callable metadata in cache identity.

## Static builtin IDs

Every VM-visible builtin (ordinary, internal, and special-call) has one explicit, immutable `u16` call index assigned in `src/builtins/catalog.rs`. `build.rs` parses that catalog and generates the `BuiltinFunction` enum discriminants, `call_index`/`from_call_index`, the `builtin_call_index` reverse lookup, and the `BUILTIN_CATALOG` iteration from the explicit IDs; no ID is derived from catalog length or source order.

- **Immutable explicit IDs.** IDs never change once assigned. Adding or reordering catalog entries never renumbers existing entries; new builtins take the next free ID in their documented block (extension `0x0000..=0xFF8F` for future builtins and host imports, special-call `0xFF90..=0xFFA1`, ordinary `0xFFA2..=0xFFFF`). The reserved sentinel gap `0xFF90..=0xFF92` stays unassigned.
- **Build-time validation.** The build fails on duplicate IDs, duplicate source names, duplicate Rust variants, out-of-block IDs, class/gate inconsistencies, a discovered runtime callable without an explicit ID, or a catalog entry without a runtime callable.
- **Shared std/no-std IDs.** `pd-vm-nostd` dispatches on the same static indices through the checked-in generated mirror `pd-vm-nostd/src/generated_builtin_ids.rs`; the workspace test `static_builtin_ids_are_frozen` fails when the mirror drifts from the catalog.
- **Format breaks are permanent.** The static ID migration bumped VMBC to v11 (and the internal bytecode ABI to 11); the `callscript` opcode break bumped both to v12. Versions below the current format are rejected, never decoded.

## Runtime model

Each script invocation owns:

- a typed continuation (`ResumeBytecode`, `ReturnToHost`, or root `Halt`);
- operand-stack and local-stack bases;
- frame-local count;
- active prototype and callable identity.

Arguments, captures, hidden callable bindings for materialized named functions, and the self binding are installed before control moves to the function entry. Recursive calls therefore allocate independent local storage and are limited to 1,024 script frames.

Branches are restricted to the active function region. Validation rejects cross-region targets before execution, and the interpreter repeats the check at runtime.

## Frame-local allocation and callable materialization

Each script invocation frame is an independent local-address space with its own `local_base`. Locals that are live at the same time inside one frame interfere and receive distinct relative slot numbers; locals that belong to different frames never interfere and may reuse the same relative slot number, because the runtime frame bases already separate them. A statically resolved named call keeps the caller's argument slots and post-call values live in the caller frame, while the callee body's locals are analyzed inside the callee frame.

Named functions receive a hidden callable slot only when runtime `Value::Callable` identity is actually required:

- the function is exported under the `ExportedCallable { local_slot }` contract;
- the function is referenced as a value (stored, passed, or returned);
- the function captures an environment;
- a dynamic call site can target the function (invoked slot or argument flow into an invoked parameter);
- the function's runtime self identity is required by a capturing or dynamic recursion path.

Functions that only receive plain direct calls — including non-capturing direct recursion — are lowered through `callscript` by prototype ID and consume no hidden callable local. The compiler reports the aggregate frame-local count (data slots plus materialized callable slots) in `FrameLocalLimitExceeded` diagnostics, so overflow reports real counts instead of a sentinel. Genuine same-frame pressure beyond 256 simultaneous locals keeps failing until wide local bytecode lands.

## Callable identity and lifetime

A callable contains its prototype ID, kind, and optional environment. The Program/Store owns the callable lifetime. Capture-free function items compare by prototype identity inside that Program; closures compare by runtime environment identity. Callable constants are forbidden; functions are initialized from Program metadata and closures are materialized at their declaration site.

Reset clears Program runtime values and rebinds root function items from Program metadata. Program replacement cancels and releases the old Store callback registry before dropping the old Program. Raw callable values are Program-local and are not portable across Program/Store boundaries.

`Vm::invoke_callable` is the synchronous host-entry API for a callable retained while the current Program is active. For resumable work, `Vm::start_callable` returns `VmStatus`; after `Yielded` or `Waiting`, continue with `Vm::resume` and read the completed value with `Vm::take_callable_result`. `ScriptCallback::start` and `Store::take_callback_result` expose the same flow for typed callbacks. `Store::script_callback` validates Store ownership, arity, and the copied callable schema and returns a typed `ScriptCallback<Args, Ret>` with no stale identity field. A callback can invoke directly, create a `Send` queued invocation on another thread, unsubscribe all aliases, or enter the Store FIFO through `enqueue_callback`; queue errors propagate and no implicit coalescing occurs. `Vm::shutdown` clears queued work, runtime values and host resources before invalidating every exported callback through the Store registry.

PDRC recordings preserve full execution-frame metadata. Callable environments use identity-table encoding, so aliases still share one environment after decode.

## Invocation item stream

`Vm::start_invocation` starts one exported callable with ordinary `Value` arguments and returns an `Invocation` handle that behaves like a fused `Stream<Item = Result<InvocationItem, InvocationError>>`:

- `InvocationItem::Event(value)` items arrive in order for each `stream::emit(value)` call; `stream::emit` still evaluates to `()` inside RSS.
- exactly one `InvocationItem::Complete(value)` carries the callable return value; events never replace it;
- cancellation, fuel exhaustion, epoch deadline expiry, runtime capability failures, and host failures each produce exactly one typed `InvocationError` item;
- every poll after `Complete` or the error item returns `Ready(None)` (fused end of stream);
- `InvocationPoll::Pending` means the VM is paused on an outstanding host operation; drive it through the embedding-owned async bridge and poll again.

Polling drives execution and provides backpressure: at most one event item is buffered between polls, and the VM does not produce items while the consumer is not polling. `stream::emit` validates only the configured per-item value bound; sequence assignment, receipts, persistence, and delivery policy belong to the embedding. At most one invocation is active per VM, `Invocation::cancel(reason)` cancels with a typed `CancellationReason`, and the low-level `Vm::run` pump is unchanged for custom drivers.

## Callable-driven HTTP streams

The HTTP client keeps buffered `http::client::request(request)` and defines `http::client::sse(request, on_event)` and `http::client::websocket(request, on_event)` as long-running ordinary host calls. Each handler has the schema `fn(map) -> map`. The host produces one protocol item, the VM runs one child callback frame, and the returned action controls continuation or a WebSocket write before another item can arrive at the VM boundary.

The callback may yield or wait in an ordinary async host call. Existing frame machinery resumes the callback first and returns its final action to the suspended HTTP call. The network future does not own or enter the VM and is not polled while the callback is active, so at most one item remains unacknowledged and callback completion supplies backpressure.

Each buffered, SSE, and WebSocket import is an independent capability. Streaming calls expose no script request IDs, handles, detached resources, `next`, `close`, or cancellation callables. Their complete event maps, action maps, terminal summaries, bounds, destination policy, and lifecycle contract are documented in [HTTP client callable contract](http-client.md).

## Optimized backends

Whole-program AOT and Trace JIT use the same builtin call path (static catalog IDs) for environment binding, native frame dispatch for `callvalue`, and prototype-direct native dispatch for `callscript`. Script-frame entry and return preserve frame-relative locals and typed continuations.

## Embedded runtime

`pd-vm-nostd` decodes the same VMBC v12 callable metadata and executes callable binding, `callvalue`, `callscript`, recursive frames, captures, and direct host targets using `core` plus `alloc`, dispatching on the identical static builtin IDs via its checked-in generated mirror.
