# pd-vm Bytes And Binary Host API Plan

## Summary

Add a real native `bytes` value type to `pd-vm`, then normalize RSS stdlib and `pd-edge` host APIs
around byte-accurate binary IO instead of routing protocol traffic through UTF-8 strings or base64
text wrappers.

## Status Snapshot (2026-03-20)

Implemented in the current workspace:

- native `bytes` value support landed end to end through VM values, VMBC, typing, validation,
  compile diagnostics, completion metadata, wasm analyzer output, and runtime builtins
- dedicated `b"..."` bytes literals are implemented in the lexer, parser, IR, formatter, and
  compiler pipeline
- zero-copy borrowing landed for read-only host or builtin byte access paths such as `&[u8]`
  extraction from VM `bytes` values
- binary-first host APIs now use native `bytes` across the main transport surfaces and MQTT gained
  `publish_binary`
- base64 helper APIs remain available as explicit compatibility helpers, but native `bytes` is the
  primary path
- Monaco syntax highlighting, the `pd-controller` Monaco copy, and the VS Code TextMate grammar all
  understand `b"..."` literals and `\xHH` escapes
- the shared RSS `bytes` stdlib exists and the examples that actually benefit from binary data
  handling, such as the complex, crypto, and binary protocol examples, were rewritten to use
  native `bytes`

Still not done or intentionally deferred:

- full AOT support and AOT verification for the new `bytes` work are intentionally not being
  carried forward because that path is being removed
- bit-level arithmetic or bitstream builtins are still out of scope
- implicit coercions between `string`, `bytes`, and `array` are still intentionally not supported
- examples are intentionally not being forced to include `bytes` when binary data is not part of
  the example's purpose

Milestone status:

- Milestone 1 is effectively complete
- Milestone 2 is effectively complete after the stdlib follow-up cleanup
- Milestone 3 is effectively complete for interpreter/JIT/runtime paths, with AOT intentionally
  skipped
- Milestone 4 is partially complete: examples and protocol samples were updated, but the full docs
  and integration-test sweep is still incomplete

The key design constraint is that binary protocol work must not be modeled as:

- "strings that happen to hold bytes"
- "arrays of ints everywhere with no native runtime support"
- "base64 text as the main binary transport contract"

Recommended conceptual layering:

- `string` = Unicode text
- `bytes` = opaque byte sequence
- `array` = general heterogeneous VM collection
- transport host APIs = binary-first for packet and frame protocols

That keeps text semantics, binary semantics, and general collection semantics separate instead of
forcing protocol code into the wrong abstraction.

## Goals

- Add native `bytes` to the VM value model, type system, wire format, runtime, and debugger tools.
- Make `bytes` work end to end through parser typing, type inference, validation, lint output,
  compile diagnostics, completion metadata, and wasm/web UI tooling rather than only at runtime.
- Keep `bytes` distinct from `string` and `array`.
- Extend core language operations so `bytes` behaves like a first-class sequence where that is
  semantically correct.
- Add a shared [`stdlib/rss/bytes.rss`](../stdlib/rss/) module with ergonomic helpers
  for byte-sequence work.
- Add native builtin conversions for UTF-8, hex, and base64 so RSS programs do not reimplement
  codecs in script.
- Normalize binary host IO around `bytes` across `tcp`, `udp`, `websocket`, and `webrtc`.
- Keep compatibility wrappers such as base64-oriented binary calls available only as deprecated
  adapters rather than as the primary binary contract.
- Unblock protocol implementations such as MQTT brokering in RSS without forcing packet parsing
  through lossy text conversions.

## Still Deferred / Out Of Scope

- Full bit-level builtin arithmetic or bitstream APIs
- Automatic implicit coercions between `string`, `bytes`, and `array`
- Full AOT support for the `bytes` runtime path
- Forcing every example in the repo to include `bytes` even when the example is not about binary
  data

## Historical State Before Bytes Landed

The current VM value model only has `Null`, `Int`, `Float`, `Bool`, `String`, `Array`, and `Map`
in [`src/bytecode.rs`](../src/bytecode.rs).

Important current consequences:

- there is no native `bytes` type in the VM or the type system
- the edge ABI only knows `Unknown`, `Null`, `Int`, `Float`, `Bool`, `String`, `Array`, and `Map`
  in [`pd-edge-abi/src/lib.rs`](../../pd-edge/pd-edge-abi/src/lib.rs)
- generic string operations are Unicode-character oriented rather than byte oriented:
  [`len`](../src/builtins/runtime/core.rs),
  [`slice`](../src/builtins/runtime/core.rs), and
  [`get`](../src/builtins/runtime/core.rs) all operate on `chars()`
- shared RSS stdlib modules exist for strings and collections under
  [`stdlib/rss/`](../stdlib/rss/), but there is no `bytes.rss`
- lint, inferred-type, and completion surfaces therefore have no way to report `bytes` as a
  distinct language type today

The transport/runtime surface is currently mixed:

- `tcp::stream::{read_binary, peek_binary, write_binary}` and
  `websocket::connection::{send_binary, read_binary}` already exist in the current workspace, but
  they are modeled as `Array`-shaped VM values instead of native `bytes`
- `udp::socket::*` still exposes binary datagrams only as base64 strings in
  [`pd-edge-abi/src/abi_spec/udp.rs`](../../pd-edge/pd-edge-abi/src/abi_spec/udp.rs)
- `webrtc::connection::*` still exposes binary messages only as base64 strings in
  [`pd-edge-abi/src/abi_spec/webrtc.rs`](../../pd-edge/pd-edge-abi/src/abi_spec/webrtc.rs)
- text-oriented TCP reads still pass raw bytes through `String::from_utf8_lossy(...)` in
  [`pd-edge/src/abi_impl/transport/tcp.rs`](../pd-edge/src/abi_impl/transport/tcp.rs)

So the current answer to "can RSS implement packet protocols cleanly today?" is: only partially.
Binary work is possible in small slices, but the runtime still lacks a stable first-class `bytes`
contract and consistent binary-first host APIs.

## Recommended End State

### 1. Add native `bytes` as a first-class VM value type

Recommended VM model:

- `Value::Bytes(SharedBytes)`
- `ValueType::Bytes`
- `AbiValueType::Bytes`
- `AbiParamType::Bytes`

Recommended runtime storage:

- `SharedBytes = Arc<Vec<u8>>`

Recommended semantic rules:

- `bytes` compares by byte content, not by heap identity
- `bytes` may be used as a map key by value, similar to `string`
- `bytes.length` means byte length, not character count
- indexing a `bytes` value returns an `int` in `0..=255`
- slicing a `bytes` value returns another `bytes`
- concatenating `bytes + bytes` returns `bytes`

Important rule:

- `bytes` must not silently alias `string`
- `bytes` must not silently alias `array`
- explicit conversions remain required at text or collection boundaries

### 2. Extend the core language runtime to understand `bytes`

The generic core builtins in [`src/builtins/runtime/core.rs`](../src/builtins/runtime/core.rs)
should treat `bytes` as a first-class sequence alongside strings and arrays where appropriate.

Recommended behavior:

- `len(bytes)` returns byte length
- `slice(bytes, start, length)` returns `bytes`
- `concat(bytes, bytes)` returns `bytes`
- `get(bytes, index)` returns `int`
- `has(bytes, index)` returns whether the byte offset exists
- `type(value)` returns `"bytes"` for byte values

Recommended non-behavior:

- do not allow `concat(string, bytes)` or `concat(bytes, string)`
- do not implicitly decode bytes in generic `to_string(bytes)`

For `to_string(bytes)`, prefer a diagnostic representation such as a short hex preview or
`bytes[len=N]` style output rather than lossy UTF-8 decoding.

### 3. Add a dedicated `bytes` builtin namespace for native conversions

The codec-heavy conversions should be builtins, not pure RSS helpers.

Recommended builtin namespace:

- `bytes::from_utf8(text) -> bytes`
- `bytes::to_utf8(payload) -> string`
- `bytes::to_utf8_lossy(payload) -> string`
- `bytes::from_hex(text) -> bytes`
- `bytes::to_hex(payload) -> string`
- `bytes::from_base64(text) -> bytes`
- `bytes::to_base64(payload) -> string`
- `bytes::from_array_u8(values) -> bytes`
- `bytes::to_array_u8(payload) -> array`

Recommended validation rules:

- `from_utf8` encodes the given string as UTF-8 bytes
- `to_utf8` fails on invalid UTF-8
- `to_utf8_lossy` is explicit and may replace invalid sequences
- `from_hex` rejects malformed hex or odd-length payloads
- `from_base64` rejects malformed base64
- `from_array_u8` rejects non-int elements and out-of-range values
- `to_array_u8` returns an array of ints in `0..=255`

This keeps `bytes` native while still providing an explicit compatibility bridge to array-backed
code during migration.

### 4. Add `bytes.rss` as the ergonomic RSS layer

Add [`stdlib/rss/bytes.rss`](../stdlib/rss/) as the shared high-level helper module.

Its public API should include:

- `bytes::empty()`
- `bytes::append(lhs, rhs)`
- `bytes::slice(buf, start, len)`
- `bytes::starts_with(buf, prefix)`
- `bytes::ends_with(buf, suffix)`
- `bytes::index_of(buf, needle)`
- `bytes::equal(lhs, rhs)`
- `bytes::push_u8(buf, value)`
- `bytes::read_u8(buf, offset)`
- `bytes::read_u16_be(buf, offset)`
- `bytes::read_u16_le(buf, offset)`
- `bytes::write_u16_be(value)`
- `bytes::write_u16_le(value)`

Recommended module design:

- thin wrappers around native `bytes` operators or builtins where possible
- pure RSS sequence helpers for prefix or suffix search and small-endian/big-endian field access
- no hidden text decoding

Recommended migration helpers:

- `bytes::from_array_u8(values)` should forward to the builtin
- `bytes::to_array_u8(payload)` should forward to the builtin

That preserves compatibility for existing array-based binary code while keeping the public direction
focused on native `bytes`.

### 5. Normalize binary-first transport host APIs onto `bytes`

All transport-family binary calls should return or accept native `bytes`, not `string` or `array`.

Recommended target APIs:

- `tcp::stream::{read_binary, peek_binary, write_binary, read_exact_binary}`
- `udp::socket::{send_binary, recv_binary}`
- `websocket::connection::{send_binary, read_binary}`
- `webrtc::connection::{send_binary, read_binary}`

Recommended compatibility policy:

- keep existing base64 helpers such as `*_binary_base64` for compatibility
- implement them as wrappers over the native `bytes` path internally
- do not freeze new binary APIs around `Array` once native `bytes` exists

Recommended transport behavior:

- TCP binary APIs are byte-stream operations
- UDP binary APIs preserve datagram boundaries
- WebSocket and WebRTC binary APIs preserve message boundaries
- `read_exact_binary` should loop until the requested byte count is satisfied or EOF/failure occurs

### 6. Keep binary DAG ownership explicit in `pd-edge`

This plan is about bytes, not about collapsing protocol DAGs.

Recommended modeling rule:

- `bytes` is the VM data representation for opaque payloads
- carrier DAGs still own transport semantics
- higher-level protocols such as MQTT still attach to `tcp`, `tls`, `websocket`, or `webrtc`
  according to their own DAG rules

Moving from `string` or `array` to `bytes` should improve binary payload fidelity without changing
protocol ownership boundaries.

### 7. Preserve source compatibility carefully

The migration should be explicit rather than magical.

Recommended compatibility rules:

- existing text APIs remain text APIs
- existing base64 APIs remain available
- array-backed binary shims remain available through explicit `bytes` conversion helpers
- any current experimental `Array`-backed `*_binary` host calls should be migrated to `Bytes`
  before being treated as stable public API

Recommended non-rule:

- do not auto-convert `bytes` to `string`
- do not auto-convert `bytes` to `array`

## Internal Architecture Changes

### A. Extend the VM value model and wire format

Required work in `pd-vm`:

- add `Value::Bytes` and `ValueType::Bytes` in
  [`src/bytecode.rs`](../src/bytecode.rs)
- add `SharedBytes` storage and structural equality or hashing support
- extend VMBC encode or decode support in [`src/vmbc.rs`](../src/vmbc.rs)
- bump the VMBC wire version as needed once a new constant encoding tag is introduced
- audit native JIT layout helpers and value-tag handling so `Bytes` is a legal runtime value even
  when typed byte fast paths are not added immediately

Important rule:

- interpreter, debugger, recorder, replay, JIT bridge, and AOT paths must all understand the new
  value tag before `Bytes` is considered shipped
- parser, linter, type inference, and diagnostics must also understand `Bytes` before it is
  considered shipped

### B. Extend the compiler and type system

Required compiler work:

- add `bytes` to declared type parsing and type display
- extend host return-type mapping from `AbiValueType::Bytes`
- extend builtin signatures and namespace metadata to return or accept `bytes`
- ensure inference and operand typing can represent `Bytes` distinctly from `Array`
- ensure lint passes and type validators treat `bytes` as a first-class known type rather than
  falling back to `unknown`
- ensure compile diagnostics render `bytes` consistently in user-facing messages

Required end-to-end tooling work:

- extend lint entrypoints in [`src/compiler/pipeline.rs`](../src/compiler/pipeline.rs)
  so inferred `bytes` locals and annotations are preserved correctly
- extend typing validation and diagnostic sites under
  [`src/compiler/typing/`](../src/compiler/typing/)
- extend completion and hover metadata emitted to the wasm playground and web UI under
  [`pd-vm-wasm/src/`](../pd-vm-wasm/src/) and
  [`playground/src/`](../../playground/src/)

Optional later work:

- bytes literal syntax
- richer bytes generic helpers

The first milestone does not need custom literal syntax as long as RSS can obtain `bytes` from
builtins and host functions.

### C. Extend builtin runtime dispatch and metadata

Required builtin work:

- extend the builtin catalog metadata to describe `bytes` return types
- add runtime dispatch for the `bytes` namespace
- extend generic sequence builtins where appropriate
- add docs and completion metadata for the new namespace

Recommended namespace split:

- generic sequence operations stay generic when semantics align
- codec and explicit conversion operations live under `bytes::*`

### D. Extend the edge ABI source of truth

Required ABI work:

- add `Bytes` to [`pd-edge-abi/src/lib.rs`](../../pd-edge/pd-edge-abi/src/lib.rs)
- add a `Bytes` callable marker alongside `Any`, `Array`, `Map`, and `Value`
- regenerate ABI manifests and generated tables
- update the compiler mapping in
  [`src/compiler/parser/symbols.rs`](../src/compiler/parser/symbols.rs)

Then update host ABI declarations so binary paths use native `Bytes`.

### E. Update `pd-edge` runtime implementations

Required runtime work:

- replace array-backed binary helpers in `tcp` and `websocket` with native `bytes`
- add `tcp::stream::read_exact_binary`
- add native `udp::socket::{send_binary, recv_binary}`
- add native `webrtc::connection::{send_binary, read_binary}`
- keep base64 wrappers as adapters over native `bytes`

Recommended implementation detail:

- centralize `Value <-> Vec<u8>` conversion helpers in one module while the migration is in
  progress, then collapse them once host calls speak `Bytes` natively end to end

### F. Add stdlib module and tests

Required shared-library work:

- add [`stdlib/rss/bytes.rss`](../stdlib/rss/)
- add [`stdlib/tests/bytes.rss`](../stdlib/tests/)
- add docs or examples showing UTF-8, hex, base64, and u16 parsing

Because `pd-edge` reuses the shared VM stdlib, this module should live in `pd-vm`, not only in the
edge crate.

## Milestones

### Milestone 0: Design Lock And ABI Shape

- add this plan and align on `Bytes` as a first-class value type
- decide `Bytes` equality and map-key semantics by value
- decide that binary transport APIs use `Bytes`, not `Array`
- explicitly defer bytes literal syntax

### Milestone 1: Native VM `Bytes`

- add `Value::Bytes` and `ValueType::Bytes`
- extend VMBC encode or decode and runtime tag handling
- update debugger, disassembler, replay, and JIT bridges
- add compiler support for the `bytes` type name
- extend generic builtins: `len`, `slice`, `concat`, `get`, `has`, and `type`
- wire `bytes` through lint, type inference, validation, diagnostics, completion metadata, and
  wasm tooling

Exit criteria:

- RSS functions can accept, return, compare, concatenate, slice, and index `bytes`
- `type(value)` reports `bytes`
- VM wire format round-trips `bytes`
- lint and inferred-type surfaces report `bytes` explicitly instead of `unknown`
- completions, hover hints, and compile diagnostics surface `bytes` consistently

### Milestone 2: `bytes::*` Builtins And `bytes.rss`

- add builtin conversions for UTF-8, hex, base64, and array-u8 bridges
- add `bytes.rss` ergonomic helpers
- add stdlib tests for codec and endian helpers

Exit criteria:

- RSS can construct and inspect binary payloads without text abuse
- common protocol helpers no longer need handwritten array loops for every byte field

### Milestone 3: Binary-First Edge Host APIs

- migrate `tcp::stream::{read_binary, peek_binary, write_binary}` to `Bytes`
- add `tcp::stream::read_exact_binary`
- add `udp::socket::{send_binary, recv_binary}`
- migrate `websocket::connection::{send_binary, read_binary}` to `Bytes`
- add `webrtc::connection::{send_binary, read_binary}`
- retain base64 wrappers as compatibility adapters

Exit criteria:

- all primary binary transport APIs exchange `bytes`
- RSS packet and frame protocols can be implemented without array- or base64-shaped detours

### Milestone 4: Edge Migration And Protocol Adoption

- update MQTT broker and protocol examples to use native `bytes`
- remove temporary array-based binary adapters where the public surface no longer needs them
- update README docs in `pd-vm` and `pd-edge`
- add integration tests for TCP, UDP, WebSocket, and WebRTC binary paths

Exit criteria:

- MQTT and other binary examples read and write native `bytes`
- repo docs consistently describe `bytes` as the binary payload type

## Testing And Verification

### VM And Compiler Coverage

- unit tests for `Value::Bytes` equality, hashing, cloning, and map-key behavior
- VMBC round-trip tests for `Bytes`
- parser and type-check tests for `bytes` annotations
- type inference tests proving locals, call returns, and builtin flows retain `bytes`
- lint tests proving `bytes` annotations and inferred locals do not degrade to `unknown`
- builtin tests for `len`, `slice`, `concat`, `get`, `has`, and `type`
- builtin namespace tests for UTF-8, hex, base64, and array-u8 conversions
- debugger or disassembler snapshot tests for `Bytes` constants and runtime values
- completion-catalog and wasm-lint tests proving `bytes` appears in user-facing tooling

### Stdlib Coverage

- `bytes.rss` tests for prefix or suffix search, equality, index search, and endian helpers
- explicit invalid-input tests for `to_utf8`, `from_hex`, and `from_base64`

### Edge Coverage

- ABI manifest tests for `Bytes` type exposure
- `cargo check` for `pd-vm`, `pd-edge-abi`, and `pd-edge`
- transport tests for native binary TCP reads and writes
- datagram tests for UDP binary send or receive
- frame or message tests for WebSocket and WebRTC binary IO
- end-to-end MQTT broker tests running through `pd-edge-transport-proxy` with native `bytes`

## Risks And Tradeoffs

- adding a new VM value tag touches interpreter, wire format, JIT, AOT, debugger, and compiler code
- keeping both `Array`-backed and `Bytes`-backed binary APIs for too long will create confusion
- adding implicit conversions would reduce friction short-term but would weaken the language model
- shipping `Bytes` without native codec builtins would leave RSS ergonomics half-finished

The main design tradeoff is scope:

- native `Bytes` is the correct long-term model
- it is a larger cross-cutting change than simply standardizing on `array<int>`

This plan intentionally chooses the larger but cleaner design because protocol and transport work in
`pd-edge` is already hitting the limits of string- and array-shaped binary handling.
