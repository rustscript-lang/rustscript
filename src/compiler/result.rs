//! Guest `Result<T, E>` resolution.
//!
//! Runtime values are ordinary maps with `tag: bool` and the active
//! `payload`. A missing payload is null only when the active branch schema
//! allows null. This pass fills constructor schemas from annotations and
//! rejects non-exhaustive or mistyped Result matches before bytecode emission.

use std::collections::HashMap;

use super::CompileError;
use super::ir::{
    AssignmentKind, ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt,
    TypeSchema,
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
    let contextual = match (expected, explicit.as_ref()) {
        (Some(expected_schema @ TypeSchema::Result { .. }), Some(explicit_schema)) => {
            if !super::typing::result_branch_schemas_compatible(
                &env.function_list,
                &env.function_impls,
                &env.struct_schemas,
                expected_schema,
                explicit_schema,
            ) {
                return Err(result_error(
                    span,
                    format!(
                        "Result::{} explicit schema does not match the expected Result schema",
                        ctor_name(ok)
                    ),
                ));
            }
            Some(expected_schema.clone())
        }
        (Some(expected_schema @ TypeSchema::Result { .. }), None) => Some(expected_schema.clone()),
        (None, Some(explicit_schema)) => Some(explicit_schema.clone()),
        (Some(other), _) => {
            return Err(result_error(
                span,
                format!("Result::{} is not a value of type {other:?}", ctor_name(ok)),
            ));
        }
        (None, None) => None,
    };
    let Some(expected_schema) = contextual else {
        return Err(result_error(
            span,
            format!(
                "Result::{} needs an explicit Result<T, E> annotation for the inactive branch",
                ctor_name(ok)
            ),
        ));
    };
    let branch_expected = active_branch(&expected_schema, ok);
    resolve_expr(payload, branch_expected, env)?;
    let Some(active) = active_branch(&expected_schema, ok) else {
        return Err(result_error(
            span,
            "internal Result schema lost its active branch".to_string(),
        ));
    };
    if let Err(detail) = super::typing::check_result_payload(
        &env.function_list,
        &env.function_impls,
        &env.struct_schemas,
        payload,
        active,
        &env.slots,
    ) {
        return Err(result_error(
            span,
            format!("Result::{} payload: {detail}", ctor_name(ok)),
        ));
    }
    *result_schema = expected_schema;
    Ok(())
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

pub(crate) fn active_branch(schema: &TypeSchema, ok: bool) -> Option<&TypeSchema> {
    match schema {
        TypeSchema::Result { ok: ok_ty, err } => {
            Some(if ok { ok_ty.as_ref() } else { err.as_ref() })
        }
        _ => None,
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
