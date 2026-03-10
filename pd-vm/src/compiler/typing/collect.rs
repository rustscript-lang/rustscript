use std::collections::HashMap;

use crate::bytecode::ValueType;

use super::super::ir::{ClosureExpr, FunctionImpl, LocalSlot, Stmt};
use super::context::TypeContext;
use super::helpers::bind_expr_result_to_slot;
use super::state::{BoundType, HostCallableSignature, LocalTypeState, stabilize_loop_state};

pub(super) fn observed_function_param_slice(
    observed: &HashMap<u16, Vec<BoundType>>,
    function_index: u16,
) -> Option<&[BoundType]> {
    observed.get(&function_index).map(Vec::as_slice)
}

pub(super) fn seed_function_param_state(
    state: &mut LocalTypeState,
    param_slots: &[LocalSlot],
    observed: Option<&[BoundType]>,
) {
    for (param_index, slot) in param_slots.iter().enumerate() {
        let ty = observed
            .and_then(|types| types.get(param_index))
            .copied()
            .unwrap_or(BoundType::Unknown);
        state.set(*slot, ty);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_function_types(
    function_index: u16,
    function_impl: &FunctionImpl,
    local_types: &mut [ValueType],
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
    for (param_index, slot) in function_impl.param_slots.iter().enumerate() {
        let ty = observed_function_param_slice(observed_function_param_types, function_index)
            .and_then(|types| types.get(param_index))
            .copied()
            .unwrap_or(BoundType::Unknown);
        record_local_type(local_types, *slot, ty);
        state.set(*slot, ty);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let ty = state.get(*source_slot);
        record_local_type(local_types, *captured_slot, ty);
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    collect_stmt_types(
        &function_impl.body_stmts,
        &mut state,
        local_types,
        &mut context,
    );
    let _ = context.infer_expr_type(&function_impl.body_expr, &state);
}

pub(super) fn collect_stmt_types(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    local_types: &mut [ValueType],
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
                let _ = context.infer_expr_type(&closure.body, state);
            }
            Stmt::FuncDecl { index, .. } => {
                if let Some(function_impl) = context.function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        let ty = state.get(*source_slot);
                        record_local_type(local_types, *captured_slot, ty);
                        let source_state = state.clone();
                        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
                    }
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let expr_state = state.clone();
                let ty = context.infer_expr_type(expr, &expr_state);
                record_local_type(local_types, *index, ty);
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, .. } => {
                let _ = context.infer_expr_type(expr, state);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = context.infer_expr_type(condition, state);
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                collect_stmt_types(then_branch, &mut then_state, local_types, context);
                collect_stmt_types(else_branch, &mut else_state, local_types, context);
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                collect_stmt_types(std::slice::from_ref(init), state, local_types, context);
                stabilize_loop_state(state, |iterated| {
                    let _ = context.infer_expr_type(condition, iterated);
                    collect_stmt_types(body, iterated, local_types, context);
                    collect_stmt_types(std::slice::from_ref(post), iterated, local_types, context);
                });
            }
            Stmt::While {
                condition, body, ..
            } => {
                stabilize_loop_state(state, |iterated| {
                    let _ = context.infer_expr_type(condition, iterated);
                    collect_stmt_types(body, iterated, local_types, context);
                });
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
