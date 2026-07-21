//! Declarative metadata for specialized builtin recording.
//!
//! Each `BuiltinSpec` captures the mechanical, per-builtin facts that the
//! recorder needs to select, analyze, and emit a specialized SSA
//! instruction. The goal is to reduce the six-layer touch-point tax
//! (selection → analysis → emit → bridge → codegen → lowering) to a
//! single authoritative spec plus dedicated semantic implementations.
//!
//! Scope: pilot (StringLen, RegexMatch, ArraySet) + family 1
//! (len/type/predicate). Non-covered builtins continue to use their
//! existing hand-written paths.

use super::ir::SsaValueRepr;
use crate::ValueType;

/// How a builtin interacts with the VM heap and failure domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinEffect {
    /// Pure read-only operation; no heap mutation, no failure exit.
    Pure,
    /// Calls a fallible bridge helper; failure triggers a deopt exit.
    FallibleHelper,
    /// Owned mutation with clone-before-transfer semantics and failure exit.
    OwnedMutation,
}

/// Runtime representation requirement for one input operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputRepr {
    /// Must be `SsaValueRepr::I64` (int).
    Int,
    /// Must be a heap pointer of the given container type.
    HeapPtr(HeapInputKind),
    /// Any tagged value (used for owned mutation values).
    Tagged,
    /// Any representation; used as-is.
    Any,
}

/// Heap container kinds relevant to builtin specialization.
#[allow(dead_code)] // Variants used by future builtin families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeapInputKind {
    String,
    Bytes,
    Array,
    Map,
}

impl HeapInputKind {
    pub(crate) fn value_type(self) -> ValueType {
        match self {
            Self::String => ValueType::String,
            Self::Bytes => ValueType::Bytes,
            Self::Array => ValueType::Array,
            Self::Map => ValueType::Map,
        }
    }
}

/// Output type produced by a specialized builtin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputKind {
    Int,
    Bool,
    Tagged(ValueType),
    /// Tagged string with `ValueInfo::type_name()` (used by `TypeOf`).
    TypeName,
    /// Tagged value whose type is not statically known.
    #[allow(dead_code)] // Used by future builtin families.
    TaggedUnknown,
}

impl OutputKind {
    pub(crate) fn repr(self) -> SsaValueRepr {
        match self {
            Self::Int => SsaValueRepr::I64,
            Self::Bool => SsaValueRepr::Bool,
            Self::Tagged(_) | Self::TypeName | Self::TaggedUnknown => SsaValueRepr::Tagged,
        }
    }
}

/// Declarative specification for one specialized builtin.
///
/// The recorder reads this to drive generic analyze/emit paths.
/// Dedicated lowering implementations in `lower.rs` remain typed and
/// are *not* replaced by this table.
pub(crate) struct BuiltinSpec {
    /// Human-readable name for diagnostics.
    pub(crate) name: &'static str,
    /// Number of arguments popped from the analysis frame (in reverse order).
    pub(crate) arity: usize,
    /// Input requirements, in pop order (last argument first).
    pub(crate) inputs: &'static [InputRepr],
    /// Output type.
    pub(crate) output: OutputKind,
    /// Effect classification.
    #[allow(dead_code)] // Read by future lowering/registry generation.
    pub(crate) effect: BuiltinEffect,
    /// Whether the builtin requires a failure exit on helper error.
    #[allow(dead_code)] // Read by future lowering/registry generation.
    pub(crate) needs_failure_exit: bool,
}

/// `string.len()` — pure read, scalar result.
pub(crate) const STRING_LEN_SPEC: BuiltinSpec = BuiltinSpec {
    name: "string_len",
    arity: 1,
    inputs: &[InputRepr::HeapPtr(HeapInputKind::String)],
    output: OutputKind::Int,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `re_match(pattern, text)` — fallible helper with bridge error.
pub(crate) const REGEX_MATCH_SPEC: BuiltinSpec = BuiltinSpec {
    name: "regex_match",
    arity: 2,
    inputs: &[
        InputRepr::HeapPtr(HeapInputKind::String), // text (popped second)
        InputRepr::HeapPtr(HeapInputKind::String), // pattern (popped first)
    ],
    output: OutputKind::Bool,
    effect: BuiltinEffect::FallibleHelper,
    needs_failure_exit: true,
};

/// `array.set(index, value)` — owned mutation, aliasing, failure exit.
///
/// Note: the recorder additionally detects the append-pattern
/// (`index == array.len()`) and rewrites to `ArrayPush`. That
/// optimization is semantic, not mechanical, and stays in the
/// recorder's typed emit path.
pub(crate) const ARRAY_SET_SPEC: BuiltinSpec = BuiltinSpec {
    name: "array_set",
    arity: 3,
    inputs: &[
        InputRepr::Any,    // value (popped third)
        InputRepr::Int,    // index (popped second)
        InputRepr::Tagged, // array (popped first, must be owned Tagged)
    ],
    output: OutputKind::Tagged(ValueType::Array),
    effect: BuiltinEffect::OwnedMutation,
    needs_failure_exit: true,
};

// ── Family 1: len / type / predicate ────────────────────────────────

/// `len(value)` — pure read, scalar result.
pub(crate) const VALUE_LEN_SPEC: BuiltinSpec = BuiltinSpec {
    name: "value_len",
    arity: 1,
    inputs: &[InputRepr::Any],
    output: OutputKind::Int,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `bytes.len()` — pure read, scalar result.
pub(crate) const BYTES_LEN_SPEC: BuiltinSpec = BuiltinSpec {
    name: "bytes_len",
    arity: 1,
    inputs: &[InputRepr::HeapPtr(HeapInputKind::Bytes)],
    output: OutputKind::Int,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `array.len()` — pure read, scalar result.
pub(crate) const ARRAY_LEN_SPEC: BuiltinSpec = BuiltinSpec {
    name: "array_len",
    arity: 1,
    inputs: &[InputRepr::HeapPtr(HeapInputKind::Array)],
    output: OutputKind::Int,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `map.len()` — pure read, scalar result.
pub(crate) const MAP_LEN_SPEC: BuiltinSpec = BuiltinSpec {
    name: "map_len",
    arity: 1,
    inputs: &[InputRepr::HeapPtr(HeapInputKind::Map)],
    output: OutputKind::Int,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `type(value)` — pure read, tagged string result.
pub(crate) const TYPE_OF_SPEC: BuiltinSpec = BuiltinSpec {
    name: "type_of",
    arity: 1,
    inputs: &[InputRepr::Any],
    output: OutputKind::TypeName,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `string.contains(needle)` — pure read, bool result.
pub(crate) const STRING_CONTAINS_SPEC: BuiltinSpec = BuiltinSpec {
    name: "string_contains",
    arity: 2,
    inputs: &[
        InputRepr::HeapPtr(HeapInputKind::String), // needle (popped second)
        InputRepr::HeapPtr(HeapInputKind::String), // text (popped first)
    ],
    output: OutputKind::Bool,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// `array.has(index)` — pure read, bool result.
pub(crate) const ARRAY_HAS_SPEC: BuiltinSpec = BuiltinSpec {
    name: "array_has",
    arity: 2,
    inputs: &[
        InputRepr::Int,                           // index (popped second)
        InputRepr::HeapPtr(HeapInputKind::Array), // array (popped first)
    ],
    output: OutputKind::Bool,
    effect: BuiltinEffect::Pure,
    needs_failure_exit: false,
};

/// Look up the spec for a specialized builtin kind, if one exists.
///
/// Returns `None` for builtins not yet covered by the spec-driven
/// path; their hand-written recorder paths remain authoritative.
pub(crate) fn spec_for(
    kind: super::recorder::SpecializedBuiltinKind,
) -> Option<&'static BuiltinSpec> {
    match kind {
        super::recorder::SpecializedBuiltinKind::StringLen => Some(&STRING_LEN_SPEC),
        super::recorder::SpecializedBuiltinKind::RegexMatch => Some(&REGEX_MATCH_SPEC),
        super::recorder::SpecializedBuiltinKind::ArraySet => Some(&ARRAY_SET_SPEC),
        super::recorder::SpecializedBuiltinKind::ValueLen => Some(&VALUE_LEN_SPEC),
        super::recorder::SpecializedBuiltinKind::BytesLen => Some(&BYTES_LEN_SPEC),
        super::recorder::SpecializedBuiltinKind::ArrayLen => Some(&ARRAY_LEN_SPEC),
        super::recorder::SpecializedBuiltinKind::MapLen => Some(&MAP_LEN_SPEC),
        super::recorder::SpecializedBuiltinKind::TypeOf => Some(&TYPE_OF_SPEC),
        super::recorder::SpecializedBuiltinKind::StringContains => Some(&STRING_CONTAINS_SPEC),
        super::recorder::SpecializedBuiltinKind::ArrayHas => Some(&ARRAY_HAS_SPEC),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SPECS: &[&BuiltinSpec] = &[
        &STRING_LEN_SPEC,
        &REGEX_MATCH_SPEC,
        &ARRAY_SET_SPEC,
        &VALUE_LEN_SPEC,
        &BYTES_LEN_SPEC,
        &ARRAY_LEN_SPEC,
        &MAP_LEN_SPEC,
        &TYPE_OF_SPEC,
        &STRING_CONTAINS_SPEC,
        &ARRAY_HAS_SPEC,
    ];

    #[test]
    fn all_specs_have_consistent_arity_and_inputs() {
        for spec in ALL_SPECS {
            assert_eq!(
                spec.arity,
                spec.inputs.len(),
                "{}: arity must match inputs",
                spec.name
            );
        }
    }

    #[test]
    fn effect_classification_is_explicit() {
        const {
            assert!(matches!(STRING_LEN_SPEC.effect, BuiltinEffect::Pure));
            assert!(!STRING_LEN_SPEC.needs_failure_exit);
            assert!(matches!(
                REGEX_MATCH_SPEC.effect,
                BuiltinEffect::FallibleHelper
            ));
            assert!(REGEX_MATCH_SPEC.needs_failure_exit);
            assert!(matches!(
                ARRAY_SET_SPEC.effect,
                BuiltinEffect::OwnedMutation
            ));
            assert!(ARRAY_SET_SPEC.needs_failure_exit);
        }
        for spec in ALL_SPECS {
            if spec.needs_failure_exit {
                assert!(
                    !matches!(spec.effect, BuiltinEffect::Pure),
                    "{}: pure builtins must not need failure exit",
                    spec.name
                );
            }
        }
    }
}
