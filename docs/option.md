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
value itself owns no resource. A resource array is not supported by the host
handle ABI and is rejected with a capability error; dynamic map/unknown payloads
and transport results such as `VmResult` inside the payload are also rejected.

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

## Deliberate limits

This foundation does not provide Option combinators (`map`, `and_then`, ...) or
the Option `?` operator, and it does not change Result `ok`/`err`, JSON,
`type_of` or display rendering. Code that requires an unsupported operation on a
logical Option is rejected rather than routed through the legacy nullable path.
