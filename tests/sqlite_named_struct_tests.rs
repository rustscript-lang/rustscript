#![cfg(feature = "sqlite")]
//! SQLite fixed-shape host maps are named structs at the catalog/compiler
//! boundary. Runtime values remain maps; positional params and row arrays stay
//! dynamic.

use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::host_api::{HostStructField, HostTypeSchema};
use vm::{
    CompiledProgram, SourcePathError, SqliteExtension, SqliteHostExt, SqlitePolicy,
    sqlite_host_catalog, standard_host_catalog,
};

fn opt(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Optional(Box::new(inner))
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
    .map(|name| HostStructField::new(name, opt(HostTypeSchema::Int)))
    .collect()
}

fn open_options_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("path", opt(HostTypeSchema::String)),
        HostStructField::new("mode", opt(HostTypeSchema::String)),
        HostStructField::new("root", opt(HostTypeSchema::String)),
        HostStructField::new("limits", opt(named("SqliteLimits", limits_fields()))),
    ]
}

fn execute_result_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("rows_affected", HostTypeSchema::Int),
        HostStructField::new("last_insert_rowid", HostTypeSchema::Int),
    ]
}

fn query_result_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("columns", array(HostTypeSchema::String)),
        HostStructField::new("rows", array(array(HostTypeSchema::Unknown))),
        HostStructField::new("truncated", HostTypeSchema::Bool),
        HostStructField::new("next_cursor", opt(HostTypeSchema::Int)),
    ]
}

fn statement_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("sql", HostTypeSchema::String),
        HostStructField::new("params", opt(array(HostTypeSchema::Unknown))),
        HostStructField::new("query", opt(HostTypeSchema::Bool)),
        HostStructField::new("limits", opt(named("SqliteLimits", limits_fields()))),
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
        HostTypeSchema::Map(_) => true,
        HostTypeSchema::Array(inner) | HostTypeSchema::Optional(inner) => {
            schema_contains_map(inner)
        }
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
        ("SqliteQueryResult", query_result_fields()),
        ("SqliteStatement", statement_fields()),
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
    assert_eq!(execute.params[2].ty, HostTypeSchema::Unknown);
    assert_eq!(
        execute.return_type,
        named("SqliteExecuteResult", execute_result_fields())
    );

    let query = function_named(&catalog, "sqlite::query");
    assert_eq!(query.params[2].name, "params");
    assert_eq!(query.params[2].ty, HostTypeSchema::Unknown);
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
    assert_eq!(transaction.return_type, array(HostTypeSchema::Unknown));

    let rows_affected = function_named(&catalog, "sqlite::rows_affected");
    assert_eq!(
        rows_affected.params[0].ty,
        named("SqliteExecuteResult", execute_result_fields())
    );
    let truncated = function_named(&catalog, "sqlite::truncated");
    assert_eq!(
        truncated.params[0].ty,
        named("SqliteQueryResult", query_result_fields())
    );
    let next_cursor = function_named(&catalog, "sqlite::next_cursor");
    assert_eq!(
        next_cursor.params[0].ty,
        named("SqliteQueryResult", query_result_fields())
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
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        sqlite::query(&db, "SELECT a FROM t", [], { max_rows: 8 });
        sqlite::transaction(&db, [{ sql: "INSERT INTO t VALUES (1)", query: false }]);
        sqlite::close(&db);
        "#,
    )
    .expect("object literal sqlite params should compile");
}

#[test]
fn empty_open_and_limits_objects_compile() {
    compile(
        r#"
        use sqlite;
        let db = sqlite::open({});
        sqlite::query(&db, "SELECT 1", [], {});
        sqlite::close(&db);
        "#,
    )
    .expect("all-optional open/limits objects should compile");
}

#[test]
fn extra_field_on_open_options_is_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        sqlite::open({ path: ":memory:", extra: 1 });
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
fn extra_field_on_limits_is_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory" });
        sqlite::query(&db, "SELECT 1", [], { max_rows: 1, nope: 2 });
        "#,
    );
    assert!(
        message.contains("sqlite::query") && message.contains("SqliteLimits"),
        "extra limits field diagnostic should name the function and struct, got {message}"
    );
    assert!(
        message.contains("nope"),
        "extra limits field diagnostic should name the extra key, got {message}"
    );
}

#[test]
fn missing_sql_on_transaction_statement_is_rejected() {
    let message = compile_err(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory" });
        sqlite::transaction(&db, [{ params: [] }]);
        "#,
    );
    assert!(
        message.contains("sqlite::transaction") && message.contains("SqliteStatement"),
        "missing sql diagnostic should name the function and struct, got {message}"
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
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        let created = sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let affected = created.rows_affected;
        let rowid = created.last_insert_rowid;
        let queried = sqlite::query(&db, "SELECT a FROM t", [], {});
        let columns = queried.columns;
        let rows = queried.rows;
        let truncated = queried.truncated;
        sqlite::close(&db);
        affected + rowid;
        "#,
    )
    .expect("typed field access on sqlite result structs should compile");
}

#[test]
fn positional_params_and_row_arrays_remain_dynamic() {
    compile(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory" });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        sqlite::execute(&db, "INSERT INTO t VALUES (?)", [7]);
        let queried = sqlite::query(&db, "SELECT a FROM t", [], {});
        let first = queried.rows[0];
        sqlite::close(&db);
        queried.truncated;
        "#,
    )
    .expect("positional params and row arrays must stay indexable");
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

#[test]
fn runtime_execute_and_query_results_remain_maps_with_typed_fields() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let inserted = sqlite::execute(&db, "INSERT INTO t VALUES (9)", []);
        let queried = sqlite::query(&db, "SELECT a FROM t", [], { max_rows: 10 });
        let affected = inserted.rows_affected;
        let rowid = inserted.last_insert_rowid;
        let columns = queried.columns;
        let rows = queried.rows;
        let truncated = queried.truncated;
        sqlite::close(&db);
        affected + rowid;
        "#,
    )
    .expect("runtime field-access script should compile");
    let mut vm = vm::vm::Vm::try_new(compiled.program).expect("vm");
    vm.install_extension(&SqliteExtension)
        .expect("sqlite extension");
    vm.configure_sqlite(SqlitePolicy::default());
    drive_to_halt(&mut vm);
}

#[test]
fn runtime_query_envelope_exposes_map_keys() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        sqlite::execute(&db, "INSERT INTO t VALUES (3)", []);
        let queried = sqlite::query(&db, "SELECT a FROM t", [], {});
        let truncated = sqlite::truncated(queried);
        sqlite::close(&db);
        truncated;
        "#,
    )
    .expect("query envelope helper script should compile");
    let mut vm = vm::vm::Vm::try_new(compiled.program).expect("vm");
    vm.install_extension(&SqliteExtension)
        .expect("sqlite extension");
    drive_to_halt(&mut vm);
}

#[test]
fn documented_open_modes_compile() {
    compile(
        r#"
        use sqlite;
        sqlite::open({ path: ":memory:", mode: "memory" });
        sqlite::open({ path: "state.db", mode: "read_only" });
        sqlite::open({ path: "state.db", mode: "read_write" });
        sqlite::open({ path: "state.db", mode: "read_write_create" });
        sqlite::open({ path: "state.db" });
        "#,
    )
    .expect("exact open modes and omitted default should compile");
}

#[test]
fn optional_null_fields_compile_as_omitted() {
    compile(
        r#"
        use sqlite;
        let db = sqlite::open({
            path: ":memory:",
            mode: "memory",
            root: null,
            limits: null,
        });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        sqlite::query(&db, "SELECT a FROM t", [], { max_rows: null });
        sqlite::transaction(&db, [{
            sql: "INSERT INTO t VALUES (1)",
            params: null,
            query: null,
            limits: null,
        }]);
        sqlite::close(&db);
        "#,
    )
    .expect("present Null on optional sqlite fields should compile");
}

#[test]
fn next_cursor_field_access_compiles() {
    compile(
        r#"
        use sqlite;
        let db = sqlite::open({ path: ":memory:", mode: "memory", limits: {} });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let queried = sqlite::query(&db, "SELECT a FROM t", [], {});
        let cursor = queried.next_cursor;
        sqlite::close(&db);
        "#,
    )
    .expect("optional next_cursor field access should compile");
}

#[test]
fn runtime_null_optional_fields_and_next_cursor_field_access() {
    let compiled = compile_standard(
        r#"
        use sqlite;
        let db = sqlite::open({
            path: ":memory:",
            mode: "memory",
            root: null,
            limits: null,
        });
        sqlite::execute(&db, "CREATE TABLE t (a INTEGER)", []);
        let empty = sqlite::query(&db, "SELECT a FROM t", [], { max_rows: null });
        let empty_cursor = empty.next_cursor;
        let empty_again = sqlite::query(&db, "SELECT a FROM t", [], {});
        let empty_helper = sqlite::next_cursor(empty_again);
        sqlite::transaction(&db, [{
            sql: "INSERT INTO t VALUES (9)",
            params: null,
            query: null,
            limits: null,
        }]);
        let queried = sqlite::query(&db, "SELECT a FROM t", [], {});
        let cursor = queried.next_cursor;
        sqlite::close(&db);
        empty_cursor;
        empty_helper;
        cursor;
        "#,
    )
    .expect("null optional fields and next_cursor access should compile");
    let mut vm = vm::vm::Vm::try_new(compiled.program).expect("vm");
    vm.install_extension(&SqliteExtension)
        .expect("sqlite extension");
    vm.configure_sqlite(SqlitePolicy::default());
    drive_to_halt(&mut vm);
}
