use std::any::Any;
use std::collections::BTreeSet;

#[cfg(all(
    feature = "runtime",
    feature = "http-client",
    not(target_family = "wasm")
))]
use vm::http_host_catalog;
#[cfg(feature = "runtime")]
use vm::sqlite_host_catalog;
#[cfg(feature = "runtime")]
use vm::{HostApiCatalog, jit_host_catalog, standard_host_catalog};
use vm::{HostStructField, HostTypeSchema};

fn assert_no_public_dynamic_root(path: &str, schema: &HostTypeSchema) {
    fn visit(path: &str, schema: &HostTypeSchema, seen: &mut BTreeSet<String>) {
        match schema {
            HostTypeSchema::Map(_) | HostTypeSchema::Unknown => {
                panic!("public host schema {path} exposes {schema:?}")
            }
            HostTypeSchema::Array(inner) => visit(&format!("{path}[]"), inner, seen),
            HostTypeSchema::Optional(inner) => visit(&format!("{path}?"), inner, seen),
            HostTypeSchema::Named { name, fields } => {
                if !seen.insert(name.clone()) {
                    return;
                }
                for field in fields {
                    visit(&format!("{path}.{name}.{}", field.name), &field.ty, seen);
                }
            }
            HostTypeSchema::Callable { params, result } => {
                for (index, param) in params.iter().enumerate() {
                    visit(&format!("{path}.callback_param[{index}]"), param, seen);
                }
                visit(&format!("{path}.callback_result"), result, seen);
            }
            HostTypeSchema::Null
            | HostTypeSchema::Bool
            | HostTypeSchema::Int
            | HostTypeSchema::Float
            | HostTypeSchema::Number
            | HostTypeSchema::String
            | HostTypeSchema::Bytes
            | HostTypeSchema::Resource(_) => {}
        }
    }

    visit(path, schema, &mut BTreeSet::new());
}

fn panic_text(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    "non-string panic payload".to_string()
}

fn assert_rejects_dynamic_schema(
    path: &str,
    schema: HostTypeSchema,
    expected_path: &str,
    expected_kind: &str,
) {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_public_dynamic_root(path, &schema);
    }))
    .expect_err("dynamic schema must be rejected");
    let message = panic_text(payload);
    assert!(
        message.contains(expected_path),
        "diagnostic should identify {expected_path}, got {message}"
    );
    assert!(
        message.contains(expected_kind),
        "diagnostic should identify {expected_kind}, got {message}"
    );
}

type SchemaWrapper = fn(HostTypeSchema) -> HostTypeSchema;

fn array_of(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(inner))
}

fn optional_of(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Optional(Box::new(inner))
}

fn named_field_of(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::named_struct("Envelope", vec![HostStructField::new("payload", inner)])
}

fn callable_param_of(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Callable {
        params: vec![inner],
        result: Box::new(HostTypeSchema::Int),
    }
}

fn callable_result_of(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Callable {
        params: vec![HostTypeSchema::Int],
        result: Box::new(inner),
    }
}

const NESTED_DYNAMIC_CASES: &[(&str, SchemaWrapper, &str)] = &[
    ("array", array_of, "[]"),
    ("optional", optional_of, "?"),
    ("named field", named_field_of, ".Envelope.payload"),
    ("callable param", callable_param_of, ".callback_param[0]"),
    ("callable result", callable_result_of, ".callback_result"),
];

#[test]
fn recursive_walker_rejects_nested_dynamic_schemas_with_paths() {
    for (kind, dynamic) in [
        ("Map", HostTypeSchema::Map(Box::new(HostTypeSchema::Int))),
        ("Unknown", HostTypeSchema::Unknown),
    ] {
        for (case, wrap, suffix) in NESTED_DYNAMIC_CASES {
            let root = format!("nested::{kind}::{case}");
            let expected_path = format!("{root}{suffix}");
            assert_rejects_dynamic_schema(&root, wrap(dynamic.clone()), &expected_path, kind);
        }
    }
}

#[cfg(feature = "runtime")]
fn assert_no_public_dynamic_schema(catalog_name: &str, catalog: &HostApiCatalog) {
    for schema in catalog.structs() {
        for field in &schema.fields {
            assert_no_public_dynamic_root(
                &format!("{catalog_name}::{}.{}", schema.name, field.name),
                &field.ty,
            );
        }
    }
    for function in catalog.functions() {
        for param in &function.params {
            assert_no_public_dynamic_root(
                &format!("{catalog_name}::{}({})", function.name, param.name),
                &param.ty,
            );
        }
        assert_no_public_dynamic_root(
            &format!("{catalog_name}::{} return", function.name),
            &function.return_type,
        );
    }
}

fn recursive_named_schema() -> HostTypeSchema {
    HostTypeSchema::named_struct(
        "RecursiveNode",
        vec![
            HostStructField::new("value", HostTypeSchema::Int),
            HostStructField::new(
                "next",
                HostTypeSchema::Optional(Box::new(HostTypeSchema::named_struct(
                    "RecursiveNode",
                    Vec::new(),
                ))),
            ),
        ],
    )
}

#[test]
fn recursive_named_struct_walk_stops_at_repeated_named_type() {
    assert_no_public_dynamic_root("recursive::node", &recursive_named_schema());
}

#[cfg(feature = "runtime")]
#[test]
fn affected_public_host_catalogs_have_no_reachable_map_or_unknown() {
    assert_no_public_dynamic_schema("jit", &jit_host_catalog());
    assert_no_public_dynamic_schema("standard", &standard_host_catalog());
    #[cfg(all(feature = "http-client", not(target_family = "wasm")))]
    assert_no_public_dynamic_schema("http", &http_host_catalog());
    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    assert_no_public_dynamic_schema("sqlite", &sqlite_host_catalog());
}

#[cfg(all(feature = "runtime", not(feature = "sqlite")))]
#[test]
fn standard_catalog_keeps_sqlite_editor_schema_without_sqlite_runtime() {
    let sqlite = sqlite_host_catalog();
    assert!(
        sqlite.function("sqlite::query").is_some(),
        "the standalone editor/compiler catalog must retain SQLite declarations"
    );
    let query = sqlite
        .function("sqlite::query")
        .expect("SQLite query declaration");
    let value = sqlite
        .struct_named("SqliteValue")
        .expect("SQLite value named struct");
    assert_eq!(
        query.params[2].ty,
        HostTypeSchema::Array(Box::new(value.as_type())),
        "SQLite query params must stay typed without the runtime feature"
    );
    assert_no_public_dynamic_schema("sqlite", &sqlite);

    let catalog = standard_host_catalog();
    assert!(
        catalog.function("sqlite::open").is_some(),
        "the editor/compiler catalog must retain SQLite schema declarations"
    );
    assert!(
        catalog.struct_named("SqliteOpenOptions").is_some(),
        "the editor/compiler catalog must retain SQLite named structs"
    );
    assert!(
        vm::default_host_callables()
            .iter()
            .all(|callable| !callable.name.starts_with("sqlite::")),
        "the executable default host surface must remain feature-gated"
    );
}
