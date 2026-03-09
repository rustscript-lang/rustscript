// Shared builtin catalog (ids, names, arity, call-index mapping).
// VM execution logic lives under vm/builtins_impl/.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinNamespaceMemberSpec {
    pub name: &'static str,
    pub arity: usize,
    pub docs: &'static str,
}

impl BuiltinNamespaceMemberSpec {
    pub const fn new(name: &'static str, arity: usize, docs: &'static str) -> Self {
        Self { name, arity, docs }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinNamespaceSpec {
    pub namespace: &'static str,
    pub alias: &'static str,
    pub docs: &'static str,
    pub runtime_supported_on_wasm: bool,
    pub supports_regex_flags: bool,
    pub members: &'static [BuiltinNamespaceMemberSpec],
}

impl BuiltinNamespaceSpec {
    pub const fn new(
        namespace: &'static str,
        alias: &'static str,
        docs: &'static str,
        runtime_supported_on_wasm: bool,
        supports_regex_flags: bool,
        members: &'static [BuiltinNamespaceMemberSpec],
    ) -> Self {
        Self {
            namespace,
            alias,
            docs,
            runtime_supported_on_wasm,
            supports_regex_flags,
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

pub(crate) const BUILTIN_CALL_BASE: u16 = 0xFFB0;
/// Number of builtins in the main range at or above BUILTIN_CALL_BASE.
pub(crate) const BUILTIN_CALL_COUNT: u16 = MAIN_RANGE_BUILTINS.len() as u16;

const SPECIAL_CALL_BUILTINS: &[(u16, BuiltinFunction)] = &[
    (BUILTIN_CALL_BASE - 4, BuiltinFunction::FormatTemplate),
    (BUILTIN_CALL_BASE - 3, BuiltinFunction::ToString),
    (BUILTIN_CALL_BASE - 2, BuiltinFunction::TypeOf),
    (BUILTIN_CALL_BASE - 1, BuiltinFunction::Assert),
];

pub fn builtin_namespace_specs() -> &'static [BuiltinNamespaceSpec] {
    BUILTIN_NAMESPACE_SPECS
}

pub(crate) fn is_builtin_namespace(namespace: &str) -> bool {
    BUILTIN_NAMESPACE_SPECS
        .iter()
        .any(|entry| entry.namespace == namespace)
}

pub(crate) fn resolve_builtin_namespace_call(
    namespace: &str,
    member: &str,
) -> Option<BuiltinFunction> {
    let entry = BUILTIN_NAMESPACE_LOOKUPS
        .iter()
        .find(|entry| entry.name == namespace)?;
    entry
        .members
        .iter()
        .find(|item| item.name == member)
        .map(|item| item.builtin)
}

pub(crate) fn namespace_supports_regex_flags(namespace: &str) -> bool {
    BUILTIN_NAMESPACE_SPECS
        .iter()
        .find(|entry| entry.namespace == namespace)
        .is_some_and(|entry| entry.supports_regex_flags)
}

pub(crate) fn builtin_namespace_hint() -> String {
    BUILTIN_NAMESPACE_SPECS
        .iter()
        .map(|entry| entry.namespace)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(feature = "runtime")]
pub(crate) fn resolve_namespaced_builtin(name: &str) -> Option<BuiltinFunction> {
    let mut parts = name.trim().split("::");
    let namespace = parts.next()?;
    let member = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    resolve_builtin_namespace_call(namespace, member)
}

impl BuiltinFunction {
    #[cfg(feature = "runtime")]
    pub(crate) fn from_namespaced_name(name: &str) -> Option<Self> {
        resolve_namespaced_builtin(name)
    }

    pub(crate) fn call_index(self) -> u16 {
        match self {
            BuiltinFunction::FormatTemplate => BUILTIN_CALL_BASE - 4,
            BuiltinFunction::ToString => BUILTIN_CALL_BASE - 3,
            BuiltinFunction::TypeOf => BUILTIN_CALL_BASE - 2,
            BuiltinFunction::Assert => BUILTIN_CALL_BASE - 1,
            _ => BUILTIN_CALL_BASE + self as u16,
        }
    }

    pub(crate) fn from_call_index(index: u16) -> Option<Self> {
        if let Some((_, builtin)) = SPECIAL_CALL_BUILTINS
            .iter()
            .find(|(call_index, _)| *call_index == index)
        {
            return Some(*builtin);
        }
        let offset = index.checked_sub(BUILTIN_CALL_BASE)?;
        if offset >= BUILTIN_CALL_COUNT {
            return None;
        }
        MAIN_RANGE_BUILTINS.get(offset as usize).copied()
    }
}
