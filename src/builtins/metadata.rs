#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallableParamType {
    Any,
    Null,
    Int,
    Float,
    Bool,
    String,
    Bytes,
    Array,
    Map,
    Number,
    Callable(CallableType),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableType {
    pub params: &'static [CallableParamType],
    pub return_type: &'static CallableParamType,
}

impl CallableParamType {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Null => "null",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Bytes => "bytes",
            Self::Array => "array",
            Self::Map => "map",
            Self::Number => "number",
            Self::Callable(_) => "function",
        }
    }

    pub fn display_label(self) -> String {
        match self {
            Self::Callable(signature) => format!(
                "fn({}) -> {}",
                signature
                    .params
                    .iter()
                    .map(|param| param.display_label())
                    .collect::<Vec<_>>()
                    .join(", "),
                signature.return_type.display_label()
            ),
            other => other.label().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableParam {
    pub name: &'static str,
    pub ty: CallableParamType,
    pub optional: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableSignature {
    pub params: &'static [CallableParam],
    pub return_type: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostExecution {
    Sync,
    MaySuspend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableDef {
    pub name: &'static str,
    pub docs: &'static str,
    pub signature: CallableSignature,
    pub host_execution: HostExecution,
}

#[allow(dead_code)]
pub mod marker {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Any;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Array;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Bytes;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Map;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Number;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Unknown;
}
