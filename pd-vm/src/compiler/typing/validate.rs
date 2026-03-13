use crate::builtins::{BuiltinFunction, CallableParam, CallableParamType, CallableSignature};

use super::super::CompileError;
use super::super::ir::{Expr, LocalSlot, MatchPattern};
use super::context::{TypeContext, infer_access_schema};
use super::helpers::{
    bind_expr_result_to_slot, bound_type_label, infer_binary_type, infer_unary_type,
    is_numeric_bound_type, refine_state_for_match_pattern, validate_stmts,
};
use super::state::{BoundType, InferredCallable, LocalTypeState, are_compatible_bound_types};

fn observe_direct_function_call_types(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let function_index = match expr {
        Expr::Call(index, _, _) if context.function_impls.contains_key(index) => Some(*index),
        Expr::LocalCall(slot, _, _) => match state.callable(*slot).cloned() {
            Some(InferredCallable::Function(index))
                if context.function_impls.contains_key(&index) =>
            {
                Some(index)
            }
            _ => None,
        },
        _ => None,
    };

    let Some(function_index) = function_index else {
        return Ok(());
    };

    let args = match expr {
        Expr::Call(_, _, args) | Expr::LocalCall(_, _, args) => args,
        _ => return Ok(()),
    };
    if context
        .function_decls
        .get(&function_index)
        .is_some_and(|decl| decl.type_params.is_empty())
    {
        context.observe_function_arg_types(function_index, args, state);
    }
    if let Some(detail) = context
        .function_param_conflicts
        .get(&function_index)
        .cloned()
    {
        return Err(CompileError::FunctionParameterTypeConflict {
            line: line_context,
            source_name: owned_source_name(source_name),
            detail,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_signature_overloads(
    callable_name: &str,
    callable_kind: &str,
    signatures: &[CallableSignature],
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    line_context: Option<u32>,
    source_name: Option<&str>,
) -> Result<(), CompileError> {
    let actual = args
        .iter()
        .map(|arg| context.infer_expr_type(arg, state))
        .collect::<Vec<_>>();
    if signatures
        .iter()
        .any(|signature| signature_matches_actual(signature, &actual))
    {
        return Ok(());
    }

    Err(CompileError::CallableArgumentTypeMismatch {
        line: line_context,
        source_name: owned_source_name(source_name),
        detail: format!(
            "{callable_kind} '{callable_name}' does not accept argument types ({}); expected {}",
            format_actual_arg_types(&actual),
            format_signature_overloads(callable_name, signatures),
        ),
    })
}

pub(super) fn validate_host_signature(
    callable_name: &str,
    params: &[CallableParam],
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    line_context: Option<u32>,
    source_name: Option<&str>,
) -> Result<(), CompileError> {
    let actual = args
        .iter()
        .map(|arg| context.infer_expr_type(arg, state))
        .collect::<Vec<_>>();
    if params_match_actual(params, &actual) {
        return Ok(());
    }

    Err(CompileError::CallableArgumentTypeMismatch {
        line: line_context,
        source_name: owned_source_name(source_name),
        detail: format!(
            "host function '{callable_name}' does not accept argument types ({}); expected {}({})",
            format_actual_arg_types(&actual),
            callable_name,
            format_param_types(params),
        ),
    })
}

fn signature_matches_actual(signature: &CallableSignature, actual: &[BoundType]) -> bool {
    params_match_actual(signature.params, actual)
}

fn params_match_actual(params: &[CallableParam], actual: &[BoundType]) -> bool {
    let required = params.iter().take_while(|param| !param.optional).count();
    if actual.len() < required || actual.len() > params.len() {
        return false;
    }
    params
        .iter()
        .take(actual.len())
        .zip(actual.iter().copied())
        .all(|(expected, actual)| param_accepts_bound_type(expected.ty, actual))
}

fn param_accepts_bound_type(expected: CallableParamType, actual: BoundType) -> bool {
    if actual == BoundType::Unknown {
        return true;
    }
    match expected {
        CallableParamType::Any => true,
        CallableParamType::Null => actual == BoundType::Null,
        CallableParamType::Int => actual == BoundType::Int,
        CallableParamType::Float => actual == BoundType::Float,
        CallableParamType::Bool => actual == BoundType::Bool,
        CallableParamType::String => actual == BoundType::String,
        CallableParamType::Array => {
            matches!(actual, BoundType::Array | BoundType::ArrayOf(_))
        }
        CallableParamType::Map => matches!(actual, BoundType::Map | BoundType::MapOf(_)),
        CallableParamType::Number => is_numeric_bound_type(actual),
    }
}

fn format_signature_overloads(name: &str, signatures: &[CallableSignature]) -> String {
    signatures
        .iter()
        .map(|signature| format!("{name}({})", format_param_types(signature.params)))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn format_param_types(params: &[CallableParam]) -> String {
    params
        .iter()
        .map(|param| {
            if param.optional {
                format!("{}?: {}", param.name, param.ty.label())
            } else {
                format!("{}: {}", param.name, param.ty.label())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_actual_arg_types(actual: &[BoundType]) -> String {
    actual
        .iter()
        .copied()
        .map(bound_type_label)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn validate_expr(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
    strict_function_add_types: bool,
) -> Result<BoundType, CompileError> {
    Ok(match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::OptionalGet { container, key, .. } => {
            let _ = validate_expr(
                container,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            let _ = validate_expr(
                key,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            ensure_expr_not_optional(
                key,
                state,
                line_context,
                source_name,
                context,
                "optional access key",
            )?;
            validate_optional_get_access(expr, state, line_context, source_name, context)?;
            context.infer_expr_type(expr, state)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            let _ = validate_expr(
                value,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            let fallback_ty = validate_expr(
                fallback,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            if !context.expr_is_optional(value, state) {
                return Err(optional_usage_error(
                    line_context,
                    source_name,
                    "unwrap_or() requires an optional value",
                ));
            }
            ensure_expr_not_optional(
                fallback,
                state,
                line_context,
                source_name,
                context,
                "unwrap_or() fallback",
            )?;
            let inner_ty = context.infer_optional_expr_inner_type(value, state);
            ensure_compatible_if_else_types(
                line_context,
                source_name,
                "unwrap_or result",
                inner_ty,
                fallback_ty,
            )?;
            context.infer_expr_type(expr, state)
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => validate_expr(
            inner,
            state,
            line_context,
            source_name,
            context,
            strict_function_add_types,
        )?,
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::FunctionRef(_) | Expr::Call(..) | Expr::LocalCall(..) | Expr::Closure(_) => {
            validate_expr_children(
                expr,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            observe_direct_function_call_types(expr, state, line_context, source_name, context)?;
            validate_schema_access(expr, state, line_context, source_name, context)?;
            context.validate_call_argument_types(expr, state, line_context, source_name)?;
            context.infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => {
            validate_expr_children(
                expr,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            validate_schema_access(expr, state, line_context, source_name, context)?;
            context.validate_call_argument_types(expr, state, line_context, source_name)?;
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
            let lhs_ty = validate_expr(
                lhs,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            let rhs_ty = validate_expr(
                rhs,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            if context.expr_is_optional(lhs, state) || context.expr_is_optional(rhs, state) {
                if matches!(expr, Expr::Eq(_, _)) && comparison_accepts_optional(lhs, rhs) {
                    return Ok(BoundType::Bool);
                } else {
                    return Err(optional_usage_error(
                        line_context,
                        source_name,
                        "binary operation",
                    ));
                }
            }
            let inferred = infer_binary_type(expr, lhs_ty, rhs_ty);
            if strict_function_add_types
                && matches!(expr, Expr::Add(_, _))
                && inferred == BoundType::Unknown
            {
                return Err(CompileError::BinaryOperandTypeMismatch {
                    line: line_context,
                    source_name: owned_source_name(source_name),
                    detail: format!(
                        "cannot infer '+' operand types in function body: {} vs {}",
                        bound_type_label(lhs_ty),
                        bound_type_label(rhs_ty)
                    ),
                });
            }
            inferred
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = validate_expr(
                inner,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            if context.expr_is_optional(inner, state) {
                return Err(optional_usage_error(
                    line_context,
                    source_name,
                    "unary operation",
                ));
            }
            infer_unary_type(expr, inner_ty)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = validate_expr(
                condition,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            let then_state = refine_state_for_condition(state, condition, true);
            let else_state = refine_state_for_condition(state, condition, false);
            let static_condition = eval_static_bool(condition);
            let (then_ty, else_ty) = match static_condition {
                Some(true) => (
                    validate_expr(
                        then_expr,
                        &then_state,
                        line_context,
                        source_name,
                        context,
                        strict_function_add_types,
                    )?,
                    context.infer_expr_type(else_expr, &else_state),
                ),
                Some(false) => (
                    context.infer_expr_type(then_expr, &then_state),
                    validate_expr(
                        else_expr,
                        &else_state,
                        line_context,
                        source_name,
                        context,
                        strict_function_add_types,
                    )?,
                ),
                None => (
                    validate_expr(
                        then_expr,
                        &then_state,
                        line_context,
                        source_name,
                        context,
                        strict_function_add_types,
                    )?,
                    validate_expr(
                        else_expr,
                        &else_state,
                        line_context,
                        source_name,
                        context,
                        strict_function_add_types,
                    )?,
                ),
            };
            ensure_compatible_if_else_types(
                line_context,
                source_name,
                "expression result",
                then_ty,
                else_ty,
            )?;
            if then_ty == else_ty || matches!(static_condition, Some(true)) {
                then_ty
            } else if matches!(static_condition, Some(false)) {
                else_ty
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
            let value_ty = validate_expr(
                value,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
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
            let mut arm_type = BoundType::Unknown;
            for (pattern, arm_expr) in arms {
                validate_match_pattern(pattern, *value_slot, &nested, line_context, source_name)?;
                let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                let ty = validate_expr(
                    arm_expr,
                    &arm_state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = validate_expr(
                default,
                &nested,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
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
            validate_stmts(
                stmts,
                &mut nested,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            validate_expr(
                expr,
                &nested,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?
        }
    })
}

fn validate_expr_children(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
    strict_function_add_types: bool,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(_, _, args) | Expr::LocalCall(_, _, args) => {
            for arg in args {
                let _ = validate_expr(
                    arg,
                    state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                ensure_expr_not_optional(
                    arg,
                    state,
                    line_context,
                    source_name,
                    context,
                    "call argument",
                )?;
            }
            if let Expr::LocalCall(slot, _, args) = expr
                && let Some(InferredCallable::Closure(closure)) = state.callable(*slot).cloned()
            {
                validate_callable_body(
                    closure.param_slots.as_slice(),
                    None,
                    closure.capture_copies.as_slice(),
                    &[],
                    &closure.body,
                    Some(args.as_slice()),
                    state,
                    line_context,
                    source_name,
                    context,
                )?;
            }
        }
        Expr::Closure(closure) => {
            validate_callable_body(
                closure.param_slots.as_slice(),
                None,
                closure.capture_copies.as_slice(),
                &[],
                &closure.body,
                None,
                state,
                line_context,
                source_name,
                context,
            )?;
        }
        Expr::ClosureCall(closure, args) => {
            for arg in args {
                let _ = validate_expr(
                    arg,
                    state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                ensure_expr_not_optional(
                    arg,
                    state,
                    line_context,
                    source_name,
                    context,
                    "call argument",
                )?;
            }
            validate_callable_body(
                closure.param_slots.as_slice(),
                None,
                closure.capture_copies.as_slice(),
                &[],
                &closure.body,
                Some(args.as_slice()),
                state,
                line_context,
                source_name,
                context,
            )?;
        }
        _ => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_callable_body(
    param_slots: &[LocalSlot],
    param_schemas: Option<&[Option<super::super::ir::TypeSchema>]>,
    capture_copies: &[(LocalSlot, LocalSlot)],
    body_stmts: &[super::super::ir::Stmt],
    body_expr: &Expr,
    args: Option<&[Expr]>,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let Some(mut nested) =
        context.build_callable_state(param_slots, param_schemas, capture_copies, args, state)
    else {
        return Ok(());
    };
    validate_stmts(
        body_stmts,
        &mut nested,
        line_context,
        source_name,
        context,
        false,
    )?;
    let _ = validate_expr(
        body_expr,
        &nested,
        line_context,
        source_name,
        context,
        false,
    )?;
    Ok(())
}

fn validate_schema_access(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let Expr::Call(index, _, args) = expr else {
        return Ok(());
    };
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Get) || args.len() != 2 {
        return Ok(());
    }
    if context.expr_is_optional(&args[0], state) {
        return Err(optional_usage_error(
            line_context,
            source_name,
            "member/index access",
        ));
    }
    let Some(container_schema) = context.infer_expr_schema(&args[0], state) else {
        return Ok(());
    };
    if !context.expr_has_struct_schema_source(&args[0], state) {
        return Ok(());
    }
    infer_access_schema(&container_schema, &args[1], context, state)
        .map(|_| ())
        .map_err(|detail| CompileError::InvalidFieldAccess {
            line: line_context,
            source_name: owned_source_name(source_name),
            detail,
        })
}

fn validate_optional_get_access(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let Expr::OptionalGet { container, key, .. } = expr else {
        return Ok(());
    };
    if context.require_declared_schema_for_optional_access
        && !context.expr_has_declared_schema(container, state)
    {
        return Err(CompileError::InvalidFieldAccess {
            line: line_context,
            source_name: owned_source_name(source_name),
            detail: "optional access requires a user-declared schema in RustScript".to_string(),
        });
    }
    if !context.expr_has_declared_schema(container, state) {
        return Ok(());
    }
    let Some(container_schema) = context.infer_optional_expr_inner_schema(container, state) else {
        return Ok(());
    };
    infer_access_schema(&container_schema, key, context, state)
        .map(|_| ())
        .map_err(|detail| CompileError::InvalidFieldAccess {
            line: line_context,
            source_name: owned_source_name(source_name),
            detail,
        })
}

fn optional_usage_error(
    line: Option<u32>,
    source_name: Option<&str>,
    context: &str,
) -> CompileError {
    CompileError::InvalidFieldAccess {
        line,
        source_name: owned_source_name(source_name),
        detail: format!("optional value must be unwrapped before {context}"),
    }
}

fn ensure_expr_not_optional(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
    usage: &str,
) -> Result<(), CompileError> {
    if context.expr_is_optional(expr, state) {
        return Err(optional_usage_error(line_context, source_name, usage));
    }
    Ok(())
}

fn comparison_accepts_optional(lhs: &Expr, rhs: &Expr) -> bool {
    matches!(lhs, Expr::Null) || matches!(rhs, Expr::Null)
}

fn validate_match_pattern(
    pattern: &MatchPattern,
    value_slot: LocalSlot,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
) -> Result<(), CompileError> {
    if pattern.requires_optional_value() && !state.is_optional(value_slot) {
        return Err(CompileError::InvalidFieldAccess {
            line: line_context,
            source_name: owned_source_name(source_name),
            detail: "Some(...) and None match patterns require an optional value".to_string(),
        });
    }
    Ok(())
}

pub(super) fn owned_source_name(source_name: Option<&str>) -> Option<String> {
    source_name.map(str::to_string)
}

pub(super) fn refine_state_for_condition(
    state: &LocalTypeState,
    condition: &Expr,
    truthy: bool,
) -> LocalTypeState {
    let mut refined = state.clone();
    if truthy && let Some((slot, ty)) = extract_type_guard(condition) {
        refined.set_with_optional_schema_origin(
            slot,
            ty,
            state.schema(slot).cloned(),
            state.has_declared_schema(slot),
            false,
        );
    }
    if truthy && let Some(slot) = extract_non_null_guard(condition) {
        refined.set_with_optional_schema_origin(
            slot,
            state.get(slot),
            state.schema(slot).cloned(),
            state.has_declared_schema(slot),
            false,
        );
    }
    refined
}

fn extract_type_guard(condition: &Expr) -> Option<(LocalSlot, BoundType)> {
    let Expr::Eq(lhs, rhs) = condition else {
        return None;
    };
    extract_type_guard_side(lhs, rhs).or_else(|| extract_type_guard_side(rhs, lhs))
}

fn extract_non_null_guard(condition: &Expr) -> Option<LocalSlot> {
    let Expr::Not(inner) = condition else {
        return None;
    };
    let Expr::Eq(lhs, rhs) = inner.as_ref() else {
        return None;
    };
    match (lhs.as_ref(), rhs.as_ref()) {
        (Expr::Var(slot), Expr::Null) | (Expr::Null, Expr::Var(slot)) => Some(*slot),
        _ => None,
    }
}

fn extract_type_guard_side(lhs: &Expr, rhs: &Expr) -> Option<(LocalSlot, BoundType)> {
    let Expr::Call(index, _, args) = lhs else {
        return None;
    };
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::TypeOf) || args.len() != 1
    {
        return None;
    }
    let Expr::Var(slot) = args[0] else {
        return None;
    };
    let Expr::String(type_name) = rhs else {
        return None;
    };
    bound_type_from_type_name(type_name).map(|ty| (slot, ty))
}

fn bound_type_from_type_name(type_name: &str) -> Option<BoundType> {
    match type_name {
        "null" => Some(BoundType::Null),
        "int" => Some(BoundType::Int),
        "float" => Some(BoundType::Float),
        "bool" => Some(BoundType::Bool),
        "string" => Some(BoundType::String),
        "array" => Some(BoundType::Array),
        "map" => Some(BoundType::Map),
        _ => None,
    }
}

fn eval_static_bool(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Bool(value) => Some(*value),
        Expr::Eq(lhs, rhs) => match (lhs.as_ref(), rhs.as_ref()) {
            (Expr::Null, Expr::Null) => Some(true),
            (Expr::Bool(lhs), Expr::Bool(rhs)) => Some(lhs == rhs),
            (Expr::Int(lhs), Expr::Int(rhs)) => Some(lhs == rhs),
            (Expr::Float(lhs), Expr::Float(rhs)) => Some(lhs == rhs),
            (Expr::String(lhs), Expr::String(rhs)) => Some(lhs == rhs),
            _ => None,
        },
        _ => None,
    }
}

fn ensure_compatible_if_else_types(
    line: Option<u32>,
    source_name: Option<&str>,
    context: &str,
    lhs: BoundType,
    rhs: BoundType,
) -> Result<(), CompileError> {
    if are_compatible_bound_types(lhs, rhs) {
        return Ok(());
    }
    Err(CompileError::IfElseBranchTypeMismatch {
        line,
        source_name: owned_source_name(source_name),
        detail: format!(
            "if/else branches produced incompatible {context}: {} vs {}",
            bound_type_label(lhs),
            bound_type_label(rhs)
        ),
    })
}

pub(super) fn validate_branch_state_merge(
    line: Option<u32>,
    source_name: Option<&str>,
    lhs: &LocalTypeState,
    rhs: &LocalTypeState,
) -> Result<(), CompileError> {
    for slot in lhs.iter_slots().chain(rhs.iter_slots()) {
        let left = lhs.get(slot);
        let right = rhs.get(slot);
        if are_compatible_bound_types(left, right) {
            continue;
        }
        return Err(CompileError::IfElseBranchTypeMismatch {
            line,
            source_name: owned_source_name(source_name),
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
