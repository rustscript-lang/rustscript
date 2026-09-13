#![cfg(all(
    feature = "http-client",
    feature = "sqlite",
    not(target_family = "wasm")
))]

use std::collections::BTreeSet;

use vm::{
    HostApiCatalog, HostTypeSchema, http_host_catalog, jit_host_catalog, sqlite_host_catalog,
    standard_host_catalog,
};

fn assert_no_public_dynamic_schema(catalog_name: &str, catalog: &HostApiCatalog) {
    fn visit(path: &str, schema: &HostTypeSchema, seen: &mut BTreeSet<String>) {
        match schema {
            HostTypeSchema::Map(_) | HostTypeSchema::Unknown => {
                panic!("public host schema {path} exposes {schema:?}")
            }
            HostTypeSchema::Array(inner) | HostTypeSchema::Optional(inner) => {
                visit(path, inner, seen)
            }
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

    for schema in catalog.structs() {
        for field in &schema.fields {
            visit(
                &format!("{catalog_name}::{}.{}", schema.name, field.name),
                &field.ty,
                &mut BTreeSet::new(),
            );
        }
    }
    for function in catalog.functions() {
        for param in &function.params {
            visit(
                &format!("{catalog_name}::{}({})", function.name, param.name),
                &param.ty,
                &mut BTreeSet::new(),
            );
        }
        visit(
            &format!("{catalog_name}::{} return", function.name),
            &function.return_type,
            &mut BTreeSet::new(),
        );
    }
}

#[test]
fn affected_public_host_catalogs_have_no_reachable_map_or_unknown() {
    assert_no_public_dynamic_schema("http", &http_host_catalog());
    assert_no_public_dynamic_schema("sqlite", &sqlite_host_catalog());
    assert_no_public_dynamic_schema("jit", &jit_host_catalog());
    assert_no_public_dynamic_schema("standard", &standard_host_catalog());
}
