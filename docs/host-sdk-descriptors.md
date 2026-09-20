# Host SDK: descriptors, resources, and effects

This guide is for anyone authoring a host module or migrating an existing host
extension onto the current SDK. It describes the preferred authoring surface:
a `#[pd_host_function]` declaration is the single source of a function's guest
schema, runtime binding, typed resource requirements, named-struct contract, and
hidden host-state effects. A module then lists its functions explicitly; the
guest catalog is derived from those descriptors.

The legacy `HostApiBuilder::{resource,named_struct,function}` and
`HostFunctionRegistry::register_*` APIs remain public and fully supported during
the compatibility window (see [Compatibility window](#compatibility-window)).
New modules should not need them.

Organization freeze outcomes:
[host-descriptor-migration-report.md](host-descriptor-migration-report.md).

## 1. The model

One host function declaration produces, in a single macro expansion:

| Part | Meaning |
|---|---|
| `schema` | The guest ABI: parameter names, types, passing modes, return type. This is the only fingerprint input. |
| `binding` | The dispatch class (`Static`, `StaticStack`, `StaticArgs`, `StaticNonYieldingArgs`, `Owned`). |
| `adapter` | The concrete adapter or owned-dispatch factory installed into a registry. |
| `effects` | Guest resource effects (borrow/borrow-mut/take-owned/create) and hidden host-state read/write effects. Runtime-only metadata; excluded from the fingerprint. |
| `resource_types` | The concrete resource-type declarations this function contributes. |

A `HostModuleDescriptor` aggregates functions in a deterministic, author-declared
order:

```rust
use vm::HostModuleDescriptor;

pub fn demo_module() -> HostModuleDescriptor {
    HostModuleDescriptor {
        name: "demo",
        functions: &[
            make_counter_descriptor,
            read_counter_descriptor,
        ],
        resources: &[counter_resource], // extra resource declarations
    }
}
```

There is deliberately **no linker inventory**: the module lists its functions, so
feature gates, ordering, WebAssembly builds, and dead-code behavior are all
explicit and reproducible.

## 2. Authoring a module

```rust
use pd_host_function::pd_host_function;
use vm::{ResourceRef, ResourceOwned, VmResult, resource};

/// A typed host resource with a canonical declaration.
pub struct Counter(u64);

impl resource::HostResource for Counter {}

impl vm::HostResourceType for Counter {
    const KEY: &'static str = "demo.counter";
    const DESCRIPTION: &'static str = "A monotonic counter";
}

/// Creates a counter and returns its handle.
#[pd_host_function(name = "demo::make_counter")]
pub fn make_counter(seed: i64) -> VmResult<resource::Resource<Counter>> {
    Ok(resource::Resource::new(Counter(seed as u64)))
}

/// Reads the current count.
#[pd_host_function(name = "demo::read_counter")]
pub fn read_counter(counter: ResourceRef<'_, Counter>) -> VmResult<i64> {
    Ok(*counter as u64 as i64)
}

/// Consumes the counter.
#[pd_host_function(name = "demo::close_counter")]
pub fn close_counter(counter: ResourceOwned<Counter>) -> VmResult<bool> {
    let _ = counter.into_inner();
    Ok(true)
}
```

Rules that follow from the table above:

- **Typed resource wrappers carry the effect.** `ResourceRef<'_, T>` is a
  `Borrow<T>` effect, `ResourceMut<'_, T>` is `BorrowMut<T>`, `ResourceOwned<T>`
  is `TakeOwned<T>`, and returning `Resource<T>` is `Create<T>`. The guest
  parameter/return schemas and the resource declarations both come from those
  wrappers; nothing needs to be repeated in a catalog.
- **One declaration per resource type.** Implement `HostResourceType` once. Every
  function that mentions `T` contributes that declaration, identical duplicates
  dedupe, and the same key claimed by a different Rust type fails before any
  registry mutation.
- **Resource borrows cannot share the mutable VM.** A synchronous function cannot
  take both `&mut Vm` and a `ResourceRef`/`ResourceMut` parameter; the generated
  wrapper borrows the same VM. Use `ResourceOwned`, or split the work into two
  calls.
- **Documentation is required.** Every declaration needs a doc comment.

### Hidden host state

Per-VM and per-scope dependencies that guests must never see are declared as
hidden parameters. They do not count toward guest arity, never appear in the
schema or fingerprint, and are resolved through the generic host-state table:

```rust
#[pd_host_function(name = "re::match")]
pub fn re_match(
    cache: vm::HostStateMut<RegexCache>, // hidden: a WriteState<RegexCache> effect
    pattern: &str,
    text: &str,
) -> VmResult<bool> {
    cache.is_match(pattern, text)
}
```

`HostStateRef<T>` produces a read effect, `HostStateMut<T>` a write effect. Both
carry a lifecycle (per VM or per scope) and an initializer (required or lazy
default), which the module validates when it installs. Hidden state cannot be
combined with resource parameters in one function, and cannot cross an async
boundary — resolve it in a separate host call.

### Named structs

Rust signatures cannot spell field names. For a fixed-shape value, implement
`HostNamedStruct` and mark the declaration:

```rust
pub struct JitConfig;

impl vm::HostNamedStruct for JitConfig {
    const NAME: &'static str = "JitConfig";
    fn host_struct_fields() -> Vec<vm::HostStructField> {
        vec![vm::HostStructField::new("enabled", vm::HostTypeSchema::Bool)]
    }
}

#[pd_host_function(name = "jit::get_config")]
#[pd_host_named_struct]
pub fn get_config(vm: &mut Vm) -> VmResult<JitConfig> { /* ... */ }
```

The generated descriptor emits the full `Named { name, fields }` schema and the
module catalog derives the named struct from it. Do **not** model public host
request/result/event values as `Map(unknown)` or `unknown`; the
`tests/typed_host_no_dynamic_contract_tests.rs` guard fails the build if a public
standard function regresses to a dynamic schema.

## 3. Async functions and declared contracts

Standard I/O and SQLite use ordinary macro-generated async host functions. A
function may keep an internal raw `i64` execution-scope token while its explicit
contract exposes a typed guest resource. Put that contract on the same function
instead of duplicating a catalog entry.

Every value that crosses an `.await` must be owned. Implement
`CaptureAsyncHostContext` to copy or clone the required VM state before the VM
borrow ends, and use owned parameters such as `String`, `VmMap`, or
`VmArrayHandle`. The current `io::open` shape is representative (body details
are abbreviated):

```rust
#[derive(Clone)]
pub(crate) struct IoPolicyContext {
    policy: Option<IoPolicy>,
}

impl vm::CaptureAsyncHostContext for IoPolicyContext {
    fn capture(vm: &mut Vm) -> VmResult<Self> {
        Ok(Self { policy: io_policy(vm) })
    }
}

#[pd_host_function(name = "io::open", contract = super::io_open_contract)]
pub(crate) async fn builtin_io_open(
    #[pd_host_context] context: IoPolicyContext,
    path: String,
    mode: String,
) -> VmResult<vm::HostFutureOutput<i64>> {
    // Mode validation and `OpenOptions` setup are omitted here.
    let path = authorize_io_path(context.policy.as_ref(), &path, writes).await?;
    let file = options
        .open(path)
        .await
        .map_err(|error| VmError::HostError(format!("io_open failed: {error}")))?;
    let handle = IoHandle::File(BufReader::new(file));
    Ok(vm::HostFutureOutput::complete(move |vm| {
        let token = vm
            .execution_scope()
            .push_resource(IoResource::new(handle))
            .map_err(|error| VmError::HostError(format!(
                "io resource insert failed: {error}"
            )))?;
        Ok(token.into_handle().raw() as i64)
    }))
}
```

When context depends on a resource-token argument, implement
`capture_with_args(vm, args)` to validate the token and clone the owned resource
state before submission. `IoHandleContext`, `SqliteConnectionContext`, and
`SqliteCloseContext` use that form; the raw token parameter remains in the Rust
signature for contract binding but is not borrowed across suspension.

The generated wrapper captures `#[pd_host_context]` and converts ordinary
parameters before creating the future. It then calls `Vm::submit_host_future`,
which assigns an operation id and submits the boxed future to the embedding's
generic `HostAsyncBridge`. The bridge owns polling on its executor; the VM tracks
that submitted operation and resolves the result through the declared contract.
The embedding must keep the bridge's executor and runtime alive and driven until
its submitted futures finish. Process-backed I/O also requires a live Tokio
runtime while close or reset cleanup is polled, as detailed below.

Owned callable schemas, including bare function types such as
`VmCallable<fn(Event) -> Action>`, are accepted. Use
`HostFutureOutput::complete` when the resolved future must briefly re-enter the
VM to insert or retire a resource. When an async opening phase must hand control
to generic VM continuation machinery, return `HostFutureOutput<T>` and use
`HostFutureOutput::continue_with`; value mapping preserves that continuation.
The HTTP SSE builtin uses this only to transfer an opened Hyper response into
the generic callable-stream driver.

### Tokio runtime lease for process-backed IO

With the `async` feature, `io::popen` and `io::close` use Tokio process and pipe
types exclusively. The embedding Tokio runtime that spawns a process-backed IO
resource must remain alive and actively driven until `io::close` completes or a
VM reset has been polled to quiescence. This includes Windows process pipes: do
not shut the runtime down while a process pipe operation or process cleanup is
still active.

Process close and reset cleanup must be polled while a live Tokio runtime is
current. If reset cleanup is polled outside such a context, it returns the
retryable resource error `runtime_required` immediately. The child, its pipes,
the closing execution scope, and the pending reset remain owned by the VM; the
VM cannot be reused. Re-enter and drive the runtime, then poll the same reset
again. `Vm::clear_async_bridge` also rejects an execution scope with live
resources or operations, and rejects a pending reset, so the bridge can be
released only after scope quiescence.

`Vm`, process IO resource, and process IO handle `Drop` paths are strictly
nonblocking. They issue only immediate best-effort direct-child kill requests
and, on Unix, a process-group signal. They do not poll cleanup futures, wait for
the leader, or run Windows tree termination. Abrupt Drop therefore has no
eventual-reap guarantee. Embeddings that require deterministic tree termination
and leader reaping must finish explicit `io::close` or reset while the runtime
lease is valid.

What the contract does and does not change:

- The contract **replaces only the guest schema**. The adapter, binding class,
  async submission, and host-state effects still come from the one macro
  expansion — there is no second registration path to keep in sync.
- Guest resource effects are **derived from the contract schema**, so the raw
  token signature cannot drift from what the guest sees.
- The contract's declared name is validated against the function name at
  construction; a renamed function cannot silently keep a stale contract.
- Resource *declarations* stay with the module: implement `HostResourceType` once
  for the concrete type and list it in `HostModuleDescriptor::resources`. Then the
  resource key, its description, and its Rust type identity have exactly one
  source. Deriving the contract's key from that declaration (as above) keeps the
  two from drifting.


## 4. Installing a module

```rust
pub struct DemoExtension;

impl vm::HostExtension for DemoExtension {
    fn register(&self, registry: &mut vm::HostFunctionRegistry) -> VmResult<()> {
        demo_module().install(registry).map(|_| ())
    }

    fn install(&self, vm: &mut vm::Vm) {
        demo_module()
            .install_state_requirements(vm)
            .expect("descriptor state requirements install");
    }
}
```

Installation is transactional and fail-closed:

- The whole module is validated first — function schemas, resource declarations,
  named-struct bodies, binding/adapter agreement, and hidden host-state
  requirements. A later failure rolls back every adapter the call installed.
- Conflicting resource keys or conflicting state providers fail before the
  registry changes at all.
- `install_from_catalog(registry, catalog)` is the descriptor path for embedders
  that compose their own catalog or a subcatalog: every descriptor must match
  exactly one import in the supplied snapshot (same name, labels, schemas,
  passing modes, return type), the adapters stay the module's own, and each
  installed import is granted its capability. A registry built with
  `HostFunctionRegistry::restricted()` keeps denying every import the module does
  not declare.

## 5. Owned dispatch

Functions whose arguments are *transferred* rather than borrowed (timer callbacks
are the standard example) use owned dispatch. Descriptors express it with
`HostBindingKind::Owned` and a `'static` factory:

```rust
struct RegisterTimerFactory { repeating: bool }

impl vm::HostOwnedAdapterFactory for RegisterTimerFactory {
    fn create(&self, context: vm::OwnedHostContext<'_>) -> Box<dyn vm::HostOwnedFunction> {
        Box::new(RegisterTimer { registry: context.registry().clone(), repeating: self.repeating })
    }
}

static TIMER_AT_FACTORY: RegisterTimerFactory = RegisterTimerFactory { repeating: false };
```

`OwnedHostCall::take_arg(index)` drains and transfers the operand; the factory
receives the immutable registry and binding configuration it needs to spawn an
isolated owned-value execution VM. The owned path keeps its guarantees: exact
operand draining, callback provenance, isolated callback VM, and transactional
owned-resource transfer. Do not route an owned function through borrowed or
static dispatch.

## 6. Aggregation and feature gates

A build composes modules through one deterministic list. The standard library's
aggregation is the reference implementation
(`src/builtins/runtime/host_modules.rs`):

```rust
let mut modules: Vec<StandardHostModule> = vec![ /* always-present modules */ ];
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
modules.push(super::http::http_host_module());
modules.push(super::io::io_host_module());
#[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
modules.push(super::sqlite_schema::sqlite_standard_host_module());
modules.push(super::timer::timer_host_module());
modules.sort_by_key(|module| module.name);
```

Each module declares two things:

- `catalog` — the guest catalog **surface** it publishes. Derive it from the
  module's descriptors; a module with no guest surface returns an empty surface.
- `owned` — **every** descriptor the module owns, including functions still
  dispatched through the generated namespaced-builtin path.

`tests/standard_host_descriptor_arch_tests.rs` checks the composed descriptors
and catalogs directly: module names are unique and deterministically ordered,
every published function belongs to its module's ownership list, and the merged
catalog preserves the expected module and aggregate fingerprints. It also checks
that resource declarations and named structs follow the selected modules, public
returns stay typed, and native-only HTTP and SQLite surfaces do not appear when
their build gates are off.

## 7. Compatibility window

Still public and supported, for downstream migration:

- `HostApiBuilder::{resource, named_struct, function}`.
- `HostFunctionRegistry::register`, `register_static`, `register_stack`,
  `register_static_stack`, `register_args`, `register_static_args`,
  `register_static_non_yielding_args`, and the catalog/exact-family
  registrations, including `register_exact_owned`.
- The per-module `register_*_builtin_module{,_from_catalog}` entry points.

Preferred for new code:

- `HostModuleDescriptor` + `install` / `install_from_catalog`,
  `HostFunctionDescriptor`, `HostResourceType`, `HostOwnedAdapterFactory`.

Removal threshold: the legacy builder and low-level registry APIs are removed
only after every repository in the organization migration matrix has passed its
gate against a frozen core SHA. Until then, legacy catalogs compose with
descriptor modules through `install_from_catalog`.

## 8. Before and after

**Before** (one function, four places to keep in sync):

```rust
// 1. the runtime adapter
#[pd_host_function(name = "demo::read_counter")]
fn read_counter(vm: &mut Vm, handle: i64) -> VmResult<i64> { /* ... */ }

// 2. a hand-written resource entry
builder.resource(ResourceTypeSchema::new(counter_key(), "A monotonic counter"));

// 3. a hand-written function schema
builder.function(HostFunctionSchema::with_return(
    "demo::read_counter",
    vec![HostParamSchema::with_passing(
        "counter",
        HostTypeSchema::Resource(counter_key()),
        HostParamPassing::Borrow,
    )],
    HostTypeSchema::Int,
));

// 4. an adapter table plus an exact registration
const ADAPTER_CONTRACTS: &[AdapterContract] = &[AdapterContract {
    name: "demo::read_counter",
    arity: 1,
    adapter: read_counter_adapter,
}];
registry.transactionally(|staged| { /* validate, register, authorize */ })
```

**After** (one function, one declaration, one module list):

```rust
#[pd_host_function(name = "demo::read_counter")]
fn read_counter(counter: ResourceRef<'_, Counter>) -> VmResult<i64> {
    Ok(*counter as u64 as i64)
}

pub fn demo_module() -> HostModuleDescriptor {
    HostModuleDescriptor {
        name: "demo",
        functions: &[read_counter_descriptor],
        resources: &[],
    }
}

demo_module().install(registry)?; // validates and installs transactionally
```

The wrapper, the schema, the resource declaration, the binding class, the
effects, and the exact registry entry all come from the single declaration.
