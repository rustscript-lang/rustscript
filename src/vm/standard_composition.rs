//! Generic boundary for the optional standard runtime surface.
//!
//! The VM owns the trait and its opaque call outcomes. The standard runtime
//! implements it from the builtin registration area; alternate embeddings can
//! provide another composition without importing that area into `src/vm`.

use std::sync::Arc;

use crate::bytecode::{HostImport, SharedArray, SharedMap};
use crate::{BuiltinFunction, Value};

use super::{CallOutcome, HostFunctionRegistry, Vm, VmError, VmResult};

/// Runtime surface operations that the VM may request without knowing their
/// concrete implementation module.
pub trait StandardSurfaceComposition: Send + Sync {
    /// Reports whether this composition owns a standard host import.
    fn import_in_standard(&self, import: &HostImport) -> bool;

    /// Stages standard host functions needed by the supplied imports.
    fn ensure_surfaces(
        &self,
        imports: &[HostImport],
        registry: &mut HostFunctionRegistry,
    ) -> VmResult<bool>;

    /// Builds a fresh registry containing this composition's standard host
    /// functions.
    fn build_default_registry(&self) -> VmResult<HostFunctionRegistry>;

    /// Binds one standard host function by source name.
    fn bind_default_name(&self, vm: &mut Vm, name: &str) -> bool;

    /// Dispatches a catalog builtin. The default keeps custom compositions
    /// source-compatible while reporting that no builtin dispatcher is present.
    fn execute_builtin_call(
        &self,
        _vm: &mut Vm,
        _builtin: BuiltinFunction,
        _args: &mut [Value],
    ) -> VmResult<CallOutcome> {
        Err(VmError::HostError(
            "standard surface has no builtin dispatcher".to_string(),
        ))
    }

    /// Optional fast paths used by the portable interpreter and native bridge.
    fn string_contains(&self, _text: &str, _needle: &str) -> Option<bool> {
        None
    }

    fn string_replace_literal(
        &self,
        _text: &str,
        _needle: &str,
        _replacement: &str,
    ) -> Option<String> {
        None
    }

    fn string_lower_ascii(&self, _text: &str) -> Option<String> {
        None
    }

    fn string_split_literal(&self, _text: &str, _delimiter: &str) -> Option<Vec<Value>> {
        None
    }

    fn value_to_string(&self, _value: &Value) -> Option<String> {
        None
    }

    fn regex_match(&self, _vm: &mut Vm, _pattern: &str, _text: &str) -> VmResult<bool> {
        Err(VmError::HostError(
            "standard surface has no regex matcher".to_string(),
        ))
    }

    fn regex_replace(
        &self,
        _vm: &mut Vm,
        _pattern: &str,
        _text: &str,
        _replacement: &str,
    ) -> VmResult<String> {
        Err(VmError::HostError(
            "standard surface has no regex replacer".to_string(),
        ))
    }

    fn ensure_supported_map_key(&self, _key: &Value) -> VmResult<()> {
        Err(VmError::HostError(
            "standard surface has no map-key validator".to_string(),
        ))
    }

    fn set_owned(&self, _container: Value, _key: Value, _value: Value) -> VmResult<Value> {
        Err(VmError::HostError(
            "standard surface has no container setter".to_string(),
        ))
    }

    fn set_map_shared(&self, _entries: SharedMap, _key: Value, _value: Value) -> Option<SharedMap> {
        None
    }

    fn array_push_shared(&self, _items: SharedArray, _value: Value) -> Option<SharedArray> {
        None
    }
}

/// Shared per-runtime handle wrapping a caller-provided composition.
#[derive(Clone)]
pub struct StandardCompositionHandle(pub Arc<dyn StandardSurfaceComposition>);

impl StandardCompositionHandle {
    /// Installs this composition on a registry's standard composition slot.
    pub fn install(&self, registry: &mut HostFunctionRegistry) {
        registry.set_standard_composition(Arc::clone(&self.0));
    }
}

/// Returns a fresh standard-surface handle for callers that want the default
/// runtime composition.
pub fn standard_composition() -> Arc<dyn StandardSurfaceComposition> {
    crate::standard_composition()
}
