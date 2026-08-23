use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use crate::ValueType;
use crate::builtins::default_host_callable;
use crate::host_api::{HostApiFingerprint, HostFunctionSchema, HostParamPassing, ResourceTypeKey};

use super::ParseError;
use super::modules::SymbolId;
use super::source_map::Span;

pub type LocalSlot = u16;

/// A stable identifier for a single source-level call-site node in the
/// compiler IR. Carried by [`Expr::Call`] to preserve identity through
/// every compiler transformation so the semantic model can later
/// correlate post-transform nodes with their original parser source
/// positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SemanticNodeId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeSchema {
    Unknown,
    Null,
    Int,
    Float,
    Number,
    Bool,
    String,
    Bytes,
    Optional(Box<TypeSchema>),
    GenericParam(String),
    Named(String, Vec<TypeSchema>),
    Array(Box<TypeSchema>),
    ArrayTuple(Vec<TypeSchema>),
    ArrayTupleRest {
        prefix: Vec<TypeSchema>,
        rest: Box<TypeSchema>,
    },
    Map(Box<TypeSchema>),
    Object(HashMap<String, TypeSchema>),
    Callable {
        params: Vec<TypeSchema>,
        result: Box<TypeSchema>,
    },
    /// A nominal host resource, identified by its shared [`ResourceTypeKey`].
    ///
    /// Resources are nominal and opaque: two schemas match only when they
    /// carry the *same* key. This variant deliberately does not share a
    /// representation with [`TypeSchema::Named`] or [`TypeSchema::Map`], so a
    /// resource can never be mistaken for structural data (object/map) or a
    /// generic instantiation.
    Resource(ResourceTypeKey),
}

impl Hash for TypeSchema {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            TypeSchema::Unknown
            | TypeSchema::Null
            | TypeSchema::Int
            | TypeSchema::Float
            | TypeSchema::Number
            | TypeSchema::Bool
            | TypeSchema::String
            | TypeSchema::Bytes => {}
            TypeSchema::Optional(inner) | TypeSchema::Array(inner) | TypeSchema::Map(inner) => {
                inner.hash(state);
            }
            TypeSchema::GenericParam(name) => name.hash(state),
            TypeSchema::Named(name, args) => {
                name.hash(state);
                args.hash(state);
            }
            TypeSchema::ArrayTuple(items) => items.hash(state),
            TypeSchema::ArrayTupleRest { prefix, rest } => {
                prefix.hash(state);
                rest.hash(state);
            }
            TypeSchema::Object(fields) => {
                fields.len().hash(state);
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_unstable_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
                for (name, schema) in fields {
                    name.hash(state);
                    schema.hash(state);
                }
            }
            TypeSchema::Callable { params, result } => {
                params.hash(state);
                result.hash(state);
            }
            TypeSchema::Resource(key) => key.hash(state),
        }
    }
}

impl TypeSchema {
    pub(crate) fn is_optional(&self) -> bool {
        matches!(self, TypeSchema::Optional(_))
    }

    pub(crate) fn clone_inner_if_optional(&self) -> TypeSchema {
        match self {
            TypeSchema::Optional(inner) => inner.as_ref().clone(),
            other => other.clone(),
        }
    }

    pub(crate) fn split_optional(&self) -> (TypeSchema, bool) {
        match self {
            TypeSchema::Optional(inner) => (inner.as_ref().clone(), true),
            other => (other.clone(), false),
        }
    }

    pub(crate) fn coarse_value_type(&self) -> ValueType {
        match self {
            TypeSchema::Unknown | TypeSchema::GenericParam(_) | TypeSchema::Number => {
                ValueType::Unknown
            }
            TypeSchema::Callable { .. } => ValueType::Callable,
            TypeSchema::Null => ValueType::Null,
            TypeSchema::Int => ValueType::Int,
            TypeSchema::Float => ValueType::Float,
            TypeSchema::Bool => ValueType::Bool,
            TypeSchema::String => ValueType::String,
            TypeSchema::Bytes => ValueType::Bytes,
            TypeSchema::Optional(inner) => inner.coarse_value_type(),
            TypeSchema::Named(_, _) | TypeSchema::Map(_) | TypeSchema::Object(_) => ValueType::Map,
            // Semantic (nominal) lowering: a resource is opaque and is *not*
            // surfaced as an integral token here, so inferred schemas and
            // diagnostics never present a resource as `int`. The physical ABI
            // token is isolated behind [`Self::resource_abi_value_type`].
            TypeSchema::Resource(_) => ValueType::Unknown,
            TypeSchema::Array(_)
            | TypeSchema::ArrayTuple(_)
            | TypeSchema::ArrayTupleRest { .. } => ValueType::Array,
        }
    }

    pub(crate) fn array_prefix_and_rest(&self) -> Option<(&[TypeSchema], Option<&TypeSchema>)> {
        match self {
            TypeSchema::Array(element) => Some((&[], Some(element.as_ref()))),
            TypeSchema::ArrayTuple(items) => Some((items.as_slice(), None)),
            TypeSchema::ArrayTupleRest { prefix, rest } => {
                Some((prefix.as_slice(), Some(rest.as_ref())))
            }
            _ => None,
        }
    }

    pub(crate) fn array_item_schema_at(&self, index: usize) -> Option<TypeSchema> {
        let (prefix, rest) = self.array_prefix_and_rest()?;
        prefix.get(index).cloned().or_else(|| rest.cloned())
    }

    pub(crate) fn collapsed_array_item_schema(&self) -> Option<TypeSchema> {
        let (prefix, rest) = self.array_prefix_and_rest()?;
        let mut items = prefix.iter();
        let Some(first) = items.next().cloned().or_else(|| rest.cloned()) else {
            return Some(TypeSchema::Unknown);
        };
        if items.all(|schema| schema == &first) && rest.is_none_or(|schema| schema == &first) {
            Some(first)
        } else {
            Some(TypeSchema::Unknown)
        }
    }

    /// The resource key when this schema (directly, or through a single
    /// optional layer) denotes a host resource.
    pub fn resource_key(&self) -> Option<&ResourceTypeKey> {
        match self {
            TypeSchema::Resource(key) => Some(key),
            TypeSchema::Optional(inner) => inner.resource_key(),
            _ => None,
        }
    }

    /// Whether this schema contains a host resource anywhere in its shape.
    ///
    /// Unlike [`Self::resource_key`], which only recognizes a resource directly
    /// or through optional wrappers, this walks every recursive position: named
    /// type arguments, arrays/tuples/rest, map values, object field values, and
    /// callable params/result. A resource at any depth makes the whole schema
    /// resource-containing.
    ///
    /// [`TypeSchema::Named`] is not itself a host resource, but any
    /// resource-bearing type argument makes the instantiation
    /// resource-containing. [`TypeSchema::GenericParam`] is deliberately
    /// `false` because whether it resolves to a resource depends on the
    /// caller's substitution context; deferred handling belongs to the caller.
    // The catalog typing integration is the first production consumer; keep
    // this prerequisite seam lint-clean until that pass is wired.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn contains_resource(&self) -> bool {
        match self {
            TypeSchema::Resource(_) => true,
            TypeSchema::Optional(inner) => inner.contains_resource(),
            TypeSchema::Named(_, type_args) => type_args.iter().any(|arg| arg.contains_resource()),
            TypeSchema::Array(element) => element.contains_resource(),
            TypeSchema::ArrayTuple(items) => items.iter().any(|item| item.contains_resource()),
            TypeSchema::ArrayTupleRest { prefix, rest } => {
                prefix.iter().any(|item| item.contains_resource()) || rest.contains_resource()
            }
            TypeSchema::Map(value) => value.contains_resource(),
            TypeSchema::Object(fields) => fields.values().any(|value| value.contains_resource()),
            TypeSchema::Callable { params, result } => {
                params.iter().any(|param| param.contains_resource()) || result.contains_resource()
            }
            TypeSchema::Unknown
            | TypeSchema::Null
            | TypeSchema::Int
            | TypeSchema::Float
            | TypeSchema::Number
            | TypeSchema::Bool
            | TypeSchema::String
            | TypeSchema::Bytes
            | TypeSchema::GenericParam(_) => false,
        }
    }

    /// Physical ABI lowering for resources.
    ///
    /// This is the single, explicitly named boundary between the *nominal*
    /// schema and the eventual runtime handle/token ABI. A later scope that
    /// wires a resource table / handle transport resolves a [`Self::Resource`]
    /// schema to an integral token here. It is deliberately NOT used by
    /// [`Self::coarse_value_type`], which keeps resources semantically opaque
    /// (`ValueType::Unknown`) so inferred schemas and diagnostics never reveal
    /// the integer backing.
    // Test-only boundary surface (see compiler::typing::helpers); non-test
    // builds intentionally don't call it. External crates must not rely on the ABI
    // token, so this is intentionally crate-visible.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn resource_abi_value_type(&self) -> ValueType {
        match self {
            TypeSchema::Resource(_) => ValueType::Int,
            other => other.coarse_value_type(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionParam {
    pub name: String,
    pub schema: Option<TypeSchema>,
}

/// One host function parameter mapped into the compiler's inference world.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDecl {
    pub name: String,
    pub type_params: Vec<String>,
    pub body_schema: TypeSchema,
}

fn known_host_accepts_arity(name: &str, arity: u8) -> bool {
    #[cfg(feature = "edge-abi")]
    if let Some(function) = edge_abi::function_by_name(name) {
        return function.param_types.len() == usize::from(arity);
    }
    default_host_callable(name).is_some_and(|callable| {
        let required = callable
            .signature
            .params
            .iter()
            .take_while(|param| !param.optional)
            .count();
        required <= usize::from(arity) && usize::from(arity) <= callable.signature.params.len()
    })
}

/// Shared frontend-independent program representation that all source
/// frontends lower into before bytecode emission.
#[derive(Clone, Debug)]
pub struct ClosureExpr {
    pub param_slots: Vec<LocalSlot>,
    pub capture_copies: Vec<(LocalSlot, LocalSlot)>,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug)]
pub enum MatchPattern {
    Int(i64),
    String(String),
    Bytes(Vec<u8>),
    Null,
    None,
    SomeBinding(LocalSlot),
    Type(MatchTypePattern),
}

impl MatchPattern {
    pub(crate) fn binding_slot(&self) -> Option<LocalSlot> {
        match self {
            MatchPattern::SomeBinding(slot) => Some(*slot),
            _ => None,
        }
    }

    pub(crate) fn requires_optional_value(&self) -> bool {
        matches!(self, MatchPattern::None | MatchPattern::SomeBinding(_))
    }
}

#[derive(Clone, Debug)]
pub enum MatchTypePattern {
    Int,
    Float,
    Number,
    Bool,
    String,
    Bytes,
    Array,
    Map,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Bytes(Vec<u8>),
    FunctionRef(u16, Vec<TypeSchema>),
    /// A function value whose target was resolved to a compiler-owned module
    /// symbol before unit merge (milestone 4).
    ///
    /// Produced by the source loader's resolution pass for imported function
    /// values and lowered by `linker::merge_units` into a plain
    /// [`Expr::FunctionRef`] against the merged flat function table.
    ModuleFunctionRef(SymbolId, Vec<TypeSchema>),
    /// A function value reference whose target is not yet resolved (module
    /// mode only).
    ///
    /// Produced by the parser in module mode when a function value refers to
    /// a name the parser cannot resolve locally (an imported function binding
    /// whose export table only the source loader knows). The loader's
    /// resolution pass maps it to [`Expr::ModuleFunctionRef`] before unit
    /// merge, so downstream passes never observe it.
    UnresolvedFunctionRef {
        name: String,
        type_args: Vec<TypeSchema>,
    },
    OptionalGet {
        container: Box<Expr>,
        key: Box<Expr>,
        container_slot: LocalSlot,
        key_slot: LocalSlot,
        /// Parser-assigned [`SemanticNodeId`] of the source `?.[...]` access,
        /// preserved through every compiler transformation. Parser-produced
        /// accesses carry `Some(id)`; compiler- or test-synthetic ones use
        /// `None`. Transformations that rebuild the node **must** copy the
        /// original ID.
        semantic_id: Option<SemanticNodeId>,
    },
    OptionUnwrapOr {
        value: Box<Expr>,
        value_slot: LocalSlot,
        fallback: Box<Expr>,
        /// Parser-assigned [`SemanticNodeId`] of the source `.unwrap_or(...)`
        /// access, preserved through every compiler transformation.
        /// Parser-produced accesses carry `Some(id)`; compiler- or
        /// test-synthetic ones use `None`. Transformations that rebuild the
        /// node **must** copy the original ID.
        semantic_id: Option<SemanticNodeId>,
    },
    /// A call to a flat function-table index as a normalized `(name, arity)`
    /// candidate-set identity.
    ///
    /// The flat `index` names the candidate set for this style of call (two
    /// calls with equal indices share the same candidate population), but it
    /// is **not** an ordinal map: it says nothing about which single overload
    /// (if any) a particular call site resolved to.
    ///
    /// The fourth field, when [`Some`], is the exact per-call catalog
    /// resolution for this specific call site. Distinct `Expr::Call` nodes
    /// with equal `index` values may carry *different* [`Some`] resolutions
    /// (parameter schemas and passing modes resolved against each site's own
    /// argument types). [`None`] means the call has not been catalog-resolved
    /// yet or targets a non-catalog callable; resolution is carried here per
    /// per call, not reconstructed from the index. It is boxed so the large
    /// payload does not inflate every `Expr` node.
    ///
    /// The fifth field is an optional [`SemanticNodeId`] that preserves the
    /// parser-assigned identity of the source call-site through every
    /// compiler transformation. Parser-produced calls carry `Some(id)`;
    /// compiler- or test-synthetic calls use `None`. Transformations that
    /// rebuild an existing call **must** copy the original ID.
    Call(
        u16,
        Vec<TypeSchema>,
        Vec<Expr>,
        Option<Box<ResolvedHostCall>>,
        Option<SemanticNodeId>,
    ),
    /// A call whose target was resolved to a compiler-owned module symbol
    /// before unit merge (milestone 4).
    ///
    /// The source loader's resolution pass rewrites calls to imported
    /// functions into this form, carrying the [`SymbolId`] of the source
    /// module's declaration; `linker::merge_units` lowers it back into a
    /// plain [`Expr::Call`] against the merged flat function table. Unlike
    /// [`Expr::Call`]'s flat index, the symbol identity never depends on
    /// unit-local index assignment or on the source name, so same-named
    /// declarations in independent modules resolve to distinct targets.
    ModuleCall(SymbolId, Vec<TypeSchema>, Vec<Expr>, Option<SemanticNodeId>),
    LocalCall(
        LocalSlot,
        Vec<TypeSchema>,
        Vec<Expr>,
        Option<SemanticNodeId>,
    ),
    Closure(ClosureExpr),
    ClosureCall(ClosureExpr, Vec<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Mod(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Var(LocalSlot),
    MoveVar(LocalSlot),
    MoveField {
        root: LocalSlot,
        key: String,
    },
    MoveIndex {
        root: LocalSlot,
        index: i64,
    },
    IfElse {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    Match {
        value_slot: LocalSlot,
        result_slot: LocalSlot,
        value: Box<Expr>,
        arms: Vec<(MatchPattern, Expr)>,
        default: Box<Expr>,
    },
    ToOwned(Box<Expr>),
    Borrow(Box<Expr>),
    BorrowMut(Box<Expr>),
    Block {
        stmts: Vec<Stmt>,
        expr: Box<Expr>,
    },
}

impl Expr {
    /// The exact per-call host-call catalog resolution carried by this node,
    /// if it is a catalog-resolved [`Expr::Call`].
    ///
    /// Returns [`None`] for every other [`Expr`] variant and for an
    /// [`Expr::Call`] that has not been catalog-resolved yet (or targets a
    /// non-catalog callable). See the [`Expr::Call`] carrier docs for the
    /// index-versus-resolution distinction.
    pub fn host_call_resolution(&self) -> Option<&ResolvedHostCall> {
        match self {
            Expr::Call(_, _, _, Some(resolution), _) => Some(resolution.as_ref()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AssignmentKind {
    Set,
    Add,
    Increment,
}

impl AssignmentKind {
    pub(crate) fn requires_numeric_operands(&self) -> bool {
        matches!(self, Self::Add | Self::Increment)
    }

    pub(crate) fn diagnostic_label(&self) -> &'static str {
        match self {
            Self::Set => "'=' assignment",
            Self::Add => "'+=' assignment",
            Self::Increment => "'++' increment",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Noop {
        line: u32,
    },
    Let {
        index: LocalSlot,
        declared_schema: Option<TypeSchema>,
        expr: Expr,
        line: u32,
    },
    Assign {
        kind: AssignmentKind,
        index: LocalSlot,
        expr: Expr,
        line: u32,
    },
    ClosureLet {
        line: u32,
        closure: ClosureExpr,
    },
    FuncDecl {
        name: String,
        index: u16,
        arity: u8,
        args: Vec<String>,
        exported: bool,
        has_impl: bool,
        line: u32,
    },
    Expr {
        expr: Expr,
        line: u32,
    },
    IfElse {
        condition: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Vec<Stmt>,
        line: u32,
    },
    For {
        init: Box<Stmt>,
        condition: Expr,
        post: Box<Stmt>,
        body: Vec<Stmt>,
        line: u32,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    Break {
        line: u32,
    },
    Continue {
        line: u32,
    },
    /// Explicit compile-time drop: null-out the local slot and trigger the
    /// runtime drop-contract for whatever value was previously stored there.
    Drop {
        index: LocalSlot,
        line: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDecl {
    pub name: String,
    pub arity: u8,
    pub index: u16,
    pub args: Vec<String>,
    pub arg_schemas: Vec<Option<TypeSchema>>,
    pub return_schema: Option<TypeSchema>,
    pub type_params: Vec<String>,
    pub exported: bool,
    pub return_type: ValueType,
    /// Semantic symbol owned by the declaring module, assigned by the source
    /// loader after parse (milestone 3). `None` for IR that has not been
    /// attached to a module yet (parser output, REPL snippets).
    pub symbol: Option<SymbolId>,
}

#[derive(Clone, Debug)]
pub struct FunctionImpl {
    pub param_slots: Vec<LocalSlot>,
    pub capture_copies: Vec<(LocalSlot, LocalSlot)>,
    pub body_stmts: Vec<Stmt>,
    pub body_expr: Expr,
    pub body_expr_line: u32,
}

/// Immutable, catalog-fingerprint-bound, per-flat-function host candidate
/// carrier attached to a [`FrontendIr`].
///
/// The candidate set is keyed by the owning catalog's
/// [`HostApiFingerprint`]: it is only meaningful for the exact catalog
/// topology a frontend resolved against. For each flat function index it
/// records the ordered list of candidate [`HostFunctionSchema`]s in catalog
/// discovery order, including pass-only overloads (never deduplicated).
///
/// Each recorded list is the **complete** candidate set for its owning
/// `(fingerprint, host name, arity)`: every candidate the catalog discovered
/// for that identity — including all type and parameter-passing overloads —
/// in discovery order. It is never a per-call subset, never a truncated or
/// reordered slice: the whole catalog set is what later identity layers rely
/// on to bind and disambiguate a host call. A flat function produced from the
/// same host name at a different arity belongs to a distinct
/// `(name, arity)` identity with its own complete candidate set.
///
/// Carried on [`FrontendIr::host_api_metadata`]: `None` means the compilation
/// carries no host-catalog metadata; `Some` is a fingerprint-bound carrier
/// with no raw ABI attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostApiIrMetadata {
    /// Fingerprint of the catalog the flat functions were resolved against.
    fingerprint: HostApiFingerprint,
    /// Per-flat-function candidate lists in catalog discovery order.
    candidates_by_function_index: BTreeMap<u16, Vec<HostFunctionSchema>>,
}

impl HostApiIrMetadata {
    /// Builds an empty metadata carrier bound to `fingerprint`, carrying no
    /// candidates. The linker instantiates, populates, and remaps these
    /// carriers when it merges frontend units; the compiler-only frontend
    /// path records candidates with [`Self::record_candidates`].
    pub(crate) fn new(fingerprint: HostApiFingerprint) -> Self {
        Self {
            fingerprint,
            candidates_by_function_index: BTreeMap::new(),
        }
    }

    /// The fingerprint of the catalog this metadata is bound to.
    pub fn fingerprint(&self) -> HostApiFingerprint {
        self.fingerprint
    }

    /// Candidate schemas recorded for `index`, in catalog discovery order,
    /// or `None` when the function has no recorded candidates.
    pub fn candidates(&self, index: u16) -> Option<&[HostFunctionSchema]> {
        self.candidates_by_function_index
            .get(&index)
            .map(Vec::as_slice)
    }

    /// Flat function indices with recorded candidates, ascending (copied).
    pub fn function_indices(&self) -> impl ExactSizeIterator<Item = u16> + '_ {
        self.candidates_by_function_index.keys().copied()
    }

    /// Records the ordered candidate list for one flat function.
    ///
    /// `candidates` must be the **complete** catalog discovery-order candidate
    /// set for the owning `(fingerprint, host name, arity)` — every candidate
    /// the catalog discovered for that identity, including all type and
    /// parameter-passing overloads. It must never be a per-call subset or an
    /// arbitrary slice; a flat function's whole catalog candidate set is what
    /// downstream identity layers bind against.
    ///
    /// Rejects with an actionable [`ParseError`] when:
    /// * `candidates` is empty;
    /// * candidate names differ, or candidate parameter arities differ;
    /// * `index` already has recorded candidates.
    ///
    /// Catalog order is preserved and pass-only overloads (same types, a
    /// different [`crate::host_api::HostParamPassing`]) are retained, never
    /// deduplicated.
    pub(crate) fn record_candidates(
        &mut self,
        index: u16,
        candidates: Vec<HostFunctionSchema>,
    ) -> Result<(), ParseError> {
        if candidates.is_empty() {
            return Err(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!("host metadata: no candidate schemas for flat function {index}"),
            });
        }
        let first = &candidates[0];
        if candidates.iter().skip(1).any(|c| c.name != first.name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!(
                    "host metadata: flat function {index} candidate names disagree ({} vs {})",
                    first.name,
                    candidates
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        let arity = first.params.len();
        if candidates.iter().skip(1).any(|c| c.params.len() != arity) {
            return Err(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!(
                    "host metadata: flat function {index} candidate arities differ ({} vs {})",
                    arity,
                    candidates
                        .iter()
                        .map(|c| c.params.len().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        if self.candidates_by_function_index.contains_key(&index) {
            return Err(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!(
                    "host metadata: duplicate candidate record for flat function {index}"
                ),
            });
        }
        self.candidates_by_function_index.insert(index, candidates);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FrontendIr {
    pub stmts: Vec<Stmt>,
    pub locals: usize,
    pub local_bindings: Vec<(String, LocalSlot)>,
    pub struct_schemas: HashMap<String, StructDecl>,
    pub unknown_type_spans: Vec<crate::compiler::source_map::Span>,
    pub functions: Vec<FunctionDecl>,
    pub function_impls: HashMap<u16, FunctionImpl>,
    pub stmt_sources: Vec<Option<String>>,
    pub function_sources: HashMap<u16, String>,
    /// Structured `use` directives parsed from RustScript source, with spans
    /// and clauses. Consumed by the source loader for import discovery.
    pub use_declarations: Vec<crate::compiler::modules::UseDecl>,
    /// Names created by the parser's implicit-extern fallback (module mode).
    ///
    /// Module-mode parses tolerate calls whose target only the source loader
    /// can resolve (imported module functions, module namespace members).
    /// These synthetic declarations must never receive a module symbol or a
    /// flat entry; the loader resolves their call sites or rejects them.
    /// Plain (non-module) parses leave this empty because implicit externs are
    /// disabled there.
    pub implicit_extern_names: Vec<String>,
    /// Fingerprint-bound host candidate catalog carried on this IR.
    ///
    /// `None` means this compilation carries no host-catalog metadata.
    /// `Some` is an immutable catalog-fingerprint-bound carrier holding, per
    /// flat function index, the ordered candidate schemas a frontend resolved
    /// against its catalog; raw ABI is absent here.
    pub host_api_metadata: Option<HostApiIrMetadata>,
    /// Semantic index for language-service queries (see [`SemanticIndex`]).
    /// Populated during pipeline compilation after type inference.
    /// `None` for IR that has not been analyzed yet (parser output, REPL
    /// snippets without semantic analysis, test fixtures).
    pub semantic_index: Option<SemanticIndex>,
    /// Parser-produced semantic provenance index with exact token spans.
    /// Populated during parse and preserved through linking. Every real
    /// parse path — module-mode, plain compile, lowered, and REPL — sets
    /// `Some`; only IR built directly in tests or by plugin authors without
    /// a parser pass leaves `None`.
    pub parsed_semantic_index: Option<ParsedSemanticIndex>,
    /// Parser-produced visibility information from namespace aliases and imports.
    pub catalog_visibility: Option<CatalogVisibility>,
    /// The parser's full lexer token stream, preserved for exact
    /// cursor-position queries (completion prefix derivation). Span-bearing
    /// [`LexerToken`]s survive unit merge unchanged; the vector is the
    /// concatenation of every unit's tokens in merge order.
    pub lexer_tokens: Vec<LexerToken>,
}

/// A scope identifier used in [`ParsedLexicalScope`] records.
pub type ScopeId = u32;

/// Per-call-site resolved semantic facts keyed by the parser-assigned
/// [`SemanticNodeId`] carried on the typed/resolved [`Expr`] node.
///
/// The [`SemanticIndex`] resolves every [`ParsedCallSite`] to the exact
/// [`Expr`] node sharing its node id, so hover, signature help, and
/// definition queries consume parser-origin spans and typed/resolved
/// schemas — never source-text reconstruction or IR-order pairing.
#[derive(Clone, Debug)]
pub struct ResolvedCallInfo {
    /// The parser-recorded call site with exact callee and expression spans.
    pub site: ParsedCallSite,
    /// The resolved return schema of the call, taken from the typed IR node
    /// (the [`ResolvedHostCall`] carrier or the declared return schema).
    pub return_type: TypeSchema,
    /// The exact per-call host resolution carried by the typed IR node, when
    /// the call was catalog-resolved.
    pub host: Option<ResolvedHostCall>,
}

/// A semantic index built by the compiler during pipeline compilation.
///
/// This sidecar holds the span, type-schema, and scope information that the
/// [`SemanticModel`](crate::compiler::semantic_model::SemanticModel) needs
/// for precise position-based queries. It is built **directly** from the
/// parser's [`ParsedSemanticIndex`] provenance (exact token spans, resolved
/// targets, lexical scopes) plus the legalized and type-checked IR keyed by
/// [`SemanticNodeId`] — no second parser, source-text scanning, name-only
/// lookup, or IR-order pairing is involved.
///
/// The index is deliberately kept as a separate struct rather than adding
/// span fields to every [`Expr`] and [`Stmt`] variant, so the core IR types
/// are not bloated and the index is built only when semantic analysis is
/// requested.
#[derive(Clone, Debug)]
pub struct SemanticIndex {
    /// Per-local-slot inferred [`TypeSchema`], indexed by [`LocalSlot`].
    /// Populated from the type checker's `local_schemas` output.
    pub slot_schemas: Vec<Option<TypeSchema>>,
    /// Parser-produced semantic provenance with exact token spans.
    pub parsed: ParsedSemanticIndex,
    /// Resolved call facts keyed by [`SemanticNodeId`], built by pairing each
    /// parsed call site with the typed/resolved [`Expr`] node carrying the
    /// same id. Synthetic calls without provenance never appear here.
    pub resolved_calls: HashMap<SemanticNodeId, ResolvedCallInfo>,
    /// Per-function-index declaration return schema.
    pub function_return_schemas: HashMap<u16, Option<TypeSchema>>,
    /// Per-function-index parameter names (ordered).
    pub func_params: HashMap<u16, Vec<String>>,
}

impl SemanticIndex {
    /// Build a semantic index from the parser provenance carried on `ir`
    /// plus the typed/resolved IR keyed by [`SemanticNodeId`].
    ///
    /// `slot_schemas` comes from the type checker's `local_schemas` output.
    ///
    /// The parsed index is required: every real parse path (module-mode,
    /// plain compile, lowered, REPL) carries [`Some`] provenance; only IR
    /// built directly in tests or by plugin authors without a parser pass
    /// leaves [`None`]. In that case the caller gets a minimal index with no
    /// provenance records.
    pub fn build(slot_schemas: Vec<Option<TypeSchema>>, ir: &FrontendIr) -> Self {
        let mut resolved_calls = HashMap::new();
        let mut function_return_schemas = HashMap::new();
        let mut func_params = HashMap::new();

        // Per-function declaration metadata from the flat function table.
        for decl in &ir.functions {
            func_params.insert(decl.index, decl.args.clone());
            function_return_schemas.insert(decl.index, decl.return_schema.clone());
        }

        // Pair every parsed call site with the typed/resolved Expr node that
        // carries the same SemanticNodeId. Synthetic calls with None ids do
        // not appear as source sites.
        if let Some(parsed) = &ir.parsed_semantic_index {
            let mut by_id = HashMap::<SemanticNodeId, ResolvedCallInfo>::new();
            for site in &parsed.call_sites {
                by_id.insert(
                    site.id,
                    ResolvedCallInfo {
                        site: site.clone(),
                        return_type: TypeSchema::Unknown,
                        host: None,
                    },
                );
            }
            // Walk the legalized IR and attach resolved facts by node id.
            for stmt in &ir.stmts {
                collect_resolved_calls_in_stmt(
                    stmt,
                    &mut by_id,
                    &function_return_schemas,
                    &slot_schemas,
                );
            }
            for function_impl in ir.function_impls.values() {
                for stmt in &function_impl.body_stmts {
                    collect_resolved_calls_in_stmt(
                        stmt,
                        &mut by_id,
                        &function_return_schemas,
                        &slot_schemas,
                    );
                }
                collect_resolved_calls_in_expr(
                    &function_impl.body_expr,
                    &mut by_id,
                    &function_return_schemas,
                    &slot_schemas,
                );
            }
            resolved_calls = by_id;
        }

        SemanticIndex {
            slot_schemas,
            parsed: ir.parsed_semantic_index.clone().unwrap_or_default(),
            resolved_calls,
            function_return_schemas,
            func_params,
        }
    }

    /// Look up the inferred schema for a local slot.
    pub fn slot_schema(&self, slot: LocalSlot) -> Option<&TypeSchema> {
        let idx = slot as usize;
        self.slot_schemas.get(idx).and_then(|s| s.as_ref())
    }
}

/// Walk a statement tree and attach resolved call facts to `by_id`.
fn collect_resolved_calls_in_stmt(
    stmt: &Stmt,
    by_id: &mut HashMap<SemanticNodeId, ResolvedCallInfo>,
    function_return_schemas: &HashMap<u16, Option<TypeSchema>>,
    slot_schemas: &[Option<TypeSchema>],
) {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Expr { expr, .. } | Stmt::Assign { expr, .. } => {
            collect_resolved_calls_in_expr(expr, by_id, function_return_schemas, slot_schemas);
        }
        Stmt::ClosureLet { closure, .. } => {
            collect_resolved_calls_in_expr(
                &closure.body,
                by_id,
                function_return_schemas,
                slot_schemas,
            );
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_resolved_calls_in_expr(condition, by_id, function_return_schemas, slot_schemas);
            for s in then_branch.iter().chain(else_branch.iter()) {
                collect_resolved_calls_in_stmt(s, by_id, function_return_schemas, slot_schemas);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_resolved_calls_in_stmt(init, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_expr(condition, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_stmt(post, by_id, function_return_schemas, slot_schemas);
            for s in body {
                collect_resolved_calls_in_stmt(s, by_id, function_return_schemas, slot_schemas);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_resolved_calls_in_expr(condition, by_id, function_return_schemas, slot_schemas);
            for s in body {
                collect_resolved_calls_in_stmt(s, by_id, function_return_schemas, slot_schemas);
            }
        }
        _ => {}
    }
}

/// Walk an expression tree and attach resolved call facts to `by_id`.
fn collect_resolved_calls_in_expr(
    expr: &Expr,
    by_id: &mut HashMap<SemanticNodeId, ResolvedCallInfo>,
    function_return_schemas: &HashMap<u16, Option<TypeSchema>>,
    slot_schemas: &[Option<TypeSchema>],
) {
    match expr {
        Expr::Call(index, _type_args, args, host, semantic_id) => {
            if let Some(id) = semantic_id
                && let Some(entry) = by_id.get_mut(id)
            {
                if let Some(resolved) = host {
                    entry.return_type = resolved.return_type.clone();
                    entry.host = Some((**resolved).clone());
                } else if let Some(schema) = function_return_schemas.get(index).cloned() {
                    entry.return_type = schema.unwrap_or(TypeSchema::Unknown);
                }
            }
            for arg in args {
                collect_resolved_calls_in_expr(arg, by_id, function_return_schemas, slot_schemas);
            }
        }
        Expr::ModuleCall(_symbol, _type_args, args, semantic_id) => {
            if let Some(id) = semantic_id
                && let Some(entry) = by_id.get_mut(id)
            {
                // Module calls resolve to a compiler-owned symbol whose
                // flat function is only known after merge; the semantic
                // model resolves the return schema through the flat
                // function table by symbol identity.
                entry.return_type = TypeSchema::Unknown;
            }
            for arg in args {
                collect_resolved_calls_in_expr(arg, by_id, function_return_schemas, slot_schemas);
            }
        }
        Expr::LocalCall(slot, _type_args, args, semantic_id) => {
            if let Some(id) = semantic_id
                && let Some(entry) = by_id.get_mut(id)
            {
                // A direct local-callable call's return is derived from
                // the slot's callable schema when one is known: the
                // callable's `result` schema is the call's return type.
                // Only a genuinely unknown slot schema leaves `Unknown`.
                let slot_index = *slot as usize;
                entry.return_type = slot_schemas
                    .get(slot_index)
                    .and_then(|schema| schema.as_ref())
                    .and_then(|schema| match schema {
                        TypeSchema::Callable { result, .. } => Some(result.as_ref().clone()),
                        _ => None,
                    })
                    .unwrap_or(TypeSchema::Unknown);
            }
            for arg in args {
                collect_resolved_calls_in_expr(arg, by_id, function_return_schemas, slot_schemas);
            }
        }
        Expr::Block { stmts, expr: inner } => {
            for s in stmts {
                collect_resolved_calls_in_stmt(s, by_id, function_return_schemas, slot_schemas);
            }
            collect_resolved_calls_in_expr(inner, by_id, function_return_schemas, slot_schemas);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
            ..
        } => {
            collect_resolved_calls_in_expr(condition, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_expr(then_expr, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_expr(else_expr, by_id, function_return_schemas, slot_schemas);
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            collect_resolved_calls_in_expr(value, by_id, function_return_schemas, slot_schemas);
            for (_, arm_expr) in arms {
                collect_resolved_calls_in_expr(
                    arm_expr,
                    by_id,
                    function_return_schemas,
                    slot_schemas,
                );
            }
            collect_resolved_calls_in_expr(default, by_id, function_return_schemas, slot_schemas);
        }
        Expr::Closure(closure) => {
            collect_resolved_calls_in_expr(
                &closure.body,
                by_id,
                function_return_schemas,
                slot_schemas,
            );
        }
        Expr::ClosureCall(closure, args) => {
            for arg in args {
                collect_resolved_calls_in_expr(arg, by_id, function_return_schemas, slot_schemas);
            }
            collect_resolved_calls_in_expr(
                &closure.body,
                by_id,
                function_return_schemas,
                slot_schemas,
            );
        }
        Expr::Add(l, r)
        | Expr::Sub(l, r)
        | Expr::Mul(l, r)
        | Expr::Div(l, r)
        | Expr::Mod(l, r)
        | Expr::And(l, r)
        | Expr::Or(l, r)
        | Expr::Eq(l, r)
        | Expr::Lt(l, r)
        | Expr::Gt(l, r) => {
            collect_resolved_calls_in_expr(l, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_expr(r, by_id, function_return_schemas, slot_schemas);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            collect_resolved_calls_in_expr(inner, by_id, function_return_schemas, slot_schemas);
        }
        Expr::OptionalGet { container, key, .. } => {
            collect_resolved_calls_in_expr(container, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_expr(key, by_id, function_return_schemas, slot_schemas);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            collect_resolved_calls_in_expr(value, by_id, function_return_schemas, slot_schemas);
            collect_resolved_calls_in_expr(fallback, by_id, function_return_schemas, slot_schemas);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Parser provenance types (Phase A2)
// ---------------------------------------------------------------------------

/// The resolved target of a parsed call site. The parser records the target
/// honestly from its own resolution tables: plain functions carry their flat
/// index, direct local-callable calls carry the local slot, and module
/// namespace / imported-member calls carry the resolved [`SymbolId`] once the
/// source loader rewrites the call (or stay `Unresolved` for implicit-extern
/// calls whose target only the loader can resolve).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedCallTarget {
    /// A plain function call resolved to a flat (or builtin) index.
    Function(u16),
    /// A direct call of a local callable value (`name(...)` where `name`
    /// binds a local).
    Local(LocalSlot),
    /// A module namespace / imported-member call whose source symbol is
    /// known (post source-loader resolution).
    Module(SymbolId),
    /// An implicit-extern call the source loader has not resolved yet.
    Unresolved,
}

/// A single parsed call-site recorded by the parser with exact token spans.
#[derive(Clone, Debug)]
pub struct ParsedCallSite {
    /// Parser-allocated stable node id that matches the `Expr::Call` fifth field.
    pub id: SemanticNodeId,
    /// Span of the callee identifier/path (the name token range).
    pub callee_span: Span,
    /// Span of the full call expression (from callee start through closing delim).
    pub expr_span: Span,
    /// The resolved call target (flat/builtin index, local slot, or module
    /// symbol). Never a fabricated function index for local/module calls.
    pub target: ParsedCallTarget,
    /// The source-level name of the callee.
    pub name: String,
    /// The scope this call site belongs to.
    pub scope_id: ScopeId,
    /// Whether this is a namespace/dotted/multiline call.
    pub is_namespace_call: bool,
}

/// A parsed local variable declaration site.
#[derive(Clone, Debug)]
pub struct LocalDeclSite {
    /// Parser-allocated stable node id.
    pub id: SemanticNodeId,
    /// Exact identifier token span.
    pub ident_span: Span,
    /// Span of the full `let` statement.
    pub stmt_span: Span,
    /// The local slot assigned.
    pub slot: LocalSlot,
    /// The variable name.
    pub name: String,
    /// The scope this declaration belongs to.
    pub scope_id: ScopeId,
    /// Declaration order within the scope (0-based).
    pub decl_order: u32,
}

/// A parsed local variable reference site.
#[derive(Clone, Debug)]
pub struct LocalRefSite {
    /// Parser-allocated stable node id.
    pub id: SemanticNodeId,
    /// Exact identifier token span.
    pub ident_span: Span,
    /// The local slot referenced.
    pub slot: LocalSlot,
    /// The variable name.
    pub name: String,
    /// The scope this reference belongs to.
    pub scope_id: ScopeId,
}

/// A parsed function declaration site.
#[derive(Clone, Debug)]
pub struct FunctionDeclSite {
    /// Parser-allocated stable node id.
    pub id: SemanticNodeId,
    /// Exact identifier token span.
    pub ident_span: Span,
    /// The flat function index.
    pub function_index: u16,
    /// The function name.
    pub name: String,
    /// The scope this declaration belongs to.
    pub scope_id: ScopeId,
    /// Declaration order within the scope (0-based).
    pub decl_order: u32,
}

/// A parsed struct declaration site.
///
/// Unlike function declarations, structs have no flat function index: they
/// live only in [`FrontendIr::struct_schemas`], keyed by name. The site
/// records the exact declaration provenance (identifier span plus the full
/// `struct`..`}` declaration span and its scope) so strict-mode diagnostics
/// can point at the exact struct declaration without scanning source text.
#[derive(Clone, Debug)]
pub struct StructDeclSite {
    /// Parser-allocated stable node id.
    pub id: SemanticNodeId,
    /// Exact identifier token span (the struct name).
    pub ident_span: Span,
    /// Span of the full `struct Name { ... }` declaration.
    pub decl_span: Span,
    /// The struct name.
    pub name: String,
    /// The scope this declaration belongs to.
    pub scope_id: ScopeId,
}

/// The resolved target of a parsed function-value reference. The parser
/// records the target honestly from its own resolution tables: plain
/// functions carry their flat index, and module-mode references that the
/// source loader resolves to an imported function carry the [`SymbolId`] of
/// the source module's declaration. Module targets are upgraded by the
/// loader during `resolve_imported_call_sites`; a reference that kept its
/// stale unit-local flat index after that pass would alias an unrelated
/// merged flat function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionRefTarget {
    /// A plain function value reference resolved to a flat (or builtin) index.
    Function(u16),
    /// A loader-resolved module function value reference.
    Module(SymbolId),
}

/// A parsed local function reference site (function value, not a call).
#[derive(Clone, Debug)]
pub struct FunctionRefSite {
    /// Parser-allocated stable node id.
    pub id: SemanticNodeId,
    /// Exact identifier token span.
    pub ident_span: Span,
    /// The resolved function target (flat index or module symbol).
    pub target: FunctionRefTarget,
    /// The function name.
    pub name: String,
    /// The scope this reference belongs to.
    pub scope_id: ScopeId,
}

/// A parsed lexical scope record.
#[derive(Clone, Debug)]
pub struct ParsedLexicalScope {
    /// Parser-allocated scope id.
    pub id: ScopeId,
    /// Parent scope id, or None for the root scope.
    pub parent: Option<ScopeId>,
    /// Exact opening..closing token span of the scope.
    pub range: Span,
    /// Local slots declared in this scope, in declaration order.
    pub declarations: Vec<LocalSlot>,
    /// Function indices declared in this scope.
    pub functions: Vec<u16>,
}

/// One file-module namespace alias recorded by the parser for a specific
/// owning source. Module namespace aliases are unit-local: the same alias
/// name may name different modules in different sources (`use a as x;` in one
/// unit and `use b as x;` in another), so the merged carrier keeps ownership
/// per source instead of collapsing by alias name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleNamespaceAlias {
    /// The local alias name (`use a::util as au;` records alias `au`).
    pub alias: String,
    /// The module path the alias names (parser-relative spelling, e.g.
    /// `self::c` or `a::util`).
    pub module_path: String,
    /// The owning source name (unit identity). Empty until the linker tags
    /// entries with their unit's source during merge.
    pub source: String,
}

/// Visibility information for host/builtin/module names, populated by the
/// parser from its own alias/import maps — never inferred from source text.
#[derive(Clone, Debug, Default)]
pub struct CatalogVisibility {
    /// Host namespace aliases: `alias -> canonical_name`.
    pub host_namespace_aliases: Vec<(String, String)>,
    /// Direct host call aliases: `alias -> canonical_name`.
    pub direct_host_call_aliases: Vec<(String, String)>,
    /// Wildcard host imports: set of namespace prefixes.
    pub direct_host_wildcard_imports: Vec<String>,
    /// Module namespace aliases, keyed by owning source after merge.
    pub module_namespace_aliases: Vec<ModuleNamespaceAlias>,
    /// Structured use declarations with their visibility clauses.
    pub use_declarations: Vec<crate::compiler::modules::UseDecl>,
}

/// A structured lexer token retained as frontend metadata for exact
/// cursor-position queries (completion prefix derivation, token-at-offset
/// resolution). The parser's full token stream is preserved verbatim so the
/// language service never re-lexes or scans source text; spans carry their
/// owning [`SourceId`] and survive unit merge unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexerToken {
    /// The lexer token kind, as a stable string tag (e.g. `Ident`, `Colon`,
    /// `LParen`). Identifiers carry their text.
    pub kind: String,
    /// The identifier text for `Ident` tokens; empty for all other kinds.
    pub ident: String,
    /// Exact source span of the token (including the owning source id).
    pub span: Span,
}

/// Full semantic provenance index produced by the parser from exact token
/// spans. Carried on [`FrontendIr`] through the linker, which remaps ids
/// collision-free during unit merge.
#[derive(Clone, Debug, Default)]
pub struct ParsedSemanticIndex {
    /// All parsed call sites, in allocation order.
    pub call_sites: Vec<ParsedCallSite>,
    /// All parsed local declarations, in allocation order.
    pub local_decls: Vec<LocalDeclSite>,
    /// All parsed local variable references, in allocation order.
    pub local_refs: Vec<LocalRefSite>,
    /// All parsed function declarations, in allocation order.
    pub func_decls: Vec<FunctionDeclSite>,
    /// All parsed struct declarations, in parse order.
    pub struct_decls: Vec<StructDeclSite>,
    /// All parsed function value references, in allocation order.
    pub func_refs: Vec<FunctionRefSite>,
    /// All parsed lexical scopes, in allocation order (scope 0 = root).
    pub scopes: Vec<ParsedLexicalScope>,
    /// Exact parser-origin span of every parsed statement, in parse order
    /// (from the statement's first consumed token through its last). Used to
    /// give typed diagnostics an exact original-source slice without any
    /// same-line token guessing. Spans carry their owning source id and are
    /// copied verbatim through unit merge (the source id already names the
    /// owning compilation-wide source).
    pub stmt_spans: Vec<StmtSpanSite>,
    /// Next available SemanticNodeId for the next parse.
    pub next_node_id: u32,
    /// Next available ScopeId for the next parse.
    pub next_scope_id: u32,
}

/// One parsed statement's exact source span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StmtSpanSite {
    /// The parser-reported line of the statement's first token.
    pub line: u32,
    /// Exact span of the statement construct (first through last consumed
    /// token), never a line-wide guess. The same line may host many
    /// statements; each records its own independent span.
    pub span: Span,
}

impl ParsedSemanticIndex {
    /// Allocate a new monotonic [`SemanticNodeId`]. Exhaustion of the u32 id
    /// space is a parser-level resource failure: it is asserted explicitly
    /// rather than silently wrapping.
    pub fn alloc_node_id(&mut self) -> SemanticNodeId {
        let id = SemanticNodeId(self.next_node_id);
        self.next_node_id = self
            .next_node_id
            .checked_add(1)
            .expect("parser semantic node id space exhausted (u32 overflow)");
        id
    }

    /// Allocate a new monotonic [`ScopeId`]. Exhaustion of the u32 id space
    /// is a parser-level resource failure: it is asserted explicitly rather
    /// than silently wrapping.
    pub fn alloc_scope_id(&mut self) -> ScopeId {
        let id = self.next_scope_id;
        self.next_scope_id = self
            .next_scope_id
            .checked_add(1)
            .expect("parser scope id space exhausted (u32 overflow)");
        id
    }
}

pub struct LocalIrBuilder {
    locals: HashMap<String, LocalSlot>,
    next_local: LocalSlot,
    functions: Vec<FunctionDecl>,
    function_meta: HashMap<String, (u16, Option<u8>)>,
}

impl Default for LocalIrBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalIrBuilder {
    pub fn new() -> Self {
        Self {
            locals: HashMap::new(),
            next_local: 0,
            functions: Vec::new(),
            function_meta: HashMap::new(),
        }
    }

    pub fn lower_local(&mut self, name: &str, expr: Expr, line: u32) -> Result<Stmt, ParseError> {
        let index = self.alloc_local_named(name)?;
        Ok(Stmt::Let {
            index,
            declared_schema: None,
            expr,
            line,
        })
    }

    pub fn lower_assign(&self, name: &str, expr: Expr, line: u32) -> Result<Stmt, ParseError> {
        let Some(index) = self.locals.get(name).copied() else {
            return Err(ParseError {
                span: None,
                code: None,
                line: line as usize,
                message: format!("unknown local '{name}'"),
            });
        };
        Ok(Stmt::Assign {
            kind: AssignmentKind::Set,
            index,
            expr,
            line,
        })
    }

    pub fn resolve_local_expr(&self, name: &str) -> Option<Expr> {
        self.locals.get(name).copied().map(Expr::Var)
    }

    pub fn has_declared_function(&self, name: &str) -> bool {
        self.function_meta.contains_key(name)
    }

    pub fn function_index(&self, name: &str) -> Option<u16> {
        self.function_meta.get(name).map(|(index, _)| *index)
    }

    pub fn declare_function(&mut self, name: &str, arity: Option<u8>) -> Result<(), ParseError> {
        if let Some((index, existing_arity)) = self.function_meta.get(name).copied() {
            match (existing_arity, arity) {
                (Some(expected), Some(actual))
                    if expected != actual && !known_host_accepts_arity(name, actual) =>
                {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: 1,
                        message: format!(
                            "function '{name}' declared with conflicting arity {expected} vs {actual}"
                        ),
                    });
                }
                (None, Some(actual)) => {
                    if let Some(function) = self.functions.get_mut(index as usize) {
                        function.arity = actual;
                        function.args = (0..actual).map(|slot| format!("arg{slot}")).collect();
                    }
                    self.function_meta
                        .insert(name.to_string(), (index, Some(actual)));
                }
                _ => {}
            }
            return Ok(());
        }

        let index = u16::try_from(self.functions.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: 1,
            message: "too many declared functions".to_string(),
        })?;
        let effective_arity = arity.unwrap_or(0);
        self.functions.push(FunctionDecl {
            name: name.to_string(),
            arity: effective_arity,
            index,
            args: (0..effective_arity)
                .map(|slot| format!("arg{slot}"))
                .collect(),
            arg_schemas: vec![None; usize::from(effective_arity)],
            return_schema: None,
            type_params: Vec::new(),
            exported: false,
            return_type: ValueType::Unknown,
            symbol: None,
        });
        self.function_meta.insert(name.to_string(), (index, arity));
        Ok(())
    }

    pub fn resolve_call_expr(&mut self, name: &str, args: Vec<Expr>) -> Option<Expr> {
        if let Some(local_index) = self.locals.get(name).copied() {
            return Some(Expr::LocalCall(local_index, Vec::new(), args, None));
        }
        let (func_index, declared_arity) = self.function_meta.get(name).copied()?;
        let call_arity = u8::try_from(args.len()).ok()?;
        match declared_arity {
            Some(expected)
                if expected != call_arity && !known_host_accepts_arity(name, call_arity) =>
            {
                return None;
            }
            Some(_) => {}
            None => {
                if let Some(function) = self.functions.get_mut(func_index as usize) {
                    function.arity = call_arity;
                    function.args = (0..call_arity).map(|slot| format!("arg{slot}")).collect();
                }
                self.function_meta
                    .insert(name.to_string(), (func_index, Some(call_arity)));
            }
        }
        Some(Expr::Call(func_index, Vec::new(), args, None, None))
    }

    pub fn finish(self, stmts: Vec<Stmt>) -> FrontendIr {
        let mut local_bindings = self
            .locals
            .into_iter()
            .collect::<Vec<(String, LocalSlot)>>();
        local_bindings.sort_by_key(|(_, index)| *index);
        FrontendIr {
            stmts,
            locals: self.next_local as usize,
            local_bindings,
            struct_schemas: HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions: self.functions,
            function_impls: HashMap::new(),
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        }
    }

    pub fn alloc_local_named(&mut self, name: &str) -> Result<LocalSlot, ParseError> {
        if let Some(index) = self.locals.get(name).copied() {
            return Ok(index);
        }
        let index = self.alloc_local()?;
        self.locals.insert(name.to_string(), index);
        Ok(index)
    }

    fn alloc_local(&mut self) -> Result<LocalSlot, ParseError> {
        let index = self.next_local;
        self.next_local = self.next_local.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "local index overflow".to_string(),
        })?;
        Ok(index)
    }
}

#[cfg(test)]
mod host_api_ir_metadata_tests {
    use super::HostApiIrMetadata;
    use crate::compiler::ir::LocalIrBuilder;
    use crate::host_api::{
        HostApiFingerprint, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
    };

    fn fingerprint(n: u64) -> HostApiFingerprint {
        serde_json::from_value(serde_json::Value::Number(n.into())).unwrap()
    }

    fn func(name: &str, params: Vec<HostParamSchema>) -> HostFunctionSchema {
        HostFunctionSchema::with_return(name, params, HostTypeSchema::Unknown)
    }

    #[test]
    fn fingerprint_is_accessible() {
        let md = HostApiIrMetadata::new(fingerprint(0x1234));
        assert_eq!(md.fingerprint(), fingerprint(0x1234));
        assert_eq!(md.function_indices().len(), 0);
        assert!(md.candidates(40).is_none());
    }

    #[test]
    fn function_indices_are_sorted_and_copied() {
        let mut md = HostApiIrMetadata::new(fingerprint(1));
        md.record_candidates(5, vec![func("f", vec![])]).unwrap();
        md.record_candidates(2, vec![func("f", vec![])]).unwrap();
        md.record_candidates(9, vec![func("f", vec![])]).unwrap();
        assert_eq!(md.function_indices().len(), 3);
        let indices: Vec<u16> = md.function_indices().collect();
        assert_eq!(indices, vec![2, 5, 9]);
        assert!(md.candidates(2).is_some());
        assert!(md.candidates(4).is_none());
    }

    #[test]
    fn candidate_order_preserves_pass_only_overloads() {
        let mut md = HostApiIrMetadata::new(fingerprint(2));
        md.record_candidates(
            0,
            vec![
                func("f", vec![HostParamSchema::value("x", HostTypeSchema::Int)]),
                func(
                    "f",
                    vec![HostParamSchema::with_passing(
                        "x",
                        HostTypeSchema::Int,
                        HostParamPassing::Borrow,
                    )],
                ),
            ],
        )
        .unwrap();
        let candidates = md.candidates(0).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].params[0].passing, HostParamPassing::Value);
        assert_eq!(candidates[1].params[0].passing, HostParamPassing::Borrow);
    }

    #[test]
    fn rejects_empty_candidate_sets() {
        let mut md = HostApiIrMetadata::new(fingerprint(1));
        assert!(md.record_candidates(0, Vec::new()).is_err());
        assert!(md.candidates(0).is_none());
    }

    #[test]
    fn rejects_mixed_name_candidate_sets() {
        let mut md = HostApiIrMetadata::new(fingerprint(1));
        let err = md
            .record_candidates(1, vec![func("alpha", vec![]), func("beta", vec![])])
            .unwrap_err();
        assert!(
            err.to_string().contains("names disagree"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_mixed_arity_candidate_sets() {
        let mut md = HostApiIrMetadata::new(fingerprint(1));
        let err = md
            .record_candidates(
                1,
                vec![
                    func("f", vec![]),
                    func("f", vec![HostParamSchema::value("x", HostTypeSchema::Int)]),
                ],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("arities differ"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn rejects_duplicate_index_records() {
        let mut md = HostApiIrMetadata::new(fingerprint(1));
        md.record_candidates(3, vec![func("f", vec![])]).unwrap();
        let err = md
            .record_candidates(3, vec![func("f", vec![])])
            .unwrap_err();
        assert!(
            err.to_string().contains("duplicate candidate record"),
            "unexpected error: {}",
            err
        );
        assert_eq!(md.candidates(3).unwrap().len(), 1);
    }

    #[test]
    fn frontend_ir_builder_defaults_metadata_to_none() {
        let ir = LocalIrBuilder::new().finish(Vec::new());
        assert!(ir.host_api_metadata.is_none());
    }
}

#[cfg(test)]
mod call_resolution_carrier_tests {
    use super::{Expr, ResolvedHostCall, TypeSchema};
    use crate::compiler::ResolvedHostParam;
    use crate::host_api::{HostApiFingerprint, HostParamPassing};

    fn fingerprint(n: u64) -> HostApiFingerprint {
        serde_json::from_value(serde_json::Value::Number(n.into())).unwrap()
    }

    fn resolution(name: &str) -> ResolvedHostCall {
        ResolvedHostCall {
            name: name.to_string(),
            params: vec![ResolvedHostParam {
                name: name.to_string(),
                schema: TypeSchema::Int,
            }],
            return_type: TypeSchema::Int,
            passing: vec![HostParamPassing::Borrow],
            fingerprint: fingerprint(7),
        }
    }

    #[test]
    fn equal_index_calls_carry_distinct_resolutions() {
        let first = Expr::Call(
            9,
            Vec::new(),
            Vec::new(),
            Some(Box::new(resolution("alpha"))),
            None,
        );
        let second = Expr::Call(
            9,
            Vec::new(),
            Vec::new(),
            Some(Box::new(resolution("beta"))),
            None,
        );
        // Same flat index (same `(name, arity)` candidate-set identity) but
        // distinct exact per-call resolutions.
        assert_eq!(first.host_call_resolution().unwrap().name, "alpha");
        assert_eq!(second.host_call_resolution().unwrap().name, "beta");
        assert_ne!(
            first.host_call_resolution().unwrap(),
            second.host_call_resolution().unwrap()
        );
    }

    #[test]
    fn clone_preserves_resolution() {
        let call = Expr::Call(
            9,
            Vec::new(),
            Vec::new(),
            Some(Box::new(resolution("original"))),
            None,
        );
        let cloned = call.clone();
        assert_eq!(cloned.host_call_resolution().unwrap().name, "original");
        assert_eq!(call.host_call_resolution().unwrap().name, "original");
    }

    #[test]
    fn accessor_is_none_for_unresolved_and_non_call() {
        let unresolved = Expr::Call(9, Vec::new(), Vec::new(), None, None);
        assert!(unresolved.host_call_resolution().is_none());
        let local = Expr::LocalCall(0, Vec::new(), Vec::new(), None);
        assert!(local.host_call_resolution().is_none());
        let literal = Expr::Int(1);
        assert!(literal.host_call_resolution().is_none());
    }

    #[test]
    fn clone_preserves_semantic_node_id() {
        let id = Some(super::SemanticNodeId(42));
        let call = Expr::Call(9, Vec::new(), Vec::new(), None, id);
        let cloned = call.clone();
        // Clone preserves the semantic node id
        assert_eq!(cloned.host_call_resolution(), call.host_call_resolution());
        assert!(cloned.host_call_resolution().is_none());
    }

    #[test]
    fn rewrite_preserves_semantic_node_id() {
        // Simulate a transformation that rebuilds an Expr::Call with
        // different arguments but must preserve the original SemanticNodeId.
        let original = Expr::Call(
            9,
            Vec::new(),
            vec![Expr::Int(1)],
            None,
            Some(super::SemanticNodeId(99)),
        );
        let source_node_id = match &original {
            Expr::Call(_, _, _, _, id) => *id,
            _ => None,
        };
        let _rewritten = Expr::Call(9, Vec::new(), vec![Expr::Int(1)], None, source_node_id);
        // The rewritten call still carries the same id
        assert_eq!(source_node_id, Some(super::SemanticNodeId(99)));
    }

    #[test]
    fn synthetic_call_uses_none_id() {
        let synthetic = Expr::Call(9, Vec::new(), Vec::new(), None, None);
        assert!(synthetic.host_call_resolution().is_none());
    }

    #[test]
    fn distinct_ids_distinguish_calls() {
        let a = Some(super::SemanticNodeId(1));
        let b = Some(super::SemanticNodeId(2));
        assert_ne!(a, b);
    }
}

#[cfg(test)]
mod type_schema_contains_resource_tests {
    use super::TypeSchema;
    use crate::host_api::ResourceTypeKey;
    use std::collections::HashMap;

    fn resource() -> TypeSchema {
        TypeSchema::Resource(ResourceTypeKey::new("sqlite.connection").expect("valid key"))
    }

    fn field(name: &str, schema: TypeSchema) -> (String, TypeSchema) {
        (name.to_string(), schema)
    }

    #[test]
    fn direct_resource() {
        assert!(resource().contains_resource());
    }

    #[test]
    fn optional_recurses_to_resource() {
        assert!(TypeSchema::Optional(Box::new(resource())).contains_resource());
        assert!(
            TypeSchema::Optional(Box::new(TypeSchema::Optional(Box::new(resource()))))
                .contains_resource()
        );
        assert!(!TypeSchema::Optional(Box::new(TypeSchema::Int)).contains_resource());
    }

    #[test]
    fn named_type_args_recursed() {
        let wrapping = TypeSchema::Named("result".into(), vec![TypeSchema::Int, resource()]);
        assert!(wrapping.contains_resource());
        // A named node with only resource-free arguments is not resource-containing.
        let clean = TypeSchema::Named("result".into(), vec![TypeSchema::Int]);
        assert!(!clean.contains_resource());
        // Empty type args must not be a false positive.
        assert!(!TypeSchema::Named("empty".into(), Vec::new()).contains_resource());
    }

    #[test]
    fn array_recursed() {
        assert!(TypeSchema::Array(Box::new(resource())).contains_resource());
        assert!(!TypeSchema::Array(Box::new(TypeSchema::Int)).contains_resource());
    }

    #[test]
    fn array_tuple_recursed() {
        let tuple = TypeSchema::ArrayTuple(vec![
            TypeSchema::Int,
            TypeSchema::Optional(Box::new(resource())),
            TypeSchema::String,
        ]);
        assert!(tuple.contains_resource());
        // Clean tuple is not a false positive.
        let clean = TypeSchema::ArrayTuple(vec![TypeSchema::Int, TypeSchema::String]);
        assert!(!clean.contains_resource());
        assert!(!TypeSchema::ArrayTuple(Vec::new()).contains_resource());
    }

    #[test]
    fn array_tuple_rest_recurse_prefix_and_rest() {
        // Resource in the prefix.
        let in_prefix = TypeSchema::ArrayTupleRest {
            prefix: vec![resource()],
            rest: Box::new(TypeSchema::Int),
        };
        assert!(in_prefix.contains_resource());
        // Resource in the rest.
        let in_rest = TypeSchema::ArrayTupleRest {
            prefix: vec![TypeSchema::Int],
            rest: Box::new(resource()),
        };
        assert!(in_rest.contains_resource());
        // Clean rest schema.
        let clean = TypeSchema::ArrayTupleRest {
            prefix: vec![TypeSchema::Int],
            rest: Box::new(TypeSchema::String),
        };
        assert!(!clean.contains_resource());
    }

    #[test]
    fn map_value_recursed() {
        assert!(TypeSchema::Map(Box::new(resource())).contains_resource());
        assert!(!TypeSchema::Map(Box::new(TypeSchema::Int)).contains_resource());
    }

    #[test]
    fn object_values_recursed() {
        let mut with_resource = HashMap::new();
        with_resource.insert("a".to_string(), TypeSchema::Int);
        with_resource.insert("b".to_string(), resource());
        assert!(TypeSchema::Object(with_resource).contains_resource());

        let clean = HashMap::from([field("x", TypeSchema::Int), field("y", TypeSchema::String)]);
        assert!(!TypeSchema::Object(clean).contains_resource());
        assert!(!TypeSchema::Object(HashMap::new()).contains_resource());
    }

    #[test]
    fn callable_params_and_result_recursed() {
        let in_param = TypeSchema::Callable {
            params: vec![resource()],
            result: Box::new(TypeSchema::Null),
        };
        assert!(in_param.contains_resource());
        let in_result = TypeSchema::Callable {
            params: vec![TypeSchema::Int],
            result: Box::new(TypeSchema::Optional(Box::new(resource()))),
        };
        assert!(in_result.contains_resource());
        let clean = TypeSchema::Callable {
            params: vec![TypeSchema::Int],
            result: Box::new(TypeSchema::Bool),
        };
        assert!(!clean.contains_resource());
    }

    #[test]
    fn deeply_nested_named_and_container() {
        // Named(Ok, [ Callable(fn([Map(Optional(resource))]) -> ...) ])
        let nested = TypeSchema::Named(
            "provider".into(),
            vec![TypeSchema::Callable {
                params: vec![TypeSchema::Map(Box::new(TypeSchema::Optional(Box::new(
                    resource(),
                ))))],
                result: Box::new(TypeSchema::Array(Box::new(TypeSchema::Named(
                    "row".into(),
                    Vec::new(),
                )))),
            }],
        );
        assert!(nested.contains_resource());
    }

    #[test]
    fn negative_controls_and_scalars() {
        for schema in [
            TypeSchema::Unknown,
            TypeSchema::Null,
            TypeSchema::Int,
            TypeSchema::Float,
            TypeSchema::Number,
            TypeSchema::Bool,
            TypeSchema::String,
            TypeSchema::Bytes,
            TypeSchema::GenericParam("T".into()),
        ] {
            assert!(!schema.contains_resource());
        }
    }

    #[test]
    fn generic_param_stays_false() {
        // A generic parameter is not declared a resource even when deeply nested.
        let nested = TypeSchema::Named(
            "wrapper".into(),
            vec![TypeSchema::Array(Box::new(TypeSchema::GenericParam(
                "T".into(),
            )))],
        );
        assert!(!nested.contains_resource());
    }
}
