use std::collections::HashMap;

use super::ParseError;

pub type LocalSlot = u16;

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
    Null,
    Type(MatchTypePattern),
}

#[derive(Clone, Debug)]
pub enum MatchTypePattern {
    Int,
    Float,
    Number,
    Bool,
    String,
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
    FunctionRef(u16),
    Call(u16, Vec<Expr>),
    LocalCall(LocalSlot, Vec<Expr>),
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
pub enum Stmt {
    Noop {
        line: u32,
    },
    Let {
        index: LocalSlot,
        expr: Expr,
        line: u32,
    },
    Assign {
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
    pub exported: bool,
}

#[derive(Clone, Debug)]
pub struct FunctionImpl {
    pub param_slots: Vec<LocalSlot>,
    pub capture_copies: Vec<(LocalSlot, LocalSlot)>,
    pub body_stmts: Vec<Stmt>,
    pub body_expr: Expr,
}

#[derive(Clone, Debug)]
pub struct FrontendIr {
    pub stmts: Vec<Stmt>,
    pub locals: usize,
    pub local_bindings: Vec<(String, LocalSlot)>,
    pub functions: Vec<FunctionDecl>,
    pub function_impls: HashMap<u16, FunctionImpl>,
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
        Ok(Stmt::Let { index, expr, line })
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
        Ok(Stmt::Assign { index, expr, line })
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
                (Some(expected), Some(actual)) if expected != actual => {
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
            exported: false,
        });
        self.function_meta.insert(name.to_string(), (index, arity));
        Ok(())
    }

    pub(crate) fn resolve_call_expr(&mut self, name: &str, args: Vec<Expr>) -> Option<Expr> {
        if let Some(local_index) = self.locals.get(name).copied() {
            return Some(Expr::LocalCall(local_index, args));
        }
        let (func_index, declared_arity) = self.function_meta.get(name).copied()?;
        let call_arity = u8::try_from(args.len()).ok()?;
        match declared_arity {
            Some(expected) if expected != call_arity => return None,
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
        Some(Expr::Call(func_index, args))
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
            functions: self.functions,
            function_impls: HashMap::new(),
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
