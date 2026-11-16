//! Private logical-declaration identity for `Stmt::Assign`.
//!
//! A physical `Drop` clears the slot schema, and compaction then reuses that
//! slot. The declaration must stay with the assignment, not with the slot.
//! It is not a field of [`Stmt`]: downstream frontends construct `Assign`
//! directly. Compaction remaps slots in place and does not reorder the tree,
//! so a structural path captured before compaction still names the same
//! assignment afterwards, including on a cloned function or closure body.

use std::collections::HashMap;

use super::ir::{ClosureExpr, Expr, FrontendIr, LocalSlot, Stmt, TypeSchema};

const STEP_STMT: u32 = 1;
const STEP_EXPR: u32 = 2;
const STEP_INIT: u32 = 3;
const STEP_POST: u32 = 4;
const STEP_COND: u32 = 5;
const STEP_THEN: u32 = 6;
const STEP_ELSE: u32 = 7;
const STEP_BODY: u32 = 8;
const STEP_TAIL: u32 = 9;
const STEP_ARG: u32 = 10;
const STEP_ARM: u32 = 11;
const STEP_DEFAULT: u32 = 12;
const STEP_PAYLOAD: u32 = 13;
const STEP_OK: u32 = 14;
const STEP_ERR: u32 = 15;
const STEP_SOME: u32 = 16;
const STEP_NONE: u32 = 17;
const STEP_VALUE: u32 = 18;
const STEP_FALLBACK: u32 = 19;
const STEP_KEY: u32 = 20;
const STEP_CONTAINER: u32 = 21;
const STEP_LEFT: u32 = 22;
const STEP_RIGHT: u32 = 23;
const STEP_OPERAND: u32 = 24;
const STEP_RECEIVER: u32 = 25;
const STEP_ARGUMENT: u32 = 26;
const STEP_CLOSURE: u32 = 27;
const STEP_CALLEE: u32 = 28;
const STEP_BODY_EXPR: u32 = 29;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AssignPath(Vec<u32>);

#[derive(Clone, Debug, Default)]
pub(super) struct AssignDeclarationSidecar {
    root: HashMap<AssignPath, TypeSchema>,
    functions: HashMap<u16, HashMap<AssignPath, TypeSchema>>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AssignDeclarationMap {
    by_stmt: HashMap<usize, TypeSchema>,
}

impl AssignDeclarationMap {
    pub(super) fn schema_of(&self, stmt: &Stmt) -> Option<&TypeSchema> {
        self.by_stmt.get(&stmt_key(stmt))
    }
}

pub(super) fn capture(ir: &FrontendIr) -> AssignDeclarationSidecar {
    let mut sidecar = AssignDeclarationSidecar::default();
    let mut root_decls = HashMap::new();
    collect_lets(&ir.stmts, None, &mut root_decls);
    record_assigns(&ir.stmts, None, &root_decls, &mut sidecar.root);

    for (index, function_impl) in &ir.function_impls {
        let mut decls = HashMap::new();
        if let Some(decl) = ir.functions.iter().find(|decl| decl.index == *index) {
            for (slot, schema) in function_impl.param_slots.iter().zip(&decl.arg_schemas) {
                if let Some(schema) = schema {
                    decls.insert(*slot, schema.clone());
                }
            }
        }
        collect_lets(
            &function_impl.body_stmts,
            Some(&function_impl.body_expr),
            &mut decls,
        );
        let mut recorded = HashMap::new();
        record_assigns(
            &function_impl.body_stmts,
            Some(&function_impl.body_expr),
            &decls,
            &mut recorded,
        );
        if !recorded.is_empty() {
            sidecar.functions.insert(*index, recorded);
        }
    }
    sidecar
}

pub(super) fn bind_stmts(
    sidecar: &AssignDeclarationSidecar,
    stmts: &[Stmt],
) -> AssignDeclarationMap {
    bind_known(&sidecar.root, stmts, None)
}

pub(super) fn bind_function(
    sidecar: &AssignDeclarationSidecar,
    index: u16,
    stmts: &[Stmt],
    body_expr: &Expr,
) -> AssignDeclarationMap {
    let Some(known) = sidecar.functions.get(&index) else {
        return AssignDeclarationMap::default();
    };
    bind_known(known, stmts, Some(body_expr))
}

/// Schemas of every assignment in `closure`, in walker preorder.
///
/// The queued closure is a clone, so its statement addresses differ from the
/// tree currently being compiled. The vec is taken from the parent node and
/// zipped onto the clone later. A length mismatch means the clone is not the
/// same shape; the caller must not apply a shifted schema.
pub(super) fn preorder_schemas(
    closure: &ClosureExpr,
    map: &AssignDeclarationMap,
) -> Vec<Option<TypeSchema>> {
    let mut out = Vec::new();
    let mut visit = Preorder {
        known: &map.by_stmt,
        out: &mut out,
    };
    let mut path = Vec::new();
    walk_expr(&closure.body, &mut path, &mut visit);
    out
}

pub(super) fn bind_preorder(
    closure: &ClosureExpr,
    schemas: &[Option<TypeSchema>],
) -> AssignDeclarationMap {
    let mut out = HashMap::new();
    let mut visit = Install {
        schemas,
        cursor: 0,
        out: &mut out,
    };
    let mut path = Vec::new();
    walk_expr(&closure.body, &mut path, &mut visit);
    if visit.cursor != schemas.len() {
        return AssignDeclarationMap::default();
    }
    AssignDeclarationMap { by_stmt: out }
}

fn bind_known(
    known: &HashMap<AssignPath, TypeSchema>,
    stmts: &[Stmt],
    body_expr: Option<&Expr>,
) -> AssignDeclarationMap {
    let mut out = HashMap::new();
    let mut visit = BindAssigns {
        known,
        out: &mut out,
    };
    walk_scope(stmts, body_expr, &mut visit);
    AssignDeclarationMap { by_stmt: out }
}

fn collect_lets(
    stmts: &[Stmt],
    body_expr: Option<&Expr>,
    decls: &mut HashMap<LocalSlot, TypeSchema>,
) {
    let mut visit = CollectLets { decls };
    walk_scope(stmts, body_expr, &mut visit);
}

fn record_assigns(
    stmts: &[Stmt],
    body_expr: Option<&Expr>,
    decls: &HashMap<LocalSlot, TypeSchema>,
    out: &mut HashMap<AssignPath, TypeSchema>,
) {
    let mut visit = RecordAssigns { decls, out };
    walk_scope(stmts, body_expr, &mut visit);
}

fn walk_scope(stmts: &[Stmt], body_expr: Option<&Expr>, visit: &mut impl Visitor) {
    let mut path = Vec::new();
    walk_stmts(stmts, &mut path, visit);
    if let Some(body_expr) = body_expr {
        path.push(STEP_BODY_EXPR);
        walk_expr(body_expr, &mut path, visit);
    }
}

trait Visitor {
    fn on_let(&mut self, slot: LocalSlot, schema: &Option<TypeSchema>);
    fn on_assign(&mut self, stmt: &Stmt, slot: LocalSlot, path: &[u32]);
}

struct CollectLets<'a> {
    decls: &'a mut HashMap<LocalSlot, TypeSchema>,
}

impl Visitor for CollectLets<'_> {
    fn on_let(&mut self, slot: LocalSlot, schema: &Option<TypeSchema>) {
        if let Some(schema) = schema {
            self.decls.insert(slot, schema.clone());
        }
    }

    fn on_assign(&mut self, _stmt: &Stmt, _slot: LocalSlot, _path: &[u32]) {}
}

struct RecordAssigns<'a> {
    decls: &'a HashMap<LocalSlot, TypeSchema>,
    out: &'a mut HashMap<AssignPath, TypeSchema>,
}

impl Visitor for RecordAssigns<'_> {
    fn on_let(&mut self, _slot: LocalSlot, _schema: &Option<TypeSchema>) {}

    fn on_assign(&mut self, _stmt: &Stmt, slot: LocalSlot, path: &[u32]) {
        if let Some(schema) = self.decls.get(&slot) {
            self.out.insert(AssignPath(path.to_vec()), schema.clone());
        }
    }
}

struct BindAssigns<'a> {
    known: &'a HashMap<AssignPath, TypeSchema>,
    out: &'a mut HashMap<usize, TypeSchema>,
}

impl Visitor for BindAssigns<'_> {
    fn on_let(&mut self, _slot: LocalSlot, _schema: &Option<TypeSchema>) {}

    fn on_assign(&mut self, stmt: &Stmt, _slot: LocalSlot, path: &[u32]) {
        if let Some(schema) = self.known.get(&AssignPath(path.to_vec())) {
            self.out.insert(stmt_key(stmt), schema.clone());
        }
    }
}

struct Preorder<'a> {
    known: &'a HashMap<usize, TypeSchema>,
    out: &'a mut Vec<Option<TypeSchema>>,
}

impl Visitor for Preorder<'_> {
    fn on_let(&mut self, _slot: LocalSlot, _schema: &Option<TypeSchema>) {}

    fn on_assign(&mut self, stmt: &Stmt, _slot: LocalSlot, _path: &[u32]) {
        self.out.push(self.known.get(&stmt_key(stmt)).cloned());
    }
}

struct Install<'a> {
    schemas: &'a [Option<TypeSchema>],
    cursor: usize,
    out: &'a mut HashMap<usize, TypeSchema>,
}

impl Visitor for Install<'_> {
    fn on_let(&mut self, _slot: LocalSlot, _schema: &Option<TypeSchema>) {}

    fn on_assign(&mut self, stmt: &Stmt, _slot: LocalSlot, _path: &[u32]) {
        if let Some(Some(schema)) = self.schemas.get(self.cursor) {
            self.out.insert(stmt_key(stmt), schema.clone());
        }
        self.cursor += 1;
    }
}

fn stmt_key(stmt: &Stmt) -> usize {
    stmt as *const Stmt as usize
}

fn walk_stmts(stmts: &[Stmt], path: &mut Vec<u32>, visit: &mut impl Visitor) {
    for (index, stmt) in stmts.iter().enumerate() {
        path.push(STEP_STMT);
        path.push(index as u32);
        walk_stmt(stmt, path, visit);
        path.pop();
        path.pop();
    }
}

fn walk_stmt(stmt: &Stmt, path: &mut Vec<u32>, visit: &mut impl Visitor) {
    match stmt {
        Stmt::Let {
            index,
            declared_schema,
            expr,
            ..
        } => {
            visit.on_let(*index, declared_schema);
            push_step(path, STEP_EXPR, |path| walk_expr(expr, path, visit));
        }
        Stmt::Assign { index, expr, .. } => {
            visit.on_assign(stmt, *index, path);
            push_step(path, STEP_EXPR, |path| walk_expr(expr, path, visit));
        }
        Stmt::Expr { expr, .. } => {
            push_step(path, STEP_EXPR, |path| walk_expr(expr, path, visit));
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            push_step(path, STEP_COND, |path| walk_expr(condition, path, visit));
            push_step(path, STEP_THEN, |path| walk_stmts(then_branch, path, visit));
            push_step(path, STEP_ELSE, |path| walk_stmts(else_branch, path, visit));
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            push_step(path, STEP_INIT, |path| walk_stmt(init, path, visit));
            push_step(path, STEP_COND, |path| walk_expr(condition, path, visit));
            push_step(path, STEP_POST, |path| walk_stmt(post, path, visit));
            push_step(path, STEP_BODY, |path| walk_stmts(body, path, visit));
        }
        Stmt::While {
            condition, body, ..
        } => {
            push_step(path, STEP_COND, |path| walk_expr(condition, path, visit));
            push_step(path, STEP_BODY, |path| walk_stmts(body, path, visit));
        }
        Stmt::ClosureLet { closure, .. } => {
            push_step(path, STEP_CLOSURE, |path| {
                walk_expr(&closure.body, path, visit)
            });
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => {}
    }
}

fn walk_expr(expr: &Expr, path: &mut Vec<u32>, visit: &mut impl Visitor) {
    match expr {
        Expr::OptionalGet { container, key, .. } => {
            push_step(path, STEP_CONTAINER, |path| {
                walk_expr(container, path, visit)
            });
            push_step(path, STEP_KEY, |path| walk_expr(key, path, visit));
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            push_step(path, STEP_VALUE, |path| walk_expr(value, path, visit));
            push_step(path, STEP_FALLBACK, |path| walk_expr(fallback, path, visit));
        }
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _) => walk_args(args, path, visit),
        Expr::Closure(closure) => {
            push_step(path, STEP_CLOSURE, |path| {
                walk_expr(&closure.body, path, visit)
            });
        }
        Expr::ClosureCall(closure, args) => {
            push_step(path, STEP_CALLEE, |path| {
                push_step(path, STEP_CLOSURE, |path| {
                    walk_expr(&closure.body, path, visit)
                });
            });
            walk_args(args, path, visit);
        }
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => {
            push_step(path, STEP_LEFT, |path| walk_expr(lhs, path, visit));
            push_step(path, STEP_RIGHT, |path| walk_expr(rhs, path, visit));
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            push_step(path, STEP_OPERAND, |path| walk_expr(inner, path, visit));
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            push_step(path, STEP_COND, |path| walk_expr(condition, path, visit));
            push_step(path, STEP_THEN, |path| walk_expr(then_expr, path, visit));
            push_step(path, STEP_ELSE, |path| walk_expr(else_expr, path, visit));
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            push_step(path, STEP_VALUE, |path| walk_expr(value, path, visit));
            for (index, (_, arm)) in arms.iter().enumerate() {
                path.push(STEP_ARM);
                path.push(index as u32);
                walk_expr(arm, path, visit);
                path.pop();
                path.pop();
            }
            push_step(path, STEP_DEFAULT, |path| walk_expr(default, path, visit));
        }
        Expr::Block { stmts, expr } => {
            push_step(path, STEP_BODY, |path| walk_stmts(stmts, path, visit));
            push_step(path, STEP_TAIL, |path| walk_expr(expr, path, visit));
        }
        Expr::ResultConstruct { payload, .. } => {
            push_step(path, STEP_PAYLOAD, |path| walk_expr(payload, path, visit));
        }
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            push_step(path, STEP_VALUE, |path| walk_expr(value, path, visit));
            push_step(path, STEP_OK, |path| walk_expr(ok_expr, path, visit));
            push_step(path, STEP_ERR, |path| walk_expr(err_expr, path, visit));
        }
        Expr::ResultQuery { value, .. }
        | Expr::ResultTry { value, .. }
        | Expr::ResultConvert { value, .. } => {
            push_step(path, STEP_VALUE, |path| walk_expr(value, path, visit));
        }
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            push_step(path, STEP_RECEIVER, |path| walk_expr(receiver, path, visit));
            push_step(path, STEP_ARGUMENT, |path| walk_expr(argument, path, visit));
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                push_step(path, STEP_PAYLOAD, |path| walk_expr(payload, path, visit));
            }
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            push_step(path, STEP_VALUE, |path| walk_expr(value, path, visit));
            push_step(path, STEP_SOME, |path| walk_expr(some_expr, path, visit));
            push_step(path, STEP_NONE, |path| walk_expr(none_expr, path, visit));
        }
        Expr::Null
        | Expr::Unit
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Bytes(_)
        | Expr::FunctionRef(_, _)
        | Expr::ModuleFunctionRef(_, _)
        | Expr::UnresolvedFunctionRef { .. }
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => {}
    }
}

fn walk_args(args: &[Expr], path: &mut Vec<u32>, visit: &mut impl Visitor) {
    for (index, arg) in args.iter().enumerate() {
        path.push(STEP_ARG);
        path.push(index as u32);
        walk_expr(arg, path, visit);
        path.pop();
        path.pop();
    }
}

fn push_step(path: &mut Vec<u32>, step: u32, walk: impl FnOnce(&mut Vec<u32>)) {
    path.push(step);
    walk(path);
    path.pop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::AssignmentKind;

    #[test]
    fn closure_clone_keeps_assign_declaration_identity() {
        let closure = ClosureExpr {
            param_slots: Vec::new(),
            capture_copies: Vec::new(),
            body: Box::new(Expr::Block {
                stmts: vec![Stmt::Assign {
                    kind: AssignmentKind::Set,
                    index: 3,
                    expr: Expr::Int(2),
                    line: 1,
                }],
                expr: Box::new(Expr::Var(3)),
            }),
        };
        let mut known = AssignDeclarationMap::default();
        let Expr::Block { stmts, .. } = closure.body.as_ref() else {
            panic!("closure body");
        };
        known
            .by_stmt
            .insert(stmt_key(&stmts[0]), TypeSchema::Number);
        let schemas = preorder_schemas(&closure, &known);
        assert_eq!(schemas, vec![Some(TypeSchema::Number)]);

        let cloned = closure.clone();
        let bound = bind_preorder(&cloned, &schemas);
        let Expr::Block { stmts, .. } = cloned.body.as_ref() else {
            panic!("cloned body");
        };
        assert_eq!(bound.schema_of(&stmts[0]), Some(&TypeSchema::Number));
        assert_ne!(stmt_key(&stmts[0]), stmt_key(closure_assign(&closure)));
    }

    fn closure_assign(closure: &ClosureExpr) -> &Stmt {
        let Expr::Block { stmts, .. } = closure.body.as_ref() else {
            panic!("closure body");
        };
        &stmts[0]
    }
}
