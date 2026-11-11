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

The `sqlite` feature publishes the complete SQLite module: its typed catalog, generated namespace
and callables, `rusqlite` and `tokio-rusqlite` dependencies, and registration exports. Builds
without that feature, and wasm-family builds, omit the SQLite namespace and catalog entirely.
Execution also requires an async host bridge and the SQLite module registered.

## Open options (`SqliteOpenOptions`)

```rust
use sqlite;

let limits: SqliteLimits = { max_rows: Option::Some(100) };
let options: SqliteOpenOptions = {
    path: Option::Some(":memory:"),
    mode: Option::Some("memory"),
    limits: Option::Some(limits),
};
let db = sqlite::open(options);
```

Every field is a logical `Option`. `Option::None` keeps the host default; `Option::Some(value)`
explicitly supplies it, including `Some(false)`, `Some(0)`, and `Some("")`. A `path` of
`Option::None` is still rejected at runtime for a usable connection.

| Field | Type | Runtime behavior |
| --- | --- | --- |
| `path` | `Option<string>` | Required and non-empty when `Some`; resolved under the embedding policy root |
| `mode` | `Option<string>` | `memory`, `read_only`, `read_write`, or `read_write_create`; `None` defaults to the last |
| `root` | `Option<string>` | Must match the embedding policy root when `Some` |
| `limits` | `Option<SqliteLimits>` | Per-connection ceilings; `None` keeps the host ceiling |

Unknown fields are outside the named-struct contract; do not rely on them.

## Limits (`SqliteLimits`)

Every limit is `Option<int>`. `Option::None` keeps the embedding ceiling; `Option::Some(0)` is a
provided invalid value rather than a default. Supplied limits must be positive and cannot exceed
that ceiling.

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

An empty `{}` is valid for a typed `SqliteLimits` local. Fields outside the named-struct contract are not part of the public schema.

## Values (`SqliteValue`)

Parameters and result cells use the same tagged named struct:

```text
SqliteValue {
    kind: string,
    int_value: Option<int>,
    float_value: Option<float>,
    text_value: Option<string>,
    blob_value: Option<bytes>,
}
```

The five `kind` values and their selected payloads are:

| `kind` | Required payload | Other payloads |
| --- | --- | --- |
| `"null"` | none | all payloads `Option::None` |
| `"int"` | `int_value` | other payloads `Option::None` |
| `"float"` | `float_value` | other payloads `Option::None` |
| `"text"` | `text_value` | other payloads `Option::None` |
| `"blob"` | `blob_value` | other payloads `Option::None` |

For example:

```rust
let values = [
    { kind: "null" },
    { kind: "int", int_value: Option::Some(7) },
    { kind: "float", float_value: Option::Some(1.5) },
    { kind: "text", text_value: Option::Some("hello") },
    { kind: "blob", blob_value: Option::Some(bytes::from_hex("000102")) },
];
```

The host validates this discriminator at runtime. It rejects an unknown `kind`, a missing
selected payload, multiple non-null payloads, and a selected payload with the wrong type.
`"null"` carries no payload. SQLite TEXT that is not valid UTF-8 is returned as `kind: "blob"`
with its original bytes.

## Absence boundary

SQLite exposes three distinct absence forms and they are not interchangeable:

- Configuration and statement entries use logical `Option`: `Option::None` keeps the
  documented default and `Option::Some(value)` supplies it. A physical `null` scalar is
  not a second reading of a present public field.
- A SQL `NULL` result cell remains the tagged `SqliteValue { kind: "null", ... }`
  business value; its four payload slots are logical `Option::None`. It is not an
  absent row and it is not rewritten into an option-of-cell.
- Result-value absence uses logical `Option`: `SqliteQueryResult.next_cursor` and
  the unselected `SqliteTransactionResult` branch are `Option::Some`/`Option::None`
  values, matched with `match` rather than a sentinel.

## Execute (`SqliteExecuteResult`)

`sqlite::execute` takes an `array<SqliteValue>` and returns a `SqliteExecuteResult`:

```rust
let inserted = sqlite::execute(
    &db,
    "INSERT INTO t (value) VALUES (?1)",
    [{ kind: "int", int_value: Option::Some(7) }],
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
let limits: SqliteLimits = { max_rows: Option::Some(32) };
let queried = sqlite::query(&db, "SELECT value FROM t ORDER BY rowid", [], limits);

let first_row = queried.rows[0];
let first_cell = first_row.cells[0];
if first_cell.kind == "int" {
    let value = first_cell.int_value.unwrap_or(-1);
}
```

| Field | Type |
| --- | --- |
| `columns` | array of string |
| `rows` | array of `SqliteRow` |
| `truncated` | bool |
| `next_cursor` | `Option<int>` |

Each `SqliteRow` has one field, `cells`, an array of `SqliteValue` in column order. Result
limits are charged from the underlying column names and cell payloads, without counting the
named-struct wrapper fields. `max_rows`, `max_columns`, and `max_result_bytes` can truncate a
query; already accepted rows remain in the result. `next_cursor` is
`Option::Some(first-column integer)` from the last accepted row, or `Option::None`
when no such value was accepted:

```rust
let next = match queried.next_cursor {
    Option::Some(cursor) => cursor,
    Option::None => -1,
};
```

## Transactions (`SqliteStatement` and `SqliteTransactionResult`)

A transaction receives an array of named `SqliteStatement` values. Each statement has a required
`sql` string and logical option `params`, `query`, and `limits` fields:

```rust
let results = sqlite::transaction(&db, [
    {
        sql: "INSERT INTO t (value) VALUES (?1)",
        params: Option::Some([{ kind: "int", int_value: Option::Some(8) }]),
    },
    {
        sql: "SELECT value FROM t ORDER BY rowid",
        query: Option::Some(true),
        limits: Option::Some(limits),
    },
]);
```

| Field | Type | Runtime behavior |
| --- | --- | --- |
| `sql` | string | Required non-empty SQL |
| `params` | `Option<array<SqliteValue>>` | `None` means no parameters |
| `query` | `Option<bool>` | `None` means execute; `Some(true)` returns a query result |
| `limits` | `Option<SqliteLimits>` | `None` keeps the connection ceiling |

The return value is an `array<SqliteTransactionResult>` in statement order:

```text
SqliteTransactionResult {
    kind: string,
    execute: Option<SqliteExecuteResult>,
    query: Option<SqliteQueryResult>,
}
```

An execute statement produces `{ kind: "execute", execute: Option::Some(...) }`; a query statement
produces `{ kind: "query", query: Option::Some(...) }`. The unselected envelope field is
`Option::None`. Discriminate with `kind` before matching `execute` or `query`:

```rust
let result = results[0];
let executed = match result.execute {
    Option::Some(value) => value.rows_affected,
    Option::None => -1,
};
``` The transaction remains atomic: statement order,
rollback on failure, transaction deadlines, and result limits are preserved. A SQLite progress
handler interrupts a transaction after its configured deadline so the transaction rolls back.

## Resource lifecycle

`sqlite::close(db)` consumes the connection, begins adapter close before suspension,
returns the guest `()` value, and removes the VM resource after close completes. Cancelling a blocked close leaves its resource-owned close
lifecycle available for a later close call to poll to completion.
VM reset interrupts active SQLite work and remains pending while the resource polls
`tokio-rusqlite`'s `Connection::close`; the connection permit is released only after the adapter
confirms that queued and running work has drained and the connection has closed. Cancelling an
individual submitted future only drops that waiter. Its pending-operation lease remains owned by
the queued adapter closure until the closure runs or is discarded, so canceled work still counts
toward `max_pending_operations`. The host layer adds no worker or stronger cancellation mechanism.
Handles are VM-local and generation-checked, so a closed or foreign handle cannot be reused.
