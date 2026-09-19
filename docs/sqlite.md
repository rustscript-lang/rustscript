# SQLite host API

RustScript exposes bounded SQLite host functions when the `sqlite` feature is enabled. The
public catalog uses named structs for options, parameters, rows, cells, and results. Connection
arguments are borrowed for operations and consumed by `sqlite::close`.

## Imports

- `sqlite::open`
- `sqlite::execute`
- `sqlite::query`
- `sqlite::transaction`
- `sqlite::close`

The embedding policy controls the allowed database root, unsafe-SQL capability, and host
ceilings. Configure that policy before opening a connection. Each operation is an ordinary
macro-owned async host function. It captures owned call data and awaits `tokio-rusqlite`, which
serializes work on the connection and owns the blocking SQLite execution thread.

## Compiler and editor catalog boundary

The typed SQLite catalog is a schema-only surface and does not construct a VM or link
`rusqlite`. `sqlite_host_catalog` and the SQLite entries in `standard_host_catalog` remain
available whenever the `runtime` feature is compiled, including builds without the `sqlite`
feature. Catalog-aware compiler callers and the LSP use these declarations for named-struct
field access and exact host signatures.

The `sqlite` feature controls the executable SQLite module, generated SQLite namespace and
callables, the `rusqlite` and `tokio-rusqlite` dependencies, and SQLite registration exports. A
runtime build without that feature can inspect the editor/compiler contract but has no SQLite
implementation to bind; execution requires a build with `sqlite` enabled, an async host bridge,
and the SQLite module registered.

## Open options (`SqliteOpenOptions`)

```rust
use sqlite;

let db = sqlite::open({
    path: ":memory:",
    mode: "memory",
    limits: { max_rows: 100 },
});
```

All fields are optional in object literals. A missing field or `null` keeps the host default,
except that `path` remains required at runtime for a usable connection.

| Field | Type | Runtime behavior |
| --- | --- | --- |
| `path` | optional string | Required and non-empty; resolved under the embedding policy root |
| `mode` | optional string | `memory`, `read_only`, `read_write`, or `read_write_create`; the last is the default |
| `root` | optional string | Must match the embedding policy root when supplied |
| `limits` | optional `SqliteLimits` | Per-connection ceilings; omitted fields keep the host ceiling |

Unknown fields are rejected by the named-struct compiler contract.

## Limits (`SqliteLimits`)

Every limit is an optional int. Omitted or `null` fields keep the embedding ceiling. Supplied
limits must be positive and cannot exceed that ceiling.

- `max_connections`
- `max_statements`
- `max_rows`
- `max_columns`
- `max_result_bytes`
- `max_statement_bytes`
- `max_parameters`
- `max_parameter_bytes`
- `max_pending_operations`
- `max_transaction_ms`
- `busy_timeout_ms`

An empty `{}` is valid. Extra fields are rejected.

## Values (`SqliteValue`)

Parameters and result cells use the same tagged named struct:

```text
SqliteValue {
    kind: string,
    int_value: optional<int>,
    float_value: optional<float>,
    text_value: optional<string>,
    blob_value: optional<bytes>,
}
```

The five `kind` values and their selected payloads are:

| `kind` | Required payload | Other payloads |
| --- | --- | --- |
| `"null"` | none | must be `null` or omitted |
| `"int"` | `int_value` | must be `null` or omitted |
| `"float"` | `float_value` | must be `null` or omitted |
| `"text"` | `text_value` | must be `null` or omitted |
| `"blob"` | `blob_value` | must be `null` or omitted |

For example:

```rust
let values = [
    { kind: "null" },
    { kind: "int", int_value: 7 },
    { kind: "float", float_value: 1.5 },
    { kind: "text", text_value: "hello" },
    { kind: "blob", blob_value: bytes::from_hex("000102") },
];
```

The host validates this discriminator at runtime. It rejects an unknown `kind`, a missing
selected payload, multiple non-null payloads, and a selected payload with the wrong type.
`"null"` carries no payload. SQLite TEXT that is not valid UTF-8 is returned as `kind: "blob"`
with its original bytes.

## Execute (`SqliteExecuteResult`)

`sqlite::execute` takes an `array<SqliteValue>` and returns a `SqliteExecuteResult`:

```rust
let inserted = sqlite::execute(
    &db,
    "INSERT INTO t (value) VALUES (?1)",
    [{ kind: "int", int_value: 7 }],
);
let affected = inserted.rows_affected;
let rowid = inserted.last_insert_rowid;
```

| Field | Type |
| --- | --- |
| `rows_affected` | int |
| `last_insert_rowid` | int |

The parameter count and decoded text/blob byte length are checked against the connection
limits before the adapter call is awaited.

## Query (`SqliteQueryResult` and `SqliteRow`)

`sqlite::query` takes an `array<SqliteValue>` and returns a `SqliteQueryResult`:

```rust
let queried = sqlite::query(
    &db,
    "SELECT value FROM t ORDER BY rowid",
    [],
    { max_rows: 32 },
);

let first_row = queried.rows[0];
let first_cell = first_row.cells[0];
if first_cell.kind == "int" {
    let value = first_cell.int_value;
}
```

| Field | Type |
| --- | --- |
| `columns` | array of string |
| `rows` | array of `SqliteRow` |
| `truncated` | bool |
| `next_cursor` | optional int |

Each `SqliteRow` has one field, `cells`, an array of `SqliteValue` in column order. Result
limits are charged from the underlying column names and cell payloads, without counting the
named-struct wrapper fields. `max_rows`, `max_columns`, and `max_result_bytes` can truncate a
query; already accepted rows remain in the result. `next_cursor` is the first-column integer
from the last accepted row, or `null` when no such value was accepted.

## Transactions (`SqliteStatement` and `SqliteTransactionResult`)

A transaction receives an array of named `SqliteStatement` values. Each statement has a required
`sql` string and optional typed `params`, `query`, and `limits` fields:

```rust
let results = sqlite::transaction(&db, [
    {
        sql: "INSERT INTO t (value) VALUES (?1)",
        params: [{ kind: "int", int_value: 8 }],
    },
    {
        sql: "SELECT value FROM t ORDER BY rowid",
        query: true,
        limits: { max_rows: 8 },
    },
]);
```

| Field | Type | Runtime behavior |
| --- | --- | --- |
| `sql` | string | Required non-empty SQL |
| `params` | optional `array<SqliteValue>` | Omitted or `null` means no parameters |
| `query` | optional bool | Omitted or `null` means execute; `true` returns a query result |
| `limits` | optional `SqliteLimits` | Omitted or `null` keeps the connection ceiling |

The return value is an `array<SqliteTransactionResult>` in statement order:

```text
SqliteTransactionResult {
    kind: string,
    execute: optional<SqliteExecuteResult>,
    query: optional<SqliteQueryResult>,
}
```

An execute statement produces `{ kind: "execute", execute: ... }`; a query statement produces
`{ kind: "query", query: ... }`. The unselected envelope field is `null`. Discriminate with
`kind` before using `execute` or `query`. The transaction remains atomic: statement order,
rollback on failure, transaction deadlines, and result limits are preserved. A SQLite progress
handler interrupts a transaction after its configured deadline so the transaction rolls back.

## Resource lifecycle

`sqlite::close(db)` consumes the connection, awaits adapter close, and removes the VM resource.
VM reset interrupts an active SQLite statement through the connection's interrupt handle, drops
the adapter handle, and retires submitted futures through the generic async bridge. Cancelling an
individual submitted future only drops that waiter; `tokio-rusqlite` may finish work already
queued or running. The host layer adds no worker or stronger cancellation mechanism. Handles are
VM-local and generation-checked, so a closed or foreign handle cannot be reused.
