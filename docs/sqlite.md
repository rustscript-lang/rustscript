# SQLite host API

RustScript exposes SQLite as bounded host imports under the `sqlite` feature. Fixed-shape
request and result objects are named structs at the catalog and compiler boundary. Runtime
values remain maps; positional parameters and query rows stay arrays.

Grant each imported callable explicitly. SQLite policy and capability bindings are snapshotted
when a call is admitted.

## Imports

- `sqlite::open`
- `sqlite::execute`
- `sqlite::query`
- `sqlite::transaction`
- `sqlite::close`
- `sqlite::rows_affected`
- `sqlite::truncated`
- `sqlite::next_cursor`

## Open options (`SqliteOpenOptions`)

```rust
use sqlite;

let db = sqlite::open({
    path: ":memory:",
    mode: "memory",
    limits: { max_rows: 100 },
});
```

All fields are optional in object literals. Runtime still requires a non-empty `path`.

| Field | Compile-time | Runtime |
| --- | --- | --- |
| `path` | optional string | required non-empty path |
| `mode` | optional string | `read`, `write`, `create`, `memory`, or omitted |
| `root` | optional string | optional filesystem root for path confinement |
| `limits` | optional `SqliteLimits` | omitted keys keep the embedding ceiling |

Unknown fields are rejected at compile time.

## Limits (`SqliteLimits`)

Every implementation-defined ceiling is an optional int. Omitted keys keep the host ceiling.
Present keys must be positive integers and cannot exceed the ceiling.

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

Empty `{}` is valid. Extra keys are rejected.

## Execute (`SqliteExecuteResult`)

```rust
let inserted = sqlite::execute(&db, "INSERT INTO t VALUES (?)", [7]);
let n = inserted.rows_affected;
let id = inserted.last_insert_rowid;
```

| Field | Type |
| --- | --- |
| `rows_affected` | int |
| `last_insert_rowid` | int |

`sqlite::execute(connection, sql, params)` takes positional `params` as an array of dynamic
cells, not a named struct. `sqlite::rows_affected(envelope)` still reads the execute result.

## Query (`SqliteQueryResult`)

```rust
let queried = sqlite::query(&db, "SELECT a FROM t", [], { max_rows: 32 });
let columns = queried.columns;
let rows = queried.rows;
let truncated = queried.truncated;
```

| Field | Type |
| --- | --- |
| `columns` | array of string |
| `rows` | array of arrays of dynamic cells |
| `truncated` | bool |
| `next_cursor` | optional int; omitted when absent |

Row cells stay positional arrays. Index `queried.rows[0]` for the first row. Do not treat rows
as objects.

`sqlite::truncated(envelope)` and `sqlite::next_cursor(envelope)` accept a query result.
`sqlite::query` takes `SqliteLimits` as its fourth argument; `{}` keeps every ceiling.

## Transactions (`SqliteStatement`)

```rust
sqlite::transaction(&db, [
    { sql: "INSERT INTO t VALUES (1)", query: false },
    { sql: "SELECT a FROM t", query: true, limits: { max_rows: 8 } },
]);
```

| Field | Compile-time |
| --- | --- |
| `sql` | required string |
| `params` | optional positional array of dynamic cells |
| `query` | optional bool |
| `limits` | optional `SqliteLimits` |

The transaction return is `array<unknown>` because execute and query envelopes mix. Params on
`execute` / `query` / `SqliteStatement` stay dynamic arrays, not named structs.

## Runtime representation

Named struct identity is a compile-time schema. Successful host values are still maps:

- execute results expose `rows_affected` and `last_insert_rowid`
- query envelopes expose `columns`, `rows`, `truncated`, and optionally `next_cursor`

Field access (`inserted.rows_affected`) is the typed script surface. Extra or wrong-typed
object-literal fields are compile errors.
