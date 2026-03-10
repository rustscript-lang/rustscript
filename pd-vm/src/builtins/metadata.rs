#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallableParamType {
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

impl CallableParamType {
    pub const fn label(self) -> &'static str {
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
pub struct CallableDef {
    pub name: &'static str,
    pub docs: &'static str,
    pub signature: CallableSignature,
}

#[allow(dead_code)]
pub mod marker {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Any;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Array;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Map;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Number;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Unknown;
}
