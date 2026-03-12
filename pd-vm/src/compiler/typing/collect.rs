use std::collections::HashMap;

use crate::bytecode::ValueType;

use super::super::ir::{ClosureExpr, Expr, FunctionImpl, LocalSlot, Stmt, TypeSchema};
use super::context::TypeContext;
use super::helpers::{bind_expr_result_to_slot, refine_state_for_match_pattern};
use super::state::{BoundType, HostCallableSignature, LocalTypeState, stabilize_loop_state};
use super::validate::refine_state_for_condition;

pub(super) fn observed_function_param_slice(
    observed: &HashMap<u16, Vec<BoundType>>,
    function_index: u16,
) -> Option<&[BoundType]> {
    observed.get(&function_index).map(Vec::as_slice)
}

pub(super) fn observed_function_param_schema_slice(
    observed: &HashMap<u16, Vec<Option<TypeSchema>>>,
    function_index: u16,
) -> Option<&[Option<TypeSchema>]> {
    observed.get(&function_index).map(Vec::as_slice)
}

pub(super) fn seed_function_param_state(
    state: &mut LocalTypeState,
    param_slots: &[LocalSlot],
    observed_types: Option<&[BoundType]>,
    observed_schemas: Option<&[Option<TypeSchema>]>,
) {
    for (param_index, slot) in param_slots.iter().enumerate() {
        let ty = observed_types
            .and_then(|types| types.get(param_index))
            .copied()
            .unwrap_or(BoundType::Unknown);
        let schema = observed_schemas
            .and_then(|schemas| schemas.get(param_index))
            .cloned()
            .flatten();
        if let Some(schema) = schema {
            state.set_with_schema_origin(*slot, ty, Some(schema), true);
        } else {
            state.set(*slot, ty);
        }
    }
}

pub(super) fn seed_function_capture_state(
    state: &mut LocalTypeState,
    function_index: u16,
    capture_copies: &[(LocalSlot, LocalSlot)],
    observed_capture_states: &HashMap<u16, LocalTypeState>,
) {
    let Some(observed) = observed_capture_states.get(&function_index) else {
        return;
    };

    for (_, captured_slot) in capture_copies {
        if let Some(callable) = observed.callable(*captured_slot).cloned() {
            state.bind_callable(*captured_slot, callable);
        } else {
            state.copy_binding_from(observed, *captured_slot, *captured_slot, None, false);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_function_types(
    function_index: u16,
    function_impl: &FunctionImpl,
    local_types: &mut [ValueType],
    callable_slots: &mut [bool],
    function_impls: &HashMap<u16, FunctionImpl>,
    function_names: &HashMap<u16, String>,
    struct_schemas: &HashMap<String, TypeSchema>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
    observed_function_param_types: &HashMap<u16, Vec<BoundType>>,
    observed_function_param_schemas: &HashMap<u16, Vec<Option<TypeSchema>>>,
    observed_function_capture_states: &HashMap<u16, LocalTypeState>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        struct_schemas,
        function_names,
        host_import_return_types,
        host_import_signatures,
        false,
    );
    seed_function_param_state(
        &mut state,
        &function_impl.param_slots,
        observed_function_param_slice(observed_function_param_types, function_index),
        observed_function_param_schema_slice(observed_function_param_schemas, function_index),
    );
    seed_function_capture_state(
        &mut state,
        function_index,
        &function_impl.capture_copies,
        observed_function_capture_states,
    );
    for slot in &function_impl.param_slots {
        record_local_type(local_types, *slot, state.get(*slot));
    }
    for (_source_slot, captured_slot) in &function_impl.capture_copies {
        let ty = state.get(*captured_slot);
        record_local_type(local_types, *captured_slot, ty);
        if state.callable(*captured_slot).is_some() {
            record_callable_slot(callable_slots, *captured_slot);
        }
    }
    collect_stmt_types(
        &function_impl.body_stmts,
        &mut state,
        local_types,
        callable_slots,
        &mut context,
    );
    let _ = context.infer_expr_type(&function_impl.body_expr, &state);
}

pub(super) fn collect_stmt_types(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    local_types: &mut [ValueType],
    callable_slots: &mut [bool],
    context: &mut TypeContext<'_>,
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
                collect_expr_types(&closure.body, state, local_types, callable_slots, context);
            }
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                if *has_impl && let Some(function_impl) = context.function_impls.get(index) {
                    context.observe_function_capture_state(
                        *index,
                        &function_impl.capture_copies,
                        state,
                    );
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        let ty = state.get(*source_slot);
                        record_local_type(local_types, *captured_slot, ty);
                        let source_state = state.clone();
                        state.copy_binding_from(
                            &source_state,
                            *source_slot,
                            *captured_slot,
                            None,
                            false,
                        );
                        if state.callable(*captured_slot).is_some() {
                            record_callable_slot(callable_slots, *captured_slot);
                        }
                    }
                }
            }
            Stmt::Let {
                index,
                declared_schema,
                expr,
                ..
            } => {
                let expr_state = state.clone();
                collect_expr_types(expr, &expr_state, local_types, callable_slots, context);
                let ty = context.infer_expr_type(expr, &expr_state);
                bind_expr_result_to_slot(
                    state,
                    *index,
                    declared_schema.as_ref(),
                    expr,
                    &expr_state,
                    ty,
                    context,
                );
                record_local_type(local_types, *index, state.get(*index));
                if state.callable(*index).is_some() {
                    record_callable_slot(callable_slots, *index);
                }
            }
            Stmt::Assign { index, expr, .. } => {
                let expr_state = state.clone();
                collect_expr_types(expr, &expr_state, local_types, callable_slots, context);
                let ty = context.infer_expr_type(expr, &expr_state);
                bind_expr_result_to_slot(state, *index, None, expr, &expr_state, ty, context);
                record_local_type(local_types, *index, state.get(*index));
                if state.callable(*index).is_some() {
                    record_callable_slot(callable_slots, *index);
                }
            }
            Stmt::Expr { expr, .. } => {
                collect_expr_types(expr, state, local_types, callable_slots, context);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                collect_expr_types(condition, state, local_types, callable_slots, context);
                let mut then_state = refine_state_for_condition(state, condition, true);
                let mut else_state = refine_state_for_condition(state, condition, false);
                collect_stmt_types(
                    then_branch,
                    &mut then_state,
                    local_types,
                    callable_slots,
                    context,
                );
                collect_stmt_types(
                    else_branch,
                    &mut else_state,
                    local_types,
                    callable_slots,
                    context,
                );
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
                    callable_slots,
                    context,
                );
                stabilize_loop_state(state, |iterated| {
                    collect_expr_types(condition, iterated, local_types, callable_slots, context);
                    collect_stmt_types(body, iterated, local_types, callable_slots, context);
                    collect_stmt_types(
                        std::slice::from_ref(post),
                        iterated,
                        local_types,
                        callable_slots,
                        context,
                    );
                });
            }
            Stmt::While {
                condition, body, ..
            } => {
                stabilize_loop_state(state, |iterated| {
                    collect_expr_types(condition, iterated, local_types, callable_slots, context);
                    collect_stmt_types(body, iterated, local_types, callable_slots, context);
                });
            }
        }
    }
}

fn collect_expr_types(
    expr: &Expr,
    state: &LocalTypeState,
    local_types: &mut [ValueType],
    callable_slots: &mut [bool],
    context: &mut TypeContext<'_>,
) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. }
        | Expr::FunctionRef(_) => {
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::OptionalGet { container, key, .. } => {
            collect_expr_types(container, state, local_types, callable_slots, context);
            collect_expr_types(key, state, local_types, callable_slots, context);
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            collect_expr_types(value, state, local_types, callable_slots, context);
            collect_expr_types(fallback, state, local_types, callable_slots, context);
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                collect_expr_types(arg, state, local_types, callable_slots, context);
            }
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::Closure(closure) => {
            if let Some(nested) = context.build_callable_state(
                &closure.param_slots,
                &closure.capture_copies,
                None,
                state,
            ) {
                collect_callable_body_types(closure, &nested, local_types, callable_slots, context);
            } else {
                let _ = context.infer_expr_type(expr, state);
            }
        }
        Expr::ClosureCall(closure, args) => {
            for arg in args {
                collect_expr_types(arg, state, local_types, callable_slots, context);
            }
            if let Some(nested) = context.build_callable_state(
                &closure.param_slots,
                &closure.capture_copies,
                Some(args),
                state,
            ) {
                collect_callable_body_types(closure, &nested, local_types, callable_slots, context);
            } else {
                let _ = context.infer_expr_type(expr, state);
            }
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
            collect_expr_types(lhs, state, local_types, callable_slots, context);
            collect_expr_types(rhs, state, local_types, callable_slots, context);
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            collect_expr_types(inner, state, local_types, callable_slots, context);
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_expr_types(condition, state, local_types, callable_slots, context);
            collect_expr_types(then_expr, state, local_types, callable_slots, context);
            collect_expr_types(else_expr, state, local_types, callable_slots, context);
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::Match {
            value_slot,
            value,
            arms,
            default,
            ..
        } => {
            collect_expr_types(value, state, local_types, callable_slots, context);
            let value_ty = context.infer_expr_type(value, state);
            let mut nested = state.clone();
            bind_expr_result_to_slot(
                &mut nested,
                *value_slot,
                None,
                value,
                state,
                value_ty,
                context,
            );
            record_local_type(local_types, *value_slot, nested.get(*value_slot));
            for (pattern, arm_expr) in arms {
                let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                if let Some(binding_slot) = pattern.binding_slot() {
                    record_local_type(local_types, binding_slot, arm_state.get(binding_slot));
                }
                collect_expr_types(arm_expr, &arm_state, local_types, callable_slots, context);
            }
            collect_expr_types(default, &nested, local_types, callable_slots, context);
            let _ = context.infer_expr_type(expr, state);
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            collect_stmt_types(stmts, &mut nested, local_types, callable_slots, context);
            collect_expr_types(expr, &nested, local_types, callable_slots, context);
        }
    }
}

fn collect_callable_body_types(
    closure: &ClosureExpr,
    nested: &LocalTypeState,
    local_types: &mut [ValueType],
    callable_slots: &mut [bool],
    context: &mut TypeContext<'_>,
) {
    for slot in &closure.param_slots {
        record_local_type(local_types, *slot, nested.get(*slot));
        if nested.callable(*slot).is_some() {
            record_callable_slot(callable_slots, *slot);
        }
    }
    collect_closure_capture_types(closure, nested, local_types);
    collect_expr_types(&closure.body, nested, local_types, callable_slots, context);
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

fn record_callable_slot(callable_slots: &mut [bool], slot: LocalSlot) {
    if let Some(entry) = callable_slots.get_mut(slot as usize) {
        *entry = true;
    }
}
