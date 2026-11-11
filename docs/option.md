# Option and Unit

RustScript has a logical `Option<T>` type and a logical Unit type `()`. They are
distinct from the VM's internal Null value and from the legacy nullable
`T | null` schema used by older Rust host `Option<T>` parameters.

## Option values

```rust
let some: Option<int> = Option::Some(0);
let none: Option<int> = Option::None;

let value = match some {
    Option::Some(inner) => inner,
    Option::None => -1,
};
```

`Option::Some` may infer its payload type when the payload itself has a fully
known schema. `Option::None` has no payload from which to infer `T`, so it
requires an `Option<T>` annotation or another contextual type. Missing type
arguments are rejected instead of being invented as `Unknown`.

A match on a logical Option must provide `Option::Some` and `Option::None`; the
two arms are exhaustive. A wildcard arm may stand in for `Option::None` when the
Some arm is present. Duplicate arms, mixing Result construction arms into the
same match, and ambiguous wildcard combinations are rejected.

`None`, `Some(None)`, `Some(Some(0))`, `Some(())`, `Some(false)`, `Some(0)` and
`Some("")` are different values. Payload truthiness, zero, the empty string and
the presence of a raw Null never choose the variant.

## Option carriers

The logical schema, not the physical shape, defines an Option. At the wire and
runtime boundary an Option is a map with a Boolean `tag`:

- `tag: true` is `Some`; the active value is stored under `payload`.
- `tag: false` is `None`; a None carrier has no active `payload` entry.
- A non-Boolean or missing tag, extra fields, and a payload on None are
  malformed and are rejected by validation.

`Some(())` uses the internal Null value as its payload. A canonicalizing writer
may omit a Null-valued payload entry, so a tag-only `Some` may be restored only
when the declared payload schema is the logical Unit. A missing payload under
any other payload schema is malformed rather than an inner None.

## Unit

The Unit type and value are spelled `()`. An empty block, a function or closure
body with no tail value, and a typed host function with no declared return type
carry logical Unit facts. Unit's physical carrier is the internal Null value, so
`()` remains compatible with existing low-level code that represents a
missing value as Null, but an explicit source `null` is not accepted as Unit and
is not inferred into Unit or Option.

```rust
fn nothing() { }

let unit: () = ();
let value = nothing();     // logical Unit
```

## Host functions

The typed host boundary uses `GuestOption<T>` for a guest Option. It converts
bidirectionally with the standard Rust `Option<T>` by variant:

```rust
use vm::{GuestOption, VmResult};

#[pd_host_function(name = "example.lookup")]
fn lookup(key: i64) -> VmResult<GuestOption<String>> {
    Ok(if key == 0 {
        GuestOption::Some(String::new())
    } else {
        GuestOption::None
    })
}
```

`GuestOption` is the public guest wrapper; the low-level standard Rust
`Option<T>` host codec keeps its existing single-mode nullable behavior
(`None` is Null, `Some(value)` is the bare value). `GuestOption` is not an
optional argument marker: a logical `None` is an explicit value and is not the
same as an omitted parameter.

The wrapper recursively supports scalar, Named-struct, array, logical Unit,
`GuestResult`, nested `GuestOption`, typed callable payloads, and legal
owned-resource payloads. A callable payload keeps its full function signature
(parameter and result schemas) under `GuestOption`/`GuestResult`; the callable
value itself owns no resource. `fn()` and `fn() -> ()` are the same Rust
function type, so an omitted return publishes the logical Unit result rather
than Null. A resource array is not supported by the host handle ABI and is
rejected with a capability error; dynamic map/unknown payloads and transport
results such as `VmResult` inside the payload are also rejected.

A host function that returns no value has the logical Unit return schema, while
the call still supplies an internal Null payload. `VmResult<()>` keeps fatal
transport errors in the outer result, as with every typed host function.

## Resources

An Option may own a resource inside `Some`. None has no owner. A selected Some
resource is published, transferred, taken or closed once through the existing
live-VM publication journal; unselected branches do not acquire ownership.
Missing payload, stale handles, wrong owner/kind and failed publication follow
the ordinary resource admission and rollback rules.

## Wire format

The current VMBC version is 14 and there are no legacy readers, adapters or
version-dependent writers. Host schemas encode logical Option with tag 15 and
Unit with tag 16; compiler schemas use tag 19 for Option and tag 20 for Unit.
Existing tag numbers and builtin IDs are unchanged.

The no-std decoder understands the same current Option and Unit schemas for
scalar and nested programs. A cleanup plan that owns a resource still exceeds
the current no-std resource capability and is refused before execution.

## Option methods

`Option<T>` provides `map`, `and_then`, `or_else`, `unwrap_or`,
`unwrap_or_else`, `ok_or` and `ok_or_else`.

```rust
let some: Option<int> = Option::Some(2);
let absent: Option<int> = Option::None;

let mapped = some.map(|value| value + 1);          // Option<int>, wraps U
let flat = some.and_then(|value| Option::Some(value + 1)); // flattens one layer
let kept = absent.or_else(|| Option::Some(3));     // zero-argument lazy fallback
let eager = some.unwrap_or(0);                     // eager fallback, returns int
let lazy = absent.unwrap_or_else(|| 4);            // lazy body, returns int
let as_result = absent.ok_or("missing");           // Result<int, string>
let lazy_error = absent.ok_or_else(|| "missing");  // lazy error callback
```

`map` wraps its callback result even when it is itself an Option or Result.
`and_then` flattens exactly the explicitly returned Option; a bare payload or a
Result return rejects. `or_else` takes a zero-argument callback and keeps the
receiver payload type; it does not copy the Result error-argument signature.
`unwrap_or` and `ok_or` evaluate their arguments eagerly; `unwrap_or_else` and
`ok_or_else` run their callback body lazily only on None. Receiver and argument
creation follow normal left-to-right order, each exactly once.

`ok_or`/`ok_or_else` produce `Result<T, E>` and do not implicitly convert
between Option and Result. An eager fallback or error value that may own a
resource is rejected when the unselected branch has no destination for it. A
resource-capturing temporary callback is rejected for the same reason.

Result values expose explicit conversions:

```rust
let value: Result<int, string> = Result::Ok(21);
let ok_value = value.ok();   // Option<int>
let err_value = value.err(); // Option<string>
```

`ok`/`err` retain the selected payload as `Some`; the discarded branch must be
resource-free or the conversion is rejected statically.

## Lexical Option propagation

`?` on a logical Option passes the Some payload and exits only the current
Option-returning function or closure on None. Later arguments, statements and
side effects do not run after a None exit; already-created owners follow their
declared cleanup. A None inside a closure does not return from its caller.

```rust
fn relay(flag: bool) -> Option<int> {
    let value: Option<int> = if flag => { Option::Some(1) } else => { Option::None };
    let item = value?;
    Option::Some(item + 1)
}
```

Result and Option propagation do not implicitly mix. Use `.ok_or(...)?` or
`.ok()?` at the boundary; Result `?` keeps its exact error type. Option `?`
accepts a wider enclosing Option payload because None carries no value, while
Some continues with its exact payload type.

## Fallible projection (`?.`)

RustScript `?.` produces a logical `Option` of the projected value. A missing
map key, a missing array index, an out-of-range string index or a None receiver
becomes None. A present value is Some, including `Some(false)`, `Some(0)`,
`Some("")`, `Some(())` and a nested `Some(None)`.

A non-optional receiver performs one fallible projection. An Option receiver
consumes exactly one and_then-equivalent receiver layer; the projected payload
keeps its own schema, so a declared `Option<int>` leaf produces
`Option<Option<int>>` rather than an arbitrary flattening. An absent dynamic key
and a present key holding explicit `Option::None` remain distinct.

Permitted omitted logical Option fields normalize to None at the RustScript
boundary, while required fields are still checked. An explicit Option::None
field is a tagged value, not a map deletion; internal raw Null deletion and the
legacy nullable contract are unchanged until the final no-null boundary work.

## Deliberate limits

This scope does not add `is_some`, `inspect`, `map_or` or other unrequested
catalogue operations. RSS null literals, legacy nullable annotations, JSON/Core
admission and standard host migration remain separate later boundary work.
