// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.

use std::sync::{Arc, OnceLock};

use crate::builtins::BuiltinFunction;
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostStructField, HostStructSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};
#[cfg(all(feature = "async", not(target_family = "wasm")))]
use crate::vm::CaptureAsyncHostContext;
#[allow(unused_imports)]
use crate::vm::{CallOutcome, CallReturn, HostOpId, Value, Vm, VmError, VmResult};

mod aot;
mod bytes;
pub(crate) mod context;
mod context_host;
pub(crate) mod core;
pub(crate) mod error;
pub(crate) mod event;
mod host;
#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
pub(crate) mod http;
#[cfg(not(target_arch = "wasm32"))]
mod io;
#[cfg(target_arch = "wasm32")]
mod io_wasm;
mod jit;
mod json;
mod map_iter;
mod math;
pub(crate) mod print;
pub(crate) mod regex;
#[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
pub(crate) mod sqlite;
pub(crate) mod standard_composition;
mod typed;

pub use jit::{
    jit_host_catalog, register_jit_builtin_module, register_jit_builtin_module_from_catalog,
};

/// Returns the editor/compiler catalog for the built-in host extensions.
///
/// The runtime implementation and the semantic catalog intentionally share only
/// these schemas. Keeping the catalog here lets non-executing tools resolve the
/// same resource-bearing calls without constructing a VM.
pub fn io_host_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| {
        let file_key = ResourceTypeKey::new("io.file").expect("built-in resource key is valid");
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(
            file_key.clone(),
            "An open file handle",
        ));
        builder.function(HostFunctionSchema::with_return(
            "io::open",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("mode", HostTypeSchema::String),
            ],
            HostTypeSchema::Resource(file_key.clone()),
        ));
        builder.function(HostFunctionSchema::with_return(
            "io::read_all",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(file_key.clone()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::String,
        ));
        builder.function(HostFunctionSchema::with_return(
            "io::close",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(file_key),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Bool,
        ));
        Arc::new(builder.build().expect("built-in IO catalog is valid"))
    }))
}

fn optional_type(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Optional(Box::new(inner))
}

fn array_type(inner: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(inner))
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
        .map(|name| HostStructField::new(name, optional_type(HostTypeSchema::Int)))
        .collect(),
    )
    .with_description("Effective SQLite host limits. Omitted keys keep the embedding ceiling.")
}

fn sqlite_open_options_struct(limits: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteOpenOptions",
        vec![
            HostStructField::new("path", optional_type(HostTypeSchema::String)),
            HostStructField::new("mode", optional_type(HostTypeSchema::String)),
            HostStructField::new("root", optional_type(HostTypeSchema::String)),
            HostStructField::new("limits", optional_type(limits.as_type())),
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

fn sqlite_query_result_struct() -> HostStructSchema {
    HostStructSchema::new(
        "SqliteQueryResult",
        vec![
            HostStructField::new("columns", array_type(HostTypeSchema::String)),
            HostStructField::new("rows", array_type(array_type(HostTypeSchema::Unknown))),
            HostStructField::new("truncated", HostTypeSchema::Bool),
            HostStructField::new("next_cursor", optional_type(HostTypeSchema::Int)),
        ],
    )
    .with_description(
        "Query result envelope. Rows stay arrays of arrays; next_cursor is omitted when absent.",
    )
}

fn sqlite_statement_struct(limits: &HostStructSchema) -> HostStructSchema {
    HostStructSchema::new(
        "SqliteStatement",
        vec![
            HostStructField::new("sql", HostTypeSchema::String),
            HostStructField::new("params", optional_type(array_type(HostTypeSchema::Unknown))),
            HostStructField::new("query", optional_type(HostTypeSchema::Bool)),
            HostStructField::new("limits", optional_type(limits.as_type())),
        ],
    )
    .with_description("One sqlite::transaction statement. Positional params stay unknown.")
}

/// Returns the editor/compiler catalog for the SQLite host extension.
pub fn sqlite_host_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(build_sqlite_host_catalog))
}

fn build_sqlite_host_catalog() -> Arc<HostApiCatalog> {
    let connection_key =
        ResourceTypeKey::new("sqlite.connection").expect("built-in resource key is valid");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        connection_key.clone(),
        "An open SQLite connection",
    ));

    let limits = sqlite_limits_struct();
    let open_options = sqlite_open_options_struct(&limits);
    let execute_result = sqlite_execute_result_struct();
    let query_result = sqlite_query_result_struct();
    let statement = sqlite_statement_struct(&limits);
    builder.named_struct(limits.clone());
    builder.named_struct(open_options.clone());
    builder.named_struct(execute_result.clone());
    builder.named_struct(query_result.clone());
    builder.named_struct(statement.clone());

    // Positional params stay unknown (arrays of dynamic cells). Transaction
    // results stay array<unknown> because execute and query envelopes mix.
    // Fixed-shape maps are named structs; runtime values remain maps.
    builder.function(HostFunctionSchema::with_return(
        "sqlite::open",
        vec![HostParamSchema::value("options", open_options.as_type())],
        HostTypeSchema::Resource(connection_key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::execute",
        vec![
            HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(connection_key.clone()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("sql", HostTypeSchema::String),
            HostParamSchema::value("params", HostTypeSchema::Unknown),
        ],
        execute_result.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::query",
        vec![
            HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(connection_key.clone()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("sql", HostTypeSchema::String),
            HostParamSchema::value("params", HostTypeSchema::Unknown),
            HostParamSchema::value("limits", limits.as_type()),
        ],
        query_result.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::transaction",
        vec![
            HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(connection_key.clone()),
                HostParamPassing::Borrow,
            ),
            HostParamSchema::value("statements", array_type(statement.as_type())),
        ],
        array_type(HostTypeSchema::Unknown),
    ));
    builder.function(HostFunctionSchema::with_return(
        "sqlite::close",
        vec![HostParamSchema::with_passing(
            "connection",
            HostTypeSchema::Resource(connection_key),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    Arc::new(builder.build().expect("built-in SQLite catalog is valid"))
}

/// Returns the combined catalog used by default source analysis.
pub fn standard_host_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| {
        let file_key = ResourceTypeKey::new("io.file").expect("built-in resource key is valid");
        let connection_key =
            ResourceTypeKey::new("sqlite.connection").expect("built-in resource key is valid");
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(
            file_key.clone(),
            "An open file handle",
        ));
        builder.resource(ResourceTypeSchema::new(
            connection_key.clone(),
            "An open SQLite connection",
        ));
        builder.function(HostFunctionSchema::with_return(
            "io::open",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("mode", HostTypeSchema::String),
            ],
            HostTypeSchema::Resource(file_key.clone()),
        ));
        builder.function(HostFunctionSchema::with_return(
            "io::read_all",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(file_key.clone()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::String,
        ));
        builder.function(HostFunctionSchema::with_return(
            "io::close",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(file_key),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Bool,
        ));
        {
            let sqlite_catalog = sqlite_host_catalog();
            for schema in sqlite_catalog.structs() {
                builder.named_struct(schema.clone());
            }
            for function in sqlite_catalog.functions() {
                builder.function(function.clone());
            }
        }
        #[cfg(all(feature = "http-client", not(target_family = "wasm")))]
        {
            let http_catalog = http::http_host_catalog();
            for resource in http_catalog.resources() {
                builder.resource(resource.clone());
            }
            for schema in http_catalog.structs() {
                builder.named_struct(schema.clone());
            }
            for function in http_catalog.functions() {
                builder.function(function.clone());
            }
        }
        {
            let jit_catalog = jit_host_catalog();
            for schema in jit_catalog.structs() {
                builder.named_struct(schema.clone());
            }
            for function in jit_catalog.functions() {
                builder.function(function.clone());
            }
        }
        Arc::new(builder.build().expect("standard host catalog is valid"))
    }))
}

#[cfg(target_arch = "wasm32")]
use io_wasm as io;

#[allow(unused_imports)]
pub(crate) use context::{RuntimeContext, RuntimeContextConfig, STREAM_EMIT_NAME};
#[allow(unused_imports)]
pub use error::{RuntimeError, RuntimeErrorCode, RuntimeResult};
#[allow(unused_imports)]
pub(crate) use event::{EventLimits, EventPayload};
#[cfg(not(target_arch = "wasm32"))]
pub use io::{IoHostExt, IoPolicy};
pub use standard_composition::standard_composition;
pub use typed::HostCallResult;
use typed::{AnyValue, IntoBuiltinCallOutcome, NumberValue, UnknownValue, VmArray, VmBytes, VmMap};
#[allow(unused_imports)]
pub use typed::{
    BorrowVmValue, FromVmValue, IntoHostCallOutcome, TakeVmValue, arg, borrow_arg, return_none,
    return_one, take_arg,
};

pub(crate) enum BuiltinCallOutcome {
    Return(CallReturn),
    #[allow(dead_code)]
    Halt,
    Pending(HostOpId),
}

include!(concat!(
    env!("OUT_DIR"),
    "/builtin_runtime_dispatch_generated.rs"
));

pub(crate) fn execute_builtin_call(
    vm: &mut Vm,
    builtin: BuiltinFunction,
    args: &mut [Value],
) -> VmResult<BuiltinCallOutcome> {
    match builtin {
        BuiltinFunction::Len => core::builtin_len(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Slice => core::builtin_slice(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Concat => core::builtin_concat(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::ArrayNew => Ok(BuiltinCallOutcome::Return(return_one(
            core::builtin_array_new_impl(),
        ))),
        BuiltinFunction::ArrayPush => {
            core::builtin_array_push(args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapNew => Ok(BuiltinCallOutcome::Return(return_one(
            core::builtin_map_new_impl(),
        ))),
        BuiltinFunction::Get => core::builtin_get(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Has => core::builtin_has(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Set => core::builtin_set(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Keys => core::builtin_keys(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::Count => core::builtin_count(args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapIterInit => map_iter::init(vm, args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapIterNext => map_iter::next(vm, args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::MapIterTakeKey => {
            map_iter::take_key(vm, args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapIterTakeValue => {
            map_iter::take_value(vm, args).map(BuiltinCallOutcome::Return)
        }
        BuiltinFunction::MapIterClose => map_iter::close(vm, args).map(BuiltinCallOutcome::Return),
        BuiltinFunction::BindCallable => {
            let prototype_id = match args.first() {
                Some(Value::Int(value)) => u32::try_from(*value)
                    .map_err(|_| crate::vm::VmError::InvalidCallablePrototype(u32::MAX))?,
                _ => return Err(crate::vm::VmError::TypeMismatch("callable prototype id")),
            };
            let captures = std::mem::replace(
                args.get_mut(1)
                    .ok_or(crate::vm::VmError::TypeMismatch("callable captures"))?,
                Value::Null,
            )
            .into_owned_array()
            .map_err(|_| crate::vm::VmError::TypeMismatch("callable captures"))?;
            vm.bind_callable_value(prototype_id, captures)
                .map(|value| BuiltinCallOutcome::Return(return_one(value)))
        }
        BuiltinFunction::DetachLocal => {
            let slot = match args.first() {
                Some(Value::Int(value)) => u8::try_from(*value)
                    .map_err(|_| crate::vm::VmError::TypeMismatch("local slot"))?,
                _ => return Err(crate::vm::VmError::TypeMismatch("local slot")),
            };
            vm.detach_local_with_drop_contract(slot)?;
            Ok(BuiltinCallOutcome::Return(return_none()))
        }
        BuiltinFunction::StringContains => core::builtin_string_contains(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::StringReplaceLiteral => core::builtin_string_replace_literal(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::StringLowerAscii => core::builtin_string_lower_ascii(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::StringSplitLiteral => core::builtin_string_split_literal(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::FormatTemplate => core::builtin_format_template(args)
            .map(IntoBuiltinCallOutcome::into_builtin_call_outcome),
        BuiltinFunction::ToString => {
            core::builtin_to_string(args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        BuiltinFunction::TypeOf => {
            core::builtin_type_of(args).map(IntoBuiltinCallOutcome::into_builtin_call_outcome)
        }
        BuiltinFunction::Assert => core::builtin_assert(args).map(|()| {
            // Successful asserts are control checks, not value-producing expressions.
            BuiltinCallOutcome::Return(return_none())
        }),
        _ => execute_namespaced_builtin_call(vm, builtin, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpCode, Program};

    #[test]
    fn builtin_assert_success_returns_no_stack_value() {
        let mut vm = Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
        let mut args = [Value::Bool(true)];

        let outcome = execute_builtin_call(&mut vm, BuiltinFunction::Assert, &mut args)
            .expect("assert should succeed");

        match outcome {
            BuiltinCallOutcome::Return(values) => assert!(
                values.is_empty(),
                "successful assert should not push a null sentinel"
            ),
            BuiltinCallOutcome::Halt => {
                panic!("assert should not halt builtin execution");
            }
            BuiltinCallOutcome::Pending(op_id) => {
                panic!("assert should not yield pending host op {op_id}")
            }
        }
    }
}
