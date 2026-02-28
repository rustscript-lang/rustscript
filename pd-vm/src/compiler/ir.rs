use std::collections::HashMap;

/// Shared frontend-independent program representation that all source
/// frontends lower into before bytecode emission.
#[derive(Clone, Debug)]
pub struct ClosureExpr {
    pub param_slots: Vec<u8>,
    pub capture_copies: Vec<(u8, u8)>,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug)]
pub enum MatchPattern {
    Int(i64),
    String(String),
}

#[derive(Clone, Debug)]
pub enum Expr {
    Null,
    Int(i64),
    Bool(bool),
    String(String),
    Call(u16, Vec<Expr>),
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
    Var(u8),
    IfElse {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    Match {
        value_slot: u8,
        result_slot: u8,
        value: Box<Expr>,
        arms: Vec<(MatchPattern, Expr)>,
        default: Box<Expr>,
    },
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
        index: u8,
        expr: Expr,
        line: u32,
    },
    Assign {
        index: u8,
        expr: Expr,
        line: u32,
    },
    ClosureLet {
        line: u32,
        closure: ClosureExpr,
    },
    FuncDecl {
        name: String,
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
    pub param_slots: Vec<u8>,
    pub body_stmts: Vec<Stmt>,
    pub body_expr: Expr,
}

#[derive(Clone, Debug)]
pub struct FrontendIr {
    pub stmts: Vec<Stmt>,
    pub locals: usize,
    pub local_bindings: Vec<(String, u8)>,
    pub functions: Vec<FunctionDecl>,
    pub function_impls: HashMap<u16, FunctionImpl>,
}

#[derive(Clone, Debug)]
pub struct LinkedIr {
    pub source: String,
    pub stmts: Vec<Stmt>,
    pub locals: usize,
    pub local_bindings: Vec<(String, u8)>,
    pub functions: Vec<FunctionDecl>,
    pub function_impls: HashMap<u16, FunctionImpl>,
}
