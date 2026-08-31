//! Concrete standard-surface composition for the host-agnostic VM core.
//!
//! This module implements [`StandardSurfaceComposition`] for the same-crate
//! standard builtin layer. It is the *only* place that knows which concrete
//! standard domains exist (`io::`, `http::`, `sqlite::`) and which builtin
//! modules implement them. `src/vm` consumes it through the generic trait and
//! never names a domain, namespace prefix, or feature.
//!
//! The implementation is *caller-provided per-instance state*: the outer
//! standard-runtime constructor installs one instance on the standard
//! [`HostFunctionRegistry`] (and on the `Vm` for the legacy fallback paths)
//! through [`standard_composition`]. There is no process-global slot and no
//! hidden installation from `HostRuntime::new()`.

use std::sync::Arc;

use crate::BuiltinFunction;
use crate::bytecode::{HostImport, SharedArray, SharedMap};
use crate::vm::standard_composition::StandardSurfaceComposition;
use crate::vm::{CallOutcome, HostFunctionRegistry, Value, Vm, VmResult};

use super::register_default_host_functions;
use crate::builtins::default_host_callable;

/// The concrete standard-surface composition for this build.
///
/// Feature-gated composition happens through the existing standard builtin
/// helpers: IO is always present under `runtime`, HTTP under `http-client`,
/// SQLite under `sqlite`. Required/present/stage is one opaque operation;
/// the VM core never sees a surface mask or count.
#[derive(Debug)]
pub(crate) struct StandardSurfaceCompositionImpl;

impl StandardSurfaceComposition for StandardSurfaceCompositionImpl {
    fn import_in_standard(&self, import: &HostImport) -> bool {
        default_host_callable(&import.name).is_some()
    }

    fn ensure_surfaces(
        &self,
        imports: &[HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> VmResult<bool> {
        let mut staged = false;
        for import in imports {
            if default_host_callable(&import.name).is_none() {
                continue;
            }
            // The default host callable is the surface: stage it if the
            // registry does not already carry the name.
            if !registry.contains_name(&import.name) {
                register_default_host_functions(registry);
                staged = true;
                break;
            }
        }
        Ok(staged)
    }

    fn build_default_registry(&self) -> VmResult<HostFunctionRegistry> {
        Ok(HostFunctionRegistry::new())
    }

    fn bind_default_name(&self, vm: &mut Vm, name: &str) -> bool {
        super::bind_default_host_function(vm, name)
    }

    fn execute_builtin_call(
        &self,
        vm: &mut Vm,
        builtin: BuiltinFunction,
        args: &mut [Value],
    ) -> VmResult<CallOutcome> {
        super::execute_builtin_call(vm, builtin, args).map(|outcome| match outcome {
            super::BuiltinCallOutcome::Return(values) => CallOutcome::Return(values),
            super::BuiltinCallOutcome::Halt => CallOutcome::Halt,
            super::BuiltinCallOutcome::Pending(op_id) => CallOutcome::Pending(op_id),
        })
    }

    fn string_contains(&self, text: &str, needle: &str) -> Option<bool> {
        Some(super::core::builtin_string_contains_impl(text, needle))
    }

    fn string_replace_literal(
        &self,
        text: &str,
        needle: &str,
        replacement: &str,
    ) -> Option<String> {
        Some(super::core::builtin_string_replace_literal_impl(
            text,
            needle,
            replacement,
        ))
    }

    fn string_lower_ascii(&self, text: &str) -> Option<String> {
        Some(super::core::builtin_string_lower_ascii_impl(text))
    }

    fn string_split_literal(&self, text: &str, delimiter: &str) -> Option<Vec<Value>> {
        Some(super::core::builtin_string_split_literal_impl(
            text, delimiter,
        ))
    }

    fn value_to_string(&self, value: &Value) -> Option<String> {
        Some(super::core::builtin_to_string_impl(value))
    }

    fn regex_match(&self, vm: &mut Vm, pattern: &str, text: &str) -> VmResult<bool> {
        super::regex::native_re_match(vm, pattern, text)
    }

    fn regex_replace(
        &self,
        vm: &mut Vm,
        pattern: &str,
        text: &str,
        replacement: &str,
    ) -> VmResult<String> {
        super::regex::native_re_replace(vm, pattern, text, replacement)
    }

    fn ensure_supported_map_key(&self, key: &Value) -> VmResult<()> {
        super::core::ensure_supported_map_key(key)
    }

    fn set_owned(&self, container: Value, key: Value, value: Value) -> VmResult<Value> {
        super::core::builtin_set_owned(container, key, value)
    }

    fn set_map_shared(&self, entries: SharedMap, key: Value, value: Value) -> Option<SharedMap> {
        Some(super::core::builtin_set_map_shared_impl(
            entries, key, value,
        ))
    }

    fn array_push_shared(&self, items: SharedArray, value: Value) -> Option<SharedArray> {
        Some(super::core::builtin_array_push_shared_impl(items, value))
    }
}

/// Returns a fresh concrete standard-surface composition instance.
///
/// The outer standard-runtime constructor installs this on the standard
/// registry and on a `Vm` when it wants default standard composition
/// behavior. Each call returns a new instance; there is no shared global.
pub fn standard_composition() -> Arc<dyn StandardSurfaceComposition> {
    Arc::new(StandardSurfaceCompositionImpl)
}
