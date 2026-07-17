# Script call frames and callable values

RustScript bytecode format version 9 introduces runtime script call frames and first-class callable values.

## Bytecode contract

- `call <import:u16> <argc:u8>` remains the direct host/builtin operation.
- `callvalue <argc:u8>` consumes a stack segment in `callee, arg0, ..., argN` order.
- callable environments are bound through the internal builtin call path; callable creation adds no bytecode opcode.
- `ret` completes the active script frame. A nested frame leaves exactly one result at the caller segment base, using `null` when the body produced no value. Root `ret` keeps the historical program-result stack behavior.

VMBC v9 is a hard format boundary. Decoders reject older versions. The stream includes script-function entry ranges, callable prototypes, function regions, and root callable bindings. PDRC recordings and AOT artifacts use their corresponding bumped format/ABI versions and include callable metadata in cache identity.

## Runtime model

Each script invocation owns:

- a typed continuation (`ResumeBytecode`, `ReturnToHost`, or root `Halt`);
- operand-stack and local-stack bases;
- frame-local count;
- active prototype and callable identity.

Arguments, captures, named callable bindings, and the self binding are installed before control moves to the function entry. Recursive calls therefore allocate independent local storage and are limited to 1,024 script frames.

Branches are restricted to the active function region. Validation rejects cross-region targets before execution, and the interpreter repeats the check at runtime.

## Callable identity and lifetime

A callable contains the owning program-instance ID, prototype ID, kind, and optional environment. Capture-free function items compare by program/prototype identity. Closures compare by runtime identity. Callable constants are forbidden; functions are initialized from program metadata and closures are materialized at their declaration site.

Resetting or replacing the VM program assigns a new program-instance ID. Calling a value from an older instance reports `StaleCallable`.

`Vm::invoke_callable` is the synchronous host-entry API for a callable retained after the root program halts. For resumable work, `Vm::start_callable` returns `VmStatus`; after `Yielded` or `Waiting`, continue with `Vm::resume` and read the completed value with `Vm::take_callable_result`. `ScriptCallback::start` and `Store::take_callback_result` expose the same flow for typed callbacks. `Store::script_callback` validates the Store identity, Program instance, arity, and copied callable schema and returns a typed `ScriptCallback<Args, Ret>`. A callback can invoke directly, create a `Send` queued invocation on another thread, unsubscribe all aliases, or enter the Store FIFO through `enqueue_callback`; queue errors propagate and no implicit coalescing occurs. `Vm::shutdown` clears queued work, runtime values and host resources before invalidating every exported callable.

PDRC recordings preserve full execution-frame metadata. Callable environments use identity-table encoding, so aliases still share one environment after decode.

## Optimized backends

Whole-program AOT and Trace JIT use the same builtin call path for environment binding and native frame dispatch for `callvalue`. Script-frame entry and return preserve frame-relative locals and typed continuations.

## Embedded runtime

`pd-vm-nostd` decodes the same VMBC v9 callable metadata and executes callable binding, `callvalue`, recursive frames, captures, and direct host targets using `core` plus `alloc`.
