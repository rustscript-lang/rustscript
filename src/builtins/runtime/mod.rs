// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.

use std::sync::{Arc, OnceLock};

use crate::builtins::BuiltinFunction;
use crate::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
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

/// Returns the editor/compiler catalog for the SQLite host extension.
pub fn sqlite_host_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| {
        let connection_key =
            ResourceTypeKey::new("sqlite.connection").expect("built-in resource key is valid");
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(
            connection_key.clone(),
            "An open SQLite connection",
        ));
        builder.function(HostFunctionSchema::with_return(
            "sqlite::open",
            vec![HostParamSchema::value("options", HostTypeSchema::Unknown)],
            HostTypeSchema::Resource(connection_key.clone()),
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
                HostParamSchema::value("options", HostTypeSchema::Unknown),
            ],
            HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
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
    }))
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
        builder.function(HostFunctionSchema::with_return(
            "sqlite::open",
            vec![HostParamSchema::value("options", HostTypeSchema::Unknown)],
            HostTypeSchema::Resource(connection_key.clone()),
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
                HostParamSchema::value("options", HostTypeSchema::Unknown),
            ],
            HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
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
        #[cfg(all(feature = "http-client", not(target_family = "wasm")))]
        {
            let http_catalog = http::http_host_catalog();
            for resource in http_catalog.resources() {
                builder.resource(resource.clone());
            }
            for function in http_catalog.functions() {
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
