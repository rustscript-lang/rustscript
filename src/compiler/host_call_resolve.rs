//! Compiler-owned host-call resolution against the shared [`HostApiCatalog`].
//!
//! This module owns the *dispatch* half of the compiler ◀▶ host boundary.
//! Given an immutable [`HostApiCatalog`] plus the actual argument schemas at
//! a call site, it selects the legal overload when exactly one is viable, and
//! otherwise returns a structured reason it cannot. It consumes the
//! host-agnostic [`crate::host_api`] model and produces compiler
//! [`TypeSchema`] values via [`HostTypeSchema::to_compiler_schema`], so the
//! catalog itself never grows a dependency on the compiler.
//!
//! The dependency direction is intentionally **compiler → host_api only**:
//! [`crate::host_api`] stays standalone. This resolver is a pure adapter with
//! no parser, source-loader or compile-entrypoint wiring; later scopes drive
//! it against a live call site.
//!
//! ## Resolution invariants
//!
//! * **Name then arity.** An unknown name is a distinct
//!   [`HostCallResolveError::UnknownFunction`]. A declared name with no
//!   overload whose arity matches the call site is a distinct
//!   [`HostCallResolveError::ArityMismatch`].
//! * **Nominal resource matching.** A [`TypeSchema::Resource`] matches an
//!   expected resource **only when the key is equal**. Different keys are
//!   incompatible and surface the `expected resource<X>, found resource<Y>`
//!   diagnostic; parameters are never matched by structural fallback.
//! * **Exact vs numeric specificity.** A pair is *exact* when `expected ==
//!   actual` with no nested [`TypeSchema::Unknown`]; the relationship order is
//!   `Mismatch < Unknown < NumericCompat < Exact`. The sole numeric-compat
//!   case is [`TypeSchema::Number`] ↔ `Int`/`Float`. Container/callable
//!   aggregation keeps the *least-specific* nested relation, so an exact
//!   `array<Int>` overload outranks a numeric-compatible `array<Number>`
//!   overload for an actual `array<Int>`.
//! * **Unknown is a deferred/dynamic fallback, not a wildcard concrete match.**
//!   When either side of a pair is [`TypeSchema::Unknown`] (at any depth) the
//!   pair is compatible but *deferred*; the resolver never uses that latitude
//!   to silently choose one of several equally-specific overloads.
//! * **Deterministic selection.** Among viable candidates the most specific
//!   one (most concrete, then fewest deferred matches) wins. Two equally
//!   specific viable overloads produce a structured
//!   [`HostCallResolveError::Ambiguous`]; with no viable overload the resolver
//!   reports the best concrete mismatch via [`HostCallResolveError::NoMatch`].
//! * **Registration-order independence.** Best-candidate selection and every
//!   structured diagnostic (`NoMatch` detail, `ArityMismatch` variants,
//!   `Ambiguous` candidates) tie-break equal specificity by a stable semantic
//!   signature label and de-duplicate, so reversed catalog registration order
//!   yields byte-identical diagnostics.
//! * **Overload identity is already legal upstream.** The catalog rejects
//!   same-name + same argument identity at build time, so every overload seen
//!   here differs by argument schema or passing mode.
//!
//! The resolved result preserves the selected function name, compiler-mapped
//! parameter schemas, the return [`TypeSchema`], the ordered
//! [`HostParamPassing`] modes (so ownership metadata survives for later
//! enforcement) and the catalog fingerprint for cache/ABI correlation.

use std::fmt;

use crate::host_api::{HostApiCatalog, HostApiFingerprint, HostFunctionSchema, HostParamPassing};

use super::TypeSchema;

/// How one (expected parameter, actual argument) pair matched, as an ordered
/// specificity relation. The relation order matters: candidates are ranked by
/// the relation of every actual argument, so `Exact` strictly outranks
/// `NumericCompat`, which outranks the deferred `Unknown`, which outranks a
/// disqualifying `Mismatch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rel {
    /// Concrete, statically known mismatch (resource keys differ, or a scalar
    /// that is neither equal nor numeric-compatible). Disqualifies the
    /// overload.
    Mismatch = 0,
    /// The pair is compatible but involved [`TypeSchema::Unknown`] somewhere,
    /// so it is a deferred/dynamic fallback rather than a concrete match.
    Unknown = 1,
    /// Numeric compatibility under the compiler's rule: [`TypeSchema::Number`]
    /// accepts `Int`/`Float` (and vice-versa) without being a fully specific
    /// match. Distinct from [`Rel::Exact`] so an exact `array<Int>` overload
    /// outranks a merely numeric-compatible `array<Number>` overload for an
    /// actual `array<Int>`.
    NumericCompat = 2,
    /// A fully concrete, statically known exact match: expected == actual with
    /// no nested [`TypeSchema::Unknown`].
    Exact = 3,
}

/// One host function parameter mapped into the compiler's inference world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHostParam {
    /// Parameter label, unique within its function.
    pub name: String,
    /// Compiler-mapped value schema (resource keys preserved nominally).
    pub schema: TypeSchema,
}

/// A successfully resolved host call.
///
/// Indexes of [`Self::params`] and [`Self::passing`] are aligned; the
/// returning [`TypeSchema`] is the compiler view of the catalog's return
/// schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHostCall {
    /// The selected function name.
    pub name: String,
    /// Compiler-mapped parameter schemas, in declared order.
    pub params: Vec<ResolvedHostParam>,
    /// The return schema mapped onto the compiler's [`TypeSchema`].
    pub return_type: TypeSchema,
    /// Ordered [`HostParamPassing`] modes, index-aligned with [`Self::params`].
    ///
    /// `Borrow`/`BorrowMut`/`TakeOwned` survive resolution verbatim so later
    /// ownership enforcement can rely on them.
    pub passing: Vec<HostParamPassing>,
    /// The catalog fingerprint at resolution time, for provenance/ABI ties.
    pub fingerprint: HostApiFingerprint,
}

/// Why a host call could not be resolved to exactly one overload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCallResolveError {
    /// The name is not declared in the catalog at all.
    UnknownFunction(String),
    /// The name is declared but no overload has the given argument count.
    ArityMismatch {
        name: String,
        actual: usize,
        /// Distinct parameter counts declared across the overloads.
        expected: Vec<usize>,
        /// Signature labels of every declared overload, for diagnostics.
        variants: Vec<String>,
    },
    /// The name and arity exist, but no overload is viable. `detail` carries
    /// the best concrete mismatch (e.g. `expected resource<io.file>, found
    /// resource<sqlite.connection>`).
    NoMatch { name: String, detail: String },
    /// Several legally-viable overloads are equally specific; the resolver
    /// will not silently choose one.
    Ambiguous {
        name: String,
        /// Signatures of the equally-viable best candidates.
        candidates: Vec<String>,
    },
}

impl fmt::Display for HostCallResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFunction(name) => {
                write!(f, "unknown host function `{name}`")
            }
            Self::ArityMismatch {
                name,
                actual,
                expected,
                ..
            } => {
                let expected_list = if expected.is_empty() {
                    "none".to_string()
                } else {
                    expected
                        .iter()
                        .map(|count| count.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                write!(
                    f,
                    "host function `{name}` takes {expected_list} argument(s), but the call \
                     site passes {actual}"
                )
            }
            Self::NoMatch { name, detail } => {
                write!(
                    f,
                    "no host function `{name}` matches the arguments: {detail}"
                )
            }
            Self::Ambiguous { name, candidates } => write!(
                f,
                "ambiguous host function `{name}`: {} equally-viable overloads are all \
                 viable; pick an explicit argument type ({})",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for HostCallResolveError {}

/// Selection key for a candidate: the per-argument [`Rel`] ranks sorted
/// descending (most concrete first), so lexicographically larger keys are
/// strictly more specific and equal keys are an ambiguity tie.
type CandidateKey = Vec<u8>;

/// A compiler-owned, stateless resolver over an immutable [`HostApiCatalog`].
///
/// ```text
/// &HostApiCatalog ──▶ HostCallResolver ──▶ ResolvedHostCall | HostCallResolveError
/// ```
#[derive(Clone, Copy, Debug)]
pub struct HostCallResolver<'a> {
    catalog: &'a HostApiCatalog,
}

impl<'a> HostCallResolver<'a> {
    /// Wraps an immutable catalog for resolution.
    pub fn new(catalog: &'a HostApiCatalog) -> Self {
        Self { catalog }
    }

    /// The catalog this resolver reads from.
    pub fn catalog(&self) -> &'a HostApiCatalog {
        self.catalog
    }

    /// The catalog fingerprint, re-read at call time so callers never cache a
    /// stale digest.
    pub fn fingerprint(&self) -> HostApiFingerprint {
        self.catalog.fingerprint()
    }

    /// Resolves a host call by name and concrete argument schemas.
    ///
    /// `args` may contain [`TypeSchema::Unknown`] entries when the compiler
    /// never learned an argument's static type; those become deferred matches
    /// and can trigger [`HostCallResolveError::Ambiguous`] rather than a
    /// silent arbitrary pick.
    pub fn resolve(
        &self,
        name: &str,
        args: &[TypeSchema],
    ) -> Result<ResolvedHostCall, HostCallResolveError> {
        let named = self.catalog.functions_named(name);
        if named.is_empty() {
            return Err(HostCallResolveError::UnknownFunction(name.to_string()));
        }

        let arity = args.len();
        let mut arity_matching: Vec<&HostFunctionSchema> = Vec::new();
        let mut expected_arities: Vec<usize> = Vec::new();
        for function in &named {
            expected_arities.push(function.params.len());
            if function.params.len() == arity {
                arity_matching.push(function);
            }
        }
        if arity_matching.is_empty() {
            expected_arities.sort_unstable();
            expected_arities.dedup();
            // Stable, order-independent structured diagnostics: sort and
            // de-duplicate the variant labels so reversed registration order
            // yields an identical `ArityMismatch` payload.
            let mut variants: Vec<String> = named
                .iter()
                .map(|function| signature_label(function))
                .collect();
            variants.sort();
            variants.dedup();
            return Err(HostCallResolveError::ArityMismatch {
                name: name.to_string(),
                actual: arity,
                expected: expected_arities,
                variants,
            });
        }

        // Classify every arity-matching candidate against the actual args.
        let mut viable: Vec<(CandidateKey, &HostFunctionSchema)> = Vec::new();
        let mut non_viable: Vec<(CandidateKey, &HostFunctionSchema)> = Vec::new();
        for function in &arity_matching {
            let expected_schemas: Vec<TypeSchema> = function
                .params
                .iter()
                .map(|param| param.ty.to_compiler_schema())
                .collect();
            let rels: Vec<Rel> = expected_schemas
                .iter()
                .zip(args.iter())
                .map(|(expected, actual)| relate(expected, actual))
                .collect();
            let all_compatible = rels.iter().all(|rel| *rel != Rel::Mismatch);
            let key = sort_key(&rels);
            if all_compatible {
                viable.push((key, function));
            } else {
                non_viable.push((key, function));
            }
        }

        // Most-specific viable candidate.
        if let Some(best) = max_candidate(&viable) {
            let mut tied: Vec<&HostFunctionSchema> = viable
                .iter()
                .filter(|(key, _)| *key == best.0)
                .map(|(_, function)| *function)
                .collect();
            if tied.len() == 1 {
                return Ok(self.build_resolved(best.1));
            }
            // Order and de-dupe for a stable diagnostic.
            tied.sort_by_key(|function| signature_label(function));
            tied.dedup();
            return Err(HostCallResolveError::Ambiguous {
                name: name.to_string(),
                candidates: tied
                    .iter()
                    .map(|function| signature_label(function))
                    .collect(),
            });
        }

        // No viable candidate: report the best concrete mismatch.
        let (best_candidate, mismatch) = best_concrete_mismatch(&non_viable, args);
        let suffix = best_candidate
            .map(|function| format!("; best candidate is `{}`", signature_label(function)))
            .unwrap_or_default();
        let detail = best_mismatch_detail(suffix, mismatch);
        Err(HostCallResolveError::NoMatch {
            name: name.to_string(),
            detail,
        })
    }

    fn build_resolved(&self, function: &HostFunctionSchema) -> ResolvedHostCall {
        ResolvedHostCall {
            name: function.name.clone(),
            params: function
                .params
                .iter()
                .map(|param| ResolvedHostParam {
                    name: param.name.clone(),
                    schema: param.ty.to_compiler_schema(),
                })
                .collect(),
            return_type: function.return_type.to_compiler_schema(),
            passing: function.params.iter().map(|param| param.passing).collect(),
            fingerprint: self.catalog.fingerprint(),
        }
    }
}

fn rel_rank(rel: Rel) -> u8 {
    rel as u8
}

fn sort_key(rels: &[Rel]) -> CandidateKey {
    let mut ranks: Vec<u8> = rels.iter().map(|rel| rel_rank(*rel)).collect();
    ranks.sort_unstable_by(|a, b| b.cmp(a));
    ranks
}

/// Whether the schema tree mentions [`TypeSchema::Unknown`] anywhere.
fn contains_unknown(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Unknown => true,
        TypeSchema::Null
        | TypeSchema::Int
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::Bool
        | TypeSchema::String
        | TypeSchema::Bytes
        | TypeSchema::Resource(_)
        | TypeSchema::GenericParam(_) => false,
        TypeSchema::Optional(inner) | TypeSchema::Array(inner) | TypeSchema::Map(inner) => {
            contains_unknown(inner)
        }
        TypeSchema::Named(_, args) => args.iter().any(contains_unknown),
        TypeSchema::ArrayTuple(items) => items.iter().any(contains_unknown),
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            prefix.iter().any(contains_unknown) || contains_unknown(rest)
        }
        TypeSchema::Object(fields) => fields.values().any(contains_unknown),
        TypeSchema::Callable { params, result } => {
            params.iter().any(contains_unknown) || contains_unknown(result)
        }
    }
}

/// Classify one (expected, actual) schema pair under the catalog rules:
/// nominal resource identity, exact non-numeric schemas, `Number` accepting
/// `Int`/`Float`, deferred on any [`TypeSchema::Unknown`].
fn relate(expected: &TypeSchema, actual: &TypeSchema) -> Rel {
    if expected == actual {
        return if contains_unknown(expected) || contains_unknown(actual) {
            Rel::Unknown
        } else {
            Rel::Exact
        };
    }
    use TypeSchema::*;
    match (expected, actual) {
        // Unknown is a deferred/dynamic fallback, not a concrete match or a
        // hard mismatch.
        (Unknown, _) | (_, Unknown) => Rel::Unknown,
        (Number, Int | Float) | (Int | Float, Number) => Rel::NumericCompat,
        (Resource(expected_key), Resource(actual_key)) => {
            if expected_key == actual_key {
                Rel::Exact
            } else {
                Rel::Mismatch
            }
        }
        (Optional(e_inner), Optional(a_inner))
        | (Array(e_inner), Array(a_inner))
        | (Map(e_inner), Map(a_inner)) => relate(e_inner, a_inner),
        (ArrayTuple(e_items), ArrayTuple(a_items)) => relate_tuple(e_items, a_items),
        (
            ArrayTupleRest {
                prefix: e_p,
                rest: e_r,
            },
            ArrayTupleRest {
                prefix: a_p,
                rest: a_r,
            },
        ) => relate_tuple(e_p, a_p).min(relate(e_r, a_r)),
        (
            Callable {
                params: e_params,
                result: e_result,
            },
            Callable {
                params: a_params,
                result: a_result,
            },
        ) => {
            if e_params.len() != a_params.len() {
                return Rel::Mismatch;
            }
            let mut parts: Vec<Rel> = e_params
                .iter()
                .zip(a_params.iter())
                .map(|(e, a)| relate(e, a))
                .collect();
            parts.push(relate(e_result, a_result));
            aggregate(&parts)
        }
        (Named(e_name, e_args), Named(a_name, a_args)) => {
            if e_name != a_name || e_args.len() != a_args.len() {
                return Rel::Mismatch;
            }
            aggregate(
                &e_args
                    .iter()
                    .zip(a_args.iter())
                    .map(|(e, a)| relate(e, a))
                    .collect::<Vec<_>>(),
            )
        }
        (Object(e_fields), Object(a_fields)) => {
            if e_fields.len() != a_fields.len() {
                return Rel::Mismatch;
            }
            if e_fields.keys().any(|name| !a_fields.contains_key(name)) {
                return Rel::Mismatch;
            }
            aggregate(
                &e_fields
                    .iter()
                    .map(|(name, e_schema)| relate(e_schema, &a_fields[name]))
                    .collect::<Vec<_>>(),
            )
        }
        _ => Rel::Mismatch,
    }
}

fn relate_tuple(e_items: &[TypeSchema], a_items: &[TypeSchema]) -> Rel {
    if e_items.len() != a_items.len() {
        return Rel::Mismatch;
    }
    aggregate(
        &e_items
            .iter()
            .zip(a_items.iter())
            .map(|(e, a)| relate(e, a))
            .collect::<Vec<_>>(),
    )
}

/// Combine a container's part relations: one mismatch poisons the whole,
/// otherwise a single deferred element makes it deferred. Uses `min`, so the
/// aggregate preserves the *least-specific* nested relation — an `array<Int>`
/// (all `Exact`) outranks an `array<Number>` (a nested `NumericCompat`) for an
/// actual `array<Int>`.
fn aggregate(parts: &[Rel]) -> Rel {
    parts.iter().copied().fold(Rel::Exact, Rel::min)
}

/// The lexicographically maximum (most-specific) candidate key.
fn max_candidate<'f>(
    viable: &[(CandidateKey, &'f HostFunctionSchema)],
) -> Option<(CandidateKey, &'f HostFunctionSchema)> {
    if viable.is_empty() {
        return None;
    }
    let mut best_index = 0;
    for index in 1..viable.len() {
        if viable[index].0 > viable[best_index].0 {
            best_index = index;
        }
    }
    let (key, function) = &viable[best_index];
    Some((key.clone(), *function))
}

/// A single concrete mismatch within the best non-viable candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConcreteMismatch {
    /// Zero-based argument index.
    index: usize,
    /// Expected host schema label, e.g. `resource<io.file>`.
    expected: String,
    /// Actual (compiler) schema label, e.g. `resource<sqlite.connection>`.
    found: String,
    /// Specificity key of the owning candidate, for tie-break reporting.
    candidate_key: CandidateKey,
}

/// Picks the most concrete non-viable candidate and its first concrete
/// mismatch: `(candidate, mismatch)`.
///
/// Selection is independent of overload registration order: among the
/// non-viable candidates it first picks the maximum (most specific) relation
/// key, then among equally-specific keys tie-breaks by the stable semantic
/// [`signature_label`] rather than first registration order, so the reported
/// `NoMatch` detail is identical no matter how the catalog overloads were
/// registered.
fn best_concrete_mismatch<'f>(
    non_viable: &[(CandidateKey, &'f HostFunctionSchema)],
    args: &[TypeSchema],
) -> (Option<&'f HostFunctionSchema>, Option<ConcreteMismatch>) {
    if non_viable.is_empty() {
        return (None, None);
    }
    // Most specific candidate key (lexicographically largest) — order free.
    let best_key = non_viable
        .iter()
        .map(|(key, _)| key)
        .max()
        .expect("non-empty slice");
    // Among equally-specific candidates, tie-break on the stable semantic
    // signature label, not the order in which overloads were registered.
    let best_candidate = non_viable
        .iter()
        .filter(|(key, _)| key == best_key)
        .map(|(_, function)| *function)
        .min_by_key(|function| signature_label(function))
        .expect("at least one candidate holds the best key");
    let mismatch = best_candidate
        .params
        .iter()
        .enumerate()
        .find_map(|(index, param)| {
            let actual = &args[index];
            if relate(&param.ty.to_compiler_schema(), actual) == Rel::Mismatch {
                Some(ConcreteMismatch {
                    index,
                    expected: schema_label(&param.ty),
                    found: tf_schema_label(actual),
                    candidate_key: best_key.clone(),
                })
            } else {
                None
            }
        });
    (Some(best_candidate), mismatch)
}

/// Render the `NoMatch` detail from the best candidate's concrete mismatch.
fn best_mismatch_detail(suffix: String, mismatch: Option<ConcreteMismatch>) -> String {
    match mismatch {
        Some(mismatch) => format!(
            "argument {}: expected {}, found {}{}",
            mismatch.index, mismatch.expected, mismatch.found, suffix
        ),
        None => format!("concrete argument types do not match any declared overload{suffix}"),
    }
}

/// Convert a compiler [`TypeSchema`] into a diagnostic label equivalent to the
/// host schema vocabulary (`int`, `float`, `resource<io.file>`, …).
fn tf_schema_label(schema: &TypeSchema) -> String {
    match schema {
        TypeSchema::Unknown => "unknown".to_string(),
        TypeSchema::Null => "null".to_string(),
        TypeSchema::Int => "int".to_string(),
        TypeSchema::Float => "float".to_string(),
        TypeSchema::Number => "number".to_string(),
        TypeSchema::Bool => "bool".to_string(),
        TypeSchema::String => "string".to_string(),
        TypeSchema::Bytes => "bytes".to_string(),
        TypeSchema::Optional(inner) => format!("optional<{}>", tf_schema_label(inner)),
        TypeSchema::Array(inner) => format!("array<{}>", tf_schema_label(inner)),
        TypeSchema::ArrayTuple(items) => format!(
            "({})",
            items
                .iter()
                .map(tf_schema_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeSchema::ArrayTupleRest { prefix, rest } => format!(
            "({}.., {})",
            prefix
                .iter()
                .map(tf_schema_label)
                .collect::<Vec<_>>()
                .join(", "),
            tf_schema_label(rest)
        ),
        TypeSchema::Map(inner) => format!("map<{}>", tf_schema_label(inner)),
        TypeSchema::Object(fields) => {
            let mut entries: Vec<(String, String)> = fields
                .iter()
                .map(|(name, ty)| (name.clone(), tf_schema_label(ty)))
                .collect();
            entries.sort_by_key(|(name, _)| name.clone());
            let body = entries
                .iter()
                .map(|(name, ty)| format!("{name}: {ty}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("object<{body}>")
        }
        TypeSchema::Callable { params, result } => format!(
            "fn({}) -> {}",
            params
                .iter()
                .map(tf_schema_label)
                .collect::<Vec<_>>()
                .join(", "),
            tf_schema_label(result)
        ),
        TypeSchema::Named(name, args) => {
            if args.is_empty() {
                name.clone()
            } else {
                format!(
                    "{name}<{}>",
                    args.iter()
                        .map(tf_schema_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        TypeSchema::GenericParam(name) => name.clone(),
        TypeSchema::Resource(key) => format!("resource<{key}>"),
    }
}

/// Compact signature label, e.g. `read(resource<io.file>)`.
fn passing_label(passing: HostParamPassing) -> &'static str {
    match passing {
        HostParamPassing::Value => "",
        HostParamPassing::Borrow => "borrow",
        HostParamPassing::BorrowMut => "borrow_mut",
        HostParamPassing::TakeOwned => "take_owned",
    }
}

fn signature_label(function: &HostFunctionSchema) -> String {
    let args = function
        .params
        .iter()
        .map(|param| {
            let base = schema_label(&param.ty);
            if param.passing == HostParamPassing::Value {
                base
            } else {
                format!("{base} {}", passing_label(param.passing))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({args})", function.name)
}

/// Render a *host* schema to a friendly label (same vocabulary as the
/// catalog's `Display`).
fn schema_label(schema: &crate::host_api::HostTypeSchema) -> String {
    use crate::host_api::HostTypeSchema;
    match schema {
        HostTypeSchema::Unknown => "unknown".to_string(),
        HostTypeSchema::Null => "null".to_string(),
        HostTypeSchema::Int => "int".to_string(),
        HostTypeSchema::Float => "float".to_string(),
        HostTypeSchema::Number => "number".to_string(),
        HostTypeSchema::Bool => "bool".to_string(),
        HostTypeSchema::String => "string".to_string(),
        HostTypeSchema::Bytes => "bytes".to_string(),
        HostTypeSchema::Array(inner) => format!("array<{}>", schema_label(inner)),
        HostTypeSchema::Map(inner) => format!("map<{}>", schema_label(inner)),
        HostTypeSchema::Optional(inner) => format!("optional<{}>", schema_label(inner)),
        HostTypeSchema::Callable { params, result } => {
            let params = params
                .iter()
                .map(schema_label)
                .collect::<Vec<_>>()
                .join(", ");
            format!("fn({params}) -> {}", schema_label(result))
        }
        HostTypeSchema::Resource(key) => format!("resource<{key}>"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::TypeSchema as Ts;
    use crate::host_api::{
        HostApiBuilder, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
        ResourceTypeKey, ResourceTypeSchema,
    };

    fn io_file() -> ResourceTypeKey {
        ResourceTypeKey::new("io.file").expect("valid key")
    }
    fn sqlite_conn() -> ResourceTypeKey {
        ResourceTypeKey::new("sqlite.connection").expect("valid key")
    }
    fn resource(key: ResourceTypeKey) -> HostTypeSchema {
        HostTypeSchema::Resource(key)
    }
    fn compiler_resource(key: ResourceTypeKey) -> Ts {
        Ts::Resource(key)
    }
    fn value_param(name: &str, ty: HostTypeSchema) -> HostParamSchema {
        HostParamSchema::value(name, ty)
    }
    fn ref_param(name: &str, ty: HostTypeSchema, passing: HostParamPassing) -> HostParamSchema {
        HostParamSchema::with_passing(name, ty, passing)
    }

    /// Two nominal resource types plus a small, overloaded function surface used
    /// by most resolution tests.
    fn concrete_catalog() -> HostApiCatalog {
        let mut b = HostApiBuilder::new();
        b.resource(ResourceTypeSchema::new(io_file(), "An open file"));
        b.resource(ResourceTypeSchema::new(
            sqlite_conn(),
            "An open SQLite connection",
        ));
        b.function(HostFunctionSchema::with_return(
            "io::open",
            vec![
                value_param("path", HostTypeSchema::String),
                value_param("mode", HostTypeSchema::String),
            ],
            resource(io_file()),
        ));
        b.function(HostFunctionSchema::with_return(
            "sqlite::open",
            vec![value_param("path", HostTypeSchema::String)],
            resource(sqlite_conn()),
        ));
        b.function(HostFunctionSchema::with_return(
            "io::read_all",
            vec![ref_param(
                "handle",
                resource(io_file()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::String,
        ));
        b.function(HostFunctionSchema::with_return(
            "file::scrub",
            vec![
                ref_param("handle", resource(io_file()), HostParamPassing::BorrowMut),
                value_param("buf", HostTypeSchema::Bytes),
            ],
            HostTypeSchema::Int,
        ));
        b.function(HostFunctionSchema::with_return(
            "file::reap",
            vec![ref_param(
                "handle",
                resource(io_file()),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Int,
        ));
        b.function(HostFunctionSchema::with_return(
            "sqlite::exec",
            vec![
                ref_param("db", resource(sqlite_conn()), HostParamPassing::BorrowMut),
                value_param("sql", HostTypeSchema::String),
            ],
            HostTypeSchema::Int,
        ));
        b.build().expect("valid catalog")
    }

    #[test]
    fn resolves_io_open_and_infers_file_return() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        let resolved = resolver
            .resolve("io::open", &[Ts::String, Ts::String])
            .expect("io::open resolves");
        assert_eq!(resolved.name, "io::open");
        assert_eq!(resolved.return_type, compiler_resource(io_file()));
        assert_eq!(resolved.params.len(), 2);
        assert_eq!(resolved.params[0].schema, Ts::String);
    }

    #[test]
    fn resolves_sqlite_open_and_infers_connection_return() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        let resolved = resolver
            .resolve("sqlite::open", &[Ts::String])
            .expect("sqlite::open resolves");
        assert_eq!(resolved.name, "sqlite::open");
        assert_eq!(resolved.return_type, compiler_resource(sqlite_conn()));
    }

    #[test]
    fn preserves_borrow_borrowmut_takeowned_passing() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);

        let read = resolver
            .resolve("io::read_all", &[compiler_resource(io_file())])
            .expect("read_all resolves");
        assert_eq!(read.passing, vec![HostParamPassing::Borrow]);

        let scrub = resolver
            .resolve("file::scrub", &[compiler_resource(io_file()), Ts::Bytes])
            .expect("scrub resolves");
        assert_eq!(
            scrub.passing,
            vec![HostParamPassing::BorrowMut, HostParamPassing::Value]
        );

        let reap = resolver
            .resolve("file::reap", &[compiler_resource(io_file())])
            .expect("reap resolves");
        assert_eq!(reap.passing, vec![HostParamPassing::TakeOwned]);

        let exec = resolver
            .resolve(
                "sqlite::exec",
                &[compiler_resource(sqlite_conn()), Ts::String],
            )
            .expect("exec resolves");
        assert_eq!(
            exec.passing,
            vec![HostParamPassing::BorrowMut, HostParamPassing::Value]
        );
    }

    #[test]
    fn overloads_by_resource_type() {
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(io_file(), "file"));
        builder.resource(ResourceTypeSchema::new(sqlite_conn(), "db"));
        builder.function(HostFunctionSchema::with_return(
            "consume",
            vec![ref_param(
                "h",
                resource(io_file()),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "consume",
            vec![ref_param(
                "h",
                resource(sqlite_conn()),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::String,
        ));
        let catalog = builder.build().expect("legal resource overloads");
        let resolver = HostCallResolver::new(&catalog);

        let file = resolver
            .resolve("consume", &[compiler_resource(io_file())])
            .expect("file overload");
        assert_eq!(file.return_type, Ts::Int);
        assert_eq!(file.passing, vec![HostParamPassing::TakeOwned]);

        let db = resolver
            .resolve("consume", &[compiler_resource(sqlite_conn())])
            .expect("db overload");
        assert_eq!(db.return_type, Ts::String);
    }

    #[test]
    fn overloads_by_scalar_type() {
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "count",
            vec![value_param("v", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "count",
            vec![value_param(
                "v",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Int)),
            )],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("validity");
        let resolver = HostCallResolver::new(&catalog);

        let scalar = resolver.resolve("count", &[Ts::Int]).expect("int overload");
        assert_eq!(scalar.params[0].schema, Ts::Int);

        let array = resolver
            .resolve("count", &[Ts::Array(Box::new(Ts::Int))])
            .expect("array overload");
        assert_eq!(scalar.params.len(), array.params.len());
    }

    #[test]
    fn number_accepts_int_and_float() {
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "amount",
            vec![value_param("n", HostTypeSchema::Number)],
            HostTypeSchema::String,
        ));
        let catalog = builder.build().expect("validity");
        let resolver = HostCallResolver::new(&catalog);

        assert_eq!(
            resolver.resolve("amount", &[Ts::Int]).unwrap().name,
            "amount"
        );
        assert_eq!(
            resolver.resolve("amount", &[Ts::Float]).unwrap().name,
            "amount"
        );
        // A String is not numeric, so the Number overload is a concrete mismatch.
        assert!(matches!(
            resolver.resolve("amount", &[Ts::String]),
            Err(HostCallResolveError::NoMatch { name, .. }) if name == "amount"
        ));
    }

    #[test]
    fn int_param_rejects_float_argument() {
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "exact",
            vec![value_param("n", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("validity");
        let resolver = HostCallResolver::new(&catalog);
        assert!(matches!(
            resolver.resolve("exact", &[Ts::Float]),
            Err(HostCallResolveError::NoMatch { .. })
        ));
    }

    #[test]
    fn unknown_argument_with_single_viable_candidate_falls_back() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        // Only one `io::read_all` overload; an Unknown argument is a deferred
        // match and resolves without ambiguity.
        let resolved = resolver
            .resolve("io::read_all", &[Ts::Unknown])
            .expect("unambiguous fallback");
        assert_eq!(resolved.name, "io::read_all");
        assert_eq!(resolved.passing, vec![HostParamPassing::Borrow]);
    }

    #[test]
    fn unknown_argument_with_tied_overloads_is_ambiguous() {
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "parse",
            vec![value_param("v", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "parse",
            vec![value_param("v", HostTypeSchema::String)],
            HostTypeSchema::String,
        ));
        let catalog = builder.build().expect("validity");
        let resolver = HostCallResolver::new(&catalog);
        assert!(matches!(resolver.resolve("parse", &[Ts::Int]), Ok(..)));
        assert!(matches!(
            resolver.resolve("parse", &[Ts::Unknown]),
            Err(HostCallResolveError::Ambiguous { name, .. }) if name == "parse"
        ));
    }

    #[test]
    fn wrong_resource_reports_expected_found_diagnostic() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        // io::read_all expects resource<io.file>; pass a sqlite connection.
        let err = resolver
            .resolve("io::read_all", &[compiler_resource(sqlite_conn())])
            .unwrap_err();
        match err {
            HostCallResolveError::NoMatch { name, detail } => {
                assert_eq!(name, "io::read_all");
                assert!(
                    detail.contains("expected resource<io.file>"),
                    "detail lacked expected resource: {detail}"
                );
                assert!(
                    detail.contains("found resource<sqlite.connection>"),
                    "detail lacked found resource: {detail}"
                );
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn wrong_resource_inside_nested_array_reports_labels() {
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(io_file(), "file"));
        builder.function(HostFunctionSchema::with_return(
            "collect",
            vec![ref_param(
                "files",
                HostTypeSchema::Array(Box::new(resource(io_file()))),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("validity");
        let resolver = HostCallResolver::new(&catalog);
        let actual = Ts::Array(Box::new(compiler_resource(sqlite_conn())));
        let err = resolver.resolve("collect", &[actual]).unwrap_err();
        match err {
            HostCallResolveError::NoMatch { detail, .. } => {
                assert!(detail.contains("expected array<resource<io.file>>"));
                assert!(detail.contains("found array<resource<sqlite.connection>>"));
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn nested_resource_schema_resolves() {
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(io_file(), "file"));
        builder.function(HostFunctionSchema::with_return(
            "collect",
            vec![ref_param(
                "files",
                HostTypeSchema::Array(Box::new(resource(io_file()))),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("validity");
        let resolver = HostCallResolver::new(&catalog);
        let resolved = resolver
            .resolve(
                "collect",
                &[Ts::Array(Box::new(compiler_resource(io_file())))],
            )
            .expect("nested resource overload");
        assert_eq!(
            resolved.params[0].schema,
            Ts::Array(Box::new(compiler_resource(io_file())))
        );
    }

    #[test]
    fn unknown_function_is_distinct_error() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        assert_eq!(
            resolver.resolve("no_such_fn", &[]),
            Err(HostCallResolveError::UnknownFunction("no_such_fn".into()))
        );
    }

    #[test]
    fn arity_mismatch_is_distinct_error() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        // sqlite::open takes exactly one argument.
        assert!(matches!(
            resolver.resolve("sqlite::open", &[Ts::String, Ts::String]),
            Err(HostCallResolveError::ArityMismatch { name, actual: 2, .. })
                if name == "sqlite::open"
        ));
        // io::read_all takes exactly one argument.
        let err = resolver
            .resolve("io::read_all", &[Ts::String, Ts::String])
            .unwrap_err();
        assert!(matches!(err, HostCallResolveError::ArityMismatch { .. }));
    }

    #[test]
    fn fingerprint_propagates_into_resolved_result() {
        let catalog = concrete_catalog();
        let resolver = HostCallResolver::new(&catalog);
        let expected = resolver.fingerprint();
        let resolved = resolver
            .resolve(
                "sqlite::exec",
                &[compiler_resource(sqlite_conn()), Ts::String],
            )
            .expect("resolves");
        assert_eq!(resolved.fingerprint, expected);
        assert_eq!(resolved.fingerprint, catalog.fingerprint());
    }

    #[test]
    fn fingerprint_differs_across_catalogs() {
        let base = concrete_catalog();
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(io_file(), "file"));
        builder.function(HostFunctionSchema::with_return(
            "io::read_all",
            vec![ref_param(
                "handle",
                resource(io_file()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Bytes, // different return => different fingerprint
        ));
        let other = builder.build().expect("validity");
        assert_ne!(base.fingerprint(), other.fingerprint());
        let resolver = HostCallResolver::new(&other);
        let resolved = resolver
            .resolve("io::read_all", &[compiler_resource(io_file())])
            .expect("resolves");
        assert_eq!(resolved.fingerprint, other.fingerprint());
    }

    #[test]
    fn scalar_exact_beats_numeric_for_int_number_float() {
        // f(Int) and f(Number), distinguished by return type: Int/Number
        // resolve exactly, Float must land on f(Number) because f(Int) is a
        // concrete (not numeric) mismatch for a Float.
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "scale",
            vec![value_param("v", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "scale",
            vec![value_param("v", HostTypeSchema::Number)],
            HostTypeSchema::String,
        ));
        let catalog = builder.build().expect("valid scalar overloads");
        let resolver = HostCallResolver::new(&catalog);

        let via_int = resolver.resolve("scale", &[Ts::Int]).expect("Int resolves");
        assert_eq!(
            via_int.return_type,
            Ts::Int,
            "f(Int) exact must beat f(Number) numeric-compat for an Int"
        );

        let via_number = resolver
            .resolve("scale", &[Ts::Number])
            .expect("Number resolves");
        assert_eq!(
            via_number.return_type,
            Ts::String,
            "f(Number) exact must beat f(Int) numeric-compat for a Number"
        );

        let via_float = resolver
            .resolve("scale", &[Ts::Float])
            .expect("Float resolves");
        assert_eq!(
            via_float.return_type,
            Ts::String,
            "Float must pick f(Number); f(Int) is a concrete mismatch for Float"
        );
    }

    #[test]
    fn nested_array_numeric_specificity_prefers_exact() {
        // array<Int> is exact for an actual array<Int> and must outrank the
        // nested numeric-compatible array<Number>; array<Number> is exact for
        // array<Number> and the only viable candidate for array<Float>.
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "sum",
            vec![value_param(
                "xs",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Int)),
            )],
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "sum",
            vec![value_param(
                "xs",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Number)),
            )],
            HostTypeSchema::String,
        ));
        let catalog = builder.build().expect("valid overloads");
        let resolver = HostCallResolver::new(&catalog);

        let ints = resolver
            .resolve("sum", &[Ts::Array(Box::new(Ts::Int))])
            .expect("int array resolves");
        assert_eq!(
            ints.return_type,
            Ts::Int,
            "exact array<Int> must beat numeric array<Number> for an actual array<Int>"
        );

        let numbers = resolver
            .resolve("sum", &[Ts::Array(Box::new(Ts::Number))])
            .expect("number array resolves");
        assert_eq!(
            numbers.return_type,
            Ts::String,
            "array<Number> is exact for an actual array<Number>"
        );

        let floats = resolver
            .resolve("sum", &[Ts::Array(Box::new(Ts::Float))])
            .expect("float array resolves");
        assert_eq!(
            floats.return_type,
            Ts::String,
            "array<Int> is non-viable for an actual array<Float>; array<Number> is nested-numeric"
        );
    }

    #[test]
    fn nomatch_detail_is_registration_order_independent() {
        fn catalog(io_first: bool) -> HostApiCatalog {
            let mut builder = HostApiBuilder::new();
            builder.resource(ResourceTypeSchema::new(io_file(), "file"));
            builder.resource(ResourceTypeSchema::new(sqlite_conn(), "db"));
            let io = HostFunctionSchema::with_return(
                "take",
                vec![ref_param(
                    "h",
                    resource(io_file()),
                    HostParamPassing::Borrow,
                )],
                HostTypeSchema::Int,
            );
            let sqlite = HostFunctionSchema::with_return(
                "take",
                vec![ref_param(
                    "h",
                    resource(sqlite_conn()),
                    HostParamPassing::Borrow,
                )],
                HostTypeSchema::String,
            );
            if io_first {
                builder.function(io);
                builder.function(sqlite);
            } else {
                builder.function(sqlite);
                builder.function(io);
            }
            builder.build().expect("valid")
        }

        // A String is a concrete mismatch for both resource overloads; both
        // are equally (in)viable, so the *reported* best candidate must not
        // depend on registration order.
        let err_a = HostCallResolver::new(&catalog(true))
            .resolve("take", &[Ts::String])
            .unwrap_err();
        let err_b = HostCallResolver::new(&catalog(false))
            .resolve("take", &[Ts::String])
            .unwrap_err();
        match (err_a, err_b) {
            (
                HostCallResolveError::NoMatch { detail: first, .. },
                HostCallResolveError::NoMatch { detail: second, .. },
            ) => {
                assert_eq!(
                    first, second,
                    "NoMatch detail must be identical regardless of registration order"
                );
                assert!(
                    first.contains("resource<io.file>"),
                    "surprising detail: {first}"
                );
            }
            (a, b) => panic!("expected NoMatch in both orders, got {a:?} / {b:?}"),
        }
    }

    #[test]
    fn arity_mismatch_structured_variants_are_order_independent() {
        fn g_catalog(forward: bool) -> HostApiCatalog {
            let mut builder = HostApiBuilder::new();
            let one = || {
                HostFunctionSchema::with_return(
                    "g",
                    vec![value_param("a", HostTypeSchema::Int)],
                    HostTypeSchema::Int,
                )
            };
            let two_int = || {
                HostFunctionSchema::with_return(
                    "g",
                    vec![
                        value_param("a", HostTypeSchema::Int),
                        value_param("b", HostTypeSchema::Int),
                    ],
                    HostTypeSchema::Int,
                )
            };
            let two_str = || {
                HostFunctionSchema::with_return(
                    "g",
                    vec![
                        value_param("a", HostTypeSchema::String),
                        value_param("b", HostTypeSchema::String),
                    ],
                    HostTypeSchema::String,
                )
            };
            if forward {
                builder.function(one());
                builder.function(two_int());
                builder.function(two_str());
            } else {
                builder.function(two_str());
                builder.function(two_int());
                builder.function(one());
            }
            builder.build().expect("valid")
        }

        let err_a = HostCallResolver::new(&g_catalog(true))
            .resolve("g", &[Ts::Int, Ts::Int, Ts::Int])
            .unwrap_err();
        let err_b = HostCallResolver::new(&g_catalog(false))
            .resolve("g", &[Ts::Int, Ts::Int, Ts::Int])
            .unwrap_err();
        match (err_a, err_b) {
            (
                HostCallResolveError::ArityMismatch {
                    actual,
                    expected,
                    variants,
                    ..
                },
                HostCallResolveError::ArityMismatch {
                    actual: actual_b,
                    expected: expected_b,
                    variants: variants_b,
                    ..
                },
            ) => {
                assert_eq!(actual, 3);
                assert_eq!(expected, vec![1, 2]);
                assert_eq!(
                    variants,
                    vec![
                        "g(int)".to_string(),
                        "g(int, int)".to_string(),
                        "g(string, string)".to_string(),
                    ]
                );
                // Reversed registration must produce byte-identical payloads.
                assert_eq!(actual_b, actual);
                assert_eq!(expected_b, expected);
                assert_eq!(variants_b, variants);
            }
            (a, b) => panic!("expected ArityMismatch in both orders, got {a:?} / {b:?}"),
        }
    }

    #[test]
    fn passing_mode_only_overloads_are_ambiguous() {
        // Same resource argument shape in all three overloads, differing only in
        // the Borrow/BorrowMut/TakeOwned passing mode. The catalog allows these
        // (distinct argument passing identity) but the call site supplies only a
        // schema and no passing intent, so resolution must stay ambiguous.
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(io_file(), "file"));
        for passing in [
            HostParamPassing::Borrow,
            HostParamPassing::BorrowMut,
            HostParamPassing::TakeOwned,
        ] {
            builder.function(HostFunctionSchema::with_return(
                "consume",
                vec![ref_param("h", resource(io_file()), passing)],
                HostTypeSchema::Int,
            ));
        }
        let catalog = builder
            .build()
            .expect("passing-mode-only overloads are legal");
        let resolver = HostCallResolver::new(&catalog);
        assert!(matches!(
            resolver.resolve("consume", &[compiler_resource(io_file())]),
            Err(HostCallResolveError::Ambiguous { name, .. }) if name == "consume"
        ));
    }
}
