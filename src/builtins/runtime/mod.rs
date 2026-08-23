// VM-side builtin execution entrypoints.
// Builtin metadata and call-index mapping live in crate::builtins.
use std::sync::{Arc, OnceLock};

use crate::builtins::BuiltinFunction;
use crate::host_api::{HostApiCatalog, HostApiFingerprint};
use crate::vm::{CallOutcome, CallReturn, HostOpId, Value, Vm, VmResult};

mod aot;
mod bytes;
pub(crate) mod cancellation;
pub(crate) mod context;
mod context_host;
pub(crate) mod core;
pub(crate) mod error;
pub(crate) mod event;
mod host;
#[cfg(feature = "http-client")]
mod http;
mod io;
#[cfg(target_arch = "wasm32")]
mod io_wasm;
mod jit;
mod json;
mod map_iter;
mod math;
pub(crate) mod print;
pub(crate) mod regex;
pub(crate) mod resource;
#[cfg(feature = "sqlite")]
mod sqlite;
mod typed;

#[cfg(feature = "http-client")]
pub use http::{
    HttpConfig, HttpHostExt, http_host_catalog, register_http_builtin_module,
    register_http_builtin_module_from_catalog,
};
pub use io::{
    IoExtension, IoHostExt, IoPolicy, io_host_catalog, register_io_builtin_module,
    register_io_builtin_module_from_catalog,
};
#[cfg(feature = "sqlite")]
pub use sqlite::{
    SqliteExtension, SqliteHostExt, SqliteLimits, SqlitePolicy, register_sqlite_builtin_module,
    register_sqlite_builtin_module_from_catalog, sqlite_host_catalog,
};

/// The authoritative standard host API catalog snapshot for this build.
///
/// This is the single combined snapshot of every *enabled* standard host
/// surface (SQLite, IO, HTTP), composed into one validated
/// [`HostApiCatalog`]. The compiler's standard compile entry and the LSP
/// consume this same snapshot, and the standard extensions register their
/// exact imports against it — so the whole-catalog fingerprint embedded in a
/// compiled `HostImport` matches the fingerprint carried by the registered
/// exact schema byte-for-byte, for any combination of enabled features.
///
/// Composition is feature-gated per member:
///
/// * `sqlite` feature → the SQLite surface is included;
/// * `runtime` feature → the IO surface is included;
/// * `http-client` feature → the HTTP surface is included.
///
/// When only one surface is enabled, this equals that surface's own
/// subcatalog; when several are enabled, it is their combined snapshot. The
/// resulting fingerprint therefore always matches what the standard compile
/// entry and the standard extension registration produce for the same build.
pub fn standard_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(&standard_host_catalog_snapshot().catalog)
}

/// Returns the cached fingerprint for [`standard_host_catalog`].
pub fn standard_host_catalog_fingerprint() -> HostApiFingerprint {
    standard_host_catalog_snapshot().fingerprint
}

struct StandardHostCatalogSnapshot {
    catalog: Arc<HostApiCatalog>,
    fingerprint: HostApiFingerprint,
}

static STANDARD_HOST_CATALOG: OnceLock<StandardHostCatalogSnapshot> = OnceLock::new();

fn standard_host_catalog_snapshot() -> &'static StandardHostCatalogSnapshot {
    STANDARD_HOST_CATALOG.get_or_init(|| {
        use crate::host_api::HostApiBuilder;

        let mut builder = HostApiBuilder::new();
        let push = |builder: &mut HostApiBuilder, catalog: &Arc<HostApiCatalog>| {
            for resource in catalog.resources() {
                builder.resource(resource.clone());
            }
            for function in catalog.functions() {
                builder.function(function.clone());
            }
        };
        #[cfg(feature = "sqlite")]
        push(&mut builder, &sqlite_host_catalog());
        push(&mut builder, &io_host_catalog());
        #[cfg(feature = "http-client")]
        push(&mut builder, &http_host_catalog());
        let catalog = Arc::new(
            builder
                .build()
                .expect("standard host catalog must be valid"),
        );
        StandardHostCatalogSnapshot {
            fingerprint: catalog.fingerprint(),
            catalog,
        }
    })
}

pub(crate) fn standard_exact_surface_requirements(
    imports: &[crate::bytecode::HostImport],
) -> (bool, bool, bool) {
    let mut io = false;
    let mut http = false;
    let mut sqlite = false;
    for import in imports {
        io |= import.name.starts_with("io::");
        http |= import.name.starts_with("http::");
        sqlite |= import.name.starts_with("sqlite::");
    }
    (io, http, sqlite)
}

pub use typed::HostCallResult;
pub(crate) use typed::VmMapHandle;

#[cfg(all(test, feature = "sqlite"))]
mod sqlite_contract_tests {
    use super::sqlite::{
        SQLITE_ADAPTER_CONTRACTS, register_sqlite_builtin_module_from_catalog, sqlite_host_catalog,
    };
    use crate::bytecode::HostImport;
    use crate::vm::HostFunctionRegistry;

    #[test]
    fn adapter_contract_covers_catalog_and_every_registered_schema() {
        let catalog = sqlite_host_catalog();
        let contract_names: std::collections::BTreeSet<&str> = SQLITE_ADAPTER_CONTRACTS
            .iter()
            .map(|entry| entry.name)
            .collect();
        let catalog_names: std::collections::BTreeSet<&str> = catalog
            .functions()
            .iter()
            .map(|function| function.name.as_str())
            .collect();
        assert_eq!(contract_names, catalog_names);

        let mut registry = HostFunctionRegistry::empty();
        register_sqlite_builtin_module_from_catalog(&mut registry, &catalog)
            .expect("register SQLite");
        for entry in SQLITE_ADAPTER_CONTRACTS {
            for schema in crate::vm::host_extension::catalog_import_schemas(&catalog, entry.name) {
                let import = HostImport {
                    name: entry.name.to_string(),
                    arity: schema.params.len() as u8,
                    return_type: schema.return_type.coarse_value_type(),
                    schema: Some(schema),
                };
                assert!(registry.resolve_import(&import).is_ok(), "{}", entry.name);
            }
        }
    }
}

// Typed argument decoders used by `#[pd_host_function]`-generated wrappers.
// Re-exported through `builtins::runtime` (and the crate root) so host SDK
// adapters outside the builtin modules can decode by reference / by take.
#[allow(unused_imports)]
use typed::{
    AnyValue, IntoBuiltinCallOutcome, NumberValue, UnknownValue, VmArray, VmBytes, VmCallable,
    VmMap, return_none,
};
pub use typed::{
    BorrowVmValue, FromVmValue, IntoHostCallOutcome, TakeVmValue, arg, borrow_arg, return_one,
    take_arg,
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

pub(crate) fn close_all_handles(vm: &mut Vm) {
    vm.host.reset_for_reuse();
}

#[cfg(test)]
mod standard_surface_tests {
    use super::standard_exact_surface_requirements;
    use crate::bytecode::HostImport;

    fn import(name: &str) -> HostImport {
        HostImport {
            name: name.to_owned(),
            arity: 0,
            return_type: crate::vm::ValueType::Null,
            schema: None,
        }
    }

    #[test]
    fn exact_binding_registers_only_imported_standard_surfaces() {
        assert_eq!(
            standard_exact_surface_requirements(&[import("io::exists"), import("io::read_all")]),
            (true, false, false)
        );
        assert_eq!(
            standard_exact_surface_requirements(&[import("http::client::request")]),
            (false, true, false)
        );
        assert_eq!(
            standard_exact_surface_requirements(&[import("sqlite::open")]),
            (false, false, true)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpCode, Program};

    #[test]
    fn builtin_assert_success_returns_no_stack_value() {
        let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
            .expect("test VM construction must not fail");
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
