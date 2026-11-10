//! Guest `Result<T, E>` resolution.
//!
//! Runtime values are ordinary maps with `tag: bool` and the active
//! `payload`. A missing payload is null only when the active branch schema
//! allows null. This pass fills constructor schemas from annotations and
//! rejects non-exhaustive or mistyped Result matches before bytecode emission.

use std::collections::HashMap;

use super::CompileError;
use super::ir::{
    AssignmentKind, ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot,
    ResultMethod, ResultMethodSlots, SemanticNodeId, Stmt, TypeSchema,
};
use super::source_map::Span;

pub(crate) const RESULT_TAG_KEY: &str = "tag";
pub(crate) const RESULT_PAYLOAD_KEY: &str = "payload";

struct Env {
    slots: HashMap<LocalSlot, TypeSchema>,
    functions: HashMap<u16, FunctionDecl>,
    function_list: Vec<FunctionDecl>,
    function_impls: HashMap<u16, FunctionImpl>,
    struct_schemas: HashMap<String, super::ir::StructDecl>,
    /// Return schema of the function or closure currently being resolved.
    /// `None` at root and inside a callable that does not return `Result`.
    lexical_return: Option<TypeSchema>,
}

pub(crate) fn resolve_result_types(
    ir: &mut FrontendIr,
    entry_local_types: &[super::typing::EntryLocalType],
) -> Result<(), CompileError> {
    let function_list = ir.functions.clone();
    let functions = function_list
        .iter()
        .map(|function| (function.index, function.clone()))
        .collect();
    let mut slots = HashMap::new();
    for entry_local in entry_local_types {
        if let Some(schema) = super::typing::logical_entry_schema(entry_local) {
            slots.insert(entry_local.slot, schema);
        }
    }
    let mut env = Env {
        slots,
        functions,
        function_list,
        function_impls: ir.function_impls.clone(),
        struct_schemas: ir.struct_schemas.clone(),
        lexical_return: None,
    };
    resolve_stmts(&mut ir.stmts, None, &mut env)?;
    let mut impls = std::mem::take(&mut ir.function_impls);
    for (index, function_impl) in impls.iter_mut() {
        resolve_function(*index, function_impl, &mut env)?;
    }
    ir.function_impls = impls;
    Ok(())
}

fn resolve_function(
    index: u16,
    function_impl: &mut FunctionImpl,
    env: &mut Env,
) -> Result<(), CompileError> {
    let saved = env.slots.clone();
    let saved_return = env.lexical_return.clone();
    if let Some(function) = env.functions.get(&index).cloned() {
        env.lexical_return = function.return_schema.clone();
        for (slot, schema) in function_impl
            .param_slots
            .iter()
            .zip(function.arg_schemas.iter())
        {
            if let Some(schema) = schema {
                env.slots.insert(*slot, schema.clone());
            }
        }
        resolve_stmts(
            &mut function_impl.body_stmts,
            function.return_schema.clone(),
            env,
        )?;
        resolve_expr(
            &mut function_impl.body_expr,
            function.return_schema.as_ref(),
            env,
        )?;
    } else {
        resolve_stmts(&mut function_impl.body_stmts, None, env)?;
        resolve_expr(&mut function_impl.body_expr, None, env)?;
    }
    env.slots = saved;
    env.lexical_return = saved_return;
    Ok(())
}

fn resolve_stmts(
    stmts: &mut [Stmt],
    expected_tail: Option<TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    for stmt in stmts {
        resolve_stmt(stmt, expected_tail.as_ref(), env)?;
    }
    Ok(())
}

fn resolve_stmt(
    stmt: &mut Stmt,
    expected_tail: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {
            Ok(())
        }
        Stmt::Let {
            index,
            declared_schema,
            expr,
            ..
        } => {
            resolve_expr(expr, declared_schema.as_ref(), env)?;
            if let Some(schema) = declared_schema
                .clone()
                .or_else(|| infer_schema(expr, env))
                .or_else(|| {
                    super::typing::infer_value_schema(
                        &env.function_list,
                        &env.function_impls,
                        &env.struct_schemas,
                        expr,
                        &env.slots,
                    )
                })
                .filter(|schema| !matches!(schema, TypeSchema::Unknown))
            {
                env.slots.insert(*index, schema);
            }
            Ok(())
        }
        Stmt::Assign {
            kind,
            index,
            expr,
            line,
        } => {
            let expected = env.slots.get(index).cloned();
            if !matches!(kind, AssignmentKind::Set) {
                let _ = (expected_tail, line);
            }
            resolve_expr(expr, expected.as_ref(), env)?;
            if let Some(schema) = infer_schema(expr, env) {
                env.slots.insert(*index, schema);
            }
            Ok(())
        }
        Stmt::ClosureLet {
            closure,
            expected_schema,
            ..
        } => resolve_closure(closure, expected_schema.as_ref(), env),
        Stmt::Expr { expr, .. } => resolve_expr(expr, None, env),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            resolve_expr(condition, Some(&TypeSchema::Bool), env)?;
            resolve_stmts(then_branch, expected_tail.cloned(), env)?;
            resolve_stmts(else_branch, expected_tail.cloned(), env)
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            resolve_stmt(init, None, env)?;
            resolve_expr(condition, Some(&TypeSchema::Bool), env)?;
            resolve_stmt(post, None, env)?;
            resolve_stmts(body, None, env)
        }
        Stmt::While {
            condition, body, ..
        } => {
            resolve_expr(condition, Some(&TypeSchema::Bool), env)?;
            resolve_stmts(body, None, env)
        }
        Stmt::Drop { index, .. } => {
            env.slots.remove(index);
            Ok(())
        }
    }
}

fn resolve_expr(
    expr: &mut Expr,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    match expr {
        Expr::ResultConstruct {
            ok,
            payload,
            result_schema,
            span,
            ..
        } => resolve_construct(*ok, payload, result_schema, *span, expected, env),
        Expr::ResultMatch {
            value,
            ok_binding,
            ok_expr,
            err_binding,
            err_expr,
            scrutinee_schema,
            span,
            ..
        } => resolve_match(
            value,
            *ok_binding,
            ok_expr,
            *err_binding,
            err_expr,
            scrutinee_schema,
            *span,
            expected,
            env,
        ),
        Expr::ResultQuery { value, span, .. } => {
            resolve_expr(value, None, env)?;
            require_result_schema(
                authoritative_schema(value, env).as_ref(),
                *span,
                "is_ok/is_err",
            )
        }
        Expr::ResultMethod {
            method,
            receiver,
            argument,
            slots,
            span,
            semantic_id,
        } => {
            let receiver = std::mem::replace(receiver.as_mut(), Expr::Null);
            let argument = std::mem::replace(argument.as_mut(), Expr::Null);
            let lowered = resolve_method(
                *method,
                receiver,
                argument,
                *slots,
                *span,
                *semantic_id,
                expected,
                env,
            )?;
            *expr = lowered;
            // The lowered branch already resolved its receiver, callback and
            // arm payloads exactly once; re-walking the same tree would
            // re-resolve nested closure bodies without their contextual
            // callable schema.
            Ok(())
        }
        Expr::ResultTry {
            value,
            operand_schema,
            return_schema,
            span,
            ..
        } => resolve_try(value, operand_schema, return_schema, *span, env),
        Expr::Block { stmts, expr } => {
            resolve_stmts(stmts, expected.cloned(), env)?;
            resolve_expr(expr, expected, env)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            resolve_expr(condition, Some(&TypeSchema::Bool), env)?;
            resolve_expr(then_expr, expected, env)?;
            resolve_expr(else_expr, expected, env)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            resolve_expr(value, None, env)?;
            for (_, arm) in arms {
                resolve_expr(arm, expected, env)?;
            }
            resolve_expr(default, expected, env)
        }
        Expr::OptionalGet { container, key, .. } => {
            resolve_expr(container, None, env)?;
            resolve_expr(key, None, env)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            resolve_expr(value, None, env)?;
            resolve_expr(fallback, expected, env)
        }
        Expr::Call(index, type_args, args, _, _) => {
            let expected_args = super::typing::argument_schemas_for_call(
                &env.function_list,
                &env.function_impls,
                &env.struct_schemas,
                *index,
                type_args,
            )
            .or_else(|| {
                super::typing::expected_container_argument_schemas(
                    &env.function_list,
                    &env.function_impls,
                    &env.struct_schemas,
                    *index,
                    args,
                    expected,
                )
            });
            for (offset, arg) in args.iter_mut().enumerate() {
                let expected_arg = expected_args
                    .as_ref()
                    .and_then(|schemas| schemas.get(offset))
                    .and_then(|schema| schema.as_ref());
                resolve_expr(arg, expected_arg, env)?;
            }
            Ok(())
        }
        Expr::LocalCall(slot, _, args, _) => {
            let params = env.slots.get(slot).and_then(|schema| match schema {
                TypeSchema::Callable { params, .. } => Some(params.clone()),
                _ => None,
            });
            for (offset, arg) in args.iter_mut().enumerate() {
                let expected_arg = params.as_ref().and_then(|schemas| schemas.get(offset));
                resolve_expr(arg, expected_arg, env)?;
            }
            Ok(())
        }
        Expr::ModuleCall(symbol, type_args, args, _) => {
            let expected_args = env
                .function_list
                .iter()
                .find(|function| function.symbol.as_ref() == Some(symbol))
                .and_then(|function| {
                    super::typing::argument_schemas_for_call(
                        &env.function_list,
                        &env.function_impls,
                        &env.struct_schemas,
                        function.index,
                        type_args,
                    )
                });
            for (offset, arg) in args.iter_mut().enumerate() {
                let expected_arg = expected_args
                    .as_ref()
                    .and_then(|schemas| schemas.get(offset))
                    .and_then(|schema| schema.as_ref());
                resolve_expr(arg, expected_arg, env)?;
            }
            Ok(())
        }
        Expr::Closure(closure) => resolve_closure(closure, expected, env),
        Expr::ClosureCall(closure, args) => {
            resolve_closure(closure, expected, env)?;
            for arg in args.iter_mut() {
                resolve_expr(arg, None, env)?;
            }
            Ok(())
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
            resolve_expr(lhs, None, env)?;
            resolve_expr(rhs, None, env)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => resolve_expr(inner, None, env),
        Expr::Null
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
        | Expr::MoveIndex { .. } => Ok(()),
    }
}

/// Resolve a structured Result combinator method into ordinary typed IR.
///
/// The receiver is evaluated once and then the callback/fallback argument is
/// evaluated once, both before the branch is selected. The selected branch
/// reuses [`Expr::ResultMatch`] payload binding plus a typed
/// `Call`/`LocalCall`; `unwrap_or` keeps the eager fallback value in a hidden
/// local and only projects it from the Err arm, while `unwrap_or_else` calls
/// the callback lazily from the Err arm.
#[allow(clippy::too_many_arguments)]
fn resolve_method(
    method: ResultMethod,
    mut receiver: Expr,
    mut argument: Expr,
    slots: ResultMethodSlots,
    span: Span,
    semantic_id: Option<SemanticNodeId>,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<Expr, CompileError> {
    let annotated = match expected {
        None | Some(TypeSchema::Unknown) => None,
        Some(schema) => Some(schema.clone()),
    };
    // Propagate the one Result branch the method keeps into the receiver so
    // unannotated chains such as `value.map(..).map_err(..).and_then(..)` can
    // still type callback constructors from the head annotation. A branch the
    // method replaces or leaves free is not constrained.
    let receiver_hint = combinator_receiver_hint(method, annotated.as_ref(), expected);
    resolve_expr(&mut receiver, receiver_hint.as_ref(), env)?;
    let receiver_schema = authoritative_schema(&receiver, env);
    let result_schema = match receiver_schema {
        Some(schema @ TypeSchema::Result { .. }) => schema,
        other => {
            // `.unwrap_or(...)` keeps the pre-existing nullable lowering for
            // every non-Result receiver; the Result methods are new syntax and
            // require an actual Result receiver.
            if method.retains_option_lowering_on_non_result() {
                resolve_expr(&mut argument, expected, env)?;
                return Ok(Expr::OptionUnwrapOr {
                    value: Box::new(receiver),
                    value_slot: slots.receiver,
                    fallback: Box::new(argument),
                    semantic_id,
                });
            }
            let found = other
                .as_ref()
                .map(render_schema)
                .unwrap_or_else(|| "an untyped value".to_string());
            return Err(result_error(
                span,
                format!(
                    ".{} requires a Result<T, E> receiver, found {}",
                    method.name(),
                    found
                ),
            ));
        }
    };
    let (ok_schema, err_schema) = branch_schemas(&result_schema).expect("driver matched Result");
    let annotated_result = match &annotated {
        Some(schema @ TypeSchema::Result { .. }) if !schema_contains_unknown(schema) => {
            Some(schema.clone())
        }
        Some(TypeSchema::Result { .. }) | None => None,
        Some(other) => {
            return Err(result_error(
                span,
                format!(
                    ".{} produces a Result value, not {}",
                    method.name(),
                    render_schema(other)
                ),
            ));
        }
    };

    match method {
        ResultMethod::UnwrapOr => {
            resolve_expr(&mut argument, Some(&ok_schema), env)?;
            let fallback_schema = authoritative_schema(&argument, env).ok_or_else(|| {
                result_error(
                    span,
                    ".unwrap_or fallback must have a concrete compile-time type".to_string(),
                )
            })?;
            require_method_schema(env, &ok_schema, &fallback_schema, span, method, "fallback")?;
            if ok_schema.contains_resource_with_named_types(&env.struct_schemas) {
                return Err(result_error(
                    span,
                    format!(
                        ".unwrap_or on Result<{}, {}> cannot discard an eager fallback owner: the unselected Ok branch has no destination for resource-typed fallback value {}",
                        render_schema(&ok_schema),
                        render_schema(&err_schema),
                        render_schema(&fallback_schema)
                    ),
                ));
            }
            if let Some(annotated) = &annotated
                && !schema_contains_unknown(annotated)
            {
                require_method_schema(env, annotated, &ok_schema, span, method, "result")?;
            }
            reject_resource_capturing_closure(&argument, span, method, env)?;
            env.slots.insert(slots.receiver, result_schema.clone());
            env.slots.insert(slots.argument, fallback_schema.clone());
            env.slots.insert(slots.ok_binding, ok_schema.clone());
            let mut branch = Expr::ResultMatch {
                value: Box::new(Expr::Var(slots.receiver)),
                value_slot: slots.receiver,
                ok_binding: Some(slots.ok_binding),
                ok_expr: Box::new(Expr::Var(slots.ok_binding)),
                err_binding: None,
                err_expr: Box::new(Expr::Var(slots.argument)),
                scrutinee_schema: result_schema.clone(),
                span,
                semantic_id: None,
            };
            resolve_lowered_result_match(&mut branch, expected, env)?;
            let lowered = bind_method_evaluations(
                &slots,
                receiver,
                argument,
                result_schema,
                fallback_schema,
                branch,
            );
            return Ok(lowered);
        }
        ResultMethod::UnwrapOrElse => {
            let hint = annotated.clone().unwrap_or(TypeSchema::Unknown);
            let callback_hint = TypeSchema::Callable {
                params: vec![err_schema.clone()],
                result: Box::new(hint),
            };
            resolve_expr(&mut argument, Some(&callback_hint), env)?;
            let callback_schema =
                method_callback_schema(&argument, &err_schema, span, method, env)?;
            let TypeSchema::Callable { params, result } = &callback_schema else {
                unreachable!("method_callback_schema returns a Callable");
            };
            if params.len() != 1 {
                return Err(result_error(
                    span,
                    format!(
                        ".unwrap_or_else callback must take exactly one error argument, found {}",
                        params.len()
                    ),
                ));
            }
            require_method_schema(
                env,
                &err_schema,
                &params[0],
                span,
                method,
                "callback parameter",
            )?;
            let returned = result.as_ref();
            require_concrete_schema(returned, span, method, "callback return")?;
            require_method_schema(env, &ok_schema, returned, span, method, "callback result")?;
            if let Some(annotated) = &annotated
                && !schema_contains_unknown(annotated)
            {
                require_method_schema(env, annotated, &ok_schema, span, method, "result")?;
            }
            reject_resource_capturing_closure(&argument, span, method, env)?;
            env.slots.insert(slots.receiver, result_schema.clone());
            env.slots.insert(slots.argument, callback_schema.clone());
            env.slots.insert(slots.ok_binding, ok_schema.clone());
            if let Some(err_binding) = slots.err_binding {
                env.slots.insert(err_binding, err_schema.clone());
            }
            let mut branch = Expr::ResultMatch {
                value: Box::new(Expr::Var(slots.receiver)),
                value_slot: slots.receiver,
                ok_binding: Some(slots.ok_binding),
                ok_expr: Box::new(Expr::Var(slots.ok_binding)),
                err_binding: slots.err_binding,
                err_expr: Box::new(Expr::LocalCall(
                    slots.argument,
                    Vec::new(),
                    vec![Expr::Var(expect_error_binding(&slots))],
                    None,
                )),
                scrutinee_schema: result_schema.clone(),
                span,
                semantic_id: None,
            };
            resolve_lowered_result_match(&mut branch, expected, env)?;
            let lowered = bind_method_evaluations(
                &slots,
                receiver,
                argument,
                result_schema,
                callback_schema,
                branch,
            );
            return Ok(lowered);
        }
        ResultMethod::Map | ResultMethod::MapErr | ResultMethod::AndThen | ResultMethod::OrElse => {
        }
    }

    let (param, callback_result_hint) = match method {
        ResultMethod::Map => (
            ok_schema.clone(),
            annotated_result
                .as_ref()
                .and_then(branch_schemas)
                .map(|(ok, _)| ok),
        ),
        ResultMethod::MapErr => (
            err_schema.clone(),
            annotated_result
                .as_ref()
                .and_then(branch_schemas)
                .map(|(_, err)| err),
        ),
        ResultMethod::AndThen => (
            ok_schema.clone(),
            // and_then keeps the receiver's error type, so the callback's
            // Result error branch is always known even without an annotation.
            Some(TypeSchema::Result {
                ok: Box::new(
                    annotated_result
                        .as_ref()
                        .and_then(branch_schemas)
                        .map(|(ok, _)| ok)
                        .unwrap_or(TypeSchema::Unknown),
                ),
                err: Box::new(err_schema.clone()),
            }),
        ),
        ResultMethod::OrElse => (
            err_schema.clone(),
            // or_else keeps the receiver's success type, so the callback's
            // Result success branch is always known even without an annotation.
            Some(TypeSchema::Result {
                ok: Box::new(ok_schema.clone()),
                err: Box::new(
                    annotated_result
                        .as_ref()
                        .and_then(branch_schemas)
                        .map(|(_, err)| err)
                        .unwrap_or(TypeSchema::Unknown),
                ),
            }),
        ),
        ResultMethod::UnwrapOr | ResultMethod::UnwrapOrElse => unreachable!("handled above"),
    };
    let callback_hint = TypeSchema::Callable {
        params: vec![param.clone()],
        result: Box::new(callback_result_hint.unwrap_or(TypeSchema::Unknown)),
    };
    resolve_expr(&mut argument, Some(&callback_hint), env)?;
    let callback_schema = method_callback_schema(&argument, &param, span, method, env)?;
    let TypeSchema::Callable { params, result } = &callback_schema else {
        unreachable!("method_callback_schema returns a Callable");
    };
    if params.len() != 1 {
        return Err(result_error(
            span,
            format!(
                ".{} callback must take exactly one argument, found {}",
                method.name(),
                params.len()
            ),
        ));
    }
    require_method_schema(env, &param, &params[0], span, method, "callback parameter")?;
    require_concrete_schema(result, span, method, "callback return")?;
    reject_resource_capturing_closure(&argument, span, method, env)?;

    let (ok_expr, err_expr) = match method {
        ResultMethod::Map => {
            let output_ok = result.as_ref().clone();
            if let Some(annotated_result) = &annotated_result {
                let (annotated_ok, _) =
                    branch_schemas(annotated_result).expect("matched Result annotation");
                require_method_schema(env, &annotated_ok, &output_ok, span, method, "result")?;
            }
            let output = TypeSchema::Result {
                ok: Box::new(output_ok),
                err: Box::new(err_schema.clone()),
            };
            let ok_expr = result_construct(
                true,
                Expr::LocalCall(
                    slots.argument,
                    Vec::new(),
                    vec![Expr::Var(expect_ok_binding(&slots))],
                    None,
                ),
                &output,
                span,
            );
            let err_expr = result_construct(
                false,
                Expr::Var(expect_error_binding(&slots)),
                &output,
                span,
            );
            (ok_expr, err_expr)
        }
        ResultMethod::MapErr => {
            let output_err = result.as_ref().clone();
            if let Some(annotated_result) = &annotated_result {
                let (_, annotated_err) =
                    branch_schemas(annotated_result).expect("matched Result annotation");
                require_method_schema(env, &annotated_err, &output_err, span, method, "result")?;
            }
            let output = TypeSchema::Result {
                ok: Box::new(ok_schema.clone()),
                err: Box::new(output_err),
            };
            let ok_expr =
                result_construct(true, Expr::Var(expect_ok_binding(&slots)), &output, span);
            let err_expr = result_construct(
                false,
                Expr::LocalCall(
                    slots.argument,
                    Vec::new(),
                    vec![Expr::Var(expect_error_binding(&slots))],
                    None,
                ),
                &output,
                span,
            );
            (ok_expr, err_expr)
        }
        ResultMethod::AndThen => {
            let TypeSchema::Result {
                ok: output_ok,
                err: callback_err,
            } = result.as_ref()
            else {
                return Err(result_error(
                    span,
                    format!(
                        ".and_then callback must return Result<U, E>, found {}",
                        render_schema(result)
                    ),
                ));
            };
            let output_ok = output_ok.as_ref().clone();
            require_concrete_schema(&output_ok, span, method, "callback success type")?;
            let callback_err = callback_err.as_ref().clone();
            require_method_schema(
                env,
                &err_schema,
                &callback_err,
                span,
                method,
                "callback error type",
            )?;
            if let Some(annotated_result) = &annotated_result {
                let (annotated_ok, annotated_err) =
                    branch_schemas(annotated_result).expect("matched Result annotation");
                require_method_schema(env, &annotated_ok, &output_ok, span, method, "result")?;
                require_method_schema(
                    env,
                    &annotated_err,
                    &err_schema,
                    span,
                    method,
                    "result error type",
                )?;
            }
            let output = TypeSchema::Result {
                ok: Box::new(output_ok),
                err: Box::new(err_schema.clone()),
            };
            let ok_expr = Expr::LocalCall(
                slots.argument,
                Vec::new(),
                vec![Expr::Var(expect_ok_binding(&slots))],
                None,
            );
            let err_expr = result_construct(
                false,
                Expr::Var(expect_error_binding(&slots)),
                &output,
                span,
            );
            (ok_expr, err_expr)
        }
        ResultMethod::OrElse => {
            let TypeSchema::Result {
                ok: callback_ok,
                err: output_err,
            } = result.as_ref()
            else {
                return Err(result_error(
                    span,
                    format!(
                        ".or_else callback must return Result<T, F>, found {}",
                        render_schema(result)
                    ),
                ));
            };
            let callback_ok = callback_ok.as_ref().clone();
            let output_err = output_err.as_ref().clone();
            require_concrete_schema(&output_err, span, method, "callback error type")?;
            require_method_schema(
                env,
                &ok_schema,
                &callback_ok,
                span,
                method,
                "callback success type",
            )?;
            if let Some(annotated_result) = &annotated_result {
                let (annotated_ok, annotated_err) =
                    branch_schemas(annotated_result).expect("matched Result annotation");
                require_method_schema(env, &annotated_ok, &ok_schema, span, method, "result")?;
                require_method_schema(
                    env,
                    &annotated_err,
                    &output_err,
                    span,
                    method,
                    "result error type",
                )?;
            }
            let output = TypeSchema::Result {
                ok: Box::new(ok_schema.clone()),
                err: Box::new(output_err),
            };
            let ok_expr =
                result_construct(true, Expr::Var(expect_ok_binding(&slots)), &output, span);
            let err_expr = Expr::LocalCall(
                slots.argument,
                Vec::new(),
                vec![Expr::Var(expect_error_binding(&slots))],
                None,
            );
            (ok_expr, err_expr)
        }
        ResultMethod::UnwrapOr | ResultMethod::UnwrapOrElse => unreachable!("handled above"),
    };

    env.slots.insert(slots.receiver, result_schema.clone());
    env.slots.insert(slots.argument, callback_schema.clone());
    env.slots.insert(slots.ok_binding, ok_schema);
    if let Some(err_binding) = slots.err_binding {
        env.slots.insert(err_binding, err_schema);
    }
    let mut branch = Expr::ResultMatch {
        value: Box::new(Expr::Var(slots.receiver)),
        value_slot: slots.receiver,
        ok_binding: Some(slots.ok_binding),
        ok_expr: Box::new(ok_expr),
        err_binding: slots.err_binding,
        err_expr: Box::new(err_expr),
        scrutinee_schema: result_schema.clone(),
        span,
        semantic_id: None,
    };
    resolve_lowered_result_match(&mut branch, expected, env)?;
    let lowered = bind_method_evaluations(
        &slots,
        receiver,
        argument,
        result_schema,
        callback_schema,
        branch,
    );
    Ok(lowered)
}

/// Resolve the lowered branch exactly once: it validates the typed arms and
/// payload schemas through the shared ResultMatch owner without re-walking the
/// already-resolved receiver/callback binder expressions.
fn resolve_lowered_result_match(
    branch: &mut Expr,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    let Expr::ResultMatch {
        value,
        ok_binding,
        ok_expr,
        err_binding,
        err_expr,
        scrutinee_schema,
        span,
        ..
    } = branch
    else {
        return Ok(());
    };
    resolve_match(
        value,
        *ok_binding,
        ok_expr,
        *err_binding,
        err_expr,
        scrutinee_schema,
        *span,
        expected,
        env,
    )
}

/// Sequence the receiver evaluation and then the callback/fallback evaluation
/// through parser-assigned hidden locals whose declared schemas are the
/// resolved receiver/callback types. The branch then reuses the existing
/// ResultMatch lowering. Explicit declarations keep strict typing on the
/// callback body instead of letting a closure parameter degrade to `Unknown`.
fn bind_method_evaluations(
    slots: &ResultMethodSlots,
    receiver: Expr,
    argument: Expr,
    receiver_schema: TypeSchema,
    argument_schema: TypeSchema,
    branch: Expr,
) -> Expr {
    Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: slots.receiver,
                declared_schema: Some(receiver_schema),
                expr: receiver,
                line: 1,
            },
            Stmt::Let {
                index: slots.argument,
                declared_schema: Some(argument_schema),
                expr: argument,
                line: 1,
            },
        ],
        expr: Box::new(branch),
    }
}

fn expect_ok_binding(slots: &ResultMethodSlots) -> LocalSlot {
    slots.ok_binding
}

fn expect_error_binding(slots: &ResultMethodSlots) -> LocalSlot {
    slots
        .err_binding
        .expect("Result method branch requires an error binding slot")
}

fn result_construct(ok: bool, payload: Expr, result_schema: &TypeSchema, span: Span) -> Expr {
    Expr::ResultConstruct {
        ok,
        payload: Box::new(payload),
        result_schema: result_schema.clone(),
        span,
        semantic_id: None,
    }
}

fn branch_schemas(schema: &TypeSchema) -> Option<(TypeSchema, TypeSchema)> {
    match schema {
        TypeSchema::Result { ok, err } => Some(((**ok).clone(), (**err).clone())),
        _ => None,
    }
}

/// The partial Result expectation propagated into a combinator receiver.
///
/// Only the branch the outer method keeps is constrained; the branch it
/// replaces (or leaves free) stays `Unknown` so it cannot reject an
/// unrelated receiver schema. `unwrap_or`/`unwrap_or_else` constrain only the
/// payload branch.
fn combinator_receiver_hint(
    method: ResultMethod,
    annotated: Option<&TypeSchema>,
    expected: Option<&TypeSchema>,
) -> Option<TypeSchema> {
    let known = |schema: &TypeSchema| !matches!(schema, TypeSchema::Unknown);
    match method {
        ResultMethod::Map | ResultMethod::AndThen => {
            let err = annotated
                .and_then(branch_schemas)
                .map(|(_, err)| err)
                .filter(known)?;
            Some(TypeSchema::Result {
                ok: Box::new(TypeSchema::Unknown),
                err: Box::new(err),
            })
        }
        ResultMethod::MapErr | ResultMethod::OrElse => {
            let ok = annotated
                .and_then(branch_schemas)
                .map(|(ok, _)| ok)
                .filter(known)?;
            Some(TypeSchema::Result {
                ok: Box::new(ok),
                err: Box::new(TypeSchema::Unknown),
            })
        }
        ResultMethod::UnwrapOr | ResultMethod::UnwrapOrElse => {
            let payload = expected.filter(|schema| known(schema))?.clone();
            Some(TypeSchema::Result {
                ok: Box::new(payload),
                err: Box::new(TypeSchema::Unknown),
            })
        }
    }
}

fn require_concrete_schema(
    schema: &TypeSchema,
    span: Span,
    method: ResultMethod,
    position: &str,
) -> Result<(), CompileError> {
    if schema_contains_unknown(schema) {
        return Err(result_error(
            span,
            format!(
                ".{} cannot type {}: callback/fallback schema {} is unresolved",
                method.name(),
                position,
                render_schema(schema)
            ),
        ));
    }
    Ok(())
}

fn schema_contains_unknown(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Unknown => true,
        TypeSchema::Optional(inner) | TypeSchema::Array(inner) | TypeSchema::Map(inner) => {
            schema_contains_unknown(inner)
        }
        TypeSchema::Result { ok, err } => {
            schema_contains_unknown(ok) || schema_contains_unknown(err)
        }
        TypeSchema::ArrayTuple(items) => items.iter().any(schema_contains_unknown),
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            prefix.iter().any(schema_contains_unknown) || schema_contains_unknown(rest)
        }
        TypeSchema::Object(fields) => fields.values().any(schema_contains_unknown),
        TypeSchema::Named(_, args) => args.iter().any(schema_contains_unknown),
        TypeSchema::Callable { params, result } => {
            params.iter().any(schema_contains_unknown) || schema_contains_unknown(result)
        }
        _ => false,
    }
}

fn require_method_schema(
    env: &Env,
    expected: &TypeSchema,
    actual: &TypeSchema,
    span: Span,
    method: ResultMethod,
    position: &str,
) -> Result<(), CompileError> {
    let mismatch = super::typing::schema_assignment_mismatch(
        &env.function_list,
        &env.function_impls,
        &env.struct_schemas,
        expected,
        actual,
    );
    if let Some(detail) = mismatch {
        return Err(result_error(
            span,
            format!(
                ".{} {} expects {} but found {} ({detail})",
                method.name(),
                position,
                render_schema(expected),
                render_schema(actual)
            ),
        ));
    }
    Ok(())
}

/// Infer the callback/fallback argument's callable schema, giving an inline
/// closure its contextual parameter types so strict typing never depends on a
/// placeholder `Unknown` parameter.
fn method_callback_schema(
    argument: &Expr,
    param: &TypeSchema,
    span: Span,
    method: ResultMethod,
    env: &Env,
) -> Result<TypeSchema, CompileError> {
    // Callable-producing expressions must contribute a callable schema whose
    // closure parameters are the contextually expected ones. Inferring an
    // `if`/`match`/block of closures from the outer state would otherwise
    // produce `Unknown` parameters for unannotated closure parameters.
    match argument {
        Expr::Closure(closure) => {
            let mut nested = env.slots.clone();
            nested.extend(super::typing::contextual_callable_slot_schemas(
                &env.function_list,
                &env.function_impls,
                &env.struct_schemas,
                &env.slots,
                &closure.param_slots,
                Some(std::slice::from_ref(param)),
                &closure.capture_copies,
            ));
            infer_callable_callback(argument, &nested, span, method, env)
        }
        Expr::IfElse {
            then_expr,
            else_expr,
            ..
        } => {
            let then_schema = method_callback_schema(then_expr, param, span, method, env)?;
            let else_schema = method_callback_schema(else_expr, param, span, method, env)?;
            merge_callable_branches(then_schema, else_schema, span, method)
        }
        Expr::Match { arms, default, .. } => {
            let mut merged: Option<TypeSchema> = None;
            for (_, arm) in arms {
                let arm_schema = method_callback_schema(arm, param, span, method, env)?;
                merged = Some(match merged {
                    Some(existing) => merge_callable_branches(existing, arm_schema, span, method)?,
                    None => arm_schema,
                });
            }
            let default_schema = method_callback_schema(default, param, span, method, env)?;
            match merged {
                Some(existing) => merge_callable_branches(existing, default_schema, span, method),
                None => Ok(default_schema),
            }
        }
        Expr::Block { expr, .. } => method_callback_schema(expr, param, span, method, env),
        _ => infer_callable_callback(argument, &env.slots, span, method, env),
    }
}

fn infer_callable_callback(
    argument: &Expr,
    slots: &HashMap<LocalSlot, TypeSchema>,
    span: Span,
    method: ResultMethod,
    env: &Env,
) -> Result<TypeSchema, CompileError> {
    let inferred = super::typing::infer_value_schema(
        &env.function_list,
        &env.function_impls,
        &env.struct_schemas,
        argument,
        slots,
    )
    .ok_or_else(|| {
        result_error(
            span,
            format!(
                ".{} requires a typed callable callback, found an untyped value",
                method.name()
            ),
        )
    })?;
    if !matches!(inferred, TypeSchema::Callable { .. }) {
        return Err(result_error(
            span,
            format!(
                ".{} requires a callable callback, found {}",
                method.name(),
                render_schema(&inferred)
            ),
        ));
    }
    Ok(inferred)
}

fn merge_callable_branches(
    then_schema: TypeSchema,
    else_schema: TypeSchema,
    span: Span,
    method: ResultMethod,
) -> Result<TypeSchema, CompileError> {
    if then_schema != else_schema {
        return Err(result_error(
            span,
            format!(
                ".{} callback branches produce different callable schemas: {} vs {}",
                method.name(),
                render_schema(&then_schema),
                render_schema(&else_schema)
            ),
        ));
    }
    Ok(then_schema)
}

/// The unselected branch of a combinator has no owner destination for
/// resources moved into an inline closure capture cell, so that composition
/// is rejected rather than silently leaked or copied. Function values and
/// already-bound callable locals keep their existing owner.
fn reject_resource_capturing_closure(
    argument: &Expr,
    span: Span,
    method: ResultMethod,
    env: &Env,
) -> Result<(), CompileError> {
    let Some(schema) = find_resource_capturing_closure(argument, env) else {
        return Ok(());
    };
    Err(result_error(
        span,
        format!(
            ".{} callback/fallback cannot move a resource value into a closure capture ({}): the unselected branch has no destination for that owner; pass a named function or bind the callable before the method",
            method.name(),
            render_schema(&schema)
        ),
    ))
}

fn find_resource_capturing_closure(expr: &Expr, env: &Env) -> Option<TypeSchema> {
    match expr {
        Expr::Closure(closure) => closure.capture_copies.iter().find_map(|(source, _)| {
            env.slots
                .get(source)
                .filter(|schema| schema.contains_resource_with_named_types(&env.struct_schemas))
                .cloned()
        }),
        // An immediately invoked closure consumes its captures in the call.
        Expr::ClosureCall(closure, args) => {
            let _ = closure;
            args.iter()
                .find_map(|child| find_resource_capturing_closure(child, env))
        }
        Expr::Block { stmts, expr } => stmts
            .iter()
            .find_map(|stmt| stmt_resource_capturing_closure(stmt, env))
            .or_else(|| find_resource_capturing_closure(expr, env)),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => [condition.as_ref(), then_expr, else_expr]
            .into_iter()
            .find_map(|child| find_resource_capturing_closure(child, env)),
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => find_resource_capturing_closure(value, env)
            .or_else(|| {
                arms.iter()
                    .find_map(|(_, arm)| find_resource_capturing_closure(arm, env))
            })
            .or_else(|| find_resource_capturing_closure(default, env)),
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _) => args
            .iter()
            .find_map(|child| find_resource_capturing_closure(child, env)),
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => find_resource_capturing_closure(lhs, env)
            .or_else(|| find_resource_capturing_closure(rhs, env)),
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => find_resource_capturing_closure(inner, env),
        Expr::ResultConstruct { payload, .. } => find_resource_capturing_closure(payload, env),
        Expr::ResultQuery { value, .. } => find_resource_capturing_closure(value, env),
        Expr::ResultTry { value, .. } => find_resource_capturing_closure(value, env),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => find_resource_capturing_closure(value, env)
            .or_else(|| find_resource_capturing_closure(ok_expr, env))
            .or_else(|| find_resource_capturing_closure(err_expr, env)),
        Expr::ResultMethod {
            receiver, argument, ..
        } => find_resource_capturing_closure(receiver, env)
            .or_else(|| find_resource_capturing_closure(argument, env)),
        Expr::OptionalGet { container, key, .. } => find_resource_capturing_closure(container, env)
            .or_else(|| find_resource_capturing_closure(key, env)),
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => find_resource_capturing_closure(value, env)
            .or_else(|| find_resource_capturing_closure(fallback, env)),
        Expr::Null
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
        | Expr::MoveIndex { .. } => None,
    }
}

fn stmt_resource_capturing_closure(stmt: &Stmt, env: &Env) -> Option<TypeSchema> {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => None,
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            find_resource_capturing_closure(expr, env)
        }
        Stmt::ClosureLet { closure, .. } => closure
            .capture_copies
            .iter()
            .find_map(|(source, _)| {
                env.slots
                    .get(source)
                    .filter(|schema| schema.contains_resource_with_named_types(&env.struct_schemas))
                    .cloned()
            })
            .or_else(|| find_resource_capturing_closure(&closure.body, env)),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => find_resource_capturing_closure(condition, env)
            .or_else(|| {
                then_branch
                    .iter()
                    .find_map(|nested| stmt_resource_capturing_closure(nested, env))
            })
            .or_else(|| {
                else_branch
                    .iter()
                    .find_map(|nested| stmt_resource_capturing_closure(nested, env))
            }),
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => stmt_resource_capturing_closure(init, env)
            .or_else(|| find_resource_capturing_closure(condition, env))
            .or_else(|| stmt_resource_capturing_closure(post, env))
            .or_else(|| {
                body.iter()
                    .find_map(|nested| stmt_resource_capturing_closure(nested, env))
            }),
        Stmt::While {
            condition, body, ..
        } => find_resource_capturing_closure(condition, env).or_else(|| {
            body.iter()
                .find_map(|nested| stmt_resource_capturing_closure(nested, env))
        }),
    }
}

fn resolve_construct(
    ok: bool,
    payload: &mut Expr,
    result_schema: &mut TypeSchema,
    span: Span,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    let explicit = match result_schema {
        TypeSchema::Result { .. } => Some(result_schema.clone()),
        _ => None,
    };
    let expected_schema = match (expected, explicit.as_ref()) {
        (Some(expected_schema @ TypeSchema::Result { .. }), Some(explicit_schema)) => {
            if !result_branches_partially_compatible(expected_schema, explicit_schema) {
                return Err(result_error(
                    span,
                    format!(
                        "Result::{} explicit schema does not match the expected Result schema",
                        ctor_name(ok)
                    ),
                ));
            }
            expected_schema.clone()
        }
        (Some(expected_schema @ TypeSchema::Result { .. }), None) => expected_schema.clone(),
        (Some(other), _) => {
            return Err(result_error(
                span,
                format!("Result::{} is not a value of type {other:?}", ctor_name(ok)),
            ));
        }
        (None, Some(explicit_schema)) => explicit_schema.clone(),
        (None, None) => {
            return Err(result_error(
                span,
                format!(
                    "Result::{} needs an explicit Result<T, E> annotation for the inactive branch",
                    ctor_name(ok)
                ),
            ));
        }
    };
    let expected_branches = branch_schemas(&expected_schema).expect("driver matched Result");
    let explicit_branches = explicit.as_ref().and_then(branch_schemas);
    let (expected_active, expected_inactive) = if ok {
        (&expected_branches.0, &expected_branches.1)
    } else {
        (&expected_branches.1, &expected_branches.0)
    };
    let (explicit_active, explicit_inactive) = match &explicit_branches {
        Some((ok_branch, err_branch)) if ok => (Some(ok_branch), Some(err_branch)),
        Some((ok_branch, err_branch)) => (Some(err_branch), Some(ok_branch)),
        None => (None, None),
    };
    // A branch that is `Unknown` in the expectation is not a constraint; the
    // explicit schema or the payload itself supplies it. This is what lets a
    // method's fixed branch (for example `and_then`'s receiver error type)
    // contextualize a constructor inside a callback while the free branch
    // stays inferred.
    let concrete_branch = |expected_branch: &TypeSchema,
                           explicit_branch: Option<&TypeSchema>|
     -> Option<TypeSchema> {
        if !matches!(expected_branch, TypeSchema::Unknown) {
            return Some(expected_branch.clone());
        }
        explicit_branch
            .filter(|branch| !matches!(branch, TypeSchema::Unknown))
            .cloned()
    };
    let active_schema = match concrete_branch(expected_active, explicit_active) {
        Some(active_schema) => active_schema,
        None => {
            resolve_expr(payload, None, env)?;
            let actual = authoritative_schema(payload, env).ok_or_else(|| {
                result_error(
                    span,
                    format!("Result::{} payload type is unresolved", ctor_name(ok)),
                )
            })?;
            if schema_contains_unknown(&actual) {
                return Err(result_error(
                    span,
                    format!("Result::{} payload type is unresolved", ctor_name(ok)),
                ));
            }
            let inactive_schema = concrete_branch(expected_inactive, explicit_inactive)
                .unwrap_or(TypeSchema::Unknown);
            *result_schema = if ok {
                TypeSchema::Result {
                    ok: Box::new(actual),
                    err: Box::new(inactive_schema),
                }
            } else {
                TypeSchema::Result {
                    ok: Box::new(inactive_schema),
                    err: Box::new(actual),
                }
            };
            return Ok(());
        }
    };
    resolve_expr(payload, Some(&active_schema), env)?;
    if let Err(detail) = super::typing::check_result_payload(
        &env.function_list,
        &env.function_impls,
        &env.struct_schemas,
        payload,
        &active_schema,
        &env.slots,
    ) {
        return Err(result_error(
            span,
            format!("Result::{} payload: {detail}", ctor_name(ok)),
        ));
    }
    let inactive_schema =
        concrete_branch(expected_inactive, explicit_inactive).unwrap_or(TypeSchema::Unknown);
    *result_schema = if ok {
        TypeSchema::Result {
            ok: Box::new(active_schema),
            err: Box::new(inactive_schema),
        }
    } else {
        TypeSchema::Result {
            ok: Box::new(inactive_schema),
            err: Box::new(active_schema),
        }
    };
    Ok(())
}

fn result_branches_partially_compatible(expected: &TypeSchema, explicit: &TypeSchema) -> bool {
    let (Some((expected_ok, expected_err)), Some((explicit_ok, explicit_err))) =
        (branch_schemas(expected), branch_schemas(explicit))
    else {
        return false;
    };
    branch_partially_compatible(&expected_ok, &explicit_ok)
        && branch_partially_compatible(&expected_err, &explicit_err)
}

fn branch_partially_compatible(expected: &TypeSchema, explicit: &TypeSchema) -> bool {
    matches!(expected, TypeSchema::Unknown) || expected == explicit
}

// Each Result arm keeps its binding and expression explicit. The remaining
// arguments are the scrutinee, its schema, the span, the expected type, and env.
#[allow(clippy::too_many_arguments)]
fn resolve_match(
    value: &mut Expr,
    ok_binding: Option<LocalSlot>,
    ok_expr: &mut Expr,
    err_binding: Option<LocalSlot>,
    err_expr: &mut Expr,
    scrutinee_schema: &mut TypeSchema,
    span: Span,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    resolve_expr(value, None, env)?;
    let schema = authoritative_schema(value, env)
        .ok_or_else(|| result_error(span, "match scrutinee must be a Result<T, E>".to_string()))?;
    let TypeSchema::Result { ok, err } = &schema else {
        return Err(result_error(
            span,
            format!("match scrutinee has type {schema:?}, expected Result<T, E>"),
        ));
    };
    *scrutinee_schema = schema.clone();
    let saved = env.slots.clone();
    if let Some(slot) = ok_binding {
        env.slots.insert(slot, ok.as_ref().clone());
    }
    resolve_expr(ok_expr, expected, env)?;
    env.slots = saved.clone();
    if let Some(slot) = err_binding {
        env.slots.insert(slot, err.as_ref().clone());
    }
    resolve_expr(err_expr, expected, env)?;
    env.slots = saved;
    Ok(())
}

fn resolve_try(
    value: &mut Expr,
    operand_schema: &mut TypeSchema,
    return_schema: &mut TypeSchema,
    span: Span,
    env: &mut Env,
) -> Result<(), CompileError> {
    resolve_expr(value, None, env)?;
    let operand = operand_schema_of(value, env).ok_or_else(|| {
        result_error(
            span,
            "? operand must be Result<T, E>, found an untyped value".to_string(),
        )
    })?;
    let (operand_ok, operand_err) = match &operand {
        TypeSchema::Result { ok, err } => ((**ok).clone(), (**err).clone()),
        other => {
            return Err(result_error(
                span,
                format!(
                    "? operand must be Result<T, E>, found {}",
                    render_schema(other)
                ),
            ));
        }
    };
    let function_return = env.lexical_return.clone().ok_or_else(|| {
        result_error(
            span,
            "? requires the enclosing function to return Result<U, E>".to_string(),
        )
    })?;
    let function_err = match &function_return {
        TypeSchema::Result { err, .. } => (**err).clone(),
        other => {
            return Err(result_error(
                span,
                format!(
                    "? requires the enclosing function to return Result<U, E>, found {}",
                    render_schema(other)
                ),
            ));
        }
    };
    if operand_err != function_err {
        return Err(result_error(
            span,
            format!(
                "? error type {} is not {}",
                render_schema(&operand_err),
                render_schema(&function_err)
            ),
        ));
    }
    *operand_schema = TypeSchema::Result {
        ok: Box::new(operand_ok),
        err: Box::new(operand_err),
    };
    *return_schema = function_return;
    Ok(())
}

fn operand_schema_of(expr: &Expr, env: &Env) -> Option<TypeSchema> {
    if let Some(schema) = authoritative_schema(expr, env) {
        return Some(schema);
    }
    match expr {
        Expr::Call(index, type_args, _, resolution, _) => {
            if let Some(resolved) = resolution.as_deref() {
                return Some(resolved.return_type.clone());
            }
            let function = env.functions.get(index)?;
            Some(substitute_schema(
                function.return_schema.clone()?,
                &function.type_params,
                type_args,
            ))
        }
        Expr::LocalCall(slot, type_args, _, _) => {
            let schema = env.slots.get(slot).cloned()?;
            callable_result_schema(&schema, type_args)
        }
        Expr::ModuleCall(symbol, type_args, _, _) => {
            let function = env
                .function_list
                .iter()
                .find(|function| function.symbol == Some(*symbol))?;
            Some(substitute_schema(
                function.return_schema.clone()?,
                &function.type_params,
                type_args,
            ))
        }
        _ => None,
    }
}

fn callable_result_schema(schema: &TypeSchema, _type_args: &[TypeSchema]) -> Option<TypeSchema> {
    match schema {
        TypeSchema::Callable { result, .. } => Some((**result).clone()),
        _ => None,
    }
}

fn substitute_schema(schema: TypeSchema, params: &[String], args: &[TypeSchema]) -> TypeSchema {
    if params.is_empty() || args.is_empty() {
        return schema;
    }
    let bindings: Vec<(&str, &TypeSchema)> = params
        .iter()
        .zip(args.iter())
        .map(|(name, schema)| (name.as_str(), schema))
        .collect();
    substitute_schema_bindings(schema, &bindings)
}

fn substitute_schema_bindings(schema: TypeSchema, bindings: &[(&str, &TypeSchema)]) -> TypeSchema {
    match schema {
        TypeSchema::GenericParam(name) => bindings
            .iter()
            .find(|(param, _)| *param == name)
            .map(|(_, replacement)| (*replacement).clone())
            .unwrap_or(TypeSchema::GenericParam(name)),
        TypeSchema::Result { ok, err } => TypeSchema::Result {
            ok: Box::new(substitute_schema_bindings(*ok, bindings)),
            err: Box::new(substitute_schema_bindings(*err, bindings)),
        },
        TypeSchema::Optional(inner) => {
            TypeSchema::Optional(Box::new(substitute_schema_bindings(*inner, bindings)))
        }
        TypeSchema::Array(inner) => {
            TypeSchema::Array(Box::new(substitute_schema_bindings(*inner, bindings)))
        }
        TypeSchema::Map(inner) => {
            TypeSchema::Map(Box::new(substitute_schema_bindings(*inner, bindings)))
        }
        TypeSchema::Named(name, args) => TypeSchema::Named(
            name,
            args.into_iter()
                .map(|schema| substitute_schema_bindings(schema, bindings))
                .collect(),
        ),
        TypeSchema::Callable { params, result } => TypeSchema::Callable {
            params: params
                .into_iter()
                .map(|schema| substitute_schema_bindings(schema, bindings))
                .collect(),
            result: Box::new(substitute_schema_bindings(*result, bindings)),
        },
        other => other,
    }
}

fn render_schema(schema: &TypeSchema) -> String {
    super::typing::render_schema_label(schema)
}

fn authoritative_schema(expr: &Expr, env: &Env) -> Option<TypeSchema> {
    infer_schema(expr, env)
        .or_else(|| {
            super::typing::infer_value_schema(
                &env.function_list,
                &env.function_impls,
                &env.struct_schemas,
                expr,
                &env.slots,
            )
        })
        .filter(|schema| !matches!(schema, TypeSchema::Unknown))
}

fn resolve_closure(
    closure: &mut ClosureExpr,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    let (param_schemas, expected_return) = match expected {
        Some(TypeSchema::Callable { params, result }) => {
            (Some(params.as_slice()), Some(result.as_ref()))
        }
        other => (None, other),
    };
    let nested = super::typing::contextual_callable_slot_schemas(
        &env.function_list,
        &env.function_impls,
        &env.struct_schemas,
        &env.slots,
        &closure.param_slots,
        param_schemas,
        &closure.capture_copies,
    );
    let saved = std::mem::replace(&mut env.slots, nested);
    let saved_return = env.lexical_return.clone();
    env.lexical_return = match expected_return {
        Some(schema @ TypeSchema::Result { .. }) => Some(schema.clone()),
        _ => None,
    };
    let resolved = resolve_expr(&mut closure.body, expected_return, env);
    env.slots = saved;
    env.lexical_return = saved_return;
    resolved
}

fn require_result_schema(
    schema: Option<&TypeSchema>,
    span: Span,
    site: &str,
) -> Result<(), CompileError> {
    match schema {
        Some(TypeSchema::Result { .. }) => Ok(()),
        Some(other) => Err(result_error(
            span,
            format!("{site} requires Result<T, E>, found {other:?}"),
        )),
        None => Err(result_error(
            span,
            format!("{site} requires a Result<T, E> value"),
        )),
    }
}

fn infer_schema(expr: &Expr, env: &Env) -> Option<TypeSchema> {
    match expr {
        Expr::Null => Some(TypeSchema::Null),
        Expr::Int(_) => Some(TypeSchema::Int),
        Expr::Float(_) => Some(TypeSchema::Float),
        Expr::Bool(_) | Expr::ResultQuery { .. } => Some(TypeSchema::Bool),
        Expr::String(_) => Some(TypeSchema::String),
        Expr::Bytes(_) => Some(TypeSchema::Bytes),
        Expr::Var(slot) | Expr::MoveVar(slot) => env.slots.get(slot).cloned(),
        Expr::ResultConstruct { result_schema, .. } => Some(result_schema.clone()),
        Expr::Add(lhs, rhs) | Expr::Sub(lhs, rhs) | Expr::Mul(lhs, rhs) => {
            match (infer_schema(lhs, env), infer_schema(rhs, env)) {
                (Some(TypeSchema::Int), Some(TypeSchema::Int)) => Some(TypeSchema::Int),
                (Some(TypeSchema::Float), Some(TypeSchema::Float | TypeSchema::Int))
                | (Some(TypeSchema::Int), Some(TypeSchema::Float)) => Some(TypeSchema::Float),
                _ => None,
            }
        }
        Expr::Block { expr, .. } => infer_schema(expr, env),
        Expr::IfElse {
            then_expr,
            else_expr,
            ..
        } => {
            let then_schema = infer_schema(then_expr, env)?;
            let else_schema = infer_schema(else_expr, env)?;
            if then_schema == else_schema {
                Some(then_schema)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn ctor_name(ok: bool) -> &'static str {
    if ok { "Ok" } else { "Err" }
}

fn result_error(span: Span, detail: String) -> CompileError {
    CompileError::StrictTypingRequired {
        line: None,
        source_name: None,
        detail,
        span: Some(span),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValueType;
    use crate::compiler::{CompileError, ModuleId, SymbolId};

    fn result_int_string() -> TypeSchema {
        TypeSchema::Result {
            ok: Box::new(TypeSchema::Int),
            err: Box::new(TypeSchema::String),
        }
    }

    fn ir_with(stmts: Vec<Stmt>, functions: Vec<FunctionDecl>) -> FrontendIr {
        FrontendIr {
            stmts,
            locals: 0,
            local_bindings: Vec::new(),
            struct_schemas: HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions,
            function_impls: HashMap::new(),
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        }
    }

    fn constructor(ok: bool, payload: Expr) -> Expr {
        Expr::ResultConstruct {
            ok,
            payload: Box::new(payload),
            result_schema: TypeSchema::Unknown,
            span: Span::new(0, 0, 1),
            semantic_id: None,
        }
    }

    #[test]
    fn module_call_formal_fills_result_constructor_before_merge() {
        let symbol = SymbolId {
            module: ModuleId(1),
            index: 0,
        };
        let mut ir = ir_with(
            vec![Stmt::Expr {
                expr: Expr::ModuleCall(
                    symbol,
                    Vec::new(),
                    vec![constructor(true, Expr::Int(6))],
                    None,
                ),
                line: 1,
            }],
            vec![FunctionDecl {
                name: "take".to_string(),
                arity: 1,
                index: 0,
                args: vec!["value".to_string()],
                arg_schemas: vec![Some(result_int_string())],
                return_schema: Some(TypeSchema::Int),
                type_params: Vec::new(),
                exported: true,
                return_type: ValueType::Int,
                symbol: Some(symbol),
            }],
        );
        resolve_result_types(&mut ir, &[])
            .expect("formal Result schema should fill the constructor");
        let Stmt::Expr {
            expr: Expr::ModuleCall(_, _, args, _),
            ..
        } = &ir.stmts[0]
        else {
            panic!("module call should remain until merge");
        };
        let Expr::ResultConstruct { result_schema, .. } = &args[0] else {
            panic!("argument should stay a constructor");
        };
        assert!(matches!(result_schema, TypeSchema::Result { .. }));
    }

    #[test]
    fn module_call_wrong_payload_is_rejected_from_formal() {
        let symbol = SymbolId {
            module: ModuleId(1),
            index: 0,
        };
        let mut ir = ir_with(
            vec![Stmt::Expr {
                expr: Expr::ModuleCall(
                    symbol,
                    Vec::new(),
                    vec![constructor(true, Expr::String("no".to_string()))],
                    None,
                ),
                line: 1,
            }],
            vec![FunctionDecl {
                name: "take".to_string(),
                arity: 1,
                index: 0,
                args: vec!["value".to_string()],
                arg_schemas: vec![Some(result_int_string())],
                return_schema: Some(TypeSchema::Int),
                type_params: Vec::new(),
                exported: true,
                return_type: ValueType::Int,
                symbol: Some(symbol),
            }],
        );
        let error = resolve_result_types(&mut ir, &[]).expect_err("wrong payload must be rejected");
        let CompileError::StrictTypingRequired { detail, .. } = error else {
            panic!("unexpected error {error:?}");
        };
        assert!(
            (detail.contains("schema") || detail.contains("assigned") || detail.contains("string"))
                && !detail.contains("inactive branch"),
            "{detail}"
        );
    }
}
