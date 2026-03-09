use std::collections::HashMap;

use crate::builtins::BuiltinFunction;
use crate::bytecode::ValueType;

use super::CompileError;
use super::ir::{ClosureExpr, Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundType {
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

impl From<BoundType> for ValueType {
    fn from(value: BoundType) -> Self {
        match value {
            BoundType::Unknown => ValueType::Unknown,
            BoundType::Null => ValueType::Null,
            BoundType::Int => ValueType::Int,
            BoundType::Float => ValueType::Float,
            BoundType::Bool => ValueType::Bool,
            BoundType::String => ValueType::String,
            BoundType::Array => ValueType::Array,
            BoundType::Map => ValueType::Map,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LocalTypeState {
    by_slot: HashMap<LocalSlot, BoundType>,
}

impl LocalTypeState {
    pub(crate) fn get(&self, slot: LocalSlot) -> BoundType {
        self.by_slot
            .get(&slot)
            .copied()
            .unwrap_or(BoundType::Unknown)
    }

    pub(crate) fn set(&mut self, slot: LocalSlot, ty: BoundType) {
        if ty == BoundType::Unknown {
            self.by_slot.remove(&slot);
        } else {
            self.by_slot.insert(slot, ty);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.by_slot.clear();
    }

    pub(crate) fn merge_from_branches(&mut self, lhs: &LocalTypeState, rhs: &LocalTypeState) {
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TypeInferenceResult {
    pub local_types: Vec<ValueType>,
}

pub(super) fn legalize_builtins_and_bind_types(mut ir: FrontendIr) -> FrontendIr {
    let mut top_state = LocalTypeState::default();
    legalize_stmts(&mut ir.stmts, &mut top_state);

    for function_impl in ir.function_impls.values_mut() {
        legalize_function_impl(function_impl);
    }

    ir
}

pub(super) fn infer_types(ir: &FrontendIr) -> TypeInferenceResult {
    let mut local_types = vec![ValueType::Unknown; ir.locals];
    let mut top_state = LocalTypeState::default();
    collect_stmt_types(
        &ir.stmts,
        &mut top_state,
        &mut local_types,
        &ir.function_impls,
    );

    for function_impl in ir.function_impls.values() {
        collect_function_types(function_impl, &mut local_types);
    }

    TypeInferenceResult { local_types }
}

pub(super) fn validate_if_else_type_consistency(ir: &FrontendIr) -> Result<(), CompileError> {
    let mut top_state = LocalTypeState::default();
    validate_stmts(&ir.stmts, &mut top_state, None)?;

    for function_impl in ir.function_impls.values() {
        validate_function_impl(function_impl)?;
    }

    Ok(())
}

pub(crate) fn infer_expr_type(expr: &Expr, state: &LocalTypeState) -> BoundType {
    match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            infer_expr_type(inner, state)
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => infer_call_like_expr_type(expr, state),
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
            let lhs_ty = infer_expr_type(lhs, state);
            let rhs_ty = infer_expr_type(rhs, state);
            infer_binary_type(expr, lhs_ty, rhs_ty)
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = infer_expr_type(inner, state);
            infer_unary_type(expr, inner_ty)
        }
        Expr::IfElse {
            condition: _,
            then_expr,
            else_expr,
        } => {
            let then_ty = infer_expr_type(then_expr, state);
            let else_ty = infer_expr_type(else_expr, state);
            if then_ty == else_ty {
                then_ty
            } else {
                BoundType::Unknown
            }
        }
        Expr::Match {
            value: _,
            arms,
            default,
            ..
        } => {
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = infer_expr_type(arm_expr, state);
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = infer_expr_type(default, state);
            if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            collect_stmt_types(stmts, &mut nested, &mut [], &HashMap::new());
            infer_expr_type(expr, &nested)
        }
    }
}

fn legalize_function_impl(function_impl: &mut FunctionImpl) {
    let mut state = LocalTypeState::default();
    for slot in &function_impl.param_slots {
        state.set(*slot, BoundType::Unknown);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        state.set(*captured_slot, state.get(*source_slot));
    }
    legalize_stmts(&mut function_impl.body_stmts, &mut state);
    let _ = legalize_expr(&mut function_impl.body_expr, &state);
}

fn validate_function_impl(function_impl: &FunctionImpl) -> Result<(), CompileError> {
    let mut state = LocalTypeState::default();
    for slot in &function_impl.param_slots {
        state.set(*slot, BoundType::Unknown);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        state.set(*captured_slot, state.get(*source_slot));
    }
    validate_stmts(&function_impl.body_stmts, &mut state, None)?;
    let _ = validate_expr(&function_impl.body_expr, &state, None)?;
    Ok(())
}

fn legalize_stmts(stmts: &mut [Stmt], state: &mut LocalTypeState) {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                state.set(*index, BoundType::Null);
            }
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
                let mut then_state = state.clone();
                let mut else_state = state.clone();
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
                state.clear();
            }
            Stmt::While {
                condition, body, ..
            } => {
                let _ = legalize_expr(condition, state);
                legalize_stmts(body, state);
                state.clear();
            }
        }
    }
}

fn validate_stmts(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    line_context: Option<u32>,
) -> Result<(), CompileError> {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                state.set(*index, BoundType::Null);
            }
            Stmt::ClosureLet { closure, .. } => {
                let _ = validate_expr(&closure.body, state, line_context)?;
            }
            Stmt::Let { index, expr, line } | Stmt::Assign { index, expr, line } => {
                let ty = validate_expr(expr, state, Some(*line))?;
                state.set(*index, ty);
            }
            Stmt::Expr { expr, line } => {
                let _ = validate_expr(expr, state, Some(*line))?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let _ = validate_expr(condition, state, Some(*line))?;
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                validate_stmts(then_branch, &mut then_state, Some(*line))?;
                validate_stmts(else_branch, &mut else_state, Some(*line))?;
                validate_branch_state_merge(Some(*line), &then_state, &else_state)?;
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                validate_stmts(std::slice::from_ref(init), state, Some(*line))?;
                let _ = validate_expr(condition, state, Some(*line))?;
                validate_stmts(body, state, Some(*line))?;
                validate_stmts(std::slice::from_ref(post), state, Some(*line))?;
                state.clear();
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                let _ = validate_expr(condition, state, Some(*line))?;
                validate_stmts(body, state, Some(*line))?;
                state.clear();
            }
        }
    }

    Ok(())
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
            infer_call_like_expr_type(expr, state)
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
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
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
            let mut nested = state.clone();
            legalize_stmts(stmts, &mut nested);
            legalize_expr(expr, &nested)
        }
    }
}

fn validate_expr(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
) -> Result<BoundType, CompileError> {
    Ok(match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            validate_expr(inner, state, line_context)?
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            validate_expr_children(expr, state, line_context)?;
            infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => {
            validate_expr_children(expr, state, line_context)?;
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
            let lhs_ty = validate_expr(lhs, state, line_context)?;
            let rhs_ty = validate_expr(rhs, state, line_context)?;
            infer_binary_type(expr, lhs_ty, rhs_ty)
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = validate_expr(inner, state, line_context)?;
            infer_unary_type(expr, inner_ty)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = validate_expr(condition, state, line_context)?;
            let then_ty = validate_expr(then_expr, state, line_context)?;
            let else_ty = validate_expr(else_expr, state, line_context)?;
            ensure_compatible_if_else_types(line_context, "expression result", then_ty, else_ty)?;
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
            let _ = validate_expr(value, state, line_context)?;
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = validate_expr(arm_expr, state, line_context)?;
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = validate_expr(default, state, line_context)?;
            if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            validate_stmts(stmts, &mut nested, line_context)?;
            validate_expr(expr, &nested, line_context)?
        }
    })
}

fn validate_expr_children(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                let _ = validate_expr(arg, state, line_context)?;
            }
        }
        Expr::Closure(closure) => {
            let _ = validate_expr(&closure.body, state, line_context)?;
        }
        Expr::ClosureCall(closure, args) => {
            let _ = validate_expr(&closure.body, state, line_context)?;
            for arg in args {
                let _ = validate_expr(arg, state, line_context)?;
            }
        }
        _ => {}
    }
    Ok(())
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
                let inferred = infer_expr_type(&args[0], state);
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
                BuiltinFunction::Set if args.len() == 3 => infer_static_len(&args[0]),
                _ => None,
            }
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            infer_static_len(inner)
        }
        _ => None,
    }
}

fn infer_call_like_expr_type(expr: &Expr, state: &LocalTypeState) -> BoundType {
    match expr {
        Expr::Call(index, args) => {
            let Some(builtin) = BuiltinFunction::from_call_index(*index) else {
                return BoundType::Unknown;
            };
            match builtin {
                BuiltinFunction::ArrayNew => BoundType::Array,
                BuiltinFunction::MapNew => BoundType::Map,
                BuiltinFunction::Len | BuiltinFunction::Count => BoundType::Int,
                BuiltinFunction::FormatTemplate
                | BuiltinFunction::ToString
                | BuiltinFunction::TypeOf => BoundType::String,
                BuiltinFunction::ArrayPush if args.len() == 2 => BoundType::Array,
                BuiltinFunction::Set if args.len() == 3 => infer_expr_type(&args[0], state),
                BuiltinFunction::Get => BoundType::Unknown,
                _ => BoundType::Unknown,
            }
        }
        Expr::Closure(_)
        | Expr::ClosureCall(_, _)
        | Expr::FunctionRef(_)
        | Expr::LocalCall(_, _) => BoundType::Unknown,
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

fn ensure_compatible_if_else_types(
    line: Option<u32>,
    context: &str,
    lhs: BoundType,
    rhs: BoundType,
) -> Result<(), CompileError> {
    if lhs == BoundType::Unknown || rhs == BoundType::Unknown || lhs == rhs {
        return Ok(());
    }
    Err(CompileError::IfElseBranchTypeMismatch {
        line,
        detail: format!(
            "if/else branches produced incompatible {context}: {} vs {}",
            bound_type_label(lhs),
            bound_type_label(rhs)
        ),
    })
}

fn validate_branch_state_merge(
    line: Option<u32>,
    lhs: &LocalTypeState,
    rhs: &LocalTypeState,
) -> Result<(), CompileError> {
    for slot in lhs.by_slot.keys().chain(rhs.by_slot.keys()) {
        let left = lhs.get(*slot);
        let right = rhs.get(*slot);
        if left == BoundType::Unknown || right == BoundType::Unknown || left == right {
            continue;
        }
        return Err(CompileError::IfElseBranchTypeMismatch {
            line,
            detail: format!(
                "if/else branches assign incompatible types to local slot {}: {} vs {}",
                slot,
                bound_type_label(left),
                bound_type_label(right)
            ),
        });
    }
    Ok(())
}

fn bound_type_label(ty: BoundType) -> &'static str {
    match ty {
        BoundType::Unknown => "unknown",
        BoundType::Null => "null",
        BoundType::Int => "int",
        BoundType::Float => "float",
        BoundType::Bool => "bool",
        BoundType::String => "string",
        BoundType::Array => "array",
        BoundType::Map => "map",
    }
}

fn collect_function_types(function_impl: &FunctionImpl, local_types: &mut [ValueType]) {
    let mut state = LocalTypeState::default();
    for slot in &function_impl.param_slots {
        record_local_type(local_types, *slot, BoundType::Unknown);
        state.set(*slot, BoundType::Unknown);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let ty = state.get(*source_slot);
        record_local_type(local_types, *captured_slot, ty);
        state.set(*captured_slot, ty);
    }
    collect_stmt_types(
        &function_impl.body_stmts,
        &mut state,
        local_types,
        &HashMap::new(),
    );
    let _ = infer_expr_type(&function_impl.body_expr, &state);
}

fn collect_stmt_types(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    local_types: &mut [ValueType],
    function_impls: &HashMap<u16, FunctionImpl>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                record_local_type(local_types, *index, BoundType::Null);
                state.set(*index, BoundType::Null);
            }
            Stmt::ClosureLet { closure, .. } => {
                collect_closure_capture_types(closure, state, local_types);
                let _ = infer_expr_type(&closure.body, state);
            }
            Stmt::FuncDecl { index, .. } => {
                if let Some(function_impl) = function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        let ty = state.get(*source_slot);
                        record_local_type(local_types, *captured_slot, ty);
                        state.set(*captured_slot, ty);
                    }
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let ty = infer_expr_type(expr, state);
                record_local_type(local_types, *index, ty);
                state.set(*index, ty);
            }
            Stmt::Expr { expr, .. } => {
                let _ = infer_expr_type(expr, state);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = infer_expr_type(condition, state);
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                collect_stmt_types(then_branch, &mut then_state, local_types, function_impls);
                collect_stmt_types(else_branch, &mut else_state, local_types, function_impls);
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                collect_stmt_types(
                    std::slice::from_ref(init),
                    state,
                    local_types,
                    function_impls,
                );
                let _ = infer_expr_type(condition, state);
                collect_stmt_types(body, state, local_types, function_impls);
                collect_stmt_types(
                    std::slice::from_ref(post),
                    state,
                    local_types,
                    function_impls,
                );
                state.clear();
            }
            Stmt::While {
                condition, body, ..
            } => {
                let _ = infer_expr_type(condition, state);
                collect_stmt_types(body, state, local_types, function_impls);
                state.clear();
            }
        }
    }
}

fn collect_closure_capture_types(
    closure: &ClosureExpr,
    state: &LocalTypeState,
    local_types: &mut [ValueType],
) {
    for (source_slot, captured_slot) in &closure.capture_copies {
        record_local_type(local_types, *captured_slot, state.get(*source_slot));
    }
}

fn record_local_type(local_types: &mut [ValueType], slot: LocalSlot, ty: BoundType) {
    let Some(entry) = local_types.get_mut(slot as usize) else {
        return;
    };
    let next = ValueType::from(ty);
    *entry = match (*entry, next) {
        (ValueType::Unknown, next) => next,
        (current, next) if current == next => current,
        _ => ValueType::Unknown,
    };
}
