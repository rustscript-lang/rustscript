use std::collections::{BTreeMap, HashMap};

use crate::ValueType;
use crate::builtins::default_host_callable;
use crate::host_api::{HostApiFingerprint, HostFunctionSchema, ResourceTypeKey};

use super::ParseError;
use super::modules::SymbolId;

pub type LocalSlot = u16;

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
    },
    OptionUnwrapOr {
        value: Box<Expr>,
        value_slot: LocalSlot,
        fallback: Box<Expr>,
    },
    Call(u16, Vec<TypeSchema>, Vec<Expr>),
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
    ModuleCall(SymbolId, Vec<TypeSchema>, Vec<Expr>),
    LocalCall(LocalSlot, Vec<TypeSchema>, Vec<Expr>),
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
            return Some(Expr::LocalCall(local_index, Vec::new(), args));
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
        Some(Expr::Call(func_index, Vec::new(), args))
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
