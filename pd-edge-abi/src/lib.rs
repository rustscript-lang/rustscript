#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiValueType {
    Unknown,
    Null,
    Int,
    Float,
    Bool,
    String,
    Array,
    Map,
}

impl AbiValueType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Null => "null",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Array => "array",
            Self::Map => "map",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiParamType {
    Any,
    Null,
    Int,
    Float,
    Bool,
    String,
    Array,
    Map,
    Number,
}

impl AbiParamType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Null => "null",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Array => "array",
            Self::Map => "map",
            Self::Number => "number",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiFunction {
    pub index: u16,
    pub name: &'static str,
    pub arity: u8,
    pub param_names: &'static [&'static str],
    pub param_types: &'static [AbiParamType],
    pub return_type: AbiValueType,
    pub docs: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostNamespaceSpec {
    pub root: &'static str,
    pub docs: &'static str,
}

pub const ABI_VERSION: u16 = 20;

#[allow(dead_code, unused_variables)]
mod callable_specs {
    mod marker {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct Map;
    }

    #[allow(unused_imports)]
    use self::marker::Map;
    use pd_host_function::pd_host_function;

    include!("abi_spec/functions.rs");
}

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
        assert!(manifest.contains("\"abi_version\": 20"));
        for function in FUNCTIONS {
            assert!(manifest.contains(function.name));
            assert!(manifest.contains(function.return_type.as_str()));
            assert!(manifest.contains(function.docs));
            for name in function.param_names {
                assert!(manifest.contains(name));
            }
            for param in function.param_types {
                assert!(manifest.contains(param.as_str()));
            }
        }
    }

    #[test]
    fn runtime_sleep_docs_are_available() {
        let function = function_by_name("runtime::sleep").expect("runtime::sleep should exist");
        assert!(
            !function.docs.trim().is_empty(),
            "expected runtime::sleep docs to be populated"
        );
    }

    #[test]
    fn runtime_exit_docs_are_available() {
        let function = function_by_name("runtime::exit").expect("runtime::exit should exist");
        assert!(
            !function.docs.trim().is_empty(),
            "expected runtime::exit docs to be populated"
        );
    }

    #[test]
    fn tcp_stream_get_phase_docs_follow_edge_impl_comments() {
        let function = function_by_name("tcp::stream::get_phase")
            .expect("tcp::stream::get_phase should exist");
        assert_eq!(
            function.docs,
            "Reports the current lifecycle phase for a TCP stream handle."
        );
    }
}
