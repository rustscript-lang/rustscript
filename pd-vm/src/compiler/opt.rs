use std::collections::HashMap;

use crate::builtins::BuiltinFunction;

use super::ir::{Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundType {
    Unknown,
    Null,
    Int,
    Float,
    Bool,
    String,
    Array,
    Map,
}

impl BoundType {
    fn type_name(self) -> Option<&'static str> {
        match self {
            BoundType::Unknown => None,
            BoundType::Null => Some("null"),
            BoundType::Int => Some("int"),
            BoundType::Float => Some("float"),
            BoundType::Bool => Some("bool"),
            BoundType::String => Some("string"),
            BoundType::Array => Some("array"),
            BoundType::Map => Some("map"),
        }
    }
}

#[derive(Default)]
struct LocalTypeState {
    by_slot: HashMap<LocalSlot, BoundType>,
}

impl LocalTypeState {
    fn get(&self, slot: LocalSlot) -> BoundType {
        self.by_slot
            .get(&slot)
            .copied()
            .unwrap_or(BoundType::Unknown)
    }

    fn set(&mut self, slot: LocalSlot, ty: BoundType) {
        self.by_slot.insert(slot, ty);
    }

    fn merge_from_branches(&mut self, lhs: &LocalTypeState, rhs: &LocalTypeState) {
        self.by_slot.clear();
        for slot in lhs.by_slot.keys().chain(rhs.by_slot.keys()) {
            let l = lhs.get(*slot);
            let r = rhs.get(*slot);
            let merged = if l == r { l } else { BoundType::Unknown };
            if merged != BoundType::Unknown {
                self.by_slot.insert(*slot, merged);
            }
        }
    }
}

pub(super) fn legalize_builtins_and_bind_types(mut ir: FrontendIr) -> FrontendIr {
    let mut top_state = LocalTypeState::default();
    legalize_stmts(&mut ir.stmts, &mut top_state);

    for function_impl in ir.function_impls.values_mut() {
        legalize_function_impl(function_impl);
    }

    ir
}

fn legalize_function_impl(function_impl: &mut FunctionImpl) {
    let mut state = LocalTypeState::default();
    for slot in &function_impl.param_slots {
        state.set(*slot, BoundType::Unknown);
    }
    legalize_stmts(&mut function_impl.body_stmts, &mut state);
    let _ = legalize_expr(&mut function_impl.body_expr, &state);
}

fn legalize_stmts(stmts: &mut [Stmt], state: &mut LocalTypeState) {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Drop { .. } => {}
            Stmt::ClosureLet { closure, .. } => {
                let _ = legalize_expr(&mut closure.body, state);
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let ty = legalize_expr(expr, state);
                state.set(*index, ty);
            }
            Stmt::Expr { expr, .. } => {
                let _ = legalize_expr(expr, state);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = legalize_expr(condition, state);
                let mut then_state = LocalTypeState {
                    by_slot: state.by_slot.clone(),
                };
                let mut else_state = LocalTypeState {
                    by_slot: state.by_slot.clone(),
                };
                legalize_stmts(then_branch, &mut then_state);
                legalize_stmts(else_branch, &mut else_state);
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                legalize_stmts(std::slice::from_mut(init), state);
                let _ = legalize_expr(condition, state);
                legalize_stmts(body, state);
                legalize_stmts(std::slice::from_mut(post), state);
                // Loops are conservatively treated as unknown type flows after one pass.
                state.by_slot.clear();
            }
            Stmt::While {
                condition, body, ..
            } => {
                let _ = legalize_expr(condition, state);
                legalize_stmts(body, state);
                state.by_slot.clear();
            }
        }
    }
}

fn legalize_expr(expr: &mut Expr, state: &LocalTypeState) -> BoundType {
    match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            legalize_expr_children(expr, state);
            BoundType::Unknown
        }
        Expr::ClosureCall(_, _) => {
            legalize_expr_children(expr, state);
            BoundType::Unknown
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
            let lhs_ty = legalize_expr(lhs, state);
            let rhs_ty = legalize_expr(rhs, state);
            infer_binary_type(expr, lhs_ty, rhs_ty)
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = legalize_expr(inner, state);
            infer_unary_type(expr, inner_ty)
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            legalize_expr(inner, state)
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { .. } | Expr::MoveIndex { .. } => BoundType::Unknown,
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = legalize_expr(condition, state);
            let then_ty = legalize_expr(then_expr, state);
            let else_ty = legalize_expr(else_expr, state);
            if then_ty == else_ty {
                then_ty
            } else {
                BoundType::Unknown
            }
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            let _ = legalize_expr(value, state);
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = legalize_expr(arm_expr, state);
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = legalize_expr(default, state);
            if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = LocalTypeState {
                by_slot: state.by_slot.clone(),
            };
            legalize_stmts(stmts, &mut nested);
            legalize_expr(expr, &nested)
        }
    }
}

fn legalize_expr_children(expr: &mut Expr, state: &LocalTypeState) {
    match expr {
        Expr::Call(index, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state);
            }
            if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                fold_builtin_call(expr, builtin, state);
            }
        }
        Expr::LocalCall(_, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state);
            }
        }
        Expr::Closure(closure) => {
            let _ = legalize_expr(&mut closure.body, state);
        }
        Expr::ClosureCall(closure, args) => {
            let _ = legalize_expr(&mut closure.body, state);
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state);
            }
        }
        _ => {}
    }
}

fn fold_builtin_call(expr: &mut Expr, builtin: BuiltinFunction, state: &LocalTypeState) {
    let Expr::Call(_, args) = expr else {
        return;
    };
    match builtin {
        BuiltinFunction::TypeOf => {
            if args.len() == 1 {
                let inferred = infer_expr_type_only(&args[0], state);
                if let Some(name) = inferred.type_name() {
                    *expr = Expr::String(name.to_string());
                }
            }
        }
        BuiltinFunction::Len => {
            if args.len() == 1
                && let Some(len) = infer_static_len(&args[0])
            {
                *expr = Expr::Int(len as i64);
            }
        }
        BuiltinFunction::Concat => {
            if args.len() == 2
                && let (Expr::String(lhs), Expr::String(rhs)) = (&args[0], &args[1])
            {
                *expr = Expr::String(format!("{lhs}{rhs}"));
            }
        }
        _ => {}
    }
}

fn infer_static_len(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::String(text) => Some(text.chars().count()),
        Expr::Call(index, args) => {
            let builtin = BuiltinFunction::from_call_index(*index)?;
            match builtin {
                BuiltinFunction::ArrayNew if args.is_empty() => Some(0),
                BuiltinFunction::MapNew if args.is_empty() => Some(0),
                BuiltinFunction::ArrayPush if args.len() == 2 => {
                    infer_static_len(&args[0]).map(|base| base.saturating_add(1))
                }
                BuiltinFunction::Set if args.len() == 3 => {
                    // Conservative: keep map/set length unknown unless map_new chain.
                    infer_static_len(&args[0])
                }
                _ => None,
            }
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            infer_static_len(inner)
        }
        _ => None,
    }
}

fn infer_expr_type_only(expr: &Expr, state: &LocalTypeState) -> BoundType {
    match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            infer_expr_type_only(inner, state)
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { .. } | Expr::MoveIndex { .. } => BoundType::Unknown,
        Expr::Call(index, args) => {
            let Some(builtin) = BuiltinFunction::from_call_index(*index) else {
                return BoundType::Unknown;
            };
            match builtin {
                BuiltinFunction::ArrayNew => BoundType::Array,
                BuiltinFunction::MapNew => BoundType::Map,
                BuiltinFunction::Len
                | BuiltinFunction::Count
                | BuiltinFunction::FormatTemplate
                | BuiltinFunction::ToString
                | BuiltinFunction::TypeOf => {
                    if builtin == BuiltinFunction::ToString
                        || builtin == BuiltinFunction::TypeOf
                        || builtin == BuiltinFunction::FormatTemplate
                    {
                        BoundType::String
                    } else {
                        BoundType::Int
                    }
                }
                BuiltinFunction::ArrayPush if args.len() == 2 => BoundType::Array,
                BuiltinFunction::Set if args.len() == 3 => infer_expr_type_only(&args[0], state),
                BuiltinFunction::Get => BoundType::Unknown,
                _ => BoundType::Unknown,
            }
        }
        Expr::Add(lhs, rhs) => {
            let l = infer_expr_type_only(lhs, state);
            let r = infer_expr_type_only(rhs, state);
            if l == BoundType::String || r == BoundType::String {
                BoundType::String
            } else if l == BoundType::Int && r == BoundType::Int {
                BoundType::Int
            } else {
                BoundType::Unknown
            }
        }
        _ => BoundType::Unknown,
    }
}

fn infer_binary_type(expr: &Expr, lhs: BoundType, rhs: BoundType) -> BoundType {
    match expr {
        Expr::Add(_, _) => {
            if lhs == BoundType::String || rhs == BoundType::String {
                BoundType::String
            } else if lhs == BoundType::Int && rhs == BoundType::Int {
                BoundType::Int
            } else if (lhs == BoundType::Int || lhs == BoundType::Float)
                && (rhs == BoundType::Int || rhs == BoundType::Float)
            {
                BoundType::Float
            } else {
                BoundType::Unknown
            }
        }
        Expr::Sub(_, _) | Expr::Mul(_, _) | Expr::Div(_, _) | Expr::Mod(_, _) => {
            if lhs == BoundType::Int && rhs == BoundType::Int {
                BoundType::Int
            } else if (lhs == BoundType::Int || lhs == BoundType::Float)
                && (rhs == BoundType::Int || rhs == BoundType::Float)
            {
                BoundType::Float
            } else {
                BoundType::Unknown
            }
        }
        Expr::And(_, _) | Expr::Or(_, _) | Expr::Eq(_, _) | Expr::Lt(_, _) | Expr::Gt(_, _) => {
            BoundType::Bool
        }
        _ => BoundType::Unknown,
    }
}

fn infer_unary_type(expr: &Expr, inner: BoundType) -> BoundType {
    match expr {
        Expr::Neg(_) => match inner {
            BoundType::Int | BoundType::Float => inner,
            _ => BoundType::Unknown,
        },
        Expr::Not(_) => BoundType::Bool,
        _ => BoundType::Unknown,
    }
}
