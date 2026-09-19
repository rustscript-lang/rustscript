//! Focused tests for the scoped SQLite host functions (PR16 commit 4).
//!
//! Connections are typed [`HostResource`]s owned by the VM's execution scope;
//! `sqlite::open` / `execute` / `query` / `transaction` / `close` are ordinary
//! macro-owned async functions backed by `tokio-rusqlite`. These tests exercise the
//! scope-backed behaviour through the public VM + SQLite API: typed-value
//! round trips and ordered transactions, read-only and SQL-safety policy,
//! row/result-byte truncation bounds, stale/foreign/typed handle rejection,
//! and adapter-owned `configure`/`clear`/`close` cleanup.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vm::operation::OperationCancelReason;
use vm::{
    CallReturn, CompileSourceFileOptions, HostAsyncBridge, HostFunctionRegistry, HostFuture,
    HostFutureOutput, HostOpId, SqliteHostExt, Vm, VmError, VmResult, VmStatus, compile_source,
    compile_source_with_flavor_and_options, register_sqlite_builtin_module_from_catalog,
    sqlite_host_catalog,
};

use super::vm_reset::reset_for_reuse_to_ready;

#[derive(Default)]
struct TokioHostDriver {
    submitted: HashMap<HostOpId, HostFuture>,
}

impl HostAsyncBridge for TokioHostDriver {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        self.submitted.insert(op_id, future);
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        Poll::Ready(Err(VmError::HostError(format!(
            "unknown external host operation {op_id}"
        ))))
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let poll = self.submitted.get_mut(&op_id).map_or_else(
            || {
                Poll::Ready(Err(VmError::HostError(format!(
                    "unknown submitted host operation {op_id}"
                ))))
            },
            |future| future.as_mut().poll(cx),
        );
        if poll.is_ready() {
            self.submitted.remove(&op_id);
        }
        poll
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.submitted.remove(&op_id);
    }

    fn request_cancel_op(
        &mut self,
        op_id: HostOpId,
        _reason: OperationCancelReason,
    ) -> VmResult<()> {
        self.submitted.remove(&op_id);
        Ok(())
    }

    fn poll_cancel_op(&mut self, _op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        Poll::Ready(Ok(()))
    }
}

fn install_host_driver(vm: &mut Vm) {
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
}

fn drive_vm_to_host_error(vm: &mut Vm) -> String {
    let mut status = match vm.run() {
        Ok(status) => status,
        Err(VmError::HostError(message)) => return message,
        Err(other) => return format!("{other:?}"),
    };
    loop {
        status = match status {
            VmStatus::Halted => panic!("expected host error, got success"),
            VmStatus::Yielded => match vm.resume() {
                Ok(status) => status,
                Err(VmError::HostError(message)) => return message,
                Err(other) => return format!("{other:?}"),
            },
            VmStatus::Waiting(_) => {
                if let Err(error) = vm.wait_for_host_op_blocking() {
                    return match error {
                        VmError::HostError(message) => message,
                        other => format!("{other:?}"),
                    };
                }
                match vm.resume() {
                    Ok(status) => status,
                    Err(VmError::HostError(message)) => return message,
                    Err(other) => return format!("{other:?}"),
                }
            }
        };
    }
}

fn start_long_sqlite_query(vm: &mut Vm) {
    let open_status = vm.run().expect("SQLite open should start");
    assert!(matches!(open_status, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("SQLite open should complete");
    let query_status = vm.resume().expect("SQLite query should start");
    assert!(matches!(query_status, VmStatus::Waiting(_)));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(vm.poll_waiting_host_op(&mut cx), Poll::Pending),
        "long SQLite query should remain pending after its first poll"
    );
}

/// Helper: run a SQLite source to completion. Scripts use `assert(...)` for
/// value checks; a failed assert surfaces as a host error.
fn run_sqlite_source(policy: vm::SqlitePolicy, source: &str) -> Result<(), VmError> {
    let wrapped = format!("use sqlite;\n{source}");
    let catalog = sqlite_host_catalog();
    let compiled = compile_source_with_flavor_and_options(
        &wrapped,
        vm::compiler::SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog.clone()),
    )
    .expect("source should compile");
    let mut vm = Vm::try_new(compiled.program)?;
    install_host_driver(&mut vm);
    let mut registry = HostFunctionRegistry::empty();
    register_sqlite_builtin_module_from_catalog(&mut registry, catalog.as_ref())?;
    registry.bind_vm_cached(&mut vm)?;
    vm.configure_sqlite(policy);

    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(()),
            VmStatus::Yielded => {
                status = vm.resume()?;
            }
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()?;
                status = vm.resume()?;
            }
        }
    }
}

/// Helper: run a legacy builtin SQLite source expecting a host error. This
/// keeps handle-validation coverage on the builtin dispatch path, which does
/// not use catalog resource passing.
fn run_sqlite_builtin_host_error(policy: vm::SqlitePolicy, source: &str) -> String {
    let wrapped = format!("use sqlite;\n{source}");
    let compiled = compile_source(&wrapped).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    install_host_driver(&mut vm);
    vm.configure_sqlite(policy);
    drive_vm_to_host_error(&mut vm)
}

/// Helper: run a SQLite source expecting a host error, returning its message.
fn run_sqlite_host_error(policy: vm::SqlitePolicy, source: &str) -> String {
    match run_sqlite_source(policy, source) {
        Ok(()) => panic!("expected host error, got success"),
        Err(VmError::HostError(message)) => message,
        Err(other) => panic!("expected host error, got: {other:?}"),
    }
}

fn temporary_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after the Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "rustscript-sqlite-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temporary SQLite root should be created");
    root
}

fn policy_for(root: &Path) -> vm::SqlitePolicy {
    vm::SqlitePolicy {
        database_root: Some(root.to_string_lossy().into_owned()),
        ..vm::SqlitePolicy::default()
    }
}

#[test]
fn sqlite_async_hosts_require_an_async_bridge() {
    let compiled = compile_source(
        "use sqlite;\nlet db = sqlite::open({ path: \":memory:\", mode: \"memory\", limits: {} });",
    )
    .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    let error = vm
        .run()
        .expect_err("async SQLite open must require a host bridge");
    assert!(
        error
            .to_string()
            .contains("async host function requires a host async bridge"),
        "unexpected missing-bridge error: {error}"
    );
}

#[test]
fn sqlite_round_trip_supports_typed_values_and_ordered_transactions() {
    let root = temporary_root("round-trip");
    let policy = policy_for(&root);
    run_sqlite_source(
        policy,
        r#"
        use bytes;
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_rows: 128, max_result_bytes: 65536, max_statements: 16, max_transaction_ms: 5000 } });
        sqlite::execute(&db, "CREATE TABLE values_table (id INTEGER PRIMARY KEY, n INTEGER, r REAL, s TEXT, b BLOB, z TEXT)", []);
        let blob_payload = bytes::from_hex("000102");
        let ins = sqlite::execute(&db, "INSERT INTO values_table (n, r, s, b, z) VALUES (?1, ?2, ?3, ?4, ?5)", [
            { kind: "int", int_value: 7, float_value: null, text_value: null, blob_value: null },
            { kind: "float", int_value: null, float_value: 1.5, text_value: null, blob_value: null },
            { kind: "text", int_value: null, float_value: null, text_value: "hello", blob_value: null },
            { kind: "blob", int_value: null, float_value: null, text_value: null, blob_value: blob_payload },
            { kind: "null", int_value: null, float_value: null, text_value: null, blob_value: null }
        ]);
        let affected = ins.rows_affected;
        assert(affected == 1);
        let rowset = sqlite::query(&db, "SELECT n, r, s, b, z FROM values_table ORDER BY id", [], { max_rows: 8, max_result_bytes: 65536 });
        let truncated = rowset.truncated;
        let next_cursor = rowset.next_cursor;
        assert(truncated == false);
        assert(next_cursor == 7);
        let columns = rowset.columns;
        assert(columns == {"n", "r", "s", "b", "z"});
        let row = rowset.rows[0];
        let cells = row.cells;
        let int_cell = cells[0];
        let int_kind = int_cell.kind;
        let int_value = int_cell.int_value;
        assert(int_kind == "int");
        assert(int_value == 7);
        let float_cell = cells[1];
        let float_kind = float_cell.kind;
        let float_value = float_cell.float_value;
        assert(float_kind == "float");
        assert(float_value == 1.5);
        let text_cell = cells[2];
        let text_kind = text_cell.kind;
        let text_value = text_cell.text_value;
        assert(text_kind == "text");
        assert(text_value == "hello");
        let blob_cell = cells[3];
        let blob_kind = blob_cell.kind;
        let blob_value = blob_cell.blob_value;
        assert(blob_kind == "blob");
        assert(blob_value == blob_payload);
        let null_cell = cells[4];
        let null_kind = null_cell.kind;
        let null_int = null_cell.int_value;
        assert(null_kind == "null");
        assert(null_int == null);

        let results = sqlite::transaction(&db, {
            { sql: "INSERT INTO values_table (n) VALUES (?1)", params: [{ kind: "int", int_value: 8 }] },
            { sql: "INSERT INTO values_table (n) VALUES (?1)", params: [{ kind: "int", int_value: 9 }] },
            { sql: "SELECT n FROM values_table ORDER BY id", query: true, limits: { max_rows: 8, max_result_bytes: 65536 } }
        });
        assert(type(results) == "array");
        let first_result = results[0];
        let first_kind = first_result.kind;
        assert(first_kind == "execute");
        assert(first_result.execute.rows_affected == 1);
        assert(first_result.query == null);
        let second_result = results[1];
        let second_kind = second_result.kind;
        assert(second_kind == "execute");
        let third_result = results[2];
        let third_kind = third_result.kind;
        assert(third_kind == "query");
        assert(third_result.execute == null);
        assert(third_result.query.rows[0].cells[0].int_value == 7);
        let count = sqlite::query(&db, "SELECT count(*) AS count FROM values_table", [], { max_rows: 8, max_result_bytes: 65536 });
        let count_row = count.rows[0];
        let count_cells = count_row.cells;
        let count_cell = count_cells[0];
        let count_kind = count_cell.kind;
        let count_value = count_cell.int_value;
        assert(count_kind == "int");
        assert(count_value == 3);
        sqlite::close(db);
        "#,
    )
    .expect("round-trip should succeed");
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_value_discriminators_reject_unknown_missing_and_multiple_payloads() {
    let root = temporary_root("value-discriminators");
    let policy = policy_for(&root);
    for (value, expected) in [
        (r#"{ kind: "unknown" }"#, "unknown SQLite value kind"),
        (r#"{ kind: "int" }"#, "missing SQLite int_value"),
        (
            r#"{ kind: "int", int_value: 1, text_value: "extra" }"#,
            "multiple non-null payloads",
        ),
    ] {
        let source = format!(
            r#"let db = sqlite::open({{ path: "state.db", mode: "read_write_create", limits: {{}} }});
            sqlite::execute(&db, "SELECT ?", [{value}]);"#
        );
        let error = run_sqlite_host_error(policy.clone(), &source);
        assert!(
            error.contains(expected),
            "expected {expected:?} for {value}, got {error}"
        );
    }
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_value_mismatched_payload_and_limits_are_rejected() {
    let mismatch_root = temporary_root("value-mismatch");
    let mismatch = run_sqlite_builtin_host_error(
        policy_for(&mismatch_root),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: {} });
        sqlite::execute(db, "SELECT ?", [{ kind: "int", int_value: "wrong" }]);
        "#,
    );
    assert!(
        mismatch.contains("SQLite int_value payload"),
        "mismatched payload type must be rejected, got: {mismatch}"
    );
    fs::remove_dir_all(mismatch_root).expect("temporary mismatch root should be removed");

    let count_root = temporary_root("parameter-count-limit");
    let count = run_sqlite_host_error(
        policy_for(&count_root),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_parameters: 1 } });
        sqlite::execute(&db, "SELECT ?1, ?2", [{ kind: "int", int_value: 1 }, { kind: "int", int_value: 2 }]);
        "#,
    );
    assert!(
        count.contains("parameter count") && count.contains("configured limit"),
        "parameter count limit must be enforced, got: {count}"
    );
    fs::remove_dir_all(count_root).expect("temporary count root should be removed");

    let bytes_root = temporary_root("parameter-bytes-limit");
    let bytes = run_sqlite_host_error(
        policy_for(&bytes_root),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_parameter_bytes: 2 } });
        sqlite::execute(&db, "SELECT ?1", [{ kind: "text", text_value: "abc" }]);
        "#,
    );
    assert!(
        bytes.contains("parameters exceed") && bytes.contains("2 byte limit"),
        "parameter byte limit must be enforced, got: {bytes}"
    );
    fs::remove_dir_all(bytes_root).expect("temporary bytes root should be removed");
}

#[test]
fn sqlite_invalid_utf8_text_is_returned_as_blob_value() {
    let root = temporary_root("invalid-utf8");
    let policy = policy_for(&root);
    run_sqlite_source(
        policy,
        r#"
        use bytes;
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: {} });
        let payload = bytes::from_hex("80ff");
        let result = sqlite::query(&db, "SELECT CAST(?1 AS TEXT)", [{ kind: "blob", blob_value: payload }], {});
        let cell = result.rows[0].cells[0];
        assert(cell.kind == "blob");
        assert(cell.blob_value == payload);
        sqlite::close(db);
        "#,
    )
    .expect("invalid UTF-8 SQLite TEXT should use the blob variant");
    fs::remove_dir_all(root).expect("temporary invalid UTF-8 root should be removed");
}

#[test]
fn sqlite_enforces_read_only_vm_local_ids_and_sql_safety() {
    let root = temporary_root("policy");
    let policy = policy_for(&root);

    run_sqlite_source(
        policy.clone(),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: {} });
        sqlite::execute(&db, "CREATE TABLE items (value INTEGER)", []);
        "#,
    )
    .expect("writer should create the table");

    let hazard = run_sqlite_host_error(
        policy.clone(),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_only", limits: {} });
        sqlite::execute(&db, "INSERT INTO items (value) VALUES (1)", []);
        "#,
    );
    assert!(
        hazard.contains("ReadOnly")
            || hazard.to_lowercase().contains("readonly")
            || hazard.to_lowercase().contains("read-only"),
        "read-only writes must be rejected, got: {hazard}"
    );

    for bad in [
        "ATTACH DATABASE 'other.db' AS other",
        "PRAGMA writable_schema = ON",
        "SELECT load_extension('not-available')",
        "CREATE TABLE first (id INTEGER); CREATE TABLE second (id INTEGER)",
    ] {
        let err = run_sqlite_host_error(
            policy.clone(),
            &format!(
                "let db = sqlite::open({{ path: \"state.db\", mode: \"read_write_create\", limits: {{}} }});\n sqlite::execute(&db, \"{bad}\", []);"
            ),
        );
        assert!(
            err.contains("not allowed")
                || err.contains("multiple statements")
                || err.contains("disabled"),
            "unsafe SQL must be rejected, got: {err}"
        );
    }

    // A SQLite id from another VM must be rejected (foreign arena).
    let other_err =
        run_sqlite_builtin_host_error(policy, "sqlite::execute(1234567, \"SELECT 1\", []);");
    assert!(
        other_err.contains("unknown SQLite database")
            || other_err.contains("invalid sqlite handle"),
        "foreign ids must be rejected, got: {other_err}"
    );

    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_query_reports_row_and_result_byte_truncation() {
    let root = temporary_root("limits");
    let policy = policy_for(&root);
    run_sqlite_source(
        policy,
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_rows: 32, max_result_bytes: 32 } });
        sqlite::execute(&db, "CREATE TABLE items (value TEXT)", []);
        sqlite::execute(&db, "INSERT INTO items (value) VALUES (?1)", [{ kind: "text", text_value: "one" }]);
        sqlite::execute(&db, "INSERT INTO items (value) VALUES (?1)", [{ kind: "text", text_value: "two" }]);
        sqlite::execute(&db, "INSERT INTO items (value) VALUES (?1)", [{ kind: "text", text_value: "three" }]);

        let row_limited = sqlite::query(&db, "SELECT value FROM items ORDER BY rowid", [], { max_rows: 1, max_result_bytes: 65536 });
        let row_limited_truncated = row_limited.truncated;
        assert(row_limited_truncated == true);
        let row_limited_row = row_limited.rows[0];
        let row_limited_cells = row_limited_row.cells;
        let row_limited_cell = row_limited_cells[0];
        let row_limited_kind = row_limited_cell.kind;
        let row_limited_value = row_limited_cell.text_value;
        assert(row_limited_kind == "text");
        assert(row_limited_value == "one");

        let byte_limited = sqlite::query(&db, "SELECT value FROM items ORDER BY rowid", [], { max_rows: 32, max_result_bytes: 8 });
        let byte_limited_truncated = byte_limited.truncated;
        assert(byte_limited_truncated == true);
        "#,
    )
    .expect("truncation should be reported");
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_uses_typed_generation_checked_resource_handles() {
    let root = temporary_root("handles");
    let policy = policy_for(&root);

    let err = run_sqlite_builtin_host_error(
        policy,
        r#"
        let a = sqlite::open({ path: "handles.db", mode: "read_write_create", limits: {} });
        sqlite::close(a);
        let b = sqlite::open({ path: "handles.db", mode: "read_write_create", limits: {} });
        assert(a != b);
        sqlite::execute(a, "SELECT 1", []);
        "#,
    );
    assert!(
        err.contains("unknown SQLite database"),
        "closed generation must stay invalid after slot reuse, got: {err}"
    );
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_connection_limit_is_enforced_by_the_adapter() {
    let root = temporary_root("connection-limit");
    let policy = policy_for(&root);
    let err = run_sqlite_host_error(
        policy,
        r#"
        let a = sqlite::open({ path: "a.db", mode: "read_write_create", limits: { max_connections: 2 } });
        let b = sqlite::open({ path: "b.db", mode: "read_write_create", limits: { max_connections: 2 } });
        let c = sqlite::open({ path: "c.db", mode: "read_write_create", limits: { max_connections: 2 } });
        "#,
    );
    assert!(
        err.contains("connection limit"),
        "max_connections must be enforced, got: {err}"
    );
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_configure_and_clear_own_the_policy() {
    let root = temporary_root("policy-config");
    let policy = policy_for(&root);

    // configure_sqlite is honoured by open (root + unsafe flag).
    run_sqlite_source(
        policy.clone(),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: {} });
        sqlite::execute(&db, "CREATE TABLE items (value INTEGER)", []);
        "#,
    )
    .expect("configured policy should allow file opens");

    // clear_sqlite restores the default (no root), so a file open is rejected.
    let compiled = compile_source("use sqlite;\nlet db = sqlite::open({ path: \"state.db\", mode: \"read_write_create\", limits: {} });")
        .expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    install_host_driver(&mut vm);
    vm.configure_sqlite(policy);
    vm.clear_sqlite();
    let err = drive_vm_to_host_error(&mut vm);
    assert!(
        err.contains("root"),
        "cleared policy must reject file opens, got: {err}"
    );

    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_close_and_reset_retire_resources() {
    let root = temporary_root("cancel-reset");
    let policy = policy_for(&root);

    // Explicit close retires the adapter connection; a fresh connection on the
    // same root then works normally.
    run_sqlite_source(
        policy.clone(),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_transaction_ms: 10000, max_result_bytes: 65536 } });
        sqlite::execute(&db, "CREATE TABLE items (value INTEGER)", []);
        let pending = sqlite::query(&db, "WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 2000000) SELECT sum(value) FROM numbers", [], { max_rows: 1, max_result_bytes: 65536 });
        sqlite::close(db);
        let db2 = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_transaction_ms: 10000, max_result_bytes: 65536 } });
        let count = sqlite::query(&db2, "SELECT count(*) AS count FROM items", [], {});
        let count_row = count.rows[0];
        let count_cells = count_row.cells;
        let count_cell = count_cells[0];
        let count_kind = count_cell.kind;
        let count_value = count_cell.int_value;
        assert(count_kind == "int");
        assert(count_value == 0);
        sqlite::close(db2);
        "#,
    )
    .expect("close should retire the connection and allow a fresh connection");

    // VM reset retires all pending sqlite operations and closes every open
    // connection through the generic scope lifecycle.
    let compiled = compile_source(
        "use sqlite;\nlet db = sqlite::open({ path: \"state.db\", mode: \"read_write_create\", limits: { max_transaction_ms: 10000, max_result_bytes: 65536 } });\nlet pending = sqlite::query(&db, \"WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 2000000) SELECT sum(value) FROM numbers\", [], { max_rows: 1, max_result_bytes: 65536 });",
    )
    .expect("reset source should compile");
    let mut vm = Vm::new(compiled.program);
    install_host_driver(&mut vm);
    vm.configure_sqlite(policy);
    // Poll the long query once so it reaches the adapter, then reset. Scope
    // close interrupts the active SQLite statement and retires the resource.
    start_long_sqlite_query(&mut vm);
    reset_for_reuse_to_ready(&mut vm).expect("reset should reach quiescence");
    assert!(
        vm.execution_scope().operations().is_empty(),
        "reset must retire all pending sqlite operations"
    );
    assert!(
        vm.execution_scope().resources().is_empty(),
        "reset must close every sqlite connection resource"
    );

    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_reset_waits_for_adapter_close_before_reuse_and_prevents_late_writes() {
    let root = temporary_root("reset-adapter-close");
    let database_path = root.join("state.db");
    let blocker =
        rusqlite::Connection::open(&database_path).expect("blocking SQLite connection should open");
    blocker
        .execute_batch("CREATE TABLE items (value INTEGER); BEGIN IMMEDIATE")
        .expect("blocking transaction should hold the writer lock");

    let compiled = compile_source(
        "use sqlite;\nlet db = sqlite::open({ path: \"state.db\", mode: \"read_write_create\", limits: { busy_timeout_ms: 5000 } });\nsqlite::execute(&db, \"INSERT INTO items (value) VALUES (1)\", []);",
    )
    .expect("reset source should compile");
    let mut vm = Vm::new(compiled.program);
    install_host_driver(&mut vm);
    vm.configure_sqlite(policy_for(&root));

    let open_status = vm.run().expect("SQLite open should start");
    assert!(matches!(open_status, VmStatus::Waiting(_)));
    vm.wait_for_host_op_blocking()
        .expect("SQLite open should complete");
    let write_status = vm.resume().expect("blocked SQLite write should start");
    assert!(matches!(write_status, VmStatus::Waiting(_)));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(vm.poll_waiting_host_op(&mut cx), Poll::Pending));
    std::thread::sleep(Duration::from_millis(50));
    assert!(matches!(vm.poll_waiting_host_op(&mut cx), Poll::Pending));

    vm.reset_for_reuse().expect("reset should start");
    assert!(
        vm.scope_reset_pending(),
        "reset must retain the scope until tokio-rusqlite confirms close"
    );
    assert!(matches!(vm.poll_reset_for_reuse(&mut cx), Poll::Pending));

    blocker
        .execute_batch("ROLLBACK")
        .expect("blocking transaction should release the writer lock");
    reset_for_reuse_to_ready(&mut vm).expect("reset should finish after adapter close");

    let verifier = rusqlite::Connection::open(&database_path)
        .expect("verification SQLite connection should open");
    let count: i64 = verifier
        .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        .expect("verification query should succeed");
    assert_eq!(count, 0, "canceled work must not mutate after reset");

    drop(verifier);
    drop(blocker);
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_pending_operation_slots_are_reclaimed_after_completion() {
    let root = temporary_root("pending-reclaim");
    let policy = policy_for(&root);
    // With `max_pending_operations: 4`, more than four sequential operations
    // must still succeed: completed operations release their slot so the
    // per-connection pending counter does not grow without bound.
    run_sqlite_source(
        policy,
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_pending_operations: 4 } });
        sqlite::execute(&db, "CREATE TABLE items (value INTEGER)", []);
        let mut i = 0;
        while i < 10 {
            sqlite::execute(&db, "INSERT INTO items (value) VALUES (?1)", [{ kind: "int", int_value: i }]);
            i = i + 1;
        }
        let count = sqlite::query(&db, "SELECT count(*) AS count FROM items", [], {});
        let count_row = count.rows[0];
        let count_cells = count_row.cells;
        let count_cell = count_cells[0];
        let count_kind = count_cell.kind;
        let count_value = count_cell.int_value;
        assert(count_kind == "int");
        assert(count_value == 10);
        sqlite::close(db);
        "#,
    )
    .expect("sequential operations beyond the pending limit should succeed after reclaim");
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_transaction_deadline_interrupts_and_rolls_back() {
    let root = temporary_root("transaction-deadline");
    let error = run_sqlite_host_error(
        policy_for(&root),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: { max_transaction_ms: 500 } });
        sqlite::execute(&db, "CREATE TABLE items (value INTEGER)", []);
        sqlite::transaction(&db, {
            { sql: "INSERT INTO items (value) VALUES (1)" },
            {
                sql: "WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 10000000) SELECT sum(value) FROM numbers",
                query: true,
                limits: { max_rows: 1 }
            }
        });
        "#,
    );
    assert!(
        error.contains("transaction exceeded") && error.contains("500 ms deadline"),
        "transaction deadline must surface explicitly, got: {error}"
    );
    run_sqlite_source(
        policy_for(&root),
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write_create", limits: {} });
        let result = sqlite::query(&db, "SELECT count(*) FROM items", [], {});
        assert(result.rows[0].cells[0].int_value == 0);
        sqlite::close(db);
        "#,
    )
    .expect("the timed-out transaction write should be rolled back");
    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}

#[test]
fn sqlite_pending_reset_repeatedly_keeps_vm_reusable() {
    let root = temporary_root("reset-stress");
    let policy = policy_for(&root);
    let compiled = compile_source(
        "use sqlite;\nlet db = sqlite::open({ path: \"state.db\", mode: \"read_write_create\", limits: { max_transaction_ms: 10000, max_result_bytes: 65536 } });\nlet pending = sqlite::query(&db, \"WITH RECURSIVE numbers(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbers LIMIT 2000000) SELECT sum(value) FROM numbers\", [], { max_rows: 1, max_result_bytes: 65536 });",
    )
    .expect("stress source should compile");
    let mut vm = Vm::new(compiled.program);
    install_host_driver(&mut vm);
    vm.configure_sqlite(policy);

    for iteration in 0..32 {
        start_long_sqlite_query(&mut vm);
        reset_for_reuse_to_ready(&mut vm).expect("stress reset should reach quiescence");
        assert!(
            vm.execution_scope().operations().is_empty(),
            "iteration {iteration} leaked an SQLite operation"
        );
        assert!(
            vm.execution_scope().resources().is_empty(),
            "iteration {iteration} leaked an SQLite resource"
        );
        assert!(
            vm.is_reusable(),
            "iteration {iteration} left VM non-reusable"
        );
    }

    fs::remove_dir_all(root).expect("temporary SQLite root should be removed");
}
