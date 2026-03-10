use std::collections::HashMap;

use crate::builtins::{BuiltinFunction, CallableParam, CallableParamType};

use super::super::CompileError;
use super::super::ir::{Expr, FunctionDecl, FunctionImpl, LocalSlot, Stmt};
use super::collect::{observed_function_param_slice, seed_function_param_state};
use super::context::TypeContext;
use super::infer_expr_type;
use super::state::{
    BoundType, HostCallableSignature, LocalTypeState, merge_bound_types,
    merge_container_element_types, stabilize_loop_state, try_stabilize_loop_state,
};
use super::validate::{owned_source_name, validate_branch_state_merge, validate_expr};

pub(super) fn legalize_function_impl(
    function_index: u16,
    function_impl: &mut FunctionImpl,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_names: &HashMap<u16, String>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
    observed_function_param_types: &HashMap<u16, Vec<BoundType>>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        function_names,
        host_import_return_types,
        host_import_signatures,
    );
    seed_function_param_state(
        &mut state,
        &function_impl.param_slots,
        observed_function_param_slice(observed_function_param_types, function_index),
    );
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    legalize_stmts(&mut function_impl.body_stmts, &mut state, &mut context);
    let _ = legalize_expr(&mut function_impl.body_expr, &state, &mut context);
}

pub(super) fn validate_function_impl(
    function_index: u16,
    function_impl: &FunctionImpl,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    if let Some(detail) = context
        .function_param_conflicts
        .get(&function_index)
        .cloned()
    {
        return Err(CompileError::FunctionParameterTypeConflict {
            line: None,
            source_name: owned_source_name(source_name),
            detail,
        });
    }
    let mut state = LocalTypeState::default();
    let strict_function_add_types = context
        .observed_function_param_types
        .contains_key(&function_index)
        && context.function_requires_strict_add_types(function_index);
    seed_function_param_state(
        &mut state,
        &function_impl.param_slots,
        observed_function_param_slice(&context.observed_function_param_types, function_index),
    );
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    validate_stmts(
        &function_impl.body_stmts,
        &mut state,
        None,
        source_name,
        context,
        strict_function_add_types,
    )?;
    let _ = validate_expr(
        &function_impl.body_expr,
        &state,
        None,
        source_name,
        context,
        strict_function_add_types,
    )?;
    Ok(())
}

pub(super) fn legalize_stmts(
    stmts: &mut [Stmt],
    state: &mut LocalTypeState,
    context: &mut TypeContext<'_>,
) {
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
                let _ = legalize_expr(&mut closure.body, state, context);
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let expr_state = state.clone();
                let ty = legalize_expr(expr, &expr_state, context);
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, .. } => {
                let _ = legalize_expr(expr, state, context);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = legalize_expr(condition, state, context);
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                legalize_stmts(then_branch, &mut then_state, context);
                legalize_stmts(else_branch, &mut else_state, context);
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                legalize_stmts(std::slice::from_mut(init), state, context);
                stabilize_loop_state(state, |iterated| {
                    let _ = legalize_expr(condition, iterated, context);
                    legalize_stmts(body, iterated, context);
                    legalize_stmts(std::slice::from_mut(post), iterated, context);
                });
            }
            Stmt::While {
                condition, body, ..
            } => {
                stabilize_loop_state(state, |iterated| {
                    let _ = legalize_expr(condition, iterated, context);
                    legalize_stmts(body, iterated, context);
                });
            }
        }
    }
}

pub(super) fn validate_stmts(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
    strict_function_add_types: bool,
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
                let _ = validate_expr(
                    &closure.body,
                    state,
                    line_context,
                    source_name,
                    context,
                    false,
                )?;
            }
            Stmt::Let { index, expr, line } | Stmt::Assign { index, expr, line } => {
                let expr_state = state.clone();
                let ty = validate_expr(
                    expr,
                    &expr_state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, line } => {
                let _ = validate_expr(
                    expr,
                    state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let _ = validate_expr(
                    condition,
                    state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                validate_stmts(
                    then_branch,
                    &mut then_state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                validate_stmts(
                    else_branch,
                    &mut else_state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                validate_branch_state_merge(Some(*line), source_name, &then_state, &else_state)?;
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                validate_stmts(
                    std::slice::from_ref(init),
                    state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                try_stabilize_loop_state(state, |iterated| {
                    let _ = validate_expr(
                        condition,
                        iterated,
                        Some(*line),
                        source_name,
                        context,
                        strict_function_add_types,
                    )?;
                    validate_stmts(
                        body,
                        iterated,
                        Some(*line),
                        source_name,
                        context,
                        strict_function_add_types,
                    )?;
                    validate_stmts(
                        std::slice::from_ref(post),
                        iterated,
                        Some(*line),
                        source_name,
                        context,
                        strict_function_add_types,
                    )
                })?;
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                try_stabilize_loop_state(state, |iterated| {
                    let _ = validate_expr(
                        condition,
                        iterated,
                        Some(*line),
                        source_name,
                        context,
                        strict_function_add_types,
                    )?;
                    validate_stmts(
                        body,
                        iterated,
                        Some(*line),
                        source_name,
                        context,
                        strict_function_add_types,
                    )
                })?;
            }
        }
    }

    Ok(())
}

pub(super) fn bind_expr_result_to_slot(
    state: &mut LocalTypeState,
    slot: LocalSlot,
    expr: &Expr,
    expr_state: &LocalTypeState,
    ty: BoundType,
    context: &mut TypeContext<'_>,
) {
    if let Some(callable) = context.callable_binding_from_expr(expr, expr_state) {
        state.bind_callable(slot, callable);
    } else {
        state.set(slot, ty);
    }
}

pub(super) fn build_function_names(functions: &[FunctionDecl]) -> HashMap<u16, String> {
    functions
        .iter()
        .map(|decl| (decl.index, decl.name.clone()))
        .collect()
}

pub(super) fn build_host_import_return_types(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<u16, BoundType> {
    functions
        .iter()
        .filter(|decl| !function_impls.contains_key(&decl.index))
        .map(|decl| (decl.index, BoundType::from(decl.return_type)))
        .collect()
}

pub(crate) fn build_host_import_signatures(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<u16, HostCallableSignature> {
    functions
        .iter()
        .filter(|decl| !function_impls.contains_key(&decl.index))
        .filter_map(|decl| {
            known_host_signature(&decl.name).map(|signature| (decl.index, signature))
        })
        .collect()
}

pub(super) fn known_host_signature(name: &str) -> Option<HostCallableSignature> {
    if let Some(callable) = crate::builtins::default_host_callable(name) {
        return Some(HostCallableSignature {
            name: callable.name.to_string(),
            params: callable.signature.params.to_vec(),
        });
    }

    let function = edge_abi::function_by_name(name)?;
    Some(HostCallableSignature {
        name: name.to_string(),
        params: function
            .param_names
            .iter()
            .copied()
            .zip(function.param_types.iter().copied())
            .map(|(param_name, param_type)| CallableParam {
                name: param_name,
                ty: callable_param_type_from_abi(param_type),
                optional: false,
            })
            .collect(),
    })
}

pub(super) fn callable_param_type_from_abi(value: edge_abi::AbiParamType) -> CallableParamType {
    match value {
        edge_abi::AbiParamType::Any => CallableParamType::Any,
        edge_abi::AbiParamType::Null => CallableParamType::Null,
        edge_abi::AbiParamType::Int => CallableParamType::Int,
        edge_abi::AbiParamType::Float => CallableParamType::Float,
        edge_abi::AbiParamType::Bool => CallableParamType::Bool,
        edge_abi::AbiParamType::String => CallableParamType::String,
        edge_abi::AbiParamType::Array => CallableParamType::Array,
        edge_abi::AbiParamType::Map => CallableParamType::Map,
        edge_abi::AbiParamType::Number => CallableParamType::Number,
    }
}

pub(super) fn merge_observed_function_param_type(
    current: BoundType,
    next: BoundType,
) -> Result<BoundType, (BoundType, BoundType)> {
    if current == BoundType::Unknown {
        return Ok(next);
    }
    if next == BoundType::Unknown || current == next {
        return Ok(current);
    }
    let merged = merge_bound_types(current, next);
    if merged != BoundType::Unknown {
        Ok(merged)
    } else {
        Err((current, next))
    }
}

pub(super) fn function_body_contains_param_add(
    param_slots: &[LocalSlot],
    stmts: &[Stmt],
    expr: &Expr,
) -> bool {
    stmts
        .iter()
        .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        || expr_contains_param_add(expr, param_slots)
}

pub(super) fn stmt_contains_param_add(stmt: &Stmt, param_slots: &[LocalSlot]) -> bool {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => false,
        Stmt::ClosureLet { closure, .. } => expr_contains_param_add(&closure.body, param_slots),
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            expr_contains_param_add(expr, param_slots)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_param_add(condition, param_slots)
                || then_branch
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
                || else_branch
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_contains_param_add(init, param_slots)
                || expr_contains_param_add(condition, param_slots)
                || stmt_contains_param_add(post, param_slots)
                || body
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        }
        Stmt::While {
            condition, body, ..
        } => {
            expr_contains_param_add(condition, param_slots)
                || body
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        }
    }
}

pub(super) fn expr_contains_param_add(expr: &Expr, param_slots: &[LocalSlot]) -> bool {
    match expr {
        Expr::Add(lhs, rhs) => {
            expr_uses_param(lhs, param_slots)
                || expr_uses_param(rhs, param_slots)
                || expr_contains_param_add(lhs, param_slots)
                || expr_contains_param_add(rhs, param_slots)
        }
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::Call(_, args) | Expr::LocalCall(_, args) => args
            .iter()
            .any(|arg| expr_contains_param_add(arg, param_slots)),
        Expr::ClosureCall(closure, args) => {
            expr_contains_param_add(&closure.body, param_slots)
                || args
                    .iter()
                    .any(|arg| expr_contains_param_add(arg, param_slots))
        }
        Expr::Closure(closure) => expr_contains_param_add(&closure.body, param_slots),
        Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => {
            expr_contains_param_add(lhs, param_slots) || expr_contains_param_add(rhs, param_slots)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => expr_contains_param_add(inner, param_slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_contains_param_add(condition, param_slots)
                || expr_contains_param_add(then_expr, param_slots)
                || expr_contains_param_add(else_expr, param_slots)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            expr_contains_param_add(value, param_slots)
                || arms
                    .iter()
                    .any(|(_, arm_expr)| expr_contains_param_add(arm_expr, param_slots))
                || expr_contains_param_add(default, param_slots)
        }
        Expr::Block { stmts, expr } => function_body_contains_param_add(param_slots, stmts, expr),
    }
}

pub(super) fn expr_uses_param(expr: &Expr, param_slots: &[LocalSlot]) -> bool {
    match expr {
        Expr::Var(slot) | Expr::MoveVar(slot) => param_slots.contains(slot),
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            args.iter().any(|arg| expr_uses_param(arg, param_slots))
        }
        Expr::ClosureCall(closure, args) => {
            expr_uses_param(&closure.body, param_slots)
                || args.iter().any(|arg| expr_uses_param(arg, param_slots))
        }
        Expr::Closure(closure) => expr_uses_param(&closure.body, param_slots),
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
            expr_uses_param(lhs, param_slots) || expr_uses_param(rhs, param_slots)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => expr_uses_param(inner, param_slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_uses_param(condition, param_slots)
                || expr_uses_param(then_expr, param_slots)
                || expr_uses_param(else_expr, param_slots)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            expr_uses_param(value, param_slots)
                || arms
                    .iter()
                    .any(|(_, arm_expr)| expr_uses_param(arm_expr, param_slots))
                || expr_uses_param(default, param_slots)
        }
        Expr::Block { stmts, expr } => {
            stmts.iter().any(|stmt| stmt_uses_param(stmt, param_slots))
                || expr_uses_param(expr, param_slots)
        }
    }
}

pub(super) fn stmt_uses_param(stmt: &Stmt, param_slots: &[LocalSlot]) -> bool {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => false,
        Stmt::ClosureLet { closure, .. } => expr_uses_param(&closure.body, param_slots),
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            expr_uses_param(expr, param_slots)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_uses_param(condition, param_slots)
                || then_branch
                    .iter()
                    .any(|stmt| stmt_uses_param(stmt, param_slots))
                || else_branch
                    .iter()
                    .any(|stmt| stmt_uses_param(stmt, param_slots))
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_uses_param(init, param_slots)
                || expr_uses_param(condition, param_slots)
                || stmt_uses_param(post, param_slots)
                || body.iter().any(|stmt| stmt_uses_param(stmt, param_slots))
        }
        Stmt::While {
            condition, body, ..
        } => {
            expr_uses_param(condition, param_slots)
                || body.iter().any(|stmt| stmt_uses_param(stmt, param_slots))
        }
    }
}

pub(super) fn display_name_for_builtin(builtin: BuiltinFunction) -> String {
    match builtin {
        BuiltinFunction::Len => "len".to_string(),
        BuiltinFunction::Slice => "slice".to_string(),
        BuiltinFunction::Concat => "concat".to_string(),
        BuiltinFunction::ArrayNew => "array_new".to_string(),
        BuiltinFunction::ArrayPush => "array_push".to_string(),
        BuiltinFunction::MapNew => "map_new".to_string(),
        BuiltinFunction::Get => "get".to_string(),
        BuiltinFunction::Set => "set".to_string(),
        BuiltinFunction::Keys => "keys".to_string(),
        BuiltinFunction::Count => "count".to_string(),
        BuiltinFunction::TypeOf => "type".to_string(),
        BuiltinFunction::Assert => "assert".to_string(),
        _ => builtin.name().replacen('_', "::", 1),
    }
}

pub(super) fn is_numeric_bound_type(value: BoundType) -> bool {
    matches!(value, BoundType::Int | BoundType::Float)
}

pub(super) fn legalize_expr(
    expr: &mut Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> BoundType {
    match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            legalize_expr_children(expr, state, context);
            context.infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => {
            legalize_expr_children(expr, state, context);
            context.infer_call_like_expr_type(expr, state)
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
            let lhs_ty = legalize_expr(lhs, state, context);
            let rhs_ty = legalize_expr(rhs, state, context);
            infer_binary_type(expr, lhs_ty, rhs_ty)
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = legalize_expr(inner, state, context);
            infer_unary_type(expr, inner_ty)
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            legalize_expr(inner, state, context)
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = legalize_expr(condition, state, context);
            let then_ty = legalize_expr(then_expr, state, context);
            let else_ty = legalize_expr(else_expr, state, context);
            if then_ty == else_ty {
                then_ty
            } else {
                BoundType::Unknown
            }
        }
        Expr::Match {
            value_slot,
            value,
            arms,
            default,
            ..
        } => {
            let mut nested = state.clone();
            let value_ty = legalize_expr(value, state, context);
            bind_expr_result_to_slot(&mut nested, *value_slot, value, state, value_ty, context);
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms.iter_mut() {
                let ty = legalize_expr(arm_expr, &nested, context);
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = legalize_expr(default, &nested, context);
            if arms.is_empty() {
                default_ty
            } else if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            legalize_stmts(stmts, &mut nested, context);
            legalize_expr(expr, &nested, context)
        }
    }
}

pub(super) fn legalize_expr_children(
    expr: &mut Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) {
    match expr {
        Expr::Call(index, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
            if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                fold_builtin_call(expr, builtin, state);
            }
        }
        Expr::LocalCall(_, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
        }
        Expr::Closure(closure) => {
            let _ = legalize_expr(&mut closure.body, state, context);
        }
        Expr::ClosureCall(closure, args) => {
            let _ = legalize_expr(&mut closure.body, state, context);
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
        }
        _ => {}
    }
}

pub(super) fn fold_builtin_call(expr: &mut Expr, builtin: BuiltinFunction, state: &LocalTypeState) {
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

pub(super) fn infer_static_len(expr: &Expr) -> Option<usize> {
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

pub(super) fn infer_binary_type(expr: &Expr, lhs: BoundType, rhs: BoundType) -> BoundType {
    match expr {
        Expr::Add(_, _) => {
            if lhs == BoundType::String || rhs == BoundType::String {
                BoundType::String
            } else if let Some(array_ty) = infer_array_concat_type(lhs, rhs) {
                array_ty
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

pub(super) fn infer_array_concat_type(lhs: BoundType, rhs: BoundType) -> Option<BoundType> {
    match (lhs, rhs) {
        (BoundType::ArrayOf(lhs), BoundType::ArrayOf(rhs)) => {
            Some(merge_container_element_types(lhs, rhs))
        }
        (BoundType::Array, BoundType::Array)
        | (BoundType::Array, BoundType::ArrayOf(_))
        | (BoundType::ArrayOf(_), BoundType::Array) => Some(BoundType::Array),
        _ => None,
    }
}

pub(super) fn infer_unary_type(expr: &Expr, inner: BoundType) -> BoundType {
    match expr {
        Expr::Neg(_) => match inner {
            BoundType::Int | BoundType::Float => inner,
            _ => BoundType::Unknown,
        },
        Expr::Not(_) => BoundType::Bool,
        _ => BoundType::Unknown,
    }
}

pub(super) fn bound_type_label(ty: BoundType) -> &'static str {
    match ty {
        BoundType::Unknown => "unknown",
        BoundType::Null => "null",
        BoundType::Int => "int",
        BoundType::Float => "float",
        BoundType::Bool => "bool",
        BoundType::String => "string",
        BoundType::Array | BoundType::ArrayOf(_) => "array",
        BoundType::Map | BoundType::MapOf(_) => "map",
    }
}
