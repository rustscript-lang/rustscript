// Shared builtin catalog (ids, names, arity, call-index mapping).
// Runtime execution logic lives under builtins/runtime/.

mod metadata;
#[cfg(feature = "runtime")]
pub(crate) mod runtime;

pub use self::metadata::{CallableDef, CallableParam, CallableParamType, CallableSignature};
use crate::ValueType;
#[cfg(feature = "runtime")]
pub(crate) use crate::vm::{HostFunctionRegistry, Value, Vm, VmResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageBuiltinSpec {
    pub name: &'static str,
    pub docs: &'static str,
    pub signatures: &'static [CallableSignature],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinNamespaceMemberSpec {
    pub name: &'static str,
    pub arity: usize,
    pub return_type: ValueType,
    pub docs: &'static str,
}

impl BuiltinNamespaceMemberSpec {
    pub const fn new(
        name: &'static str,
        arity: usize,
        return_type: ValueType,
        docs: &'static str,
    ) -> Self {
        Self {
            name,
            arity,
            return_type,
            docs,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinNamespaceSpec {
    pub namespace: &'static str,
    pub docs: &'static str,
    pub runtime_supported_on_wasm: bool,
    pub members: &'static [BuiltinNamespaceMemberSpec],
}

impl BuiltinNamespaceSpec {
    pub const fn new(
        namespace: &'static str,
        docs: &'static str,
        runtime_supported_on_wasm: bool,
        members: &'static [BuiltinNamespaceMemberSpec],
    ) -> Self {
        Self {
            namespace,
            docs,
            runtime_supported_on_wasm,
            members,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct BuiltinNamespaceMemberLookup {
    name: &'static str,
    builtin: BuiltinFunction,
}

impl BuiltinNamespaceMemberLookup {
    const fn new(name: &'static str, builtin: BuiltinFunction) -> Self {
        Self { name, builtin }
    }
}

#[derive(Clone, Copy, Debug)]
struct BuiltinNamespaceLookup {
    name: &'static str,
    members: &'static [BuiltinNamespaceMemberLookup],
}

impl BuiltinNamespaceLookup {
    const fn new(name: &'static str, members: &'static [BuiltinNamespaceMemberLookup]) -> Self {
        Self { name, members }
    }
}

include!(concat!(env!("OUT_DIR"), "/builtin_catalog_generated.rs"));
