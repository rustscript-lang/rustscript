//! SQLite guest contracts and the SQLite host catalog surface.
//!
//! This module is deliberately not feature-gated: the standard guest catalog
//! declares the SQLite surface for every build, exactly as it did before the
//! descriptors, so a program compiled against the standard catalog always
//! resolves the same imports. The concrete adapters and the connection
//! resource type live in the feature-gated [`super::sqlite`] module; when that
//! module is compiled out, the surface is still declared from the same
//! contract functions with an adapter that fails closed.
//!
//! Every named struct and every function schema below is the single source of
//! the SQLite guest contract: the feature-gated host functions attach these
//! contracts directly, and the catalog is derived from them.

use crate::host_api::{
    HostFunctionSchema, HostParamPassing, HostParamSchema, HostStructField, HostStructSchema,
    HostTypeSchema,
};
use crate::host_extension::{HostFunctionDescriptor, HostResourceTypeMeta};

/// The canonical `sqlite.connection` resource key.
pub(super) const SQLITE_CONNECTION_KEY: &str = "sqlite.connection";
/// The canonical `sqlite.connection` resource description.
pub(super) const SQLITE_CONNECTION_DESCRIPTION: &str = "An open SQLite connection";

/// Resource type declaration used when the SQLite host module is compiled out.
///
/// The concrete scope resource is [`super::sqlite`]'s `SqliteResource`; this
/// marker only carries the guest-visible declaration for the catalog, so the
/// disabled build keeps the same resource key and description.
#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
pub(super) struct SqliteConnectionResourceMarker;

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
impl crate::vm::resource::HostResource for SqliteConnectionResourceMarker {}

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
impl crate::host_extension::HostResourceType for SqliteConnectionResourceMarker {
    const KEY: &'static str = SQLITE_CONNECTION_KEY;
    const DESCRIPTION: &'static str = SQLITE_CONNECTION_DESCRIPTION;
}

/// The canonical declaration for the `sqlite.connection` resource type.
pub(super) fn sqlite_connection_resource() -> HostResourceTypeMeta {
    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    {
        super::sqlite::concrete_sqlite_connection_resource()
    }
    #[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
    {
        HostResourceTypeMeta::of::<SqliteConnectionResourceMarker>()
    }
}

/// The `sqlite.connection` resource key, taken from its single declaration.
fn sqlite_connection_key() -> crate::host_api::ResourceTypeKey {
    sqlite_connection_resource().schema.key
}

fn sqlite_optional_type(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Optional(Box::new(inner))
}

fn sqlite_limits_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SqliteLimits",
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
        .map(|name| HostStructField::new(name, sqlite_optional_type(HostTypeSchema::Int)))
        .collect(),
    )
    .with_description("Effective SQLite host limits. Omitted keys keep the embedding ceiling.")
}

fn sqlite_open_options_struct(limits: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteOpenOptions",
        vec![
            HostStructField::new("path", sqlite_optional_type(HostTypeSchema::String)),
            HostStructField::new("mode", sqlite_optional_type(HostTypeSchema::String)),
            HostStructField::new("root", sqlite_optional_type(HostTypeSchema::String)),
            HostStructField::new("limits", sqlite_optional_type(limits.as_type())),
        ],
    )
    .with_description("SQLite open options. Runtime still requires a non-empty path.")
}

fn sqlite_execute_result_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SqliteExecuteResult",
        vec![
            HostStructField::new("rows_affected", HostTypeSchema::Int),
            HostStructField::new("last_insert_rowid", HostTypeSchema::Int),
        ],
    )
    .with_description("Result envelope for sqlite::execute. Runtime value remains a map.")
}

fn sqlite_value_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SqliteValue",
        vec![
            HostStructField::new("kind", HostTypeSchema::String),
            HostStructField::new("int_value", sqlite_optional_type(HostTypeSchema::Int)),
            HostStructField::new("float_value", sqlite_optional_type(HostTypeSchema::Float)),
            HostStructField::new("text_value", sqlite_optional_type(HostTypeSchema::String)),
            HostStructField::new("blob_value", sqlite_optional_type(HostTypeSchema::Bytes)),
        ],
    )
    .with_description(
        "A tagged SQLite parameter or result cell. Exactly one payload matches kind, except null.",
    )
}

fn sqlite_row_struct(value: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteRow",
        vec![HostStructField::new(
            "cells",
            sqlite_array_type(value.as_type()),
        )],
    )
    .with_description("One SQLite result row containing typed cells in column order.")
}

fn sqlite_query_result_struct(row: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteQueryResult",
        vec![
            HostStructField::new("columns", sqlite_array_type(HostTypeSchema::String)),
            HostStructField::new("rows", sqlite_array_type(row.as_type())),
            HostStructField::new("truncated", HostTypeSchema::Bool),
            HostStructField::new("next_cursor", sqlite_optional_type(HostTypeSchema::Int)),
        ],
    )
    .with_description("Query result envelope with typed rows; next_cursor is omitted when absent.")
}

fn sqlite_statement_struct(
    limits: &HostStructSchema,
    value: &HostStructSchema,
) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteStatement",
        vec![
            HostStructField::new("sql", HostTypeSchema::String),
            HostStructField::new(
                "params",
                sqlite_optional_type(sqlite_array_type(value.as_type())),
            ),
            HostStructField::new("query", sqlite_optional_type(HostTypeSchema::Bool)),
            HostStructField::new("limits", sqlite_optional_type(limits.as_type())),
        ],
    )
    .with_description("One sqlite::transaction statement with optional typed parameters.")
}

fn sqlite_transaction_result_struct(
    execute: &HostStructSchema,
    query: &HostStructSchema,
) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteTransactionResult",
        vec![
            HostStructField::new("kind", HostTypeSchema::String),
            HostStructField::new("execute", sqlite_optional_type(execute.as_type())),
            HostStructField::new("query", sqlite_optional_type(query.as_type())),
        ],
    )
    .with_description("A tagged ordered SQLite transaction result envelope.")
}

fn sqlite_array_type(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(inner))
}

fn sqlite_limits() -> HostStructSchema {
    sqlite_limits_struct()
}

fn sqlite_open_options() -> HostStructSchema {
    sqlite_open_options_struct(&sqlite_limits())
}

fn sqlite_value() -> HostStructSchema {
    sqlite_value_struct()
}

fn sqlite_execute_result() -> HostStructSchema {
    sqlite_execute_result_struct()
}

fn sqlite_query_result() -> HostStructSchema {
    sqlite_query_result_struct(&sqlite_row_struct(&sqlite_value()))
}

fn sqlite_statement() -> HostStructSchema {
    sqlite_statement_struct(&sqlite_limits(), &sqlite_value())
}

fn sqlite_transaction_result() -> HostStructSchema {
    sqlite_transaction_result_struct(&sqlite_execute_result(), &sqlite_query_result())
}

/// Guest contract for `sqlite::open`.
pub(super) fn sqlite_open_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "sqlite::open",
        vec![HostParamSchema::value(
            "options",
            sqlite_open_options().as_type(),
        )],
        HostTypeSchema::Resource(sqlite_connection_key()),
    )
}

/// Guest contract for `sqlite::execute`.
pub(super) fn sqlite_execute_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "sqlite::execute",
        vec![
            HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(sqlite_connection_key()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("sql", HostTypeSchema::String),
            HostParamSchema::value("params", sqlite_array_type(sqlite_value().as_type())),
        ],
        sqlite_execute_result().as_type(),
    )
}

/// Guest contract for `sqlite::query`.
pub(super) fn sqlite_query_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "sqlite::query",
        vec![
            HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(sqlite_connection_key()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("sql", HostTypeSchema::String),
            HostParamSchema::value("params", sqlite_array_type(sqlite_value().as_type())),
            HostParamSchema::value("limits", sqlite_limits().as_type()),
        ],
        sqlite_query_result().as_type(),
    )
}

/// Guest contract for `sqlite::transaction`.
pub(super) fn sqlite_transaction_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "sqlite::transaction",
        vec![
            HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(sqlite_connection_key()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value(
                "statements",
                sqlite_array_type(sqlite_statement().as_type()),
            ),
        ],
        sqlite_array_type(sqlite_transaction_result().as_type()),
    )
}

/// Guest contract for `sqlite::close`.
pub(super) fn sqlite_close_contract() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "sqlite::close",
        vec![HostParamSchema::with_passing(
            "connection",
            HostTypeSchema::Resource(sqlite_connection_key()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    )
}

/// Documentation for the named structs this module's catalog declares.
pub(super) const SQLITE_NAMED_STRUCTS: &[(&str, &str)] = &[
    (
        "SqliteLimits",
        "Effective SQLite host limits. Omitted keys keep the embedding ceiling.",
    ),
    (
        "SqliteOpenOptions",
        "SQLite open options. Runtime still requires a non-empty path.",
    ),
    (
        "SqliteExecuteResult",
        "Result envelope for sqlite::execute. Runtime value remains a map.",
    ),
    (
        "SqliteValue",
        "A tagged SQLite parameter or result cell. Exactly one payload matches kind, except null.",
    ),
    (
        "SqliteRow",
        "One SQLite result row containing typed cells in column order.",
    ),
    (
        "SqliteQueryResult",
        "Query result envelope with typed rows; next_cursor is omitted when absent.",
    ),
    (
        "SqliteStatement",
        "One sqlite::transaction statement with optional typed parameters.",
    ),
    (
        "SqliteTransactionResult",
        "A tagged ordered SQLite transaction result envelope.",
    ),
];

// The SQLite host catalog surface: one descriptor per `sqlite::*` function.
//
// The SQLite surface publishes every member it owns, so catalog and ownership
// coincide; parameter labels, result cells, and mixed transaction outputs use
// the typed named structs declared in `super`.

/// Fails closed when the SQLite host module is not compiled into the build.
#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_module_unavailable(
    _vm: &mut crate::vm::Vm,
    _args: &[crate::vm::Value],
) -> crate::vm::VmResult<crate::vm::CallOutcome> {
    Err(crate::vm::VmError::HostError(
        "the SQLite host module is not compiled into this build".to_string(),
    ))
}

/// One SQLite catalog entry for builds without the SQLite host module.
#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_unavailable_descriptor(
    contract: fn() -> HostFunctionSchema,
) -> crate::host_extension::HostFunctionDescriptor {
    crate::host_extension::HostFunctionDescriptor {
        schema: contract(),
        binding: crate::host_extension::HostBindingDescriptor {
            kind: crate::host_extension::HostBindingKind::StaticStack,
        },
        effects: crate::host_extension::guest_resource_effects(&contract()),
        adapter: crate::host_extension::HostAdapterDescriptor::StaticStack(
            sqlite_module_unavailable,
        ),
        resource_types: Vec::new(),
    }
}

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_open_unavailable_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    sqlite_unavailable_descriptor(sqlite_open_contract)
}

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_execute_unavailable_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    sqlite_unavailable_descriptor(sqlite_execute_contract)
}

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_query_unavailable_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    sqlite_unavailable_descriptor(sqlite_query_contract)
}

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_transaction_unavailable_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    sqlite_unavailable_descriptor(sqlite_transaction_contract)
}

#[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
fn sqlite_close_unavailable_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    sqlite_unavailable_descriptor(sqlite_close_contract)
}

/// Every SQLite catalog function this build owns.
pub(super) const SQLITE_FUNCTIONS: &[fn() -> HostFunctionDescriptor] = {
    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    {
        super::sqlite::SQLITE_CATALOG_FUNCTIONS
    }
    #[cfg(not(all(feature = "sqlite", not(target_arch = "wasm32"))))]
    {
        &[
            sqlite_open_unavailable_descriptor,
            sqlite_execute_unavailable_descriptor,
            sqlite_query_unavailable_descriptor,
            sqlite_transaction_unavailable_descriptor,
            sqlite_close_unavailable_descriptor,
        ]
    }
};

/// The SQLite catalog surface for this build.
pub(super) fn sqlite_catalog_module() -> crate::host_extension::HostModuleDescriptor {
    super::host_modules::catalog_module("sqlite", SQLITE_FUNCTIONS, &[sqlite_connection_resource])
}

/// The standard `sqlite` host module.
///
/// The guest catalog surface is part of the standard catalog in **every**
/// build, exactly as before the descriptors: a program compiled against the
/// standard catalog always resolves the same SQLite imports. The features only
/// select the adapters — with the SQLite host module compiled in they are the
/// real host functions, otherwise they fail closed at the runtime boundary.
pub(super) fn sqlite_standard_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::StandardHostModule;

    StandardHostModule {
        name: "sqlite",
        catalog: sqlite_catalog_module,
        owned: SQLITE_FUNCTIONS,
        named_structs: SQLITE_NAMED_STRUCTS,
    }
}
