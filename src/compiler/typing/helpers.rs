use std::collections::{HashMap, HashSet};

use crate::builtins::{BuiltinFunction, CallableParam, CallableParamType};

use super::super::CompileError;
use super::super::TypingMode;
use super::super::ir::{
    AssignmentKind, Expr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern, Stmt, StructDecl,
    TypeSchema,
};
use super::collect::{
    observed_function_param_schema_slice, observed_function_param_slice,
    seed_function_capture_state, seed_function_param_state,
};
use super::context::{TypeContext, render_schema_label};
use super::infer_expr_type;
use super::state::{
    BoundType, HostCallableSignature, LocalTypeState, merge_bound_types,
    merge_container_element_types, stabilize_loop_state, try_stabilize_loop_state,
};
use super::validate::{
    DiagnosticSite, owned_source_name, refine_state_for_condition, validate_branch_state_merge,
    validate_expr,
};

pub(super) struct FunctionLegalizeEnv<'a> {
    pub(super) function_impls: &'a HashMap<u16, FunctionImpl>,
    pub(super) function_decls: &'a HashMap<u16, FunctionDecl>,
    pub(super) function_names: &'a HashMap<u16, String>,
    pub(super) struct_schemas: &'a HashMap<String, StructDecl>,
    pub(super) host_import_return_types: &'a HashMap<u16, BoundType>,
    pub(super) host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    pub(super) observed_function_param_types: &'a HashMap<u16, Vec<BoundType>>,
    pub(super) observed_function_param_schemas: &'a HashMap<u16, Vec<Option<TypeSchema>>>,
    pub(super) observed_function_param_callables:
        &'a HashMap<u16, Vec<Option<super::state::InferredCallable>>>,
    pub(super) observed_function_param_capture_states:
        &'a HashMap<u16, Vec<Option<LocalTypeState>>>,
    pub(super) observed_function_capture_states: &'a HashMap<u16, LocalTypeState>,
}

pub(super) fn legalize_function_impl(
    function_index: u16,
    function_impl: &mut FunctionImpl,
    env: &FunctionLegalizeEnv<'_>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        env.function_impls,
        env.function_decls,
        env.struct_schemas,
        env.function_names,
        env.host_import_return_types,
        env.host_import_signatures,
        TypingMode::DynamicHints,
    );
    seed_function_param_state(
        &mut state,
        &function_impl.param_slots,
        env.function_decls
            .get(&function_index)
            .map(|decl| decl.arg_schemas.as_slice()),
        observed_function_param_slice(env.observed_function_param_types, function_index),
        observed_function_param_schema_slice(env.observed_function_param_schemas, function_index),
        env.observed_function_param_callables
            .get(&function_index)
            .map(Vec::as_slice),
        env.observed_function_param_capture_states
            .get(&function_index)
            .map(Vec::as_slice),
    );
    seed_function_capture_state(
        &mut state,
        function_index,
        &function_impl.capture_copies,
        env.observed_function_capture_states,
    );
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
        context
            .function_decls
            .get(&function_index)
            .map(|decl| decl.arg_schemas.as_slice()),
        observed_function_param_slice(&context.observed_function_param_types, function_index),
        observed_function_param_schema_slice(
            &context.observed_function_param_schemas,
            function_index,
        ),
        context
            .observed_function_param_callables
            .get(&function_index)
            .map(Vec::as_slice),
        context
            .observed_function_param_capture_states
            .get(&function_index)
            .map(Vec::as_slice),
    );
    seed_function_capture_state(
        &mut state,
        function_index,
        &function_impl.capture_copies,
        &context.observed_function_capture_states,
    );
    let restore_typing_mode = context.typing_mode;
    if context
        .function_decls
        .get(&function_index)
        .is_some_and(|decl| !decl.type_params.is_empty())
    {
        context.typing_mode = TypingMode::DynamicHints;
    }
    let body_validation = (|| -> Result<BoundType, CompileError> {
        validate_stmts(
            &function_impl.body_stmts,
            &mut state,
            None,
            source_name,
            context,
            strict_function_add_types,
        )?;
        validate_expr(
            &function_impl.body_expr,
            &state,
            Some(function_impl.body_expr_line),
            source_name,
            context,
            strict_function_add_types,
        )
    })();
    context.typing_mode = restore_typing_mode;
    let body_ty = body_validation?;
    let observed_body_ty = if body_ty == BoundType::Unknown {
        context.infer_observed_function_return(function_index, &[])
    } else {
        body_ty
    };
    let actual_schema = inferred_expr_assignment_schema(&function_impl.body_expr, &state, context)
        .or_else(|| context.infer_observed_function_return_schema(function_index, &[]));
    let actual_optional = context.expr_is_optional(&function_impl.body_expr, &state);
    let declared_return_schema = context
        .function_decls
        .get(&function_index)
        .and_then(|decl| decl.return_schema.as_ref());
    let function_name = context.function_name(function_index).to_string();
    if let Some(schema) = declared_return_schema {
        validate_declared_return_schema(
            &function_name,
            schema,
            observed_body_ty,
            actual_optional,
            actual_schema.as_ref(),
            context,
            Some(function_impl.body_expr_line),
            source_name,
        )?;
    } else if context
        .callable_binding_from_expr(&function_impl.body_expr, &state)
        .is_some()
    {
        return Err(CompileError::CallableUsedAsValue);
    } else if context.is_strict()
        && observed_body_ty == BoundType::Unknown
        && actual_schema.is_none()
    {
        return Err(CompileError::StrictTypingRequired {
            line: Some(function_impl.body_expr_line),
            source_name: owned_source_name(source_name),
            detail: format!(
                "function '{}' return type cannot be inferred; add a return schema or make the body type-stable",
                function_name
            ),
        });
    }
    Ok(())
}

pub(super) fn legalize_stmts(
    stmts: &mut [Stmt],
    state: &mut LocalTypeState,
    context: &mut TypeContext<'_>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                if *has_impl && let Some(function_impl) = context.function_impls.get(index) {
                    context.observe_function_capture_state(
                        *index,
                        &function_impl.capture_copies,
                        state,
                    );
                }
            }
            Stmt::Drop { index, .. } => {
                state.set(*index, BoundType::Null);
            }
            Stmt::ClosureLet { closure, .. } => {
                let _ = legalize_expr(&mut closure.body, state, context);
            }
            Stmt::Let {
                index,
                declared_schema,
                expr,
                ..
            } => {
                let expr_state = state.clone();
                let ty = legalize_expr(expr, &expr_state, context);
                bind_expr_result_to_slot(
                    state,
                    *index,
                    declared_schema.as_ref(),
                    expr,
                    &expr_state,
                    ty,
                    context,
                );
            }
            Stmt::Assign { index, expr, .. } => {
                let expr_state = state.clone();
                let ty = legalize_expr(expr, &expr_state, context);
                bind_expr_result_to_slot(state, *index, None, expr, &expr_state, ty, context);
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
                let mut stabilized_state = state.clone();
                stabilize_loop_state(&mut stabilized_state, |iterated| {
                    let mut condition_probe = condition.clone();
                    let mut body_probe = body.clone();
                    let mut post_probe = post.as_ref().clone();
                    let _ = legalize_expr(&mut condition_probe, iterated, context);
                    legalize_stmts(&mut body_probe, iterated, context);
                    legalize_stmts(std::slice::from_mut(&mut post_probe), iterated, context);
                });
                let mut loop_state = stabilized_state.clone();
                let _ = legalize_expr(condition, &loop_state, context);
                legalize_stmts(body, &mut loop_state, context);
                legalize_stmts(std::slice::from_mut(post), &mut loop_state, context);
                *state = stabilized_state;
            }
            Stmt::While {
                condition, body, ..
            } => {
                let mut stabilized_state = state.clone();
                stabilize_loop_state(&mut stabilized_state, |iterated| {
                    let mut condition_probe = condition.clone();
                    let mut body_probe = body.clone();
                    let _ = legalize_expr(&mut condition_probe, iterated, context);
                    legalize_stmts(&mut body_probe, iterated, context);
                });
                let mut loop_state = stabilized_state.clone();
                let _ = legalize_expr(condition, &loop_state, context);
                legalize_stmts(body, &mut loop_state, context);
                *state = stabilized_state;
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
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                if *has_impl && let Some(function_impl) = context.function_impls.get(index) {
                    context.observe_function_capture_state(
                        *index,
                        &function_impl.capture_copies,
                        state,
                    );
                }
            }
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
            Stmt::Let {
                index,
                declared_schema,
                expr,
                line,
            } => {
                let expr_state = state.clone();
                let ty = validate_expr(
                    expr,
                    &expr_state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                let actual_schema = inferred_expr_assignment_schema(expr, &expr_state, context);
                validate_declared_local_schema(
                    declared_schema.as_ref(),
                    ty,
                    context.expr_is_optional(expr, &expr_state),
                    actual_schema.as_ref(),
                    context,
                    Some(*line),
                    source_name,
                )?;
                bind_expr_result_to_slot(
                    state,
                    *index,
                    declared_schema.as_ref(),
                    expr,
                    &expr_state,
                    ty,
                    context,
                );
            }
            Stmt::Assign {
                kind,
                index,
                expr,
                line,
            } => {
                let expr_state = state.clone();
                let ty = validate_expr(
                    expr,
                    &expr_state,
                    Some(*line),
                    source_name,
                    context,
                    strict_function_add_types,
                )?;
                validate_numeric_assignment_operands(
                    kind,
                    *index,
                    expr,
                    &expr_state,
                    ty,
                    DiagnosticSite {
                        line: Some(*line),
                        source_name,
                    },
                    context,
                )?;
                let declared_schema = state
                    .has_declared_schema(*index)
                    .then(|| state.schema(*index).cloned())
                    .flatten();
                let actual_schema = inferred_expr_assignment_schema(expr, &expr_state, context);
                validate_declared_local_schema(
                    declared_schema.as_ref(),
                    ty,
                    context.expr_is_optional(expr, &expr_state),
                    actual_schema.as_ref(),
                    context,
                    Some(*line),
                    source_name,
                )?;
                bind_expr_result_to_slot(state, *index, None, expr, &expr_state, ty, context);
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
                let mut then_state = refine_state_for_condition(state, condition, true);
                let mut else_state = refine_state_for_condition(state, condition, false);
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
                validate_branch_state_merge(
                    Some(*line),
                    source_name,
                    state,
                    &then_state,
                    &else_state,
                    context.is_strict(),
                )?;
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

fn validate_declared_local_schema(
    schema: Option<&TypeSchema>,
    actual: BoundType,
    actual_optional: bool,
    actual_schema: Option<&TypeSchema>,
    context: &mut TypeContext<'_>,
    line: Option<u32>,
    source_name: Option<&str>,
) -> Result<(), CompileError> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let resolved = context.resolve_schema(schema);
    let (expected_schema, expected_optional) = resolved.split_optional();
    let expected = context.bound_type_for_schema(&expected_schema);
    if actual_optional && !expected_optional {
        return Err(CompileError::InvalidFieldAccess {
            line,
            source_name: owned_source_name(source_name),
            detail: format!(
                "local is declared as schema type '{}' but was assigned an optional value",
                schema_type_label(schema)
            ),
        });
    }
    if actual == BoundType::Null && !expected_optional && expected != BoundType::Null {
        return Err(CompileError::InvalidFieldAccess {
            line,
            source_name: owned_source_name(source_name),
            detail: format!(
                "local is declared as schema type '{}' but was assigned null",
                schema_type_label(schema)
            ),
        });
    }
    if actual == BoundType::Unknown
        || (expected_optional && actual == BoundType::Null)
        || actual == expected
        || (expected == BoundType::Number && is_numeric_bound_type(actual))
        || (expected == BoundType::Array && matches!(actual, BoundType::ArrayOf(_)))
        || (expected == BoundType::Map && matches!(actual, BoundType::MapOf(_)))
    {
        if let Some(actual_schema) = actual_schema
            && let Some(detail) = find_declared_schema_mismatch(
                &expected_schema,
                actual_schema,
                context,
                String::new(),
            )
        {
            return Err(CompileError::InvalidFieldAccess {
                line,
                source_name: owned_source_name(source_name),
                detail,
            });
        }
        return Ok(());
    }
    Err(CompileError::InvalidFieldAccess {
        line,
        source_name: owned_source_name(source_name),
        detail: format!(
            "local is declared as schema type '{}' but was assigned {}",
            schema_type_label(schema),
            bound_type_label(actual)
        ),
    })
}

fn validate_declared_return_schema(
    function_name: &str,
    schema: &TypeSchema,
    actual: BoundType,
    actual_optional: bool,
    actual_schema: Option<&TypeSchema>,
    context: &mut TypeContext<'_>,
    line: Option<u32>,
    source_name: Option<&str>,
) -> Result<(), CompileError> {
    let resolved = context.resolve_schema(schema);
    let (expected_schema, expected_optional) = resolved.split_optional();
    let expected = context.bound_type_for_schema(&expected_schema);
    if actual_optional && !expected_optional {
        return Err(CompileError::StrictTypingRequired {
            line,
            source_name: owned_source_name(source_name),
            detail: format!(
                "function '{function_name}' is declared to return '{}' but produced an optional value",
                schema_type_label(schema)
            ),
        });
    }
    if actual == BoundType::Null && !expected_optional && expected != BoundType::Null {
        return Err(CompileError::StrictTypingRequired {
            line,
            source_name: owned_source_name(source_name),
            detail: format!(
                "function '{function_name}' is declared to return '{}' but produced null",
                schema_type_label(schema)
            ),
        });
    }
    if actual == BoundType::Unknown
        || (expected_optional && actual == BoundType::Null)
        || actual == expected
        || (expected == BoundType::Number && is_numeric_bound_type(actual))
        || (expected == BoundType::Array && matches!(actual, BoundType::ArrayOf(_)))
        || (expected == BoundType::Map && matches!(actual, BoundType::MapOf(_)))
    {
        if let Some(actual_schema) = actual_schema
            && let Some(detail) = find_declared_schema_mismatch(
                &expected_schema,
                actual_schema,
                context,
                String::new(),
            )
        {
            return Err(CompileError::StrictTypingRequired {
                line,
                source_name: owned_source_name(source_name),
                detail: format!("function '{function_name}' return type mismatch: {detail}"),
            });
        }
        return Ok(());
    }
    Err(CompileError::StrictTypingRequired {
        line,
        source_name: owned_source_name(source_name),
        detail: format!(
            "function '{function_name}' is declared to return '{}' but produced {}",
            schema_type_label(schema),
            bound_type_label(actual)
        ),
    })
}

fn validate_numeric_assignment_operands(
    kind: &AssignmentKind,
    index: LocalSlot,
    expr: &Expr,
    expr_state: &LocalTypeState,
    expr_ty: BoundType,
    site: DiagnosticSite<'_>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    if !kind.requires_numeric_operands() {
        return Ok(());
    }

    let target_ty = expr_state.get(index);
    if !is_numeric_bound_type(target_ty) {
        return Err(CompileError::BinaryOperandTypeMismatch {
            line: site.line,
            source_name: owned_source_name(site.source_name),
            detail: format!(
                "{} requires a numeric local, found {}",
                kind.diagnostic_label(),
                bound_type_label(target_ty)
            ),
        });
    }

    let rhs_ty = match expr {
        Expr::Add(_, rhs) => context.infer_expr_type(rhs, expr_state),
        _ => expr_ty,
    };
    if !is_numeric_bound_type(rhs_ty) || !is_numeric_bound_type(expr_ty) {
        return Err(CompileError::BinaryOperandTypeMismatch {
            line: site.line,
            source_name: owned_source_name(site.source_name),
            detail: format!(
                "{} requires numeric operands, found {} and {}",
                kind.diagnostic_label(),
                bound_type_label(target_ty),
                bound_type_label(rhs_ty)
            ),
        });
    }

    Ok(())
}

fn inferred_expr_assignment_schema(
    expr: &Expr,
    expr_state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> Option<TypeSchema> {
    if context.expr_is_optional(expr, expr_state) {
        context.infer_optional_expr_inner_schema(expr, expr_state)
    } else {
        context.infer_expr_schema(expr, expr_state)
    }
}

pub(super) fn find_declared_schema_mismatch(
    expected: &TypeSchema,
    actual: &TypeSchema,
    context: &mut TypeContext<'_>,
    path: String,
) -> Option<String> {
    let mut recursive_named = HashSet::new();
    find_declared_schema_mismatch_with_recursion(
        expected,
        actual,
        context,
        path,
        &mut recursive_named,
        false,
    )
}

fn find_declared_schema_mismatch_with_recursion(
    expected: &TypeSchema,
    actual: &TypeSchema,
    context: &mut TypeContext<'_>,
    path: String,
    recursive_named: &mut HashSet<String>,
    allow_partial_object: bool,
) -> Option<String> {
    let expected = match expected {
        TypeSchema::Optional(inner) => {
            return match actual {
                TypeSchema::Null => None,
                TypeSchema::Optional(actual_inner) => find_declared_schema_mismatch_with_recursion(
                    inner,
                    actual_inner,
                    context,
                    path,
                    recursive_named,
                    allow_partial_object,
                ),
                _ => find_declared_schema_mismatch_with_recursion(
                    inner,
                    actual,
                    context,
                    path,
                    recursive_named,
                    allow_partial_object,
                ),
            };
        }
        TypeSchema::Named(name, type_args) => {
            let instance = render_schema_path_segment(name, type_args);
            let is_recursive = !recursive_named.insert(instance.clone());
            let resolved = context.resolve_schema(expected);
            let mismatch = find_declared_schema_mismatch_with_recursion(
                &resolved,
                actual,
                context,
                path,
                recursive_named,
                is_recursive,
            );
            if !is_recursive {
                recursive_named.remove(&instance);
            }
            return mismatch;
        }
        _ => context.substitute_schema_generics(expected),
    };
    let actual = context.resolve_schema(actual);
    let actual = match actual {
        TypeSchema::Optional(inner) => *inner,
        other => other,
    };

    match (&expected, &actual) {
        (_, TypeSchema::Unknown | TypeSchema::Null) | (TypeSchema::Unknown, _) => None,
        (TypeSchema::Int, TypeSchema::Int)
        | (TypeSchema::Number, TypeSchema::Int)
        | (TypeSchema::Float, TypeSchema::Float)
        | (TypeSchema::Number, TypeSchema::Float)
        | (TypeSchema::Number, TypeSchema::Number)
        | (TypeSchema::Bool, TypeSchema::Bool)
        | (TypeSchema::String, TypeSchema::String)
        | (TypeSchema::Bytes, TypeSchema::Bytes) => None,
        (expected, actual)
            if expected.array_prefix_and_rest().is_some()
                && actual.array_prefix_and_rest().is_some() =>
        {
            find_declared_array_mismatch_with_recursion(
                expected,
                actual,
                context,
                path,
                recursive_named,
                allow_partial_object,
            )
        }
        (TypeSchema::GenericParam(_), _) | (_, TypeSchema::GenericParam(_)) => None,
        (
            TypeSchema::Callable {
                params: expected_params,
                result: expected_result,
            },
            TypeSchema::Callable {
                params: actual_params,
                result: actual_result,
            },
        ) => {
            if expected_params.len() != actual_params.len() {
                return Some(format!(
                    "{} is declared as schema type '{}' but was assigned {}",
                    schema_path_label(&path),
                    schema_type_label(&expected),
                    schema_type_label(&actual)
                ));
            }
            for (index, (expected_param, actual_param)) in
                expected_params.iter().zip(actual_params.iter()).enumerate()
            {
                let param_path = if path.is_empty() {
                    format!("arg[{index}]")
                } else {
                    format!("{path}.arg[{index}]")
                };
                if let Some(detail) = find_declared_schema_mismatch_with_recursion(
                    expected_param,
                    actual_param,
                    context,
                    param_path,
                    recursive_named,
                    allow_partial_object,
                ) {
                    return Some(detail);
                }
            }
            find_declared_schema_mismatch_with_recursion(
                expected_result,
                actual_result,
                context,
                if path.is_empty() {
                    "return".to_string()
                } else {
                    format!("{path}.return")
                },
                recursive_named,
                allow_partial_object,
            )
        }
        (TypeSchema::Map(expected), TypeSchema::Map(actual)) => {
            find_declared_schema_mismatch_with_recursion(
                expected,
                actual,
                context,
                path,
                recursive_named,
                allow_partial_object,
            )
        }
        (TypeSchema::Map(expected), TypeSchema::Object(fields)) => {
            for (name, field_schema) in fields {
                let field_path = extend_schema_path(&path, name);
                if let Some(detail) = find_declared_schema_mismatch_with_recursion(
                    expected,
                    field_schema,
                    context,
                    field_path,
                    recursive_named,
                    allow_partial_object,
                ) {
                    return Some(detail);
                }
            }
            None
        }
        (TypeSchema::Object(expected_fields), TypeSchema::Object(actual_fields)) => {
            if !allow_partial_object {
                for (name, expected_field) in expected_fields {
                    let field_path = extend_schema_path(&path, name);
                    let Some(actual_field) = actual_fields.get(name) else {
                        return Some(format!(
                            "field '{}' is required by the declared schema but is missing",
                            field_path
                        ));
                    };
                    if let Some(detail) = find_declared_schema_mismatch_with_recursion(
                        expected_field,
                        actual_field,
                        context,
                        field_path,
                        recursive_named,
                        false,
                    ) {
                        return Some(detail);
                    }
                }
                return None;
            }

            for (name, actual_field) in actual_fields {
                let Some(expected_field) = expected_fields.get(name) else {
                    continue;
                };
                let field_path = extend_schema_path(&path, name);
                if let Some(detail) = find_declared_schema_mismatch_with_recursion(
                    expected_field,
                    actual_field,
                    context,
                    field_path,
                    recursive_named,
                    true,
                ) {
                    return Some(detail);
                }
            }
            None
        }
        _ => Some(format!(
            "{} is declared as schema type '{}' but was assigned {}",
            schema_path_label(&path),
            schema_type_label(&expected),
            schema_type_label(&actual)
        )),
    }
}

fn find_declared_array_mismatch_with_recursion(
    expected: &TypeSchema,
    actual: &TypeSchema,
    context: &mut TypeContext<'_>,
    path: String,
    recursive_named: &mut HashSet<String>,
    allow_partial_object: bool,
) -> Option<String> {
    let (expected_prefix, expected_rest) = expected.array_prefix_and_rest()?;
    let Some((actual_prefix, actual_rest)) = actual.array_prefix_and_rest() else {
        return Some(format!(
            "{} is declared as schema type '{}' but was assigned {}",
            schema_path_label(&path),
            schema_type_label(expected),
            schema_type_label(actual)
        ));
    };

    for (index, expected_item) in expected_prefix.iter().enumerate() {
        let actual_item = actual_prefix.get(index).or(actual_rest);
        let Some(actual_item) = actual_item else {
            return Some(format!(
                "{} is required by the declared schema but is missing",
                schema_path_label(&extend_array_schema_path(&path, index))
            ));
        };
        let item_path = extend_array_schema_path(&path, index);
        if let Some(detail) = find_declared_schema_mismatch_with_recursion(
            expected_item,
            actual_item,
            context,
            item_path,
            recursive_named,
            allow_partial_object,
        ) {
            return Some(detail);
        }
    }

    if let Some(expected_rest) = expected_rest {
        for (index, actual_item) in actual_prefix.iter().enumerate().skip(expected_prefix.len()) {
            let item_path = extend_array_schema_path(&path, index);
            if let Some(detail) = find_declared_schema_mismatch_with_recursion(
                expected_rest,
                actual_item,
                context,
                item_path,
                recursive_named,
                allow_partial_object,
            ) {
                return Some(detail);
            }
        }
        if let Some(actual_rest) = actual_rest {
            let item_path = extend_array_schema_path(&path, expected_prefix.len());
            if let Some(detail) = find_declared_schema_mismatch_with_recursion(
                expected_rest,
                actual_rest,
                context,
                item_path,
                recursive_named,
                allow_partial_object,
            ) {
                return Some(detail);
            }
        }
    }

    None
}

fn extend_schema_path(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_string()
    } else {
        format!("{path}.{segment}")
    }
}

fn extend_array_schema_path(path: &str, index: usize) -> String {
    if path.is_empty() {
        format!("[{index}]")
    } else {
        format!("{path}[{index}]")
    }
}

fn schema_path_label(path: &str) -> String {
    if path.is_empty() {
        "value".to_string()
    } else if path.starts_with('[') {
        format!("index {path}")
    } else {
        format!("field '{path}'")
    }
}

fn schema_type_label(schema: &TypeSchema) -> String {
    render_schema_label(schema)
}

fn render_schema_path_segment(name: &str, type_args: &[TypeSchema]) -> String {
    if type_args.is_empty() {
        name.to_string()
    } else {
        format!(
            "{}<{}>",
            name,
            type_args
                .iter()
                .map(render_schema_label)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

pub(super) fn bind_expr_result_to_slot(
    state: &mut LocalTypeState,
    slot: LocalSlot,
    declared_schema: Option<&TypeSchema>,
    expr: &Expr,
    expr_state: &LocalTypeState,
    ty: BoundType,
    context: &mut TypeContext<'_>,
) {
    if let Some(callable) = context.callable_binding_from_expr(expr, expr_state) {
        let declared_binding = declared_schema.map(TypeSchema::split_optional).or_else(|| {
            expr_state
                .has_declared_schema(slot)
                .then(|| {
                    (
                        expr_state.schema(slot).cloned(),
                        expr_state.is_optional(slot),
                    )
                })
                .and_then(|(schema, optional)| schema.map(|schema| (schema, optional)))
        });
        let slot_declared_schema = declared_binding.as_ref().map(|(schema, _)| schema.clone());
        let declared_optional = declared_binding
            .as_ref()
            .map(|(_, optional)| *optional)
            .unwrap_or(false);
        let optional = context.expr_is_optional(expr, expr_state) || declared_optional;
        let schema = slot_declared_schema.clone().or_else(|| {
            expr_state
                .callable_schema(slot)
                .cloned()
                .or_else(|| context.infer_expr_schema(expr, expr_state))
        });
        let from_declared_schema =
            slot_declared_schema.is_some() || expr_state.has_declared_schema(slot);
        state.bind_callable_with_schema(slot, callable, schema, from_declared_schema, optional);
    } else {
        let declared_binding = declared_schema.map(TypeSchema::split_optional).or_else(|| {
            expr_state
                .has_declared_schema(slot)
                .then(|| {
                    (
                        expr_state.schema(slot).cloned(),
                        expr_state.is_optional(slot),
                    )
                })
                .and_then(|(schema, optional)| schema.map(|schema| (schema, optional)))
        });
        let slot_declared_schema = declared_binding.as_ref().map(|(schema, _)| schema.clone());
        let declared_optional = declared_binding
            .as_ref()
            .map(|(_, optional)| *optional)
            .unwrap_or(false);
        let optional = context.expr_is_optional(expr, expr_state) || declared_optional;
        let schema = slot_declared_schema.clone().or_else(|| {
            if optional {
                context.infer_optional_expr_inner_schema(expr, expr_state)
            } else {
                context.infer_expr_schema(expr, expr_state)
            }
        });
        let from_declared_schema = slot_declared_schema.is_some()
            || expr_state.has_declared_schema(slot)
            || context.expr_has_declared_schema(expr, expr_state);
        let inferred_ty = if optional {
            context.infer_optional_expr_inner_type(expr, expr_state)
        } else {
            ty
        };
        let ty = slot_declared_schema
            .as_ref()
            .map(|schema| context.bound_type_for_schema(schema))
            .unwrap_or(inferred_ty);
        state.set_with_optional_schema_origin(slot, ty, schema, from_declared_schema, optional);
    }
}

pub(super) fn refine_state_for_match_pattern(
    state: &LocalTypeState,
    pattern: &MatchPattern,
    value_slot: LocalSlot,
) -> LocalTypeState {
    let mut refined = state.clone();
    if let Some(binding_slot) = pattern.binding_slot() {
        refined.set_with_optional_schema_origin(
            binding_slot,
            state.get(value_slot),
            state.schema(value_slot).cloned(),
            state.has_declared_schema(value_slot),
            false,
        );
    }
    refined
}

pub(super) fn build_function_names(functions: &[FunctionDecl]) -> HashMap<u16, String> {
    functions
        .iter()
        .map(|decl| (decl.index, decl.name.clone()))
        .collect()
}

pub(super) fn build_function_decl_map(functions: &[FunctionDecl]) -> HashMap<u16, FunctionDecl> {
    functions
        .iter()
        .cloned()
        .map(|decl| (decl.index, decl))
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
        edge_abi::AbiParamType::Bytes => CallableParamType::Bytes,
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
        | Expr::Bytes(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::OptionalGet { container, key, .. } => {
            expr_contains_param_add(container, param_slots)
                || expr_contains_param_add(key, param_slots)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            expr_contains_param_add(value, param_slots)
                || expr_contains_param_add(fallback, param_slots)
        }
        Expr::Call(_, _, args) | Expr::LocalCall(_, _, args) => args
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
        | Expr::Bytes(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::OptionalGet { container, key, .. } => {
            expr_uses_param(container, param_slots) || expr_uses_param(key, param_slots)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => expr_uses_param(value, param_slots) || expr_uses_param(fallback, param_slots),
        Expr::Call(_, _, args) | Expr::LocalCall(_, _, args) => {
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
        BuiltinFunction::Has => "has".to_string(),
        BuiltinFunction::Set => "set".to_string(),
        BuiltinFunction::Keys => "keys".to_string(),
        BuiltinFunction::Count => "count".to_string(),
        BuiltinFunction::TypeOf => "type".to_string(),
        BuiltinFunction::Assert => "assert".to_string(),
        _ => builtin.name().replacen('_', "::", 1),
    }
}

pub(super) fn is_numeric_bound_type(value: BoundType) -> bool {
    matches!(value, BoundType::Int | BoundType::Float | BoundType::Number)
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
        Expr::Bytes(_) => BoundType::Bytes,
        Expr::String(_) => BoundType::String,
        Expr::OptionalGet { container, key, .. } => {
            let _ = legalize_expr(container, state, context);
            let _ = legalize_expr(key, state, context);
            context.infer_expr_type(expr, state)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            let _ = legalize_expr(value, state, context);
            let _ = legalize_expr(fallback, state, context);
            context.infer_expr_type(expr, state)
        }
        Expr::FunctionRef(_) | Expr::Call(..) | Expr::LocalCall(..) | Expr::Closure(_) => {
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
            for (pattern, arm_expr) in arms.iter_mut() {
                let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                let ty = legalize_expr(arm_expr, &arm_state, context);
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
        Expr::Call(index, _, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
            if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                fold_builtin_call(expr, builtin, state);
            }
        }
        Expr::LocalCall(_, _, args) => {
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
    let Expr::Call(_, _, args) = expr else {
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
        Expr::Bytes(bytes) => Some(bytes.len()),
        Expr::String(text) => Some(text.chars().count()),
        Expr::Call(index, _, args) => {
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
            if lhs == BoundType::Bytes || rhs == BoundType::Bytes {
                if lhs == BoundType::Bytes && rhs == BoundType::Bytes {
                    BoundType::Bytes
                } else {
                    BoundType::Unknown
                }
            } else if lhs == BoundType::String || rhs == BoundType::String {
                BoundType::String
            } else if let Some(array_ty) = infer_array_concat_type(lhs, rhs) {
                array_ty
            } else if lhs == BoundType::Int && rhs == BoundType::Int {
                BoundType::Int
            } else if lhs == BoundType::Float || rhs == BoundType::Float {
                if is_numeric_bound_type(lhs) && is_numeric_bound_type(rhs) {
                    BoundType::Float
                } else {
                    BoundType::Unknown
                }
            } else if is_numeric_bound_type(lhs) && is_numeric_bound_type(rhs) {
                BoundType::Number
            } else {
                BoundType::Unknown
            }
        }
        Expr::Sub(_, _) | Expr::Mul(_, _) | Expr::Div(_, _) | Expr::Mod(_, _) => {
            if lhs == BoundType::Int && rhs == BoundType::Int {
                BoundType::Int
            } else if lhs == BoundType::Float || rhs == BoundType::Float {
                if is_numeric_bound_type(lhs) && is_numeric_bound_type(rhs) {
                    BoundType::Float
                } else {
                    BoundType::Unknown
                }
            } else if is_numeric_bound_type(lhs) && is_numeric_bound_type(rhs) {
                BoundType::Number
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
        (BoundType::Array, BoundType::ArrayOf(_)) | (BoundType::ArrayOf(_), BoundType::Array) => {
            Some(BoundType::Array)
        }
        (BoundType::Array, BoundType::Array) => Some(BoundType::Array),
        _ => None,
    }
}

pub(super) fn infer_unary_type(expr: &Expr, inner: BoundType) -> BoundType {
    match expr {
        Expr::Neg(_) => match inner {
            BoundType::Int | BoundType::Float | BoundType::Number => inner,
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
        BoundType::Number => "number",
        BoundType::Bool => "bool",
        BoundType::String => "string",
        BoundType::Bytes => "bytes",
        BoundType::Array | BoundType::ArrayOf(_) => "array",
        BoundType::Map | BoundType::MapOf(_) => "map",
    }
}
