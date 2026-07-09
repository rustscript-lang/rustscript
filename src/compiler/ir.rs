use std::collections::HashMap;

use crate::ValueType;
use crate::builtins::default_host_callable;

use super::ParseError;

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
            TypeSchema::Unknown
            | TypeSchema::GenericParam(_)
            | TypeSchema::Number
            | TypeSchema::Callable { .. } => ValueType::Unknown,
            TypeSchema::Null => ValueType::Null,
            TypeSchema::Int => ValueType::Int,
            TypeSchema::Float => ValueType::Float,
            TypeSchema::Bool => ValueType::Bool,
            TypeSchema::String => ValueType::String,
            TypeSchema::Bytes => ValueType::Bytes,
            TypeSchema::Optional(inner) => inner.coarse_value_type(),
            TypeSchema::Named(_, _) | TypeSchema::Map(_) | TypeSchema::Object(_) => ValueType::Map,
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
    FunctionRef(u16),
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
}

#[derive(Clone, Debug)]
pub struct FunctionImpl {
    pub param_slots: Vec<LocalSlot>,
    pub capture_copies: Vec<(LocalSlot, LocalSlot)>,
    pub body_stmts: Vec<Stmt>,
    pub body_expr: Expr,
    pub body_expr_line: u32,
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
}

pub(crate) struct LocalIrBuilder {
    locals: HashMap<String, LocalSlot>,
    next_local: LocalSlot,
    functions: Vec<FunctionDecl>,
    function_meta: HashMap<String, (u16, Option<u8>)>,
}

impl LocalIrBuilder {
    pub(crate) fn new() -> Self {
        Self {
            locals: HashMap::new(),
            next_local: 0,
            functions: Vec::new(),
            function_meta: HashMap::new(),
        }
    }

    pub(crate) fn lower_local(
        &mut self,
        name: &str,
        expr: Expr,
        line: u32,
    ) -> Result<Stmt, ParseError> {
        let index = self.alloc_local_named(name)?;
        Ok(Stmt::Let {
            index,
            declared_schema: None,
            expr,
            line,
        })
    }

    pub(crate) fn lower_assign(
        &self,
        name: &str,
        expr: Expr,
        line: u32,
    ) -> Result<Stmt, ParseError> {
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

    pub(crate) fn resolve_local_expr(&self, name: &str) -> Option<Expr> {
        self.locals.get(name).copied().map(Expr::Var)
    }

    pub(crate) fn has_declared_function(&self, name: &str) -> bool {
        self.function_meta.contains_key(name)
    }

    pub(crate) fn function_index(&self, name: &str) -> Option<u16> {
        self.function_meta.get(name).map(|(index, _)| *index)
    }

    pub(crate) fn declare_function(
        &mut self,
        name: &str,
        arity: Option<u8>,
    ) -> Result<(), ParseError> {
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
        });
        self.function_meta.insert(name.to_string(), (index, arity));
        Ok(())
    }

    pub(crate) fn resolve_call_expr(&mut self, name: &str, args: Vec<Expr>) -> Option<Expr> {
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

    pub(crate) fn finish(self, stmts: Vec<Stmt>) -> FrontendIr {
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
        }
    }

    pub(crate) fn alloc_local_named(&mut self, name: &str) -> Result<LocalSlot, ParseError> {
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
