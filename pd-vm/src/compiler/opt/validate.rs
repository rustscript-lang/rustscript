use super::*;

fn observe_direct_function_call_types(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    source_name: Option<&str>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let function_index = match expr {
        Expr::Call(index, _) if context.function_impls.contains_key(index) => Some(*index),
        Expr::LocalCall(slot, _) => match state.callable(*slot).cloned() {
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
        Expr::Call(_, args) | Expr::LocalCall(_, args) => args,
        _ => return Ok(()),
    };
    context.observe_function_arg_types(function_index, args, state);
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
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            validate_expr_children(
                expr,
                state,
                line_context,
                source_name,
                context,
                strict_function_add_types,
            )?;
            observe_direct_function_call_types(expr, state, line_context, source_name, context)?;
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
            if let Some(true) = eval_static_bool(condition) {
                validate_expr(
                    then_expr,
                    &then_state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?
            } else if let Some(false) = eval_static_bool(condition) {
                validate_expr(
                    else_expr,
                    &else_state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?
            } else {
                let then_ty = validate_expr(
                    then_expr,
                    &then_state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                let else_ty = validate_expr(
                    else_expr,
                    &else_state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                ensure_compatible_if_else_types(
                    line_context,
                    source_name,
                    "expression result",
                    then_ty,
                    else_ty,
                )?;
                if then_ty == else_ty {
                    then_ty
                } else {
                    BoundType::Unknown
                }
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
            bind_expr_result_to_slot(&mut nested, *value_slot, value, state, value_ty, context);
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = validate_expr(
                    arm_expr,
                    &nested,
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
            super::validate_stmts(
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
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                let _ = validate_expr(
                    arg,
                    state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
            }
        }
        Expr::Closure(closure) => {
            let _ = validate_expr(
                &closure.body,
                state,
                line_context,
                source_name,
                context,
                false,
            )?;
        }
        Expr::ClosureCall(closure, args) => {
            let _ = validate_expr(
                &closure.body,
                state,
                line_context,
                source_name,
                context,
                false,
            )?;
            for arg in args {
                let _ = validate_expr(
                    arg,
                    state,
                    line_context,
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn owned_source_name(source_name: Option<&str>) -> Option<String> {
    source_name.map(str::to_string)
}

fn refine_state_for_condition(
    state: &LocalTypeState,
    condition: &Expr,
    truthy: bool,
) -> LocalTypeState {
    let mut refined = state.clone();
    if truthy
        && let Some((slot, ty)) = extract_type_guard(condition)
    {
        refined.set(slot, ty);
    }
    refined
}

fn extract_type_guard(condition: &Expr) -> Option<(LocalSlot, BoundType)> {
    let Expr::Eq(lhs, rhs) = condition else {
        return None;
    };
    extract_type_guard_side(lhs, rhs).or_else(|| extract_type_guard_side(rhs, lhs))
}

fn extract_type_guard_side(lhs: &Expr, rhs: &Expr) -> Option<(LocalSlot, BoundType)> {
    let Expr::Call(index, args) = lhs else {
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
    for slot in lhs.by_slot.keys().chain(rhs.by_slot.keys()) {
        let left = lhs.get(*slot);
        let right = rhs.get(*slot);
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
