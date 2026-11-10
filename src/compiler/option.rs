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
    TypeSchema,
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
    if let Some(function) = env.functions.get(&index).cloned() {
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
        Expr::ResultTry { value, .. } => resolve_expr(value, None, env),
        Expr::ResultMethod {
            receiver, argument, ..
        } => {
            resolve_expr(receiver, None, env)?;
            resolve_expr(argument, None, env)
        }
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
    let resolved = resolve_expr(&mut closure.body, expected_return, env);
    env.slots = saved;
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
