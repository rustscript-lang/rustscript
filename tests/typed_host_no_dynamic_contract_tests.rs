use std::any::Any;
use std::collections::BTreeSet;

#[cfg(all(
    feature = "runtime",
    feature = "http-client",
    not(target_family = "wasm")
))]
use vm::http_host_catalog;
#[cfg(all(feature = "runtime", feature = "sqlite", not(target_family = "wasm")))]
use vm::sqlite_host_catalog;
#[cfg(feature = "runtime")]
use vm::{HostApiCatalog, jit_host_catalog, standard_host_catalog, timer_host_catalog};
use vm::{HostStructField, HostTypeSchema};

/// How the recursive schema walker treats dynamic (`Map`/`Unknown`)
/// occurrences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DynamicPolicy {
    /// No `Map` or `Unknown` anywhere: the schema is fully typed.
    Strict,
    /// The narrow discarded-callable-result exception.
    ///
    /// A callable's *result* may be `Unknown`, because the receiving host
    /// discards the callback result and therefore declares no type for it
    /// (the standard timer surface's callback is the reference case). Every
    /// other dynamic occurrence — a `Map` result, an `Unknown` nested inside
    /// a result, an `Unknown` parameter, and any `Map`/`Unknown` outside a
    /// callable result — stays rejected.
    AllowDiscardedCallableResults,
}

impl DynamicPolicy {
    fn allows_discarded_callable_results(self) -> bool {
        matches!(self, Self::AllowDiscardedCallableResults)
    }
}

fn visit(path: &str, schema: &HostTypeSchema, seen: &mut BTreeSet<String>, policy: DynamicPolicy) {
    match schema {
        HostTypeSchema::Map(_) | HostTypeSchema::Unknown => {
            panic!("public host schema {path} exposes {schema:?}")
        }
        HostTypeSchema::Array(inner) => visit(&format!("{path}[]"), inner, seen, policy),
        HostTypeSchema::Optional(inner) => visit(&format!("{path}?"), inner, seen, policy),
        HostTypeSchema::Named { name, fields } => {
            if !seen.insert(name.clone()) {
                return;
            }
            for field in fields {
                visit(
                    &format!("{path}.{name}.{}", field.name),
                    &field.ty,
                    seen,
                    policy,
                );
            }
        }
        HostTypeSchema::Callable { params, result } => {
            for (index, param) in params.iter().enumerate() {
                visit(
                    &format!("{path}.callback_param[{index}]"),
                    param,
                    seen,
                    policy,
                );
            }
            if policy.allows_discarded_callable_results()
                && matches!(result.as_ref(), HostTypeSchema::Unknown)
            {
                // The one allowed occurrence: a directly declared `Unknown`
                // callable result whose value the host discards.
                return;
            }
            visit(&format!("{path}.callback_result"), result, seen, policy);
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

fn assert_no_public_dynamic_root(path: &str, schema: &HostTypeSchema) {
    visit(path, schema, &mut BTreeSet::new(), DynamicPolicy::Strict);
}

fn assert_no_public_dynamic_root_with_policy(
    path: &str,
    schema: &HostTypeSchema,
    policy: DynamicPolicy,
) {
    visit(path, schema, &mut BTreeSet::new(), policy);
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

fn assert_rejects_dynamic_schema_with_policy(
    path: &str,
    schema: HostTypeSchema,
    expected_path: &str,
    expected_kind: &str,
    policy: DynamicPolicy,
) {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_public_dynamic_root_with_policy(path, &schema, policy);
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

fn assert_rejects_dynamic_schema(
    path: &str,
    schema: HostTypeSchema,
    expected_path: &str,
    expected_kind: &str,
) {
    assert_rejects_dynamic_schema_with_policy(
        path,
        schema,
        expected_path,
        expected_kind,
        DynamicPolicy::Strict,
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

/// The discarded-callable-result exception accepts exactly one occurrence:
/// a directly declared `Unknown` callable result. Every neighbouring dynamic
/// occurrence stays rejected under the same policy.
#[test]
fn discarded_callable_result_unknown_is_a_narrow_policy_exception() {
    let policy = DynamicPolicy::AllowDiscardedCallableResults;

    // The exception itself: the timer-shaped callback surface.
    assert_no_public_dynamic_root_with_policy(
        "exception::callback",
        &HostTypeSchema::Callable {
            params: vec![HostTypeSchema::Bool],
            result: Box::new(HostTypeSchema::Unknown),
        },
        policy,
    );

    // Narrowness: a map result, a nested unknown result, an unknown
    // parameter, and dynamic roots stay rejected under the same policy.
    for (case, schema, expected_path, expected_kind) in [
        (
            "map result",
            callable_result_of(HostTypeSchema::Map(Box::new(HostTypeSchema::Int))),
            ".callback_result",
            "Map",
        ),
        (
            "nested unknown result",
            callable_result_of(HostTypeSchema::Array(Box::new(HostTypeSchema::Unknown))),
            ".callback_result[]",
            "Unknown",
        ),
        (
            "unknown parameter",
            callable_param_of(HostTypeSchema::Unknown),
            ".callback_param[0]",
            "Unknown",
        ),
        ("unknown root", HostTypeSchema::Unknown, "", "Unknown"),
        (
            "map root",
            HostTypeSchema::Map(Box::new(HostTypeSchema::Int)),
            "",
            "Map",
        ),
        (
            "unknown named field",
            named_field_of(HostTypeSchema::Unknown),
            ".Envelope.payload",
            "Unknown",
        ),
    ] {
        let root = format!("exception::narrow::{case}");
        let expected = format!("{root}{expected_path}");
        assert_rejects_dynamic_schema_with_policy(&root, schema, &expected, expected_kind, policy);
    }
}

#[cfg(feature = "runtime")]
fn assert_no_public_dynamic_schema(catalog_name: &str, catalog: &HostApiCatalog) {
    for schema in catalog.structs() {
        for field in &schema.fields {
            assert_no_public_dynamic_root_with_policy(
                &format!("{catalog_name}::{}.{}", schema.name, field.name),
                &field.ty,
                DynamicPolicy::AllowDiscardedCallableResults,
            );
        }
    }
    for function in catalog.functions() {
        for param in &function.params {
            assert_no_public_dynamic_root_with_policy(
                &format!("{catalog_name}::{}({})", function.name, param.name),
                &param.ty,
                DynamicPolicy::AllowDiscardedCallableResults,
            );
        }
        assert_no_public_dynamic_root_with_policy(
            &format!("{catalog_name}::{} return", function.name),
            &function.return_type,
            DynamicPolicy::AllowDiscardedCallableResults,
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
    // The standard catalog composes the timer surface, whose callback result
    // is the discarded-callable-result exception. Pin it so the exception is
    // provably exercised rather than vacuous.
    let standard = standard_host_catalog();
    let timer_at = standard
        .function("timer::at")
        .expect("the standard catalog composes the timer surface");
    assert_eq!(
        timer_at.params[1].ty,
        HostTypeSchema::Callable {
            params: vec![HostTypeSchema::Bool],
            result: Box::new(HostTypeSchema::Unknown),
        },
        "the timer callback result is the narrow discarded-callable-result exception"
    );

    assert_no_public_dynamic_schema("jit", &jit_host_catalog());
    assert_no_public_dynamic_schema("standard", &standard);
    assert_no_public_dynamic_schema("timer", &timer_host_catalog());
    #[cfg(all(feature = "http-client", not(target_family = "wasm")))]
    assert_no_public_dynamic_schema("http", &http_host_catalog());
    #[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
    assert_no_public_dynamic_schema("sqlite", &sqlite_host_catalog());
}
