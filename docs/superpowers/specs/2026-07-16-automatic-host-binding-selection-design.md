# Automatic host binding selection design

## Goal

Make generated default host bindings select the most capable safe VM binding from the annotated Rust function signature. In particular, a function such as `fn foo(x: i64) -> i64` must bind through `bind_static_non_yielding_args_function` so native JIT traces may continue across the call.

## Scope

This change covers `#[pd_host_function]` implementations discovered by the RustScript build generator and the equivalent `HostFunctionRegistry` cached-binding path. Dynamic trait-object bindings remain explicit public APIs because a plain annotated function cannot supply a factory or stateful trait object.

## Signature classification

Introduce one shared build-time classification with these ordered rules:

1. A function with a `Vm` context parameter uses `StaticStack`.
2. An args-only function whose normalized return type is `CallOutcome`, including `VmResult<CallOutcome>` and `HostResult<CallOutcome>`, uses `StaticArgs`.
3. Every other valid args-only annotated function uses `StaticNonYieldingArgs`.

The third rule is safe because the generated wrapper converts all supported ordinary outputs through `IntoVmValue` into exactly one `Value`. This includes implicit `()`, explicit `()`, and `Option<T>`, which become `Value::Null` when appropriate. `VmResult<T>` and `HostResult<T>` may still return an error; successful calls return exactly one value synchronously.

`CallOutcome` stays on the general static args ABI because it can represent no return value, halt, yield, or pending work. Any signature that cannot be classified safely falls back to the general compatible static binding.

## Generated binding paths

The build generator will use the same classification for both generated surfaces:

| Kind | Registry generation | Direct VM generation |
|---|---|---|
| `StaticStack` | `register_static_stack` | `bind_static_stack_function` |
| `StaticArgs` | `register_static_args` | `bind_static_args_function` |
| `StaticNonYieldingArgs` | `register_static_non_yielding_args` | `bind_static_non_yielding_args_function` |

This keeps `HostFunctionRegistry::bind_vm_cached` and direct default-host binding behavior equivalent.

## Registry support

Extend `HostFunctionRegistry` with a static non-yielding args entry and public registration method. Binding plans will preserve that entry kind and install it through `Vm::register_static_non_yielding_args_function`.

Re-registration under an existing name replaces the entry kind and invalidates the plan cache, matching the current registry behavior for other host ABI kinds.

## Tests

Add focused tests for:

- signature classification for plain values, implicit and explicit unit, options, fallible values, `CallOutcome`, wrapped `CallOutcome`, and VM-aware functions;
- generated registry and direct-bind code selecting the expected method;
- registry cached binding preserving the non-yielding host kind;
- execution of a representative typed host function;
- native JIT tracing through a generated or registry-bound non-yielding args host call when the JIT feature is enabled.

Run the narrow tests first, then the relevant workspace tests, formatting, Clippy, and build checks without touching unrelated worktree changes.
