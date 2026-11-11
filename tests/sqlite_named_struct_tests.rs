#![cfg(all(feature = "sqlite", not(target_family = "wasm")))]
//! SQLite fixed-shape host maps are named structs at the catalog/compiler
//! boundary. Runtime values remain maps; positional params, row cells, and
//! transaction results use named wrappers.

use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::host_api::{HostStructField, HostTypeSchema};
use vm::{
    CallReturn, CompiledProgram, HostAsyncBridge, HostFunctionRegistry, HostFuture,
    HostFutureOutput, HostOpId, SourcePathError, SqliteHostExt, SqlitePolicy, VmError, VmResult,
    register_sqlite_builtin_module, sqlite_host_catalog, standard_host_catalog,
};

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

    fn request_cancel_op(
        &mut self,
        op_id: HostOpId,
        _reason: vm::operation::OperationCancelReason,
    ) -> VmResult<()> {
        self.submitted.remove(&op_id);
        Ok(())
    }

    fn poll_cancel_op(&mut self, _op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn cleanup_op(&mut self, op_id: HostOpId, _terminal: vm::HostAsyncOpTerminal) -> VmResult<()> {
        self.submitted.remove(&op_id);
        Ok(())
    }
}

fn array(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(inner))
}

fn named(name: &str, fields: Vec<HostStructField>) -> HostTypeSchema {
    HostTypeSchema::named_struct(name, fields)
}

fn limits_fields() -> Vec<HostStructField> {
    [
        "max_connections",
        "max_statements",
        "max_rows",
        "max_columns",
        "max_result_bytes",
        "max_statement_bytes",
        "max_parameters",
        "max_parameter_bytes",
        "max_pending_operations",
        "max_transaction_ms",
        "busy_timeout_ms",
    ]
    .into_iter()
    .map(|name| HostStructField::new(name, option(HostTypeSchema::Int)))
    .collect()
}

fn open_options_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("path", option(HostTypeSchema::String)),
        HostStructField::new("mode", option(HostTypeSchema::String)),
        HostStructField::new("root", option(HostTypeSchema::String)),
        HostStructField::new("limits", option(named("SqliteLimits", limits_fields()))),
    ]
}

fn execute_result_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("rows_affected", HostTypeSchema::Int),
        HostStructField::new("last_insert_rowid", HostTypeSchema::Int),
    ]
}

fn sqlite_value_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("kind", HostTypeSchema::String),
        HostStructField::new("int_value", option(HostTypeSchema::Int)),
        HostStructField::new("float_value", option(HostTypeSchema::Float)),
        HostStructField::new("text_value", option(HostTypeSchema::String)),
        HostStructField::new("blob_value", option(HostTypeSchema::Bytes)),
    ]
}

fn row_fields() -> Vec<HostStructField> {
    vec![HostStructField::new(
        "cells",
        array(named("SqliteValue", sqlite_value_fields())),
    )]
}

fn query_result_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("columns", array(HostTypeSchema::String)),
        HostStructField::new("rows", array(named("SqliteRow", row_fields()))),
        HostStructField::new("truncated", HostTypeSchema::Bool),
        HostStructField::new("next_cursor", option(HostTypeSchema::Int)),
    ]
}

fn statement_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("sql", HostTypeSchema::String),
        HostStructField::new(
            "params",
            option(array(named("SqliteValue", sqlite_value_fields()))),
        ),
        HostStructField::new("query", option(HostTypeSchema::Bool)),
        HostStructField::new("limits", option(named("SqliteLimits", limits_fields()))),
    ]
}

fn transaction_result_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("kind", HostTypeSchema::String),
        HostStructField::new(
            "execute",
            option(named("SqliteExecuteResult", execute_result_fields())),
        ),
        HostStructField::new(
            "query",
            option(named("SqliteQueryResult", query_result_fields())),
        ),
    ]
}

fn compile_with(
    source: &str,
    catalog: Arc<vm::host_api::HostApiCatalog>,
) -> Result<CompiledProgram, SourcePathError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
}

fn compile(source: &str) -> Result<CompiledProgram, SourcePathError> {
    compile_with(source, sqlite_host_catalog())
}

fn compile_standard(source: &str) -> Result<CompiledProgram, SourcePathError> {
    compile_with(source, standard_host_catalog())
}

fn compile_err(source: &str) -> String {
    match compile(source) {
        Ok(_) => panic!("expected compile error for {source}"),
        Err(err) => err.to_string(),
    }
}

fn function_named<'a>(
    catalog: &'a vm::host_api::HostApiCatalog,
    name: &str,
) -> &'a vm::host_api::HostFunctionSchema {
    catalog
        .functions_named(name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing sqlite function {name}"))
}

fn schema_contains_map(schema: &HostTypeSchema) -> bool {
    match schema {
        HostTypeSchema::Map(_) | HostTypeSchema::Unknown => true,
        HostTypeSchema::Array(inner)
        | HostTypeSchema::Optional(inner)
        | HostTypeSchema::Option(inner) => schema_contains_map(inner),
        HostTypeSchema::Named { fields, .. } => {
            fields.iter().any(|field| schema_contains_map(&field.ty))
        }
        HostTypeSchema::Callable { params, result } => {
            params.iter().any(schema_contains_map) || schema_contains_map(result)
        }
        _ => false,
    }
}

#[test]
fn sqlite_catalog_declares_fixed_shape_named_structs() {
    let catalog = sqlite_host_catalog();
    for (name, fields) in [
        ("SqliteOpenOptions", open_options_fields()),
        ("SqliteLimits", limits_fields()),
        ("SqliteExecuteResult", execute_result_fields()),
        ("SqliteValue", sqlite_value_fields()),
        ("SqliteRow", row_fields()),
        ("SqliteQueryResult", query_result_fields()),
        ("SqliteStatement", statement_fields()),
        ("SqliteTransactionResult", transaction_result_fields()),
    ] {
        let schema = catalog
            .struct_named(name)
            .unwrap_or_else(|| panic!("sqlite catalog must declare {name}"));
        assert_eq!(schema.fields, fields, "{name} fields");
    }
}

#[test]
fn sqlite_function_schemas_use_named_structs_not_maps() {
    let catalog = sqlite_host_catalog();
    let open = function_named(&catalog, "sqlite::open");
    assert_eq!(
        open.params[0].ty,
        named("SqliteOpenOptions", open_options_fields())
    );

    let execute = function_named(&catalog, "sqlite::execute");
    assert_eq!(execute.params[2].name, "params");
    assert_eq!(
        execute.params[2].ty,
        array(named("SqliteValue", sqlite_value_fields()))
    );
    assert_eq!(
        execute.return_type,
        named("SqliteExecuteResult", execute_result_fields())
    );

    let query = function_named(&catalog, "sqlite::query");
    assert_eq!(query.params[2].name, "params");
    assert_eq!(
        query.params[2].ty,
        array(named("SqliteValue", sqlite_value_fields()))
    );
    assert_eq!(query.params[3].ty, named("SqliteLimits", limits_fields()));
    assert_eq!(
        query.return_type,
        named("SqliteQueryResult", query_result_fields())
    );

    let transaction = function_named(&catalog, "sqlite::transaction");
    assert_eq!(
        transaction.params[1].ty,
        array(named("SqliteStatement", statement_fields()))
    );
    assert_eq!(
        transaction.return_type,
        array(named(
            "SqliteTransactionResult",
            transaction_result_fields()
        ))
    );

    for function in catalog.functions() {
        assert!(
            !schema_contains_map(&function.return_type)
                && function
                    .params
                    .iter()
                    .all(|param| !schema_contains_map(&param.ty)),
            "{} must not keep HostTypeSchema::Map after the named-struct migration",
            function.name
        );
    }
}

#[test]
fn standard_catalog_includes_sqlite_named_structs() {
    let catalog = standard_host_catalog();
    assert!(
        catalog.struct_named("SqliteOpenOptions").is_some(),
        "standard catalog merge must copy sqlite named structs"
    );
    assert_eq!(
        function_named(&catalog, "sqlite::query").return_type,
        named("SqliteQueryResult", query_result_fields())
    );
}

#[test]
fn object_literal_open_query_and_statement_params_compile() {
    compile(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = {
            path: Option::Some(":memory:"),
            mode: Option::Some("memory"),
        };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let query_limits: SqliteLimits = { max_rows: Option::Some(8) };
        sqlite::query(&db, "SELECT a FROM t", [], query_limits);
        let statement: SqliteStatement = { sql: "INSERT INTO t VALUES (1)", query: Option::Some(false) };
        let statements: [SqliteStatement] = [statement];
        sqlite::transaction(&db, statements);
        sqlite::close(db);
        "#,
    )
    .expect("object literal sqlite params should compile");
}

#[test]
fn empty_open_and_limits_objects_compile() {
    compile(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = {};
        let db = sqlite::open(options);
        let limits: SqliteLimits = {};
        sqlite::query(&db, "SELECT 1", [], limits);
        sqlite::close(db);
        "#,
    )
    .expect("all-optional open/limits objects should compile");
}

#[test]
fn extra_field_on_open_options_is_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        sqlite::open({ path: Option::Some(":memory:"), extra: 1 });
        "#,
    );
    assert!(
        message.contains("sqlite::open") && message.contains("SqliteOpenOptions"),
        "extra field diagnostic should name the function and struct, got {message}"
    );
    assert!(
        message.contains("extra"),
        "extra field diagnostic should name the extra key, got {message}"
    );
}

#[test]
fn extra_field_on_a_sqlite_value_is_rejected_at_the_native_boundary() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        let value: SqliteValue = { kind: "null", nope: 2 };
        let values: [SqliteValue] = [value];
        sqlite::execute(&db, "SELECT 1", values);
        "#,
    )
    .expect("the extra payload field should compile into the typed value");
    let message = run_compiled_sqlite_host_error(compiled);
    assert!(
        message.contains("nope") && message.contains("unknown field"),
        "the native boundary must reject extra public value fields, got {message}"
    );
}

#[test]
fn missing_sql_on_transaction_statement_is_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        let statement: SqliteStatement = {};
        let statements: [SqliteStatement] = [statement];
        sqlite::transaction(&db, statements);
        "#,
    );
    assert!(
        message.contains("sql") && message.contains("required"),
        "missing sql diagnostic should name the missing required field, got {message}"
    );
}

#[test]
fn wrong_field_type_on_open_path_is_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        sqlite::open({ path: 1 });
        "#,
    );
    assert!(
        message.contains("sqlite::open") && message.contains("SqliteOpenOptions"),
        "wrong path type diagnostic should name the function and struct, got {message}"
    );
    assert!(
        message.contains("path") && message.contains("int"),
        "wrong path type diagnostic should mention path and int, got {message}"
    );
}

#[test]
fn typed_field_access_on_execute_and_query_results_compiles() {
    compile(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        let created = sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let affected = created.rows_affected;
        let rowid = created.last_insert_rowid;
        let limits: SqliteLimits = {};
        let queried = sqlite::query(&db, "SELECT a FROM t", [], limits);
        let columns = queried.columns;
        let rows = queried.rows;
        let truncated = queried.truncated;
        sqlite::close(db);
        affected + rowid;
        "#,
    )
    .expect("typed field access on sqlite result structs should compile");
}

#[test]
fn typed_params_rows_and_transaction_results_are_structured() {
    compile(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let params: [SqliteValue] = [{ kind: "int", int_value: Option::Some(7) }];
        sqlite::execute(&db, "INSERT INTO t VALUES (?)", params);
        let limits: SqliteLimits = {};
        let queried = sqlite::query(&db, "SELECT a FROM t", [], limits);
        let first = queried.rows[0].cells[0];
        assert(first.kind == "int");
        let first_value = match first.int_value {
            Option::Some(value) => value,
            Option::None => -1,
        };
        assert(first_value == 7);
        let statement: SqliteStatement = { sql: "SELECT a FROM t", query: Option::Some(true) };
        let statements: [SqliteStatement] = [statement];
        let results = sqlite::transaction(&db, statements);
        let first_result = results[0];
        assert(first_result.kind == "query");
        let present_value = match first_result.query {
            Option::Some(query_result) => match query_result.rows[0].cells[0].int_value {
                Option::Some(value) => value,
                Option::None => -1,
            },
            Option::None => -1,
        };
        assert(present_value == 7);
        sqlite::close(db);
        queried.truncated;
        "#,
    )
    .expect("typed params, rows, and transaction results should be indexable");
}

#[test]
fn raw_sqlite_scalar_params_are_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        sqlite::execute(&db, "SELECT ?", [7]);
        "#,
    );
    assert!(
        message.contains("sqlite::execute") && message.contains("SqliteValue"),
        "raw scalar parameter diagnostic should name the typed value contract, got {message}"
    );
}

#[test]
fn wrong_nested_sqlite_value_shapes_are_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        let values: [SqliteValue] = [{ kind: "int", int_value: Option::Some("wrong") }];
        let limits: SqliteLimits = {};
        sqlite::query(&db, "SELECT ?", values, limits);
        "#,
    );
    assert!(
        message.contains("int") && message.contains("string"),
        "wrong logical Option payload diagnostic should name the declared and actual types, got {message}"
    );
}

// ------------------------------------------------- logical Option/Unit RED

fn option(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Option(Box::new(inner))
}

#[test]
fn sqlite_nullable_fields_are_logical_option() {
    let catalog = sqlite_host_catalog();
    let query = catalog
        .struct_named("SqliteQueryResult")
        .expect("SqliteQueryResult");
    assert_eq!(
        query.fields[3].ty,
        option(HostTypeSchema::Int),
        "next_cursor must be a logical Option<int>"
    );
    let transaction = catalog
        .struct_named("SqliteTransactionResult")
        .expect("SqliteTransactionResult");
    assert_eq!(
        transaction.fields[1].ty,
        option(named("SqliteExecuteResult", execute_result_fields())),
        "the unselected execute branch must be a logical Option"
    );
    assert_eq!(
        transaction.fields[2].ty,
        option(named("SqliteQueryResult", query_result_fields())),
        "the unselected query branch must be a logical Option"
    );

    // SQL NULL cells stay the tagged `kind: "null"` business value with the
    // existing optional payload entries; they are not folded into Option.
    let value = catalog.struct_named("SqliteValue").expect("SqliteValue");
    assert_eq!(
        value.fields[1].ty,
        option(HostTypeSchema::Int),
        "SqliteValue keeps its tagged SQL NULL payload contract"
    );
    let statement = catalog
        .struct_named("SqliteStatement")
        .expect("SqliteStatement");
    assert_eq!(
        statement.fields[1].ty,
        option(array(named("SqliteValue", sqlite_value_fields()))),
        "SqliteStatement keeps its optional-entry params contract"
    );
}

#[test]
fn sqlite_close_contract_is_logical_unit() {
    let catalog = sqlite_host_catalog();
    assert_eq!(
        function_named(&catalog, "sqlite::close").return_type,
        HostTypeSchema::Unit,
        "sqlite::close is a void host call and must expose logical Unit"
    );
}

#[test]
fn logical_option_sqlite_consumer_compiles_and_runs() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let open_limits: SqliteLimits = { max_rows: Option::Some(8) };
        let options: SqliteOpenOptions = {
            path: Option::Some(":memory:"),
            mode: Option::Some("memory"),
            limits: Option::Some(open_limits),
        };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER, z TEXT)", []);
        let insert_params: [SqliteValue] = [
            { kind: "int", int_value: Option::Some(7) },
            { kind: "null" }
        ];
        sqlite::execute(&db, "INSERT INTO t (a, z) VALUES (?1, ?2)", insert_params);
        let query_limits: SqliteLimits = { max_rows: Option::Some(8) };
        let q = sqlite::query(&db, "SELECT a, z FROM t", [], query_limits);
        let cursor = match q.next_cursor {
            Option::Some(value) => value,
            Option::None => -1,
        };
        assert(cursor == 7);
        let cell = q.rows[0].cells[1];
        assert(cell.kind == "null");
        let text_absent = match cell.text_value {
            Option::Some(_) => false,
            Option::None => true,
        };
        assert(text_absent);
        let insert_statement_params: [SqliteValue] = [{ kind: "int", int_value: Option::Some(8) }];
        let insert_statement: SqliteStatement = { sql: "INSERT INTO t (a) VALUES (?1)", params: Option::Some(insert_statement_params) };
        let query_statement: SqliteStatement = { sql: "SELECT a FROM t", query: Option::Some(true) };
        let statements: [SqliteStatement] = [insert_statement, query_statement];
        let results = sqlite::transaction(&db, statements);
        let execute_absent = match results[0].query {
            Option::Some(result) => false,
            Option::None => true,
        };
        assert(execute_absent);
        let query_present = match results[1].query {
            Option::Some(result) => true,
            Option::None => false,
        };
        assert(query_present);
        let closed: () = sqlite::close(db);
        closed;
"#,
    )
    .expect("logical Option SQLite consumer should compile");
    run_compiled_sqlite(compiled);
}

fn noop_waker() -> Waker {
    struct LocalNoop;
    impl Wake for LocalNoop {
        fn wake(self: Arc<Self>) {}
    }
    Waker::from(Arc::new(LocalNoop))
}

fn drive_to_halt(vm: &mut vm::vm::Vm) {
    loop {
        match vm.run() {
            Ok(vm::vm::VmStatus::Halted) => return,
            Ok(vm::vm::VmStatus::Waiting(_)) => {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let mut stuck = 0u64;
                loop {
                    match vm.poll_waiting_host_op(&mut cx) {
                        Poll::Ready(Ok(())) => break,
                        Poll::Ready(Err(error)) => panic!("sqlite await failed: {error}"),
                        Poll::Pending => {
                            stuck += 1;
                            assert!(stuck < 100_000, "sqlite await should complete");
                            std::thread::yield_now();
                        }
                    }
                }
            }
            Ok(other) => panic!("unexpected vm status {other:?}"),
            Err(error) => panic!("sqlite script failed: {error}"),
        }
    }
}

fn run_compiled_sqlite(compiled: CompiledProgram) {
    let mut vm = vm::vm::Vm::try_new(compiled.program).expect("vm");
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    let mut registry = HostFunctionRegistry::empty();
    register_sqlite_builtin_module(&mut registry)
        .expect("sqlite exact registration should succeed");
    registry
        .bind_vm_cached(&mut vm)
        .expect("sqlite exact host imports should bind");
    vm.configure_sqlite(SqlitePolicy::default());
    drive_to_halt(&mut vm);
}

fn run_compiled_sqlite_host_error(compiled: CompiledProgram) -> String {
    let mut vm = vm::vm::Vm::try_new(compiled.program).expect("vm");
    vm.set_async_bridge(Box::<TokioHostDriver>::default())
        .expect("test async bridge should install");
    let mut registry = HostFunctionRegistry::empty();
    register_sqlite_builtin_module(&mut registry)
        .expect("sqlite exact registration should succeed");
    registry
        .bind_vm_cached(&mut vm)
        .expect("sqlite exact host imports should bind");
    vm.configure_sqlite(SqlitePolicy::default());
    let mut status = match vm.run() {
        Ok(status) => status,
        Err(VmError::HostError(message)) => return message,
        Err(other) => panic!("unexpected VM error: {other:?}"),
    };
    loop {
        status = match status {
            vm::vm::VmStatus::Halted => panic!("expected host error, got success"),
            vm::vm::VmStatus::Yielded => match vm.resume() {
                Ok(status) => status,
                Err(VmError::HostError(message)) => return message,
                Err(other) => panic!("unexpected VM error: {other:?}"),
            },
            vm::vm::VmStatus::Waiting(_) => {
                if let Err(error) = vm.wait_for_host_op_blocking() {
                    return match error {
                        VmError::HostError(message) => message,
                        other => format!("{other:?}"),
                    };
                }
                match vm.resume() {
                    Ok(status) => status,
                    Err(VmError::HostError(message)) => return message,
                    Err(other) => panic!("unexpected VM error: {other:?}"),
                }
            }
        };
    }
}

#[test]
fn runtime_execute_and_query_results_remain_maps_with_typed_fields() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let inserted = sqlite::execute(&db, "INSERT INTO t VALUES (9)", []);
        let limits: SqliteLimits = { max_rows: Option::Some(10) };
        let queried = sqlite::query(&db, "SELECT a FROM t", [], limits);
        let affected = inserted.rows_affected;
        let rowid = inserted.last_insert_rowid;
        let columns = queried.columns;
        let rows = queried.rows;
        let truncated = queried.truncated;
        sqlite::close(db);
        affected + rowid;
        "#,
    )
    .expect("runtime field-access script should compile");
    run_compiled_sqlite(compiled);
}

#[test]
fn documented_open_modes_compile() {
    compile(
        r#"
        use sqlite;
        let memory: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        sqlite::open(memory);
        let read_only: SqliteOpenOptions = { path: Option::Some("state.db"), mode: Option::Some("read_only") };
        sqlite::open(read_only);
        let read_write: SqliteOpenOptions = { path: Option::Some("state.db"), mode: Option::Some("read_write") };
        sqlite::open(read_write);
        let read_write_create: SqliteOpenOptions = { path: Option::Some("state.db"), mode: Option::Some("read_write_create") };
        sqlite::open(read_write_create);
        let default_mode: SqliteOpenOptions = { path: Option::Some("state.db") };
        sqlite::open(default_mode);
        "#,
    )
    .expect("exact open modes and omitted default should compile");
}

#[test]
fn explicit_none_option_configuration_fields_compile() {
    compile(
        r#"
        use sqlite;
        let no_root: Option<string> = Option::None;
        let no_open_limits: Option<SqliteLimits> = Option::None;
        let options: SqliteOpenOptions = {
            path: Option::Some(":memory:"),
            mode: Option::Some("memory"),
            root: no_root,
            limits: no_open_limits,
        };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let no_max_rows: Option<int> = Option::None;
        let query_limits: SqliteLimits = { max_rows: no_max_rows };
        let empty = sqlite::query(&db, "SELECT a FROM t", [], query_limits);
        let no_params: Option<[SqliteValue]> = Option::None;
        let no_query: Option<bool> = Option::None;
        let no_statement_limits: Option<SqliteLimits> = Option::None;
        let statement: SqliteStatement = {
            sql: "INSERT INTO t VALUES (1)",
            params: no_params,
            query: no_query,
            limits: no_statement_limits,
        };
        let statements: [SqliteStatement] = [statement];
        sqlite::transaction(&db, statements);
        sqlite::close(db);
        "#,
    )
    .expect("explicit None logical Option fields should compile");
}

#[test]
fn next_cursor_field_access_compiles() {
    compile(
        r#"
        use sqlite;
        let options: SqliteOpenOptions = { path: Option::Some(":memory:"), mode: Option::Some("memory") };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let limits: SqliteLimits = {};
        let queried = sqlite::query(&db, "SELECT a FROM t", [], limits);
        let cursor = queried.next_cursor;
        sqlite::close(db);
        "#,
    )
    .expect("optional next_cursor field access should compile");
}

#[test]
fn runtime_null_optional_fields_and_next_cursor_field_access() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let no_root: Option<string> = Option::None;
        let no_open_limits: Option<SqliteLimits> = Option::None;
        let options: SqliteOpenOptions = {
            path: Option::Some(":memory:"),
            mode: Option::Some("memory"),
            root: no_root,
            limits: no_open_limits,
        };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let no_max_rows: Option<int> = Option::None;
        let empty_limits: SqliteLimits = { max_rows: no_max_rows };
        let empty = sqlite::query(&db, "SELECT a FROM t", [], empty_limits);
        let empty_cursor = match empty.next_cursor {
            Option::Some(value) => value,
            Option::None => -1,
        };
        assert(empty_cursor == -1);
        let no_params: Option<[SqliteValue]> = Option::None;
        let no_query: Option<bool> = Option::None;
        let no_statement_limits: Option<SqliteLimits> = Option::None;
        let statement: SqliteStatement = {
            sql: "INSERT INTO t VALUES (9)",
            params: no_params,
            query: no_query,
            limits: no_statement_limits,
        };
        let statements: [SqliteStatement] = [statement];
        sqlite::transaction(&db, statements);
        let queried_limits: SqliteLimits = {};
        let queried = sqlite::query(&db, "SELECT a FROM t", [], queried_limits);
        let cursor = match queried.next_cursor {
            Option::Some(value) => value,
            Option::None => -2,
        };
        assert(cursor == 9);
        sqlite::close(db);
        empty_cursor + cursor;
        "#,
    )
    .expect("null optional fields and next_cursor access should compile");
    run_compiled_sqlite(compiled);
}

// ------------------------------------------- public field Option completion

fn schema_contains_legacy_optional(schema: &HostTypeSchema) -> bool {
    match schema {
        HostTypeSchema::Optional(_) => true,
        HostTypeSchema::Array(inner)
        | HostTypeSchema::Option(inner)
        | HostTypeSchema::Map(inner) => schema_contains_legacy_optional(inner),
        HostTypeSchema::Named { fields, .. } => fields
            .iter()
            .any(|field| schema_contains_legacy_optional(&field.ty)),
        HostTypeSchema::Callable { params, result } => {
            params.iter().any(schema_contains_legacy_optional)
                || schema_contains_legacy_optional(result)
        }
        _ => false,
    }
}

#[test]
fn sqlite_public_optional_fields_are_all_logical_option() {
    let catalog = sqlite_host_catalog();
    let limits = catalog.struct_named("SqliteLimits").expect("SqliteLimits");
    for field in &limits.fields {
        assert_eq!(
            field.ty,
            option(HostTypeSchema::Int),
            "SqliteLimits.{} must be a logical Option<int>",
            field.name
        );
    }

    let open = catalog
        .struct_named("SqliteOpenOptions")
        .expect("SqliteOpenOptions");
    for (index, expected) in [
        option(HostTypeSchema::String),
        option(HostTypeSchema::String),
        option(HostTypeSchema::String),
        option(named("SqliteLimits", limits_fields())),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            open.fields[index].ty, expected,
            "SqliteOpenOptions field {index}"
        );
    }

    let value = catalog.struct_named("SqliteValue").expect("SqliteValue");
    for (index, expected) in [
        option(HostTypeSchema::Int),
        option(HostTypeSchema::Float),
        option(HostTypeSchema::String),
        option(HostTypeSchema::Bytes),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            value.fields[index + 1].ty,
            expected,
            "SqliteValue payload field {index}"
        );
    }

    let statement = catalog
        .struct_named("SqliteStatement")
        .expect("SqliteStatement");
    assert_eq!(
        statement.fields[1].ty,
        option(array(named("SqliteValue", sqlite_value_fields()))),
        "SqliteStatement.params must be a logical Option"
    );
    assert_eq!(
        statement.fields[2].ty,
        option(HostTypeSchema::Bool),
        "SqliteStatement.query must be a logical Option"
    );
    assert_eq!(
        statement.fields[3].ty,
        option(named("SqliteLimits", limits_fields())),
        "SqliteStatement.limits must be a logical Option"
    );

    for function in catalog.functions() {
        assert!(
            !schema_contains_legacy_optional(&function.return_type)
                && function
                    .params
                    .iter()
                    .all(|param| !schema_contains_legacy_optional(&param.ty)),
            "{} must not keep a public legacy Optional field or result",
            function.name
        );
    }
}

#[test]
fn logical_option_configuration_and_value_consumers_run() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let open_limits: SqliteLimits = { max_rows: Option::Some(8) };
        let options: SqliteOpenOptions = {
            path: Option::Some(":memory:"),
            mode: Option::Some("memory"),
            limits: Option::Some(open_limits),
        };
        let db = sqlite::open(options);
        sqlite::execute(&db, "CREATE TABLE t (i INTEGER, f REAL, s TEXT, z INTEGER)", []);
        let insert_params: [SqliteValue] = [
            { kind: "int", int_value: Option::Some(7) },
            { kind: "float", float_value: Option::Some(1.5) },
            { kind: "text", text_value: Option::Some("") },
            { kind: "null" }
        ];
        sqlite::execute(
            &db,
            "INSERT INTO t (i, f, s, z) VALUES (?1, ?2, ?3, ?4)",
            insert_params
        );
        let query_limits: SqliteLimits = { max_rows: Option::Some(8) };
        let rows = sqlite::query(&db, "SELECT i, f, s, z FROM t", [], query_limits);
        let int_cell = rows.rows.copy()[0].cells.copy()[0];
        let int_value = match int_cell.int_value {
            Option::Some(value) => value,
            Option::None => -1,
        };
        let int_float_absent = match int_cell.float_value {
            Option::Some(_) => false,
            Option::None => true,
        };
        let float_cell = rows.rows.copy()[0].cells.copy()[1];
        let float_value = match float_cell.float_value {
            Option::Some(value) => value,
            Option::None => -1.0,
        };
        let text_cell = rows.rows.copy()[0].cells.copy()[2];
        let text_value = match text_cell.text_value {
            Option::Some(value) => value,
            Option::None => "missing",
        };
        let null_cell = rows.rows.copy()[0].cells.copy()[3];
        let null_absent = match null_cell.int_value {
            Option::Some(_) => false,
            Option::None => true,
        };
        assert(int_value == 7 && int_float_absent);
        assert(float_value == 1.5);
        assert(text_value == "");
        assert(null_cell.kind == "null" && null_absent);
        let insert_statement_params: [SqliteValue] = [{ kind: "int", int_value: Option::Some(8) }];
        let empty_params: [SqliteValue] = [];
        let statement_limits: SqliteLimits = { max_rows: Option::Some(8) };
        let insert_statement: SqliteStatement = {
            sql: "INSERT INTO t (i) VALUES (?1)",
            params: Option::Some(insert_statement_params),
            query: Option::Some(false),
        };
        let query_statement: SqliteStatement = {
            sql: "SELECT i FROM t",
            params: Option::Some(empty_params),
            query: Option::Some(true),
            limits: Option::Some(statement_limits),
        };
        let default_statement: SqliteStatement = { sql: "INSERT INTO t (i) VALUES (9)" };
        let statements: [SqliteStatement] = [insert_statement, query_statement, default_statement];
        let results = sqlite::transaction(&db, statements);
        assert(results.copy()[0].kind == "execute");
        assert(results.copy()[1].kind == "query");
        assert(results.copy()[2].kind == "execute");
        let present = match results.copy()[1].query {
            Option::Some(result) => result.rows.length,
            Option::None => 0,
        };
        assert(present == 2);
        sqlite::close(db);
        "#,
    )
    .expect("logical Option sqlite consumers should compile");
    run_compiled_sqlite(compiled);
}

#[test]
fn legacy_scalar_and_physical_null_public_fields_are_rejected() {
    for source in [
        r#"use sqlite; sqlite::open({ path: ":memory:", mode: Option::Some("memory") });"#,
        r#"use sqlite; sqlite::open({ path: Option::Some(":memory:"), mode: ":memory:" });"#,
        r#"use sqlite; let db = sqlite::open({ path: Option::Some(":memory:"), mode: Option::Some("memory") }); sqlite::execute(&db, "SELECT ?", [{ kind: "int", int_value: 7, float_value: Option::None, text_value: Option::None, blob_value: Option::None }]);"#,
        r#"use sqlite; let db = sqlite::open({ path: Option::Some(":memory:"), mode: Option::Some("memory") }); sqlite::execute(&db, "SELECT ?", [{ kind: "int", int_value: null, float_value: Option::None, text_value: Option::None, blob_value: Option::None }]);"#,
        r#"use sqlite; let db = sqlite::open({ path: Option::Some(":memory:"), mode: Option::Some("memory") }); sqlite::query(&db, "SELECT 1", [], { max_rows: 8 });"#,
    ] {
        let message = compile_err(source);
        assert!(
            message.contains("sqlite") && message.contains("expected"),
            "legacy field reading must be rejected, got {message}"
        );
    }
}

#[test]
fn logical_option_limit_zero_is_a_provided_invalid_value() {
    let compiled = compile_standard(
        r#"
        let options: SqliteOpenOptions = {
            path: Option::Some(":memory:"),
            mode: Option::Some("memory"),
        };
        let db = sqlite::open(options);
        let limits: SqliteLimits = { max_rows: Option::Some(0) };
        sqlite::query(&db, "SELECT 1", [], limits);
        "#,
    )
    .expect("the explicit Some(0) limit should compile");
    let message = run_compiled_sqlite_host_error(compiled);
    assert!(
        message.contains("max_rows must be positive"),
        "Some(0) must be a provided invalid limit rather than the default, got {message}"
    );
}
