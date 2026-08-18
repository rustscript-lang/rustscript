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
//! * **Exact scalar/container/callable matching** with one documented
//!   exception: [`TypeSchema::Number`] accepts `Int`/`Float` (and vice-versa),
//!   mirroring the compiler's existing numeric-compatibility rule.
//! * **Unknown is a deferred/dynamic fallback, not a wildcard concrete match.**
//!   When either side of a pair is [`TypeSchema::Unknown`] (at any depth) the
//!   pair is compatible but *deferred*; the resolver never uses that latitude
//!   to silently choose one of several equally-specific overloads.
//! * **Deterministic selection.** Among viable candidates the most specific
//!   one (most concrete, then fewest deferred matches) wins. Two equally
//!   specific viable overloads produce a structured
//!   [`HostCallResolveError::Ambiguous`]; with no viable overload the resolver
//!   reports the best concrete mismatch via [`HostCallResolveError::NoMatch`].
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

/// How one (expected parameter, actual argument) pair matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rel {
    /// Concrete, statically known mismatch (resource keys differ, or a scalar
    /// that is neither equal nor numeric-compatible). Disqualifies the
    /// overload.
    Mismatch = 0,
    /// The pair is compatible but involved [`TypeSchema::Unknown`] somewhere,
    /// so it is a deferred/dynamic fallback rather than a concrete match.
    Unknown = 1,
    /// A fully concrete, statically known match.
    Concrete = 2,
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
            return Err(HostCallResolveError::ArityMismatch {
                name: name.to_string(),
                actual: arity,
                expected: expected_arities,
                variants: named
                    .iter()
                    .map(|function| signature_label(function))
                    .collect(),
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
            Rel::Concrete
        };
    }
    use TypeSchema::*;
    match (expected, actual) {
        // Unknown is a deferred/dynamic fallback, not a concrete match or a
        // hard mismatch.
        (Unknown, _) | (_, Unknown) => Rel::Unknown,
        (Number, Int | Float) | (Int | Float, Number) => Rel::Concrete,
        (Resource(expected_key), Resource(actual_key)) => {
            if expected_key == actual_key {
                Rel::Concrete
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
/// otherwise a single deferred element makes it deferred.
fn aggregate(parts: &[Rel]) -> Rel {
    parts.iter().copied().fold(Rel::Concrete, Rel::min)
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
fn best_concrete_mismatch<'f>(
    non_viable: &[(CandidateKey, &'f HostFunctionSchema)],
    args: &[TypeSchema],
) -> (Option<&'f HostFunctionSchema>, Option<ConcreteMismatch>) {
    let mut best_candidate: Option<&'f HostFunctionSchema> = None;
    let mut best_key: Option<CandidateKey> = None;
    let mut best_mismatch: Option<ConcreteMismatch> = None;
    for (key, function) in non_viable {
        // Prefer the more specific candidate; on exact ties keep the first
        // (registration order).
        match (&best_key, key) {
            (None, _) => {}
            (Some(best), candidate) if candidate <= best => continue,
            _ => {}
        }
        let mismatch = function
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
                        candidate_key: key.clone(),
                    })
                } else {
                    None
                }
            });
        best_candidate = Some(function);
        best_key = Some(key.clone());
        best_mismatch = mismatch.or(best_mismatch);
    }
    (best_candidate, best_mismatch)
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
fn signature_label(function: &HostFunctionSchema) -> String {
    let args = function
        .params
        .iter()
        .map(|param| schema_label(&param.ty))
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
}
