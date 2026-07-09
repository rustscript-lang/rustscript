use crate::builtins::{BuiltinFunction, CallableParam, CallableParamType, CallableSignature};

use super::super::CompileError;
use super::super::ir::{Expr, LocalSlot, MatchPattern, TypeSchema};
use super::context::{TypeContext, infer_access_schema, render_schema_label};
use super::helpers::{
    bind_expr_result_to_slot, bound_type_label, find_declared_schema_mismatch, infer_binary_type,
    infer_unary_type, is_numeric_bound_type, refine_state_for_match_pattern, validate_stmts,
};
use super::state::{
    BoundType, InferredCallable, LocalTypeState, are_compatible_bound_types, merge_bound_types,
};

#[derive(Clone, Copy)]
pub(super) struct DiagnosticSite<'a> {
    pub(super) line: Option<u32>,
    pub(super) source_name: Option<&'a str>,
}

struct CallableBody<'a> {
    param_slots: &'a [LocalSlot],
    param_schemas: Option<&'a [Option<TypeSchema>]>,
    result_schema: Option<&'a TypeSchema>,
    capture_copies: &'a [(LocalSlot, LocalSlot)],
    body_stmts: &'a [super::super::ir::Stmt],
    body_expr: &'a Expr,
    args: Option<&'a [Expr]>,
}

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

pub(super) fn validate_signature_overloads(
    callable_name: &str,
    callable_kind: &str,
    signatures: &[CallableSignature],
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    site: DiagnosticSite<'_>,
) -> Result<(), CompileError> {
    let actual = args
        .iter()
        .map(|arg| context.infer_expr_type(arg, state))
        .collect::<Vec<_>>();
    if signatures
        .iter()
        .any(|signature| signature_matches_actual(signature, &actual, context.is_strict()))
    {
        return Ok(());
    }

    Err(CompileError::CallableArgumentTypeMismatch {
        line: site.line,
        source_name: owned_source_name(site.source_name),
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
    if params_match_actual(params, &actual, context.is_strict()) {
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

fn callable_argument_mismatch(
    site: DiagnosticSite<'_>,
    detail: String,
) -> Result<(), CompileError> {
    Err(CompileError::CallableArgumentTypeMismatch {
        line: site.line,
        source_name: owned_source_name(site.source_name),
        detail,
    })
}

fn bound_type_matches_schema(
    expected: &TypeSchema,
    actual: BoundType,
    context: &mut TypeContext<'_>,
) -> bool {
    let resolved = context.resolve_schema(expected);
    let (expected, expected_optional) = resolved.split_optional();
    if expected_optional && actual == BoundType::Null {
        return true;
    }
    let expected = context.bound_type_for_schema(&expected);
    match expected {
        BoundType::Unknown => false,
        BoundType::Number => is_numeric_bound_type(actual),
        BoundType::Array => matches!(actual, BoundType::Array | BoundType::ArrayOf(_)),
        BoundType::Map => matches!(actual, BoundType::Map | BoundType::MapOf(_)),
        _ => actual == expected,
    }
}

fn actual_expr_schema(
    expr: &Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> Option<TypeSchema> {
    context.infer_callable_expr_schema(expr, state)
}

fn validate_expr_matches_schema(
    label: &str,
    expected_schema: &TypeSchema,
    expr: &Expr,
    state: &LocalTypeState,
    site: DiagnosticSite<'_>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let expected_schema = context.resolve_schema(expected_schema);
    let (expected_schema, expected_optional) = expected_schema.split_optional();
    let actual_optional = context.expr_is_optional(expr, state);
    if actual_optional && !expected_optional {
        return callable_argument_mismatch(
            site,
            format!("{label} is optional; unwrap or refine it before use"),
        );
    }
    let actual_ty = if actual_optional {
        context.infer_optional_expr_inner_type(expr, state)
    } else {
        context.infer_expr_type(expr, state)
    };
    let actual_schema = if actual_optional {
        context.infer_optional_expr_inner_schema(expr, state)
    } else {
        actual_expr_schema(expr, state, context)
    };
    if let Some(actual_schema) = actual_schema.as_ref() {
        if let Some(detail) =
            find_declared_schema_mismatch(&expected_schema, actual_schema, context, String::new())
        {
            return callable_argument_mismatch(site, format!("{label} type mismatch: {detail}"));
        }
        return Ok(());
    }
    if bound_type_matches_schema(&expected_schema, actual_ty, context) {
        return Ok(());
    }
    callable_argument_mismatch(
        site,
        format!(
            "{label} expects '{}' but got {}",
            render_schema_label(&expected_schema),
            bound_type_label(actual_ty)
        ),
    )
}

fn validate_callable_expr_against_schema(
    label: &str,
    expected_schema: &TypeSchema,
    expr: &Expr,
    state: &LocalTypeState,
    site: DiagnosticSite<'_>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let expected_schema = context.resolve_schema(expected_schema);
    let TypeSchema::Callable { params, result } = &expected_schema else {
        return validate_expr_matches_schema(label, &expected_schema, expr, state, site, context);
    };

    if let Expr::Closure(closure) = expr {
        if closure.param_slots.len() != params.len() {
            return callable_argument_mismatch(
                site,
                format!(
                    "{label} expects '{}' but the provided closure takes {} parameters",
                    render_schema_label(&expected_schema),
                    closure.param_slots.len()
                ),
            );
        }
        let param_schemas = params.iter().cloned().map(Some).collect::<Vec<_>>();
        return validate_callable_body(
            CallableBody {
                param_slots: closure.param_slots.as_slice(),
                param_schemas: Some(param_schemas.as_slice()),
                result_schema: Some(result.as_ref()),
                capture_copies: closure.capture_copies.as_slice(),
                body_stmts: &[],
                body_expr: &closure.body,
                args: None,
            },
            state,
            site,
            context,
        );
    }

    validate_expr_matches_schema(label, &expected_schema, expr, state, site, context)
}

pub(super) fn validate_function_argument_schemas(
    callable_name: &str,
    callable_kind: &str,
    param_names: &[String],
    param_schemas: &[Option<TypeSchema>],
    args: &[Expr],
    state: &LocalTypeState,
    site: DiagnosticSite<'_>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    for (index, arg) in args.iter().enumerate() {
        let Some(expected_schema) = param_schemas.get(index).and_then(|schema| schema.as_ref())
        else {
            continue;
        };
        let param_name = param_names.get(index).map(String::as_str).unwrap_or("arg");
        let label = format!("{callable_kind} '{callable_name}' argument '{param_name}'");
        validate_callable_expr_against_schema(&label, expected_schema, arg, state, site, context)?;
    }
    Ok(())
}

fn validate_json_schema(
    schema: &TypeSchema,
    context: &mut TypeContext<'_>,
    path: &str,
) -> Result<(), String> {
    match context.resolve_schema(schema) {
        TypeSchema::Unknown => Err(format!("{path} has unknown schema")),
        TypeSchema::Null
        | TypeSchema::Int
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::Bool
        | TypeSchema::String => Ok(()),
        TypeSchema::Bytes => Err(format!(
            "{path} uses bytes, which json::encode does not support"
        )),
        TypeSchema::Optional(inner) => validate_json_schema(&inner, context, path),
        TypeSchema::GenericParam(name) => Err(format!(
            "{path} depends on generic schema parameter '{name}', which is not concrete enough for json::encode"
        )),
        TypeSchema::Callable { .. } => Err(format!(
            "{path} is callable, which json::encode does not support"
        )),
        TypeSchema::Named(_, _) | TypeSchema::Object(_) => match context.resolve_schema(schema) {
            TypeSchema::Object(fields) => {
                for (field, value_schema) in &fields {
                    let child_path = if path.is_empty() {
                        format!("field '{field}'")
                    } else {
                        format!("{path}.{field}")
                    };
                    validate_json_schema(value_schema, context, child_path.as_str())?;
                }
                Ok(())
            }
            other => validate_json_schema(&other, context, path),
        },
        TypeSchema::Array(element) => validate_json_schema(&element, context, path),
        TypeSchema::ArrayTuple(items) => {
            for (index, item) in items.iter().enumerate() {
                validate_json_schema(item, context, format!("{path}[{index}]").as_str())?;
            }
            Ok(())
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            for (index, item) in prefix.iter().enumerate() {
                validate_json_schema(item, context, format!("{path}[{index}]").as_str())?;
            }
            validate_json_schema(&rest, context, path)
        }
        TypeSchema::Map(_) => Err(format!(
            "{path} is a generic map; json::encode in RustScript requires object/struct-shaped data so keys are provably strings"
        )),
    }
}

pub(super) fn validate_json_encode_argument(
    arg: &Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    site: DiagnosticSite<'_>,
) -> Result<(), CompileError> {
    if context.expr_is_optional(arg, state) {
        return callable_argument_mismatch(
            site,
            "builtin 'json::encode' does not accept optional values; unwrap or refine the value first"
                .to_string(),
        );
    }
    if let Some(schema) = actual_expr_schema(arg, state, context) {
        return validate_json_schema(&schema, context, "value").map_err(|detail| {
            CompileError::CallableArgumentTypeMismatch {
                line: site.line,
                source_name: owned_source_name(site.source_name),
                detail: format!("builtin 'json::encode' cannot encode this value: {detail}"),
            }
        });
    }
    match context.infer_expr_type(arg, state) {
        BoundType::Null
        | BoundType::Int
        | BoundType::Float
        | BoundType::Number
        | BoundType::Bool
        | BoundType::String => Ok(()),
        BoundType::Bytes => callable_argument_mismatch(
            site,
            "builtin 'json::encode' does not support bytes values".to_string(),
        ),
        _ => callable_argument_mismatch(
            site,
            "builtin 'json::encode' requires a concrete JSON-encodable schema for arrays, maps, and higher-order values"
                .to_string(),
        ),
    }
}

fn signature_matches_actual(
    signature: &CallableSignature,
    actual: &[BoundType],
    strict: bool,
) -> bool {
    params_match_actual(signature.params, actual, strict)
}

fn params_match_actual(params: &[CallableParam], actual: &[BoundType], strict: bool) -> bool {
    let required = params.iter().take_while(|param| !param.optional).count();
    if actual.len() < required || actual.len() > params.len() {
        return false;
    }
    params
        .iter()
        .take(actual.len())
        .zip(actual.iter().copied())
        .all(|(expected, actual)| param_accepts_bound_type(expected.ty, actual, strict))
}

fn param_accepts_bound_type(expected: CallableParamType, actual: BoundType, strict: bool) -> bool {
    if actual == BoundType::Unknown && !strict {
        return true;
    }
    match expected {
        CallableParamType::Any => true,
        CallableParamType::Null => actual == BoundType::Null,
        CallableParamType::Int => actual == BoundType::Int,
        CallableParamType::Float => actual == BoundType::Float,
        CallableParamType::Bool => actual == BoundType::Bool,
        CallableParamType::String => actual == BoundType::String,
        CallableParamType::Bytes => actual == BoundType::Bytes,
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
        Expr::Bytes(_) => BoundType::Bytes,
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
                context.is_strict(),
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
            validate_callable_value_usage(expr, state, context)?;
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
                context.is_strict(),
            )?;
            if then_ty == else_ty || matches!(static_condition, Some(true)) {
                then_ty
            } else if matches!(static_condition, Some(false)) {
                else_ty
            } else {
                merge_bound_types(then_ty, else_ty)
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
            let mut arm_type = None;
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
                arm_type = Some(match arm_type {
                    None => ty,
                    Some(current) => {
                        ensure_compatible_if_else_types(
                            line_context,
                            source_name,
                            "match arm result",
                            current,
                            ty,
                            context.is_strict(),
                        )?;
                        merge_bound_types(current, ty)
                    }
                });
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
            } else {
                let arm_type = arm_type.expect("non-empty match should infer an arm type");
                ensure_compatible_if_else_types(
                    line_context,
                    source_name,
                    "match result",
                    arm_type,
                    default_ty,
                    context.is_strict(),
                )?;
                merge_bound_types(arm_type, default_ty)
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

fn validate_callable_value_usage(
    expr: &Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let Expr::Call(index, _, args) = expr else {
        return Ok(());
    };
    let Some(builtin) = BuiltinFunction::from_call_index(*index) else {
        return Ok(());
    };
    let value_arg = match builtin {
        BuiltinFunction::ArrayPush if args.len() == 2 => args.get(1),
        BuiltinFunction::Set if args.len() == 3 => args.get(2),
        _ => None,
    };
    if value_arg
        .and_then(|value| context.callable_binding_from_expr(value, state))
        .is_some()
    {
        return Err(CompileError::CallableUsedAsValue);
    }
    Ok(())
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
            }
            if let Expr::LocalCall(slot, _, args) = expr
                && let Some(InferredCallable::Closure(closure)) = state.callable(*slot).cloned()
            {
                let declared_callable = state.callable_schema(*slot).cloned();
                let declared_param_schemas = declared_callable.as_ref().and_then(|schema| {
                    let TypeSchema::Callable { params, .. } = schema else {
                        return None;
                    };
                    Some(params.iter().cloned().map(Some).collect::<Vec<_>>())
                });
                let declared_result_schema = declared_callable.as_ref().and_then(|schema| {
                    let TypeSchema::Callable { result, .. } = schema else {
                        return None;
                    };
                    Some(result.as_ref())
                });
                validate_callable_body(
                    CallableBody {
                        param_slots: closure.param_slots.as_slice(),
                        param_schemas: declared_param_schemas.as_deref(),
                        result_schema: declared_result_schema,
                        capture_copies: closure.capture_copies.as_slice(),
                        body_stmts: &[],
                        body_expr: &closure.body,
                        args: Some(args.as_slice()),
                    },
                    state,
                    DiagnosticSite {
                        line: line_context,
                        source_name,
                    },
                    context,
                )?;
            }
        }
        Expr::Closure(closure) => {
            if closure.param_slots.is_empty() {
                validate_callable_body(
                    CallableBody {
                        param_slots: closure.param_slots.as_slice(),
                        param_schemas: None,
                        result_schema: None,
                        capture_copies: closure.capture_copies.as_slice(),
                        body_stmts: &[],
                        body_expr: &closure.body,
                        args: None,
                    },
                    state,
                    DiagnosticSite {
                        line: line_context,
                        source_name,
                    },
                    context,
                )?;
            }
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
            }
            validate_callable_body(
                CallableBody {
                    param_slots: closure.param_slots.as_slice(),
                    param_schemas: None,
                    result_schema: None,
                    capture_copies: closure.capture_copies.as_slice(),
                    body_stmts: &[],
                    body_expr: &closure.body,
                    args: Some(args.as_slice()),
                },
                state,
                DiagnosticSite {
                    line: line_context,
                    source_name,
                },
                context,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn validate_callable_body(
    callable: CallableBody<'_>,
    state: &LocalTypeState,
    site: DiagnosticSite<'_>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let CallableBody {
        param_slots,
        param_schemas,
        result_schema,
        capture_copies,
        body_stmts,
        body_expr,
        args,
    } = callable;
    let Some(mut nested) =
        context.build_callable_state(param_slots, param_schemas, capture_copies, args, state)
    else {
        return Ok(());
    };
    validate_stmts(
        body_stmts,
        &mut nested,
        site.line,
        site.source_name,
        context,
        false,
    )?;
    let _ = validate_expr(
        body_expr,
        &nested,
        site.line,
        site.source_name,
        context,
        false,
    )?;
    if let Some(result_schema) = result_schema {
        validate_expr_matches_schema(
            "callable body result",
            result_schema,
            body_expr,
            &nested,
            site,
            context,
        )?;
    }
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
    if context.is_strict() && !context.expr_has_declared_schema(container, state) {
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
        "bytes" => Some(BoundType::Bytes),
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
    strict: bool,
) -> Result<(), CompileError> {
    if are_compatible_bound_types_in_mode(lhs, rhs, strict) {
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
    entry: &LocalTypeState,
    lhs: &LocalTypeState,
    rhs: &LocalTypeState,
    strict: bool,
) -> Result<(), CompileError> {
    for slot in lhs.iter_slots().chain(rhs.iter_slots()) {
        let left_present = lhs.has_binding(slot);
        let right_present = rhs.has_binding(slot);
        if left_present != right_present && !entry.has_binding(slot) {
            continue;
        }
        let left = lhs.get(slot);
        let right = rhs.get(slot);
        if are_compatible_bound_types_in_mode(left, right, strict) {
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

fn are_compatible_bound_types_in_mode(lhs: BoundType, rhs: BoundType, strict: bool) -> bool {
    if strict
        && ((lhs == BoundType::Unknown && rhs != BoundType::Unknown)
            || (rhs == BoundType::Unknown && lhs != BoundType::Unknown))
    {
        return false;
    }
    are_compatible_bound_types(lhs, rhs)
}
