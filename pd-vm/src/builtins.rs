// Shared builtin catalog (ids, names, arity, call-index mapping).
// VM execution logic lives under vm/builtins_impl/.

use crate::ValueType;

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
    StringOrArray,
    ArrayOrMap,
    StringArrayOrMap,
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
            Self::StringOrArray => "string | array",
            Self::ArrayOrMap => "array | map",
            Self::StringArrayOrMap => "string | array | map",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallableSignature {
    pub params: &'static [CallableParamType],
    pub return_type: &'static str,
}

impl CallableSignature {
    pub const fn new(params: &'static [CallableParamType], return_type: &'static str) -> Self {
        Self {
            params,
            return_type,
        }
    }
}

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

const LEN_SIGNATURES: [CallableSignature; 3] = [
    CallableSignature::new(&[CallableParamType::String], "int"),
    CallableSignature::new(&[CallableParamType::Array], "int"),
    CallableSignature::new(&[CallableParamType::Map], "int"),
];
const SLICE_SIGNATURES: [CallableSignature; 2] = [
    CallableSignature::new(
        &[CallableParamType::String, CallableParamType::Int, CallableParamType::Int],
        "string",
    ),
    CallableSignature::new(
        &[CallableParamType::Array, CallableParamType::Int, CallableParamType::Int],
        "array",
    ),
];
const CONCAT_SIGNATURES: [CallableSignature; 2] = [
    CallableSignature::new(
        &[CallableParamType::String, CallableParamType::String],
        "string",
    ),
    CallableSignature::new(&[CallableParamType::Array, CallableParamType::Array], "array"),
];
const ARRAY_NEW_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[], "array")];
const ARRAY_PUSH_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Array, CallableParamType::Any], "array")];
const MAP_NEW_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(&[], "map")];
const GET_SIGNATURES: [CallableSignature; 3] = [
    CallableSignature::new(&[CallableParamType::String, CallableParamType::Int], "string"),
    CallableSignature::new(&[CallableParamType::Array, CallableParamType::Int], "unknown"),
    CallableSignature::new(&[CallableParamType::Map, CallableParamType::Any], "unknown"),
];
const SET_SIGNATURES: [CallableSignature; 2] = [
    CallableSignature::new(
        &[CallableParamType::Array, CallableParamType::Int, CallableParamType::Any],
        "array",
    ),
    CallableSignature::new(
        &[CallableParamType::Map, CallableParamType::Any, CallableParamType::Any],
        "map",
    ),
];
const KEYS_SIGNATURES: [CallableSignature; 2] = [
    CallableSignature::new(&[CallableParamType::Array], "array"),
    CallableSignature::new(&[CallableParamType::Map], "array"),
];
const COUNT_SIGNATURES: [CallableSignature; 2] = [
    CallableSignature::new(&[CallableParamType::Array], "int"),
    CallableSignature::new(&[CallableParamType::Map], "int"),
];
const TYPE_OF_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Any], "string")];
const ASSERT_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Bool], "null")];
const FORMAT_TEMPLATE_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::String, CallableParamType::Array],
    "string",
)];
const TO_STRING_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Any], "string")];
const RE_MATCH_ALIAS_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[
        CallableParamType::String,
        CallableParamType::String,
        CallableParamType::String,
    ],
    "bool",
)];
const RE_IS_MATCH_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::String, CallableParamType::String],
    "bool",
)];
const RE_FIND_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::String, CallableParamType::String],
    "string | null",
)];
const RE_REPLACE_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[
        CallableParamType::String,
        CallableParamType::String,
        CallableParamType::String,
    ],
    "string",
)];
const RE_SPLIT_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::String, CallableParamType::String],
    "array",
)];
const JSON_ENCODE_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Any], "string")];
const JSON_DECODE_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::String], "unknown")];
const IO_OPEN_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::String, CallableParamType::String],
    "int",
)];
const IO_READ_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Int], "string")];
const IO_WRITE_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::Int, CallableParamType::String],
    "int",
)];
const IO_HANDLE_BOOL_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Int], "bool")];
const IO_EXISTS_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::String], "bool")];
const JIT_SET_CONFIG_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[
        CallableParamType::Bool,
        CallableParamType::Int,
        CallableParamType::Int,
    ],
    "map",
)];
const JIT_GET_CONFIG_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(&[], "map")];
const JIT_SET_ENABLED_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Bool], "bool")];
const JIT_GET_ENABLED_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(&[], "bool")];
const JIT_SET_INT_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Int], "int")];
const JIT_GET_INT_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(&[], "int")];
const MATH_CONST_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(&[], "float")];
const MATH_NUMBER_TO_NUMBER_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Number], "int | float")];
const MATH_NUMBER_TO_FLOAT_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Number], "float")];
const MATH_NUMBER_TO_BOOL_SIGNATURES: [CallableSignature; 1] =
    [CallableSignature::new(&[CallableParamType::Number], "bool")];
const MATH_TWO_NUMBER_TO_FLOAT_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::Number, CallableParamType::Number],
    "float",
)];
const MATH_NUMBER_INT_TO_FLOAT_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::Number, CallableParamType::Int],
    "float",
)];
const MATH_TWO_NUMBER_TO_NUMBER_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[CallableParamType::Number, CallableParamType::Number],
    "int | float",
)];
const MATH_THREE_NUMBER_TO_NUMBER_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[
        CallableParamType::Number,
        CallableParamType::Number,
        CallableParamType::Number,
    ],
    "int | float",
)];
const MATH_MUL_ADD_SIGNATURES: [CallableSignature; 1] = [CallableSignature::new(
    &[
        CallableParamType::Number,
        CallableParamType::Number,
        CallableParamType::Number,
    ],
    "float",
)];

const LANGUAGE_BUILTIN_SPECS: [LanguageBuiltinSpec; 13] = [
    LanguageBuiltinSpec {
        name: "len",
        docs: "Return the length of a string, array, or map.",
        signatures: &LEN_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "slice",
        docs: "Slice a string or array from the given start and length.",
        signatures: &SLICE_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "concat",
        docs: "Concatenate two strings or two arrays.",
        signatures: &CONCAT_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "array_new",
        docs: "Create an empty array.",
        signatures: &ARRAY_NEW_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "array_push",
        docs: "Append a value to an array and return the updated array.",
        signatures: &ARRAY_PUSH_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "map_new",
        docs: "Create an empty map.",
        signatures: &MAP_NEW_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "get",
        docs: "Read a string, array, or map entry.",
        signatures: &GET_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "set",
        docs: "Update an array or map entry and return the updated container.",
        signatures: &SET_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "keys",
        docs: "Return an array of container keys or indices.",
        signatures: &KEYS_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "count",
        docs: "Return the number of entries in an array or map.",
        signatures: &COUNT_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "type",
        docs: "Return the runtime type name of a value.",
        signatures: &TYPE_OF_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "typeof",
        docs: "Return the runtime type name of a value.",
        signatures: &TYPE_OF_SIGNATURES,
    },
    LanguageBuiltinSpec {
        name: "assert",
        docs: "Abort execution if the condition is false.",
        signatures: &ASSERT_SIGNATURES,
    },
];

pub fn language_builtin_specs() -> &'static [LanguageBuiltinSpec] {
    &LANGUAGE_BUILTIN_SPECS
}

pub fn callable_signatures_for_builtin_namespace_member(
    namespace: &str,
    member: &str,
    arity: usize,
) -> Option<&'static [CallableSignature]> {
    if namespace == "re" && member == "match" && arity == 3 {
        return Some(&RE_MATCH_ALIAS_SIGNATURES);
    }
    let builtin = resolve_builtin_namespace_call(namespace, member)?;
    let signatures = builtin.callable_signatures();
    if signatures.iter().any(|signature| signature.params.len() == arity) {
        Some(signatures)
    } else {
        None
    }
}

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
    pub(crate) fn callable_signatures(self) -> &'static [CallableSignature] {
        match self {
            BuiltinFunction::Len => &LEN_SIGNATURES,
            BuiltinFunction::Slice => &SLICE_SIGNATURES,
            BuiltinFunction::Concat => &CONCAT_SIGNATURES,
            BuiltinFunction::ArrayNew => &ARRAY_NEW_SIGNATURES,
            BuiltinFunction::ArrayPush => &ARRAY_PUSH_SIGNATURES,
            BuiltinFunction::MapNew => &MAP_NEW_SIGNATURES,
            BuiltinFunction::Get => &GET_SIGNATURES,
            BuiltinFunction::Set => &SET_SIGNATURES,
            BuiltinFunction::Keys => &KEYS_SIGNATURES,
            BuiltinFunction::Count => &COUNT_SIGNATURES,
            BuiltinFunction::FormatTemplate => &FORMAT_TEMPLATE_SIGNATURES,
            BuiltinFunction::ToString => &TO_STRING_SIGNATURES,
            BuiltinFunction::TypeOf => &TYPE_OF_SIGNATURES,
            BuiltinFunction::Assert => &ASSERT_SIGNATURES,
            BuiltinFunction::ReIsMatch => &RE_IS_MATCH_SIGNATURES,
            BuiltinFunction::ReFind => &RE_FIND_SIGNATURES,
            BuiltinFunction::ReReplace => &RE_REPLACE_SIGNATURES,
            BuiltinFunction::ReSplit | BuiltinFunction::ReCaptures => &RE_SPLIT_SIGNATURES,
            BuiltinFunction::JsonEncode => &JSON_ENCODE_SIGNATURES,
            BuiltinFunction::JsonDecode => &JSON_DECODE_SIGNATURES,
            BuiltinFunction::IoOpen | BuiltinFunction::IoPopen => &IO_OPEN_SIGNATURES,
            BuiltinFunction::IoReadAll | BuiltinFunction::IoReadLine => &IO_READ_SIGNATURES,
            BuiltinFunction::IoWrite => &IO_WRITE_SIGNATURES,
            BuiltinFunction::IoFlush | BuiltinFunction::IoClose => &IO_HANDLE_BOOL_SIGNATURES,
            BuiltinFunction::IoExists => &IO_EXISTS_SIGNATURES,
            BuiltinFunction::JitSetConfig => &JIT_SET_CONFIG_SIGNATURES,
            BuiltinFunction::JitGetConfig => &JIT_GET_CONFIG_SIGNATURES,
            BuiltinFunction::JitSetEnabled => &JIT_SET_ENABLED_SIGNATURES,
            BuiltinFunction::JitGetEnabled => &JIT_GET_ENABLED_SIGNATURES,
            BuiltinFunction::JitSetHotLoopThreshold | BuiltinFunction::JitSetMaxTraceLen => {
                &JIT_SET_INT_SIGNATURES
            }
            BuiltinFunction::JitGetHotLoopThreshold | BuiltinFunction::JitGetMaxTraceLen => {
                &JIT_GET_INT_SIGNATURES
            }
            BuiltinFunction::MathPi
            | BuiltinFunction::MathTau
            | BuiltinFunction::MathE
            | BuiltinFunction::MathEpsilon
            | BuiltinFunction::MathInf
            | BuiltinFunction::MathNegInf
            | BuiltinFunction::MathNaN => &MATH_CONST_SIGNATURES,
            BuiltinFunction::MathAbs
            | BuiltinFunction::MathFloor
            | BuiltinFunction::MathCeil
            | BuiltinFunction::MathRound
            | BuiltinFunction::MathTrunc
            | BuiltinFunction::MathSignum => {
                &MATH_NUMBER_TO_NUMBER_SIGNATURES
            }
            BuiltinFunction::MathSqrt
            | BuiltinFunction::MathCbrt
            | BuiltinFunction::MathExp
            | BuiltinFunction::MathExp2
            | BuiltinFunction::MathLn
            | BuiltinFunction::MathLn1p
            | BuiltinFunction::MathLog2
            | BuiltinFunction::MathLog10
            | BuiltinFunction::MathSin
            | BuiltinFunction::MathCos
            | BuiltinFunction::MathTan
            | BuiltinFunction::MathAsin
            | BuiltinFunction::MathAcos
            | BuiltinFunction::MathAtan
            | BuiltinFunction::MathSinh
            | BuiltinFunction::MathCosh
            | BuiltinFunction::MathTanh
            | BuiltinFunction::MathFract
            | BuiltinFunction::MathToDegrees
            | BuiltinFunction::MathToRadians => {
                &MATH_NUMBER_TO_FLOAT_SIGNATURES
            }
            BuiltinFunction::MathIsNaN
            | BuiltinFunction::MathIsInfinite
            | BuiltinFunction::MathIsFinite => {
                &MATH_NUMBER_TO_BOOL_SIGNATURES
            }
            BuiltinFunction::MathAtan2
            | BuiltinFunction::MathPowF
            | BuiltinFunction::MathHypot
            | BuiltinFunction::MathLog
            | BuiltinFunction::MathCopySign => {
                &MATH_TWO_NUMBER_TO_FLOAT_SIGNATURES
            }
            BuiltinFunction::MathPowI => &MATH_NUMBER_INT_TO_FLOAT_SIGNATURES,
            BuiltinFunction::MathMin | BuiltinFunction::MathMax => &MATH_TWO_NUMBER_TO_NUMBER_SIGNATURES,
            BuiltinFunction::MathClamp => &MATH_THREE_NUMBER_TO_NUMBER_SIGNATURES,
            BuiltinFunction::MathMulAdd => &MATH_MUL_ADD_SIGNATURES,
        }
    }

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
