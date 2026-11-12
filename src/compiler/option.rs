//! Guest `Option<T>` resolution and Unit typing owner.
//!
//! Runtime Option values are ordinary maps with a Boolean `tag` (`true` =
//! Some) and an active `payload` only on Some. This pass fills constructor
//! schemas from annotations and rejects non-exhaustive or mistyped Option
//! matches before bytecode emission. It never treats an arbitrary map, an
//! explicit null or the legacy nullable Optional schema as an Option.

use std::collections::HashMap;

use super::CompileError;
use super::ir::{
    AssignmentKind, ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt,
    StructDecl, TypeSchema,
};
use super::source_map::Span;

pub(crate) const OPTION_TAG_KEY: &str = "tag";
pub(crate) const OPTION_PAYLOAD_KEY: &str = "payload";

struct Env {
    slots: HashMap<LocalSlot, TypeSchema>,
    functions: HashMap<u16, FunctionDecl>,
    function_list: Vec<FunctionDecl>,
    function_impls: HashMap<u16, FunctionImpl>,
    struct_schemas: HashMap<String, super::ir::StructDecl>,
    lexical_return: Option<TypeSchema>,
}

pub(crate) fn resolve_option_types(
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
        env.lexical_return = None;
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
        Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::FuncDecl { .. } => {
            Ok(())
        }
        Stmt::Let {
            index,
            declared_schema,
            expr,
            ..
        } => {
            resolve_expr(expr, declared_schema.as_ref(), env)?;
            let inferred = declared_schema
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
                });
            if let Some(schema) = inferred.filter(|schema| !matches!(schema, TypeSchema::Unknown)) {
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
            if !matches!(kind, AssignmentKind::Set) {
                let _ = (expected_tail, line);
            }
            let expected = env.slots.get(index).cloned();
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
        Expr::OptionConstruct {
            some,
            payload,
            option_schema,
            span,
            ..
        } => resolve_construct(
            *some,
            payload.as_deref_mut(),
            option_schema,
            *span,
            expected,
            env,
        ),
        Expr::OptionMatch {
            value,
            some_binding,
            some_expr,
            none_expr,
            scrutinee_schema,
            span,
            ..
        } => resolve_match(
            value,
            *some_binding,
            some_expr,
            none_expr,
            scrutinee_schema,
            *span,
            expected,
            env,
        ),
        Expr::ResultConstruct {
            ok,
            result_schema,
            payload,
            ..
        } => {
            // The Result owner ran first and filled both branches. Resolve the
            // selected payload with its own schema so Option constructors
            // nested under Result::Ok/Err keep their context.
            let active = match result_schema {
                TypeSchema::Result {
                    ok: ok_branch,
                    err: err_branch,
                } => Some(if *ok {
                    ok_branch.as_ref().clone()
                } else {
                    err_branch.as_ref().clone()
                }),
                _ => None,
            };
            resolve_expr(payload, active.as_ref(), env)
        }
        Expr::ResultMatch {
            value,
            ok_binding,
            ok_expr,
            err_binding,
            err_expr,
            scrutinee_schema,
            ..
        } => {
            resolve_expr(value, None, env)?;
            let saved = env.slots.clone();
            if let TypeSchema::Result { ok, .. } = scrutinee_schema.clone()
                && let Some(slot) = ok_binding.as_ref()
            {
                env.slots.insert(*slot, ok.as_ref().clone());
            }
            resolve_expr(ok_expr, expected, env)?;
            env.slots = saved.clone();
            if let TypeSchema::Result { err, .. } = scrutinee_schema.clone()
                && let Some(slot) = err_binding.as_ref()
            {
                env.slots.insert(*slot, err.as_ref().clone());
            }
            resolve_expr(err_expr, expected, env)?;
            env.slots = saved;
            Ok(())
        }
        Expr::ResultQuery { value, .. } => resolve_expr(value, None, env),
        Expr::ResultMethod {
            receiver, argument, ..
        } => {
            // The Result pass owns lowering for every local method node. A
            // residual node is unresolved structural input; keep its children
            // available to the Option constructor/match resolver.
            resolve_expr(receiver, None, env)?;
            resolve_expr(argument, None, env)
        }
        Expr::OptionMethod {
            receiver, argument, ..
        } => {
            resolve_expr(receiver, None, env)?;
            resolve_expr(argument, None, env)
        }
        Expr::ResultTry {
            value,
            operand_schema,
            return_schema,
            span,
            ..
        } => resolve_option_try(value, operand_schema, return_schema, *span, env),
        Expr::ResultConvert { value, .. } => resolve_expr(value, None, env),
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
        Expr::OptionalGet {
            logical_projection,
            option_receiver,
            logical_unit_field,
            container,
            key,
            ..
        } => {
            resolve_expr(container, None, env)?;
            resolve_expr(key, None, env)?;
            if *logical_projection {
                match authoritative_schema(container, env) {
                    Some(TypeSchema::Option(_)) => *option_receiver = true,
                    Some(_) => *option_receiver = false,
                    None => {}
                }
                let container_schema = match authoritative_schema(container, env) {
                    Some(TypeSchema::Option(inner)) | Some(TypeSchema::Optional(inner))
                        if *logical_projection =>
                    {
                        Some(inner.as_ref().clone())
                    }
                    other => other,
                };
                if matches!(key.as_ref(), Expr::String(_))
                    && matches!(
                        container_schema,
                        Some(TypeSchema::Object(_)) | Some(TypeSchema::Named(_, _))
                    )
                    && let Some(container_schema) = container_schema
                    && let Some(access) = super::typing::infer_access_value_schema(
                        &env.function_list,
                        &env.function_impls,
                        &env.struct_schemas,
                        &container_schema,
                        key.as_ref(),
                        &env.slots,
                    )
                {
                    *logical_unit_field = matches!(access, TypeSchema::Unit);
                }
            }
            Ok(())
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            resolve_expr(value, None, env)?;
            resolve_expr(fallback, expected, env)
        }
        Expr::Call(index, type_args, args, resolution, _) => {
            let expected_args = resolution.as_ref().map(|resolved| {
                resolved
                    .params
                    .iter()
                    .map(|param| Some(param.schema.clone()))
                    .collect::<Vec<_>>()
            });
            let expected_args = expected_args
                .or_else(|| {
                    super::typing::argument_schemas_for_call(
                        &env.function_list,
                        &env.function_impls,
                        &env.struct_schemas,
                        *index,
                        type_args,
                    )
                })
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
        Expr::Unit
        | Expr::Null
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

fn resolve_construct(
    some: bool,
    payload: Option<&mut Expr>,
    option_schema: &mut TypeSchema,
    span: Span,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    let explicit = match option_schema {
        TypeSchema::Option(_) => Some(option_schema.clone()),
        _ => None,
    };
    let expected_schema = match (expected, explicit.as_ref()) {
        (Some(expected @ TypeSchema::Option(expected_inner)), Some(explicit_schema)) => {
            let TypeSchema::Option(explicit_inner) = explicit_schema else {
                unreachable!("matched Option");
            };
            if !matches!(expected_inner.as_ref(), TypeSchema::Unknown)
                && !matches!(explicit_inner.as_ref(), TypeSchema::Unknown)
                && expected_inner.as_ref() != explicit_inner.as_ref()
            {
                return Err(option_error(
                    span,
                    "Option constructor explicit schema does not match the expected Option schema"
                        .to_string(),
                ));
            }
            expected.clone()
        }
        (Some(expected @ TypeSchema::Option(_)), None) => expected.clone(),
        (Some(other), _) => {
            return Err(option_error(
                span,
                format!("Option::{} is not a value of type {other}", ctor_name(some)),
            ));
        }
        (None, Some(explicit_schema)) => explicit_schema.clone(),
        (None, None) if some => {
            // Some may infer its payload when that payload has a fully known
            // schema; None can never invent its missing type argument.
            let Some(payload) = payload else {
                unreachable!("Some has a payload");
            };
            resolve_expr(payload, None, env)?;
            let actual = infer_schema(payload, env)
                .or_else(|| {
                    super::typing::infer_value_schema(
                        &env.function_list,
                        &env.function_impls,
                        &env.struct_schemas,
                        payload,
                        &env.slots,
                    )
                })
                .ok_or_else(|| {
                    option_error(
                        span,
                        "Option::Some payload type is unresolved; annotate Option<T>".to_string(),
                    )
                })?;
            if schema_contains_unknown(&actual) || matches!(actual, TypeSchema::GenericParam(_)) {
                return Err(option_error(
                    span,
                    "Option::Some payload type is unresolved; annotate Option<T>".to_string(),
                ));
            }
            *option_schema = TypeSchema::Option(Box::new(actual));
            return Ok(());
        }
        (None, None) => {
            return Err(option_error(
                span,
                "Option::None needs an Option<T> annotation".to_string(),
            ));
        }
    };
    let TypeSchema::Option(inner) = &expected_schema else {
        unreachable!("driver matched Option");
    };
    let inner = inner.as_ref().clone();
    if !some {
        if matches!(inner, TypeSchema::Unknown) {
            return Err(option_error(
                span,
                "Option::None needs a resolved Option<T> annotation".to_string(),
            ));
        }
        *option_schema = expected_schema;
        return Ok(());
    }
    let Some(payload) = payload else {
        unreachable!("Some has a payload");
    };
    let active = if matches!(inner, TypeSchema::Unknown) {
        resolve_expr(payload, None, env)?;
        let actual = infer_schema(payload, env)
            .or_else(|| {
                super::typing::infer_value_schema(
                    &env.function_list,
                    &env.function_impls,
                    &env.struct_schemas,
                    payload,
                    &env.slots,
                )
            })
            .ok_or_else(|| {
                option_error(span, "Option::Some payload type is unresolved".to_string())
            })?;
        if schema_contains_unknown(&actual) || matches!(actual, TypeSchema::GenericParam(_)) {
            return Err(option_error(
                span,
                "Option::Some payload type is unresolved".to_string(),
            ));
        }
        actual
    } else {
        resolve_expr(payload, Some(&inner), env)?;
        if !empty_container_payload_matches(payload, &inner)
            && let Err(detail) = super::typing::check_result_payload(
                &env.function_list,
                &env.function_impls,
                &env.struct_schemas,
                payload,
                &inner,
                &env.slots,
            )
        {
            return Err(option_error(
                span,
                format!("Option::Some payload: {detail}"),
            ));
        }
        inner
    };
    *option_schema = TypeSchema::Option(Box::new(active));
    Ok(())
}

/// An empty array/map literal has no element whose type could contradict the
/// contextual element schema, so its inferred `Array<Unknown>`/`Map<Unknown>`
/// shape is compatible with the expected container payload.
fn empty_container_payload_matches(payload: &Expr, expected: &TypeSchema) -> bool {
    let Expr::Call(index, _, args, _, _) = payload else {
        return false;
    };
    if !args.is_empty() {
        return false;
    }
    matches!(
        (crate::BuiltinFunction::from_call_index(*index), expected),
        (Some(crate::BuiltinFunction::ArrayNew), TypeSchema::Array(_))
            | (Some(crate::BuiltinFunction::MapNew), TypeSchema::Map(_))
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_match(
    value: &mut Expr,
    some_binding: Option<LocalSlot>,
    some_expr: &mut Expr,
    none_expr: &mut Expr,
    scrutinee_schema: &mut TypeSchema,
    span: Span,
    expected: Option<&TypeSchema>,
    env: &mut Env,
) -> Result<(), CompileError> {
    resolve_expr(value, None, env)?;
    let schema = authoritative_schema(value, env)
        .ok_or_else(|| option_error(span, "match scrutinee must be an Option<T>".to_string()))?;
    let TypeSchema::Option(inner) = &schema else {
        return Err(option_error(
            span,
            format!("match scrutinee has type {schema}, expected Option<T>"),
        ));
    };
    *scrutinee_schema = schema.clone();
    let saved = env.slots.clone();
    if let Some(slot) = some_binding {
        env.slots.insert(slot, inner.as_ref().clone());
    }
    resolve_expr(some_expr, expected, env)?;
    env.slots = saved;
    resolve_expr(none_expr, expected, env)
}

fn resolve_option_try(
    value: &mut Expr,
    operand_schema: &mut TypeSchema,
    return_schema: &mut TypeSchema,
    span: Span,
    env: &mut Env,
) -> Result<(), CompileError> {
    if matches!(operand_schema, TypeSchema::Result { .. }) {
        // The Result pass already accepted and typed this operand.
        return Ok(());
    }
    let lexical_option = match env.lexical_return.clone() {
        Some(schema @ TypeSchema::Option(_)) => Some(schema),
        _ => None,
    };
    resolve_expr(value, lexical_option.as_ref(), env)?;
    let operand = authoritative_schema(value, env).ok_or_else(|| {
        option_error(
            span,
            "? operand must be a logical Option<T> when the enclosing callable returns Option<U>"
                .to_string(),
        )
    })?;
    let TypeSchema::Option(_) = &operand else {
        let lexical_label = env
            .lexical_return
            .as_ref()
            .map(super::result::render_schema)
            .unwrap_or_else(|| "a non-Option callable".to_string());
        return Err(option_error(
            span,
            format!(
                "? operand must be a logical Option<T> in an Option-returning callable, found {} with enclosing return {}",
                super::result::render_schema(&operand),
                lexical_label
            ),
        ));
    };
    let Some(return_schema_value) = lexical_option else {
        let found = env
            .lexical_return
            .as_ref()
            .map(super::result::render_schema)
            .unwrap_or_else(|| "a non-Option callable".to_string());
        return Err(option_error(
            span,
            format!(
                "? requires the enclosing function or closure to return Option<U>, found {}; convert with .ok()? before Option propagation",
                found
            ),
        ));
    };
    *operand_schema = operand;
    *return_schema = return_schema_value;
    Ok(())
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
    env.lexical_return = expected_return.cloned();
    let resolved = resolve_expr(&mut closure.body, expected_return, env);
    env.slots = saved;
    env.lexical_return = saved_return;
    resolved
}

fn infer_schema(expr: &Expr, env: &Env) -> Option<TypeSchema> {
    match expr {
        Expr::Unit => Some(TypeSchema::Unit),
        Expr::Null => Some(TypeSchema::Null),
        Expr::Int(_) => Some(TypeSchema::Int),
        Expr::Float(_) => Some(TypeSchema::Float),
        Expr::Bool(_) => Some(TypeSchema::Bool),
        Expr::String(_) => Some(TypeSchema::String),
        Expr::Bytes(_) => Some(TypeSchema::Bytes),
        Expr::Var(slot) | Expr::MoveVar(slot) => env.slots.get(slot).cloned(),
        Expr::OptionConstruct { option_schema, .. } => Some(option_schema.clone()),
        Expr::ResultConstruct { result_schema, .. } => Some(result_schema.clone()),
        Expr::ResultTry { operand_schema, .. } => match operand_schema {
            TypeSchema::Option(inner) => Some(inner.as_ref().clone()),
            TypeSchema::Result { ok, .. } => Some(ok.as_ref().clone()),
            _ => None,
        },
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

fn schema_contains_unknown(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Unknown => true,
        TypeSchema::Optional(inner)
        | TypeSchema::Option(inner)
        | TypeSchema::Array(inner)
        | TypeSchema::Map(inner) => schema_contains_unknown(inner),
        TypeSchema::Named(_, args) | TypeSchema::ArrayTuple(args) => {
            args.iter().any(schema_contains_unknown)
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            prefix.iter().any(schema_contains_unknown) || schema_contains_unknown(rest)
        }
        TypeSchema::Object(fields) => fields.values().any(schema_contains_unknown),
        TypeSchema::Callable { params, result } => {
            params.iter().any(schema_contains_unknown) || schema_contains_unknown(result)
        }
        TypeSchema::Result { ok, err } => {
            schema_contains_unknown(ok) || schema_contains_unknown(err)
        }
        TypeSchema::Null
        | TypeSchema::Unit
        | TypeSchema::Int
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::Bool
        | TypeSchema::String
        | TypeSchema::Bytes
        | TypeSchema::Resource(_)
        | TypeSchema::GenericParam(_) => false,
    }
}

struct NormalizeEnv<'a> {
    function_list: &'a [FunctionDecl],
    function_impls: &'a HashMap<u16, FunctionImpl>,
    struct_schemas: &'a HashMap<String, StructDecl>,
    slots: HashMap<LocalSlot, TypeSchema>,
}

/// Materialize permitted omitted logical Option fields as explicit None map
/// entries at construction sites where the surrounding typed context supplies
/// the expected record schema. Generic named fields are instantiated through
/// the typing owner's existing substitution rules before an entry is added.
pub(crate) fn normalize_omitted_option_fields(ir: &mut FrontendIr) {
    let functions = ir.functions.clone();
    let structs = ir.struct_schemas.clone();
    let impls = ir.function_impls.clone();
    let mut env = NormalizeEnv {
        function_list: &functions,
        function_impls: &impls,
        struct_schemas: &structs,
        slots: HashMap::new(),
    };
    for stmt in &mut ir.stmts {
        normalize_stmt_omitted(stmt, None, &mut env);
    }
    let mut targets = std::mem::take(&mut ir.function_impls);
    for (index, implementation) in targets.iter_mut() {
        let Some(function) = functions.iter().find(|function| function.index == *index) else {
            continue;
        };
        env.slots.clear();
        for (slot, schema) in implementation
            .param_slots
            .iter()
            .zip(function.arg_schemas.iter())
        {
            if let Some(schema) = schema {
                env.slots.insert(*slot, schema.clone());
            }
        }
        let expected = function.return_schema.clone();
        for stmt in &mut implementation.body_stmts {
            normalize_stmt_omitted(stmt, expected.clone(), &mut env);
        }
        normalize_expr_omitted(&mut implementation.body_expr, expected.as_ref(), &mut env);
    }
    ir.function_impls = targets;
}

fn infer_omitted_schema(expr: &Expr, env: &NormalizeEnv<'_>) -> Option<TypeSchema> {
    super::typing::infer_value_schema(
        env.function_list,
        env.function_impls,
        env.struct_schemas,
        expr,
        &env.slots,
    )
    .filter(|schema| !matches!(schema, TypeSchema::Unknown))
}

fn normalize_stmt_omitted(
    stmt: &mut Stmt,
    expected_tail: Option<TypeSchema>,
    env: &mut NormalizeEnv<'_>,
) {
    match stmt {
        Stmt::Let {
            index,
            declared_schema,
            expr,
            ..
        } => {
            normalize_expr_omitted(expr, declared_schema.as_ref(), env);
            let inferred = declared_schema
                .clone()
                .or_else(|| infer_omitted_schema(expr, env));
            if let Some(schema) = inferred.filter(|schema| !matches!(schema, TypeSchema::Unknown)) {
                env.slots.insert(*index, schema);
            }
        }
        Stmt::Assign { index, expr, .. } => {
            let expected = env.slots.get(index).cloned();
            normalize_expr_omitted(expr, expected.as_ref(), env);
            if !env.slots.contains_key(index)
                && let Some(schema) = infer_omitted_schema(expr, env)
            {
                env.slots.insert(*index, schema);
            }
        }
        Stmt::Expr { expr, .. } => normalize_expr_omitted(expr, None, env),
        Stmt::ClosureLet {
            closure,
            expected_schema,
            ..
        } => normalize_closure_omitted(closure, expected_schema.as_ref(), env),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            normalize_expr_omitted(condition, Some(&TypeSchema::Bool), env);
            for nested in then_branch {
                normalize_stmt_omitted(nested, expected_tail.clone(), env);
            }
            for nested in else_branch {
                normalize_stmt_omitted(nested, expected_tail.clone(), env);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            normalize_stmt_omitted(init, None, env);
            normalize_expr_omitted(condition, Some(&TypeSchema::Bool), env);
            normalize_stmt_omitted(post, None, env);
            for nested in body {
                normalize_stmt_omitted(nested, None, env);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            normalize_expr_omitted(condition, Some(&TypeSchema::Bool), env);
            for nested in body {
                normalize_stmt_omitted(nested, None, env);
            }
        }
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, .. } => {
            env.slots.remove(index);
        }
    }
}

fn normalize_expr_omitted(
    expr: &mut Expr,
    expected: Option<&TypeSchema>,
    env: &mut NormalizeEnv<'_>,
) {
    if let Some(expected) = expected
        && normalize_map_literal_omitted(expr, expected, env)
    {
        return;
    }
    match expr {
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                normalize_stmt_omitted(stmt, expected.cloned(), env);
            }
            normalize_expr_omitted(expr, expected, env);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            normalize_expr_omitted(condition, Some(&TypeSchema::Bool), env);
            normalize_expr_omitted(then_expr, expected, env);
            normalize_expr_omitted(else_expr, expected, env);
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            normalize_expr_omitted(value, None, env);
            for (_, arm) in arms {
                normalize_expr_omitted(arm, expected, env);
            }
            normalize_expr_omitted(default, expected, env);
        }
        Expr::Call(index, type_args, args, resolution, _) => {
            let expected_args = resolution.as_ref().map(|resolved| {
                resolved
                    .params
                    .iter()
                    .map(|param| Some(param.schema.clone()))
                    .collect::<Vec<_>>()
            });
            let expected_args = expected_args
                .or_else(|| {
                    super::typing::argument_schemas_for_call(
                        env.function_list,
                        env.function_impls,
                        env.struct_schemas,
                        *index,
                        type_args,
                    )
                })
                .or_else(|| {
                    super::typing::expected_container_argument_schemas(
                        env.function_list,
                        env.function_impls,
                        env.struct_schemas,
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
                normalize_expr_omitted(arg, expected_arg, env);
            }
        }
        Expr::ModuleCall(symbol, type_args, args, _) => {
            let expected_args = env
                .function_list
                .iter()
                .find(|function| function.symbol.as_ref() == Some(symbol))
                .and_then(|function| {
                    super::typing::argument_schemas_for_call(
                        env.function_list,
                        env.function_impls,
                        env.struct_schemas,
                        function.index,
                        type_args,
                    )
                });
            for (offset, arg) in args.iter_mut().enumerate() {
                let expected_arg = expected_args
                    .as_ref()
                    .and_then(|schemas| schemas.get(offset))
                    .and_then(|schema| schema.as_ref());
                normalize_expr_omitted(arg, expected_arg, env);
            }
        }
        Expr::LocalCall(slot, _, args, _) => {
            let params = env.slots.get(slot).and_then(|schema| match schema {
                TypeSchema::Callable { params, .. } => Some(params.clone()),
                _ => None,
            });
            for (offset, arg) in args.iter_mut().enumerate() {
                let expected_arg = params.as_ref().and_then(|schemas| schemas.get(offset));
                normalize_expr_omitted(arg, expected_arg, env);
            }
        }
        Expr::ClosureCall(closure, args) => {
            normalize_closure_omitted(closure, expected, env);
            for arg in args.iter_mut() {
                normalize_expr_omitted(arg, None, env);
            }
        }
        Expr::Closure(closure) => normalize_closure_omitted(closure, expected, env),
        Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner)
        | Expr::Neg(inner)
        | Expr::Not(inner) => normalize_expr_omitted(inner, expected, env),
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
            normalize_expr_omitted(lhs, None, env);
            normalize_expr_omitted(rhs, None, env);
        }
        Expr::ResultConstruct {
            ok,
            payload,
            result_schema,
            ..
        } => {
            let active = match result_schema {
                TypeSchema::Result { ok: ok_branch, err } => Some(if *ok {
                    ok_branch.as_ref().clone()
                } else {
                    err.as_ref().clone()
                }),
                _ => expected.and_then(|schema| match schema {
                    TypeSchema::Result { ok: ok_branch, err } => Some(if *ok {
                        ok_branch.as_ref().clone()
                    } else {
                        err.as_ref().clone()
                    }),
                    _ => None,
                }),
            };
            normalize_expr_omitted(payload, active.as_ref(), env);
        }
        Expr::OptionConstruct {
            payload: Some(payload),
            option_schema,
            ..
        } => {
            let active = match option_schema {
                TypeSchema::Option(inner) => Some(inner.as_ref().clone()),
                _ => match expected {
                    Some(TypeSchema::Option(inner)) => Some(inner.as_ref().clone()),
                    _ => None,
                },
            };
            normalize_expr_omitted(payload, active.as_ref(), env);
        }
        Expr::OptionConstruct { payload: None, .. } => {}
        Expr::ResultMatch {
            value,
            ok_binding,
            ok_expr,
            err_binding,
            err_expr,
            scrutinee_schema,
            ..
        } => {
            normalize_expr_omitted(value, None, env);
            let saved = env.slots.clone();
            if let TypeSchema::Result { ok, .. } = scrutinee_schema.clone()
                && let Some(slot) = *ok_binding
            {
                env.slots.insert(slot, ok.as_ref().clone());
            }
            normalize_expr_omitted(ok_expr, expected, env);
            env.slots = saved.clone();
            if let TypeSchema::Result { err, .. } = scrutinee_schema.clone()
                && let Some(slot) = *err_binding
            {
                env.slots.insert(slot, err.as_ref().clone());
            }
            normalize_expr_omitted(err_expr, expected, env);
            env.slots = saved;
        }
        Expr::OptionMatch {
            value,
            some_binding,
            some_expr,
            none_expr,
            scrutinee_schema,
            ..
        } => {
            normalize_expr_omitted(value, None, env);
            let saved = env.slots.clone();
            if let TypeSchema::Option(inner) = scrutinee_schema.clone()
                && let Some(slot) = *some_binding
            {
                env.slots.insert(slot, inner.as_ref().clone());
            }
            normalize_expr_omitted(some_expr, expected, env);
            env.slots = saved;
            normalize_expr_omitted(none_expr, expected, env);
        }
        Expr::OptionalGet { container, key, .. } => {
            normalize_expr_omitted(container, None, env);
            normalize_expr_omitted(key, None, env);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            normalize_expr_omitted(value, None, env);
            normalize_expr_omitted(fallback, expected, env);
        }
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            normalize_expr_omitted(receiver, None, env);
            normalize_expr_omitted(argument, None, env);
        }
        Expr::ResultConvert { value, .. } | Expr::ResultTry { value, .. } => {
            normalize_expr_omitted(value, None, env);
        }
        Expr::ResultQuery { value, .. } => normalize_expr_omitted(value, None, env),
        Expr::Null
        | Expr::Unit
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
        | Expr::MoveIndex { .. } => {}
    }
}

fn normalize_closure_omitted(
    closure: &mut ClosureExpr,
    expected: Option<&TypeSchema>,
    env: &mut NormalizeEnv<'_>,
) {
    let (param_schemas, expected_return) = match expected {
        Some(TypeSchema::Callable { params, result }) => {
            (Some(params.as_slice()), Some(result.as_ref()))
        }
        other => (None, other),
    };
    let nested = super::typing::contextual_callable_slot_schemas(
        env.function_list,
        env.function_impls,
        env.struct_schemas,
        &env.slots,
        &closure.param_slots,
        param_schemas,
        &closure.capture_copies,
    );
    let saved = std::mem::replace(&mut env.slots, nested);
    normalize_expr_omitted(&mut closure.body, expected_return, env);
    env.slots = saved;
}

fn object_fields_for_normalization(
    expected: &TypeSchema,
    env: &NormalizeEnv<'_>,
) -> Option<HashMap<String, TypeSchema>> {
    super::typing::object_fields_for_schema(
        env.function_list,
        env.function_impls,
        env.struct_schemas,
        expected,
    )
}

fn normalize_map_literal_omitted(
    expr: &mut Expr,
    expected: &TypeSchema,
    env: &mut NormalizeEnv<'_>,
) -> bool {
    let Some(fields) = object_fields_for_normalization(expected, env) else {
        return false;
    };
    let mut keys = Vec::new();
    if !collect_map_literal_keys(expr, &mut keys) {
        return false;
    }
    normalize_map_literal_values(expr, &fields, env);
    let mut additions = Vec::new();
    for (name, schema) in &fields {
        if keys.iter().any(|key| key == name) {
            continue;
        }
        let TypeSchema::Option(_) = schema else {
            continue;
        };
        additions.push((name.clone(), schema.clone()));
    }
    if additions.is_empty() {
        return true;
    }
    additions.sort_unstable_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
    let original = std::mem::replace(expr, Expr::Null);
    let mut out = original;
    for (name, schema) in additions {
        out = Expr::Call(
            crate::BuiltinFunction::Set.call_index(),
            Vec::new(),
            vec![
                out,
                Expr::String(name),
                Expr::OptionConstruct {
                    some: false,
                    payload: None,
                    option_schema: schema,
                    span: super::source_map::Span::new(0, 0, 0),
                    semantic_id: None,
                },
            ],
            None,
            None,
        );
    }
    *expr = out;
    true
}

/// Recurse into the value positions of a recovered `MapNew` + `Set` literal
/// chain with each field's expected schema. The container positions are not
/// revisited with the record expectation: the chain has already been matched
/// at the construction root, and nested records are handled at their value.
fn normalize_map_literal_values(
    expr: &mut Expr,
    fields: &HashMap<String, TypeSchema>,
    env: &mut NormalizeEnv<'_>,
) {
    match expr {
        Expr::Call(index, _, args, _, _)
            if crate::BuiltinFunction::from_call_index(*index)
                == Some(crate::BuiltinFunction::MapNew) =>
        {
            if !args.is_empty() {
                for arg in args {
                    normalize_expr_omitted(arg, None, env);
                }
            }
        }
        Expr::Call(index, _, args, _, _)
            if matches!(
                crate::BuiltinFunction::from_call_index(*index),
                Some(crate::BuiltinFunction::Set)
            ) && args.len() == 3
                || matches!(
                    crate::BuiltinFunction::from_call_index(*index),
                    Some(crate::BuiltinFunction::MapEdit)
                ) && args.len() == 4 =>
        {
            normalize_map_literal_values(&mut args[0], fields, env);
            let expected_value = match &args[1] {
                Expr::String(key) => fields.get(key).cloned(),
                _ => None,
            };
            normalize_expr_omitted(&mut args[2], expected_value.as_ref(), env);
        }
        _ => {}
    }
}

/// Recover the string keys of a pure `MapNew` + `Set` literal chain. A map
/// variable or any non-literal root is not normalized.
fn collect_map_literal_keys(expr: &Expr, keys: &mut Vec<String>) -> bool {
    match expr {
        Expr::Call(index, _, args, _, _)
            if crate::BuiltinFunction::from_call_index(*index)
                == Some(crate::BuiltinFunction::MapNew) =>
        {
            args.is_empty()
        }
        Expr::Call(index, _, args, _, _)
            if matches!(
                crate::BuiltinFunction::from_call_index(*index),
                Some(crate::BuiltinFunction::Set)
            ) && args.len() == 3
                || matches!(
                    crate::BuiltinFunction::from_call_index(*index),
                    Some(crate::BuiltinFunction::MapEdit)
                ) && args.len() == 4 =>
        {
            if let Expr::String(key) = &args[1] {
                keys.push(key.clone());
            }
            collect_map_literal_keys(&args[0], keys)
        }
        _ => false,
    }
}

fn ctor_name(some: bool) -> &'static str {
    if some { "Some" } else { "None" }
}

fn option_error(span: Span, detail: String) -> CompileError {
    CompileError::StrictTypingRequired {
        line: None,
        source_name: None,
        detail,
        span: Some(span),
    }
}
