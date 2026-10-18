# Result values and function-local propagation

`Result<T, E>` represents a typed success value (`T`) or a typed guest error
(`E`). Both parameters are required. It is a compiler built-in, not an ordinary
map type or an exception mechanism.

## Construct, inspect, and match

```rss
// expected: 8
fn choose(flag: bool, n: int) -> Result<int, string> {
    if flag => { Result::Ok(n) } else => { Result::Err("bad input") }
}
let value: Result<int, string> = choose(true, 7);
let success = value.is_ok();
let answer = match value {
    Result::Ok(n) => n + 1,
    Result::Err(message) => -1,
};
answer;
```

`is_ok()` and `is_err()` inspect the Boolean discriminant without consuming
an owned payload. A match evaluates its input once and runs only the selected
arm. `Ok(false)`, `Err(false)`, `Ok(0)`, `Err(0)`, and nullable payloads remain
distinct; payload truthiness never selects a branch.

A complete match has one `Result::Ok(binding)` arm and one
`Result::Err(binding)` arm, or a wildcard covering the missing branch. Duplicate,
missing, and unreachable arms are rejected. Nested matches are supported; general
nested patterns and guards are not part of this interface.

A declaration, function return, or typed argument can supply the inactive
constructor parameter. Bare `Result::Ok(7)` without enough type context is
rejected. There is no implicit wrapping of `T`, default error parameter, error
conversion, or conversion from an arbitrary guest map. `Result` cannot be
redeclared as a user or host type. Parameters are invariant. Result can be nested
inside arrays, optional values, named structs, generic types, and callables.

## `?` returns from the current function

```rss
// expected: 9
fn inner(flag: bool) -> Result<int, int> {
    if flag => { Result::Ok(7) } else => { Result::Err(9) }
}
fn relay(flag: bool) -> Result<string, int> {
    let number = inner(flag)?;
    Result::Ok("continued")
}
let answer = match relay(false) {
    Result::Ok(text) => 0,
    Result::Err(code) => code,
};
answer;
```

The operand is evaluated once. Ok transfers its payload to the surrounding
expression. Err transfers the original error into the enclosing function's
Result and returns through that function's normal return continuation. Later
arguments, statements, and other unexecuted sibling expressions do not run.
A closure's `?` exits that closure, not its caller; recursion and callable aliases
use the same lexical boundary. The enclosing Ok type may differ from the operand
Ok type; the error type must match without implicit conversion.

Root scripts, non-Result-returning functions, non-Result operands, optional Result
operands, and incompatible error types reject `?`. `T?` remains an optional type,
and `object?.field` remains optional access. Other parser dialects do not gain
this syntax unless they enable the Result dialect capability. There are no
`unwrap`, `map`, `and_then`, generic Try traits, or guest throw/catch/finally here.

## Ownership and cleanup: explicit normal close, automatic Err cleanup

A Result containing a resource in either parameter is move-only. Construction
moves the active payload. Match and `?` consume the old container and transfer
only the selected owner. Borrow checks, kind, arena, generation, catalog, and
Program identity remain in force; the discriminant grants no resource authority.

**Only the Err edge of `?` installs automatic remaining-local cleanup.** The
returned Err payload is first preserved as the returned owner; other remaining
function-local resources are closed by the recorded cleanup plan. Cleanup failure
is a fatal VM failure, not a replacement guest Err.

**Ordinary Ok continuation retains the existing explicit-close contract.**
Explicitly close or transfer resources on normal execution. A fresh resource
created by a guest host call is not automatically closed merely because the
function later returns Ok. This is not general function-exit RAII.

VM-aware owned callback *arguments* have a separate publication journal. Abandoned
argument publications are reclaimed on failure or success, while returned owners,
surviving guest captures, and resources transferred by a typed native take are
preserved. A fresh guest-host local outside that argument journal still needs an
explicit normal-path close. Opaque native state must take ownership through the
typed API; storing a numeric handle or an externally held container is not an
ownership declaration. Unresolved publication ownership quarantines execution and
new publications until explicit scope shutdown/reset actually completes.

The maintained generated-host example in
[`result_sdk_try_integration_tests.rs`](../tests/result_sdk_try_integration_tests.rs)
uses `ResourceOwned<T>`, `ResourceRef<'_, T>`, `HostResourceType`,
`vm_script_callback_by_name`, and `call_with_vm`. It checks resource state **before**
VM teardown and exercises normal explicit close and Err cleanup independently.

## Guest errors and VM errors are different channels

In a descriptor-generating extension's `functions` module (with the SDK's
usual parent-module imports; see the [descriptor guide](host-sdk-descriptors.md)):

```rust
use pd_host_function::pd_host_function;
use vm::{GuestResult, VmError, VmResult};

/// Returns a guest value or a fatal transport failure.
#[pd_host_function(name = "example::read")]
fn read(mode: i64) -> VmResult<GuestResult<i64, String>> {
    match mode {
        0 => Ok(GuestResult::Ok(7)),
        1 => Ok(GuestResult::Err("missing".into())),
        _ => Err(VmError::HostError("transport failed".into())),
    }
}
```

`GuestResult<T, E>` is the public Rust data wrapper. Outer `VmResult` retains its
existing meaning. Fuel/epoch suspension, cancellation, pending host work, quotas,
invalid representation, and outer `VmError` never become catchable guest Errs.
Host descriptors and build-script schema generation preserve both parameters and
resource effects. Borrowed-resource returns remain rejected, including nested
GuestResult combinations. Existing HTTP/database business return types are not
implicitly migrated.

Ordinary context-free callbacks use `IntoScriptValue` / `ScriptResult` and
`script_callback_by_name`. Resource-owning callbacks use `VmScriptValue`,
`VmScriptArgs`, and a live `ScriptValueContext` through the VM-aware callback API.
Use the generated descriptor's exact catalog; host-call admission validates the
selected payload before entering a guest body, and typed host return validation
runs before the guest continues. Callback handles are Store/VM/Program-bound and
are invalidated by replacement/reset as specified by that API.

## Frontends, wire, and execution

Plugins can construct `TypeSchema::Result { ok, err }`,
`HostTypeSchema::Result { ok, err }`, and typed `Expr::ResultConstruct`,
`ResultMatch`, and `ResultTry` IR. Preserve the full schemas through typing and
ownership passes; do not substitute Unknown or invent a named-map identity.

The current VMBC contract is **v13 only**. Std writer/reader and no-std reader use
one current schema layout. Non-v13, unknown schema tags, malformed carriers, and
invalid schema budgets are rejected; there is no legacy reader, adapter, or
version-dependent writer. The physical carrier is the existing Map with a Boolean
`tag` (`true` = Ok) and the active `payload`. Missing payload means Null only if
the selected schema permits it. Missing/non-Boolean tag, extra fields, or a wrong
selected payload fail validation. Raw map construction does not establish guest
Result typing.

Interpreter, Trace JIT, and AOT use existing instructions and the existing Value
and native ABI. Logical payload type alone does not establish a native scalar
representation: generic Map reads can remain Tagged and require checked unboxing.
Dynamic payload tests cover integer, signed zero/NaN, false, string, optional/null,
nested Result, callables/aliases, and owned resources. Native call/collection
helpers and interpreter continuations may still be involved. In particular,
resource move helpers and an Err cleanup Ret can cross an interpreter boundary;
native tag/payload execution is not a claim that every instruction is native.
Standalone host-root callable execution respects the configured backend.
AOT still rejects some unhinted Tagged arithmetic, including addition in a
captured generic Result closure when scalar operand facts are unavailable.
A forwarding captured-closure vector has separate actual native coverage; its
success does not establish native execution of that addition workload. Existing
optional-index String reads and other unsupported operations retain their own
capability limits.

AOT artifacts remain bound to the checked Program and schemas. Loading can rebuild
current-process native code; loading is not zero-cost relocation. Reset/reuse,
Program replacement, stale callbacks, and cross-VM rejection retain their lifecycle
contracts.

No-std executes std-encoded scalar/nested Result and cleanup-free `?` programs.
Resource cleanup plans that the no-std runtime cannot honor return
`UnsupportedCapability`; this is not general no-std resource support. Wasm32
analyzer exports preserve recursive Result schemas and provenance; runtime builds
execute guest branches through the exported JSON ABI. Native analyzer tests,
wasm32 compilation, and actual wasm ABI execution are separate checks.

Formatter/type hints preserve constructors, matches, and `?`. ResultMatch
validation diagnostics currently extend to the following token in some cases;
`?` mismatch diagnostics retain their dedicated location. Do not depend on
ResultMatch diagnostics covering only the match expression.

## Costs and reproducible characterization

Result is **not zero-cost or allocation-free** in this implementation. Escaping
carriers allocate Maps. Constructor/match lowering adds tag/payload operations and
sometimes temporary locals; `?` adds a branch and function-exit work. Native
helper/checkpoint transitions can cost more than interpreting a small workload.
No speculative scalar replacement is part of this interface.

`tests/result_cost_tests.rs` is a finite opt-in harness comparing Result with a
manual Boolean-tag/payload `Pair` Map and a clearly separate no-Result integer
control. Direct controls record instruction/branch/local counts, assert that
removing the unused carrier declaration leaves their emitted code and local
counts unchanged, and check that no Map construction was introduced. This is a
same-compiler control, not a historical wire-byte compatibility test.
It measures construction/match, function returns, and looped `?` success
and Err exits with dynamic host inputs. All three use the same integer identity
boundary; the manual error loop uses an explicit Boolean stop condition because
RustScript has no general source return statement. Those control-flow differences
are part of the comparison, not instruction-for-instruction equivalence.

```sh
cargo test --locked -p pd-vm --test result_cost_tests \
  measured_result_map_direct_costs -- --ignored --exact --test-threads=1 --nocapture
```

The harness emits every sample, compilation time, bytecode/native code size,
requested allocation counts/bytes, live-byte deltas, RSS, and real native counters.
It uses three same-VM warmups and four alternating rounds, excludes compilation,
reset and assertions from execution timing, and checks cache inventories after
warmup. The default debug profile and same-machine load make these characterization
numbers, not optimized-release throughput claims. The direct control has no
Result carrier; it still includes ordinary VM/host/call costs. Allocator requested
bytes are not total process memory or allocator overhead. Report medians/ranges
and all samples rather than a single speedup or a universal allocation claim.
