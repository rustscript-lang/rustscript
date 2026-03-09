#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiFunction {
    pub index: u16,
    pub name: &'static str,
    pub arity: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostNamespaceSpec {
    pub root: &'static str,
    pub docs: &'static str,
}

pub const ABI_VERSION: u16 = 10;

include!(concat!(env!("OUT_DIR"), "/edge_abi_generated.rs"));

fn functions_by_name() -> &'static std::collections::HashMap<&'static str, &'static AbiFunction> {
    static LOOKUP: std::sync::OnceLock<
        std::collections::HashMap<&'static str, &'static AbiFunction>,
    > = std::sync::OnceLock::new();
    LOOKUP.get_or_init(|| {
        let mut map = std::collections::HashMap::with_capacity(FUNCTIONS.len());
        for function in FUNCTIONS.iter() {
            map.insert(function.name, function);
        }
        map
    })
}

pub fn function_by_index(index: u16) -> Option<&'static AbiFunction> {
    FUNCTIONS.iter().find(|function| function.index == index)
}

pub fn function_by_name(name: &str) -> Option<&'static AbiFunction> {
    functions_by_name().get(name).copied()
}

pub fn host_namespace_specs() -> &'static [HostNamespaceSpec] {
    &HOST_NAMESPACES
}

pub fn abi_json() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/edge_abi_manifest.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_are_dense_and_ordered() {
        for (position, function) in FUNCTIONS.iter().enumerate() {
            assert_eq!(function.index as usize, position);
        }
        assert_eq!(HOST_FUNCTION_COUNT as usize, FUNCTIONS.len());
    }

    #[test]
    fn abi_json_contains_declared_functions() {
        let manifest = abi_json();
        assert!(manifest.contains("\"abi_version\": 10"));
        for function in FUNCTIONS {
            assert!(manifest.contains(function.name));
        }
    }
}
