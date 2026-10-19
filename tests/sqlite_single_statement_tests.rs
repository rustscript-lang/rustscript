#![cfg(feature = "sqlite")]
//! Single-statement parsing through the public typed SQLite host API.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use vm::{
    CompileSourceFileOptions, HostFunctionRegistry, SqliteHostExt, SqlitePolicy, Vm, VmError,
    VmStatus, compile_source_with_flavor_and_options, register_sqlite_builtin_module_from_catalog,
    standard_host_catalog,
};

#[path = "support/vm_reset.rs"]
mod vm_reset;

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sqlite-statement-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("private fixture root");
        Self(path)
    }

    fn run(&self, unsafe_sql: bool, source: &str) -> Result<(), VmError> {
        run(
            SqlitePolicy {
                database_root: Some(self.0.to_string_lossy().into_owned()),
                allow_unsafe_sql: unsafe_sql,
                ..SqlitePolicy::default()
            },
            &format!(
                r#"let db = sqlite::open({{ path: "state.db", mode: "read_write_create", limits: {{ max_pending_operations: 1, max_transaction_ms: 5000 }} }});
                {source}
                sqlite::close(db);"#
            ),
        )
    }

    fn ok(&self, source: &str) {
        self.run(false, source).expect("formal SQLite operation");
    }

    fn rejected(&self, unsafe_sql: bool, source: &str, expected: &str) {
        let error = self.run(unsafe_sql, source).expect_err("SQL must fail");
        assert!(
            matches!(&error, VmError::HostError(message) if message.to_lowercase().contains(&expected.to_lowercase())),
            "expected {expected:?}, got {error:?}"
        );
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0)
            .expect("remove synthetic fixture without reading database bytes");
    }
}

#[derive(Clone, Copy)]
enum Route {
    Execute,
    Query,
    TransactionExecute,
    TransactionQuery,
}

const ROUTES: [Route; 4] = [
    Route::Execute,
    Route::Query,
    Route::TransactionExecute,
    Route::TransactionQuery,
];

fn call(route: Route, sql: &str) -> String {
    match route {
        Route::Execute => format!("sqlite::execute(&db, {sql:?}, []);"),
        Route::Query => format!("sqlite::query(&db, {sql:?}, [], {{}});"),
        Route::TransactionExecute | Route::TransactionQuery => format!(
            "sqlite::transaction(&db, [{{ sql: {sql:?}, query: {} }}]);",
            matches!(route, Route::TransactionQuery)
        ),
    }
}

fn configured_vm(policy: SqlitePolicy, source: &str) -> Result<Vm, VmError> {
    let catalog = standard_host_catalog();
    let compiled = compile_source_with_flavor_and_options(
        &format!("use sqlite;\n{source}"),
        vm::compiler::SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog.clone()),
    )
    .expect("typed RSS source should compile");
    let mut vm = Vm::try_new(compiled.program)?;
    let mut registry = HostFunctionRegistry::empty();
    register_sqlite_builtin_module_from_catalog(&mut registry, catalog.as_ref())?;
    registry.bind_vm_cached(&mut vm)?;
    vm.configure_sqlite(policy);
    Ok(vm)
}

fn run(policy: SqlitePolicy, source: &str) -> Result<(), VmError> {
    let mut vm = configured_vm(policy, source)?;
    let result = (|| {
        let mut status = vm.run()?;
        loop {
            match status {
                VmStatus::Halted => return Ok(()),
                VmStatus::Yielded => status = vm.resume()?,
                VmStatus::Waiting(_) => {
                    vm.wait_for_host_op_blocking()?;
                    status = vm.resume()?;
                }
            }
        }
    })();
    vm_reset::reset_for_reuse_to_ready(&mut vm).expect("scope reset must drain after SQL outcome");
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(vm.host_context().resource_count(), 0);
    assert!(vm.is_reusable());
    result
}

#[test]
fn execute_accepts_exact_revision_trigger_and_fires() {
    run(
        SqlitePolicy::default(),
        r#"
        let db = sqlite::open({ path: ":memory:", mode: "memory" });
        sqlite::execute(&db, "CREATE TABLE messages (id INTEGER)", []);
        sqlite::execute(&db, "CREATE TABLE revisions (revision INTEGER)", []);
        sqlite::execute(&db, "INSERT INTO revisions VALUES (0)", []);
        sqlite::execute(&db, "CREATE TRIGGER revision_insert AFTER INSERT ON messages BEGIN UPDATE revisions SET revision=revision+1; END", []);
        sqlite::execute(&db, "INSERT INTO messages VALUES (1)", []);
        let result = sqlite::query(&db, "SELECT revision FROM revisions", [], {});
        assert(result.rows[0].cells[0].int_value == 1);
        sqlite::close(db);
        "#,
    )
    .expect("one legal trigger must execute and fire without unsafe SQL");
}

#[test]
fn transaction_parses_against_earlier_schema_and_fires_all_revision_events() {
    for terminator in ["", ";"] {
        run(
            SqlitePolicy::default(),
            &format!(
                r#"
                let db = sqlite::open({{ path: ":memory:", mode: "memory" }});
                let results = sqlite::transaction(&db, [
                    {{ sql: "CREATE TABLE messages (id INTEGER)" }},
                    {{ sql: "CREATE INDEX message_ids ON messages(id)" }},
                    {{ sql: "CREATE TABLE revisions (revision INTEGER)" }},
                    {{ sql: "INSERT INTO revisions VALUES (0)" }},
                    {{ sql: "CREATE TRIGGER revision_insert AFTER INSERT ON messages BEGIN UPDATE revisions SET revision=revision+1; END{terminator}" }},
                    {{ sql: "CREATE TRIGGER revision_update AFTER UPDATE ON messages BEGIN UPDATE revisions SET revision=revision+1; END{terminator}" }},
                    {{ sql: "CREATE TRIGGER revision_delete AFTER DELETE ON messages BEGIN UPDATE revisions SET revision=revision+1; END{terminator}" }},
                    {{ sql: "INSERT INTO messages VALUES (1)" }},
                    {{ sql: "UPDATE messages SET id=2" }},
                    {{ sql: "DELETE FROM messages" }},
                    {{ sql: "SELECT revision FROM revisions", query: true }}
                ]);
                assert(results[10].query.rows[0].cells[0].int_value == 3);
                assert(results[8].execute.rows_affected == 1);
                let result = sqlite::query(&db, "SELECT count(*) FROM messages", [], {{}});
                assert(result.rows[0].cells[0].int_value == 0);
                sqlite::close(db);
                "#
            ),
        )
        .expect("ordered schema-dependent trigger transaction must commit");
    }
}

#[test]
fn sqlite_grammar_handles_case_end_quoted_identifiers_strings_and_comments() {
    for terminator in ["", "; /* tail END; */ -- tail ;"] {
        run(
            SqlitePolicy::default(),
            &format!(
                r#"
                let db = sqlite::open({{ path: ":memory:", mode: "memory" }});
                sqlite::execute(&db, "CREATE TABLE [messages;END] ([id] INTEGER)", []);
                sqlite::execute(&db, "CREATE TABLE revisions (revision INTEGER, note TEXT)", []);
                sqlite::execute(&db, "INSERT INTO revisions VALUES (0, '')", []);
                sqlite::execute(&db, "/* BEGIN; */ CREATE TRIGGER [trigger;END] AFTER INSERT ON [messages;END] BEGIN UPDATE revisions SET revision=CASE WHEN NEW.id=1 THEN revision+1 ELSE revision+2 END, note='END; ''quoted'' /* not comment */'; -- body END;\n UPDATE revisions SET revision=revision+1; END{terminator}", []);
                sqlite::execute(&db, "INSERT INTO [messages;END] VALUES (1); /* ; */", []);
                let result = sqlite::query(&db, "SELECT revision, note FROM revisions; -- ; END", [], {{}});
                assert(result.rows.copy()[0].cells[0].int_value == 2);
                assert(result.rows[0].cells[1].text_value == "END; 'quoted' /* not comment */");
                sqlite::close(db);
                "#
            ),
        )
        .expect("SQLite grammar must determine trigger boundaries");
    }
}

#[test]
fn every_route_rejects_trigger_tail_before_creating_trigger_or_dropping_table() {
    for unsafe_sql in [false, true] {
        for route in ROUTES {
            let db = Database::new();
            db.ok(r#"
                sqlite::execute(&db, "CREATE TABLE messages(id INTEGER)", []);
                sqlite::execute(&db, "CREATE TABLE revisions(revision INTEGER)", []);
                sqlite::execute(&db, "INSERT INTO revisions VALUES(0)", []);
            "#);
            for separator in [" ", " /* second END; */ ", " -- comment\n "] {
                db.rejected(
                    unsafe_sql,
                    &call(route, &format!("CREATE TRIGGER revision_insert AFTER INSERT ON messages BEGIN UPDATE revisions SET revision=revision+1; END;{separator}DROP TABLE messages")),
                    "multiple SQLite statements are not allowed",
                );
                db.ok(r#"
                    let schema = sqlite::query(&db, "SELECT count(*) FROM sqlite_schema WHERE type='trigger'", [], {});
                    assert(schema.rows[0].cells[0].int_value == 0);
                    let rows = sqlite::query(&db, "SELECT count(*) FROM messages", [], {});
                    assert(rows.rows[0].cells[0].int_value == 0);
                "#);
            }
        }
    }
}

#[test]
fn every_route_rejects_multiple_selects_even_with_unsafe_sql() {
    for unsafe_sql in [false, true] {
        for route in ROUTES {
            let db = Database::new();
            db.rejected(
                unsafe_sql,
                &call(route, "SELECT 1; SELECT 2"),
                "multiple SQLite statements are not allowed",
            );
            db.ok(r#"
                let result = sqlite::query(&db, "SELECT 7; /* only a comment */", [], {});
                assert(result.rows[0].cells[0].int_value == 7);
            "#);
        }
    }
}

#[test]
fn rejected_tails_never_apply_first_dml() {
    for route in ROUTES {
        let db = Database::new();
        db.ok(r#"sqlite::execute(&db, "CREATE TABLE messages(id INTEGER)", []);"#);
        for tail in [
            " /* gap */ INSERT INTO messages VALUES(2)",
            " -- gap\n SELECT 2",
            " nonsense",
            ";",
        ] {
            db.rejected(
                false,
                &call(route, &format!("INSERT INTO messages VALUES(1);{tail}")),
                "multiple SQLite statements are not allowed",
            );
            db.ok(r#"
                let rows = sqlite::query(&db, "SELECT count(*) FROM messages", [], {});
                assert(rows.rows[0].cells[0].int_value == 0);
            "#);
        }
    }
}

#[test]
fn invalid_later_transaction_statement_rolls_back_schema_and_all_prior_effects() {
    for query in [false, true] {
        let db = Database::new();
        db.ok(r#"
            sqlite::execute(&db, "CREATE TABLE messages(id INTEGER)", []);
            sqlite::execute(&db, "CREATE TABLE revisions(revision INTEGER)", []);
            sqlite::execute(&db, "INSERT INTO revisions VALUES(0)", []);
        "#);
        for invalid in [
            "INSERT INTO messages VALUES(2); SELECT 1",
            "INSERT INTO missing_table VALUES(2)",
        ] {
            let error = db.run(false, &format!(r#"
                sqlite::transaction(&db, [
                    {{ sql: "CREATE TABLE receipt(id INTEGER)" }},
                    {{ sql: "CREATE INDEX receipt_id ON receipt(id)" }},
                    {{ sql: "CREATE TRIGGER revision_insert AFTER INSERT ON messages BEGIN UPDATE revisions SET revision=revision+1; END" }},
                    {{ sql: "INSERT INTO messages VALUES(1)" }},
                    {{ sql: "INSERT INTO receipt VALUES(1)" }},
                    {{ sql: {invalid:?}, query: {query} }}
                ]);
            "#)).expect_err("invalid late statement must roll back");
            assert!(matches!(error, VmError::HostError(_)));
            db.ok(r#"
                let schema = sqlite::query(&db, "SELECT count(*) FROM sqlite_schema WHERE name IN ('receipt','receipt_id','revision_insert')", [], {});
                assert(schema.rows[0].cells[0].int_value == 0);
                let count = sqlite::query(&db, "SELECT count(*) FROM messages", [], {});
                assert(count.rows[0].cells[0].int_value == 0);
                let revision = sqlite::query(&db, "SELECT revision FROM revisions", [], {});
                assert(revision.rows[0].cells[0].int_value == 0);
            "#);
        }
    }
}

#[test]
fn before_guards_protect_committed_history_and_referenced_graphs() {
    let db = Database::new();
    db.ok(r#"
        sqlite::transaction(&db, [
            { sql: "CREATE TABLE managed_checkpoints(graph_ref_id TEXT, record_json TEXT)" },
            { sql: "CREATE TABLE managed_checkpoint_messages(content_json TEXT)" },
            { sql: "CREATE TABLE plugin_graph_refs(ref_id TEXT)" },
            { sql: "CREATE TRIGGER managed_checkpoint_immutable BEFORE UPDATE ON managed_checkpoints BEGIN SELECT RAISE(ABORT,'managed checkpoint immutable'); END" },
            { sql: "CREATE TRIGGER managed_checkpoint_message_immutable BEFORE UPDATE ON managed_checkpoint_messages BEGIN SELECT RAISE(ABORT,'managed checkpoint history immutable'); END" },
            { sql: "CREATE TRIGGER managed_checkpoint_ref_delete BEFORE DELETE ON plugin_graph_refs WHEN EXISTS(SELECT 1 FROM managed_checkpoints WHERE graph_ref_id=OLD.ref_id) BEGIN SELECT RAISE(ABORT,'managed checkpoint graph retained'); END" },
            { sql: "INSERT INTO managed_checkpoints VALUES('graph-a','original')" },
            { sql: "INSERT INTO managed_checkpoint_messages VALUES('history')" },
            { sql: "INSERT INTO plugin_graph_refs VALUES('graph-a'),('ordinary')" }
        ]);
    "#);
    db.rejected(
        false,
        &call(
            Route::Execute,
            "UPDATE managed_checkpoints SET record_json='tampered'",
        ),
        "managed checkpoint immutable",
    );
    db.rejected(
        false,
        &call(
            Route::Execute,
            "UPDATE managed_checkpoint_messages SET content_json='tampered'",
        ),
        "managed checkpoint history immutable",
    );
    db.rejected(
        false,
        &call(
            Route::Execute,
            "DELETE FROM plugin_graph_refs WHERE ref_id='graph-a'",
        ),
        "managed checkpoint graph retained",
    );
    db.ok(r#"
        let deleted = sqlite::execute(&db, "DELETE FROM plugin_graph_refs WHERE ref_id='ordinary'", []);
        assert(deleted.rows_affected == 1);
        let refs = sqlite::query(&db, "SELECT ref_id FROM plugin_graph_refs", [], {});
        assert(refs.rows[0].cells[0].text_value == "graph-a");
        let record = sqlite::query(&db, "SELECT record_json FROM managed_checkpoints", [], {});
        assert(record.rows[0].cells[0].text_value == "original");
        let history = sqlite::query(&db, "SELECT content_json FROM managed_checkpoint_messages", [], {});
        assert(history.rows[0].cells[0].text_value == "history");
    "#);
}

#[test]
fn application_checked_revision_overflow_aborts_triggering_write_atomically() {
    let db = Database::new();
    // SQLite promotes overflowing INTEGER arithmetic to REAL. The application
    // must constrain the stored type as well as its range; this is a synthetic
    // checked-counter schema, not a claim about any downstream migration.
    db.ok(r#"
        sqlite::transaction(&db, [
            { sql: "CREATE TABLE messages(id INTEGER)" },
            { sql: "CREATE TABLE revisions(revision INTEGER CHECK(typeof(revision)='integer' AND revision BETWEEN 0 AND 9223372036854775807))" },
            { sql: "INSERT INTO revisions VALUES(9223372036854775806)" },
            { sql: "CREATE TRIGGER revision_insert AFTER INSERT ON messages BEGIN UPDATE revisions SET revision=revision+1; END" },
            { sql: "CREATE TRIGGER revision_update AFTER UPDATE ON messages BEGIN UPDATE revisions SET revision=revision+1; END" },
            { sql: "CREATE TRIGGER revision_delete AFTER DELETE ON messages BEGIN UPDATE revisions SET revision=revision+1; END" },
            { sql: "INSERT INTO messages VALUES(1)" }
        ]);
    "#);
    for sql in [
        "INSERT INTO messages VALUES(2)",
        "UPDATE messages SET id=2",
        "DELETE FROM messages",
    ] {
        db.rejected(false, &call(Route::Execute, sql), "CHECK constraint failed");
        db.ok(r#"
            let revision = sqlite::query(&db, "SELECT revision, typeof(revision) FROM revisions", [], {});
            assert(revision.rows.copy()[0].cells[0].int_value == 9223372036854775807);
            assert(revision.rows[0].cells[1].text_value == "integer");
            let rows = sqlite::query(&db, "SELECT id FROM messages", [], {});
            assert(rows.rows[0].cells[0].int_value == 1);
            let count = sqlite::query(&db, "SELECT count(*) FROM messages", [], {});
            assert(count.rows[0].cells[0].int_value == 1);
        "#);
    }
}

#[test]
fn safe_policy_denies_privileged_actions_on_every_route() {
    for route in ROUTES {
        let db = Database::new();
        for sql in [
            "ATTACH DATABASE ':memory:' AS other",
            "DETACH DATABASE other",
            "PRAGMA foreign_keys=ON",
            "PRAGMA writable_schema=ON",
            "VACUUM",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT s",
            "RELEASE s",
            "CREATE VIRTUAL TABLE v USING fts5(value)",
            "SELECT load_extension('missing')",
            "SELECT \"load_extension\"('missing')",
        ] {
            let error = db
                .run(false, &call(route, sql))
                .expect_err("safe policy must deny privileged SQL");
            assert!(matches!(error, VmError::HostError(_)), "{sql}: {error:?}");
        }
        db.ok(r#"
            sqlite::execute(&db, "CREATE TABLE ordinary(id INTEGER)", []);
            sqlite::execute(&db, "INSERT INTO ordinary VALUES(1)", []);
            let rows = sqlite::query(&db, "SELECT count(*) FROM ordinary", [], {});
            assert(rows.rows[0].cells[0].int_value == 1);
        "#);
    }
}

#[test]
fn trigger_body_cannot_bypass_extension_authorization() {
    let db = Database::new();
    db.ok(r#"sqlite::execute(&db, "CREATE TABLE messages(id INTEGER)", []);"#);
    db.rejected(false, &call(Route::Execute, "CREATE TRIGGER denied AFTER INSERT ON messages BEGIN SELECT load_extension('missing'); END"), "extension loading is disabled");
    // SQLite defers compiling a trigger body until its triggering statement.
    // Quoting the function name avoids the lexical deny and proves that the
    // authorizer still rejects the body, including on the parser-probe path.
    db.ok(r#"sqlite::execute(&db, "CREATE TRIGGER deferred AFTER INSERT ON messages BEGIN SELECT \"load_extension\"('missing'); END", []);"#);
    db.rejected(
        false,
        &call(Route::Execute, "INSERT INTO messages VALUES(1)"),
        "not authorized",
    );
    db.ok(r#"
        let rows = sqlite::query(&db, "SELECT count(*) FROM messages", [], {});
        assert(rows.rows[0].cells[0].int_value == 0);
        sqlite::execute(&db, "CREATE TABLE recovery(id INTEGER)", []);
        sqlite::execute(&db, "INSERT INTO recovery VALUES(7)", []);
    "#);
}

#[test]
fn safe_policy_still_denies_drop_virtual_table() {
    let db = Database::new();
    // Existing unsafe embedding policy is used solely to construct a denied
    // feature fixture. Ordinary trigger support above always uses safe policy.
    db.run(
        true,
        &call(Route::Execute, "CREATE VIRTUAL TABLE v USING fts5(value)"),
    )
    .expect("bundled SQLite FTS5 fixture");
    db.rejected(
        false,
        &call(Route::Execute, "DROP TABLE v"),
        "AuthorizationForStatementDenied",
    );
    db.ok(r#"
        let schema = sqlite::query(&db, "SELECT count(*) FROM sqlite_schema WHERE name='v'", [], {});
        assert(schema.rows[0].cells[0].int_value == 1);
    "#);
}

#[test]
fn malformed_empty_nul_and_oversized_sql_still_fail_on_every_route() {
    for unsafe_sql in [false, true] {
        for route in ROUTES {
            let db = Database::new();
            for sql in [
                "",
                "  ",
                "-- only comment",
                "/* only comment */",
                ";",
                "SELECT 'unterminated",
                "SELECT [unterminated",
                "SELECT 1; /* unterminated",
                "SELECT 1\0; SELECT 2",
            ] {
                let error = db
                    .run(unsafe_sql, &call(route, sql))
                    .expect_err("invalid SQL must fail");
                assert!(matches!(error, VmError::HostError(_)), "{sql:?}: {error:?}");
            }
            db.ok(r#"
                let result = sqlite::query(&db, "SELECT 'valid; END'; -- trailing comment", [], {});
                assert(result.rows[0].cells[0].text_value == "valid; END");
            "#);
        }
    }
    for route in ROUTES {
        let sql = format!("SELECT 1 /* {} */", "x".repeat(128));
        let source = format!(
            r#"let db = sqlite::open({{ path: ":memory:", mode: "memory", limits: {{ max_statement_bytes: 64 }} }}); {}"#,
            call(route, &sql)
        );
        let error = run(SqlitePolicy::default(), &source).expect_err("statement byte ceiling");
        assert!(matches!(error, VmError::HostError(message) if message.contains("64 byte limit")));
    }
}

#[test]
fn reset_cancels_triggered_write_and_retires_worker_resources() {
    let db = Database::new();
    db.ok(r#"
        sqlite::execute(&db, "CREATE TABLE messages(id INTEGER)", []);
        sqlite::execute(&db, "CREATE TABLE revisions(revision INTEGER)", []);
        sqlite::execute(&db, "INSERT INTO revisions VALUES(0)", []);
        sqlite::execute(&db, "CREATE TRIGGER slow_insert AFTER INSERT ON messages BEGIN UPDATE revisions SET revision=(WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers LIMIT 2000000) SELECT sum(n) FROM numbers); END", []);
    "#);
    let mut vm = configured_vm(
        SqlitePolicy {
            database_root: Some(db.0.to_string_lossy().into_owned()),
            ..SqlitePolicy::default()
        },
        r#"
        let db = sqlite::open({ path: "state.db", mode: "read_write", limits: { max_transaction_ms: 5000, max_pending_operations: 1 } });
        sqlite::execute(&db, "INSERT INTO messages VALUES(1)", []);
        "#,
    ).expect("configured cancellation VM");
    assert!(matches!(
        vm.run().expect("schedule triggered insert"),
        VmStatus::Waiting(_)
    ));
    vm_reset::reset_for_reuse_to_ready(&mut vm).expect("cancel and drain triggered write");
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(vm.host_context().resource_count(), 0);
    assert!(vm.is_reusable());
    // The cancelled invocation has no reusable connection handle. A new
    // invocation reads the same file only through the formal SQLite API.
    db.ok(r#"
        let rows = sqlite::query(&db, "SELECT count(*) FROM messages", [], {});
        assert(rows.rows[0].cells[0].int_value == 0);
        let revision = sqlite::query(&db, "SELECT revision FROM revisions", [], {});
        assert(revision.rows[0].cells[0].int_value == 0);
        sqlite::execute(&db, "CREATE TABLE recovered(id INTEGER)", []);
        sqlite::execute(&db, "INSERT INTO recovered VALUES(1)", []);
    "#);
}

#[test]
fn required_v7_trigger_forms_execute_with_safe_policy() {
    let db = Database::new();
    // Reduced synthetic tables retain every referenced column. Trigger SQL is
    // the downstream v7 shape; this does not execute the downstream migration.
    db.ok(r#"
        sqlite::transaction(&db, [
            { sql: "CREATE TABLE sessions(id TEXT, generation INTEGER, metadata_json TEXT, model TEXT, provider TEXT, system_prompt TEXT, toolset_hash TEXT)" },
            { sql: "CREATE TABLE runs(id TEXT)" },
            { sql: "CREATE TABLE messages(session_id TEXT)" },
            { sql: "CREATE TABLE compactions(session_id TEXT)" },
            { sql: "CREATE TABLE managed_session_revisions(session_id TEXT PRIMARY KEY, revision INTEGER)" },
            { sql: "CREATE TABLE managed_checkpoints(graph_ref_id TEXT, source_session_id TEXT, source_run_id TEXT)" },
            { sql: "CREATE TABLE managed_checkpoint_messages(content_json TEXT)" },
            { sql: "CREATE TABLE plugin_graph_refs(ref_id TEXT)" },
            { sql: "CREATE TABLE managed_checkpoint_restores(new_run_id TEXT)" },
            { sql: "CREATE TABLE managed_run_bindings(run_id TEXT, graph_ref_id TEXT)" },
            { sql: "CREATE TRIGGER managed_message_insert AFTER INSERT ON messages WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id=NEW.session_id) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id=NEW.session_id; END" },
            { sql: "CREATE TRIGGER managed_message_update AFTER UPDATE ON messages WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id IN (NEW.session_id,OLD.session_id)) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id IN (NEW.session_id,OLD.session_id); END" },
            { sql: "CREATE TRIGGER managed_message_delete AFTER DELETE ON messages WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id=OLD.session_id) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id=OLD.session_id; END" },
            { sql: "CREATE TRIGGER managed_compaction_insert AFTER INSERT ON compactions WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id=NEW.session_id) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id=NEW.session_id; END" },
            { sql: "CREATE TRIGGER managed_compaction_update AFTER UPDATE ON compactions WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id IN (NEW.session_id,OLD.session_id)) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id IN (NEW.session_id,OLD.session_id); END" },
            { sql: "CREATE TRIGGER managed_compaction_delete AFTER DELETE ON compactions WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id=OLD.session_id) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id=OLD.session_id; END" },
            { sql: "CREATE TRIGGER managed_session_update AFTER UPDATE OF generation,metadata_json,model,provider,system_prompt,toolset_hash ON sessions WHEN EXISTS(SELECT 1 FROM managed_session_revisions WHERE session_id=NEW.id) BEGIN UPDATE managed_session_revisions SET revision=revision+1 WHERE session_id=NEW.id; END" },
            { sql: "CREATE TRIGGER managed_checkpoint_immutable BEFORE UPDATE ON managed_checkpoints BEGIN SELECT RAISE(ABORT,'managed checkpoint immutable'); END" },
            { sql: "CREATE TRIGGER managed_checkpoint_message_immutable BEFORE UPDATE ON managed_checkpoint_messages BEGIN SELECT RAISE(ABORT,'managed checkpoint history immutable'); END" },
            { sql: "CREATE TRIGGER managed_checkpoint_ref_delete BEFORE DELETE ON plugin_graph_refs WHEN EXISTS(SELECT 1 FROM managed_checkpoints WHERE graph_ref_id=OLD.ref_id) OR EXISTS(SELECT 1 FROM managed_checkpoint_restores r JOIN managed_run_bindings b ON b.run_id=r.new_run_id WHERE b.graph_ref_id=OLD.ref_id) BEGIN SELECT RAISE(ABORT,'managed checkpoint graph retained'); END" },
            { sql: "CREATE TRIGGER managed_checkpoint_source_delete BEFORE DELETE ON sessions WHEN EXISTS(SELECT 1 FROM managed_checkpoints WHERE source_session_id=OLD.id) BEGIN SELECT RAISE(ABORT,'managed checkpoint source retained'); END" },
            { sql: "CREATE TRIGGER managed_checkpoint_run_delete BEFORE DELETE ON runs WHEN EXISTS(SELECT 1 FROM managed_checkpoints WHERE source_run_id=OLD.id) BEGIN SELECT RAISE(ABORT,'managed checkpoint source run retained'); END" }
        ]);
        sqlite::execute(&db, "INSERT INTO sessions(id,generation) VALUES('s',0)", []);
        sqlite::execute(&db, "INSERT INTO runs VALUES('run')", []);
        sqlite::execute(&db, "INSERT INTO managed_session_revisions VALUES('s',0)", []);
        sqlite::execute(&db, "INSERT INTO messages VALUES('s')", []);
        sqlite::execute(&db, "UPDATE messages SET session_id='s'", []);
        sqlite::execute(&db, "DELETE FROM messages", []);
        sqlite::execute(&db, "INSERT INTO compactions VALUES('s')", []);
        sqlite::execute(&db, "UPDATE compactions SET session_id='s'", []);
        sqlite::execute(&db, "DELETE FROM compactions", []);
        sqlite::execute(&db, "UPDATE sessions SET generation=1 WHERE id='s'", []);
        let revision = sqlite::query(&db, "SELECT revision FROM managed_session_revisions", [], {});
        assert(revision.rows[0].cells[0].int_value == 7);
        sqlite::execute(&db, "INSERT INTO managed_checkpoints VALUES('checkpoint','s','run')", []);
        sqlite::execute(&db, "INSERT INTO managed_checkpoint_messages VALUES('original')", []);
        sqlite::execute(&db, "INSERT INTO plugin_graph_refs VALUES('checkpoint'),('restored')", []);
        sqlite::execute(&db, "INSERT INTO managed_checkpoint_restores VALUES('restored-run')", []);
        sqlite::execute(&db, "INSERT INTO managed_run_bindings VALUES('restored-run','restored')", []);
    "#);
    for (sql, expected) in [
        (
            "UPDATE managed_checkpoints SET graph_ref_id='tampered'",
            "managed checkpoint immutable",
        ),
        (
            "UPDATE managed_checkpoint_messages SET content_json='tampered'",
            "managed checkpoint history immutable",
        ),
        (
            "DELETE FROM plugin_graph_refs WHERE ref_id='checkpoint'",
            "managed checkpoint graph retained",
        ),
        (
            "DELETE FROM plugin_graph_refs WHERE ref_id='restored'",
            "managed checkpoint graph retained",
        ),
        (
            "DELETE FROM sessions WHERE id='s'",
            "managed checkpoint source retained",
        ),
        (
            "DELETE FROM runs WHERE id='run'",
            "managed checkpoint source run retained",
        ),
    ] {
        db.rejected(false, &call(Route::Execute, sql), expected);
    }
}
