//! Classify map stores from checked schemas before ownership and codegen.
//!
//! Parser `local_schemas` only sees declared annotations. Inferred maps, factory
//! results, and generic parameters are known only after typing. This pass
//! rewrites those stores onto the private MapEdit insert path so Unit data is
//! kept. Proven non-null values and arrays stay on Set. An unresolved receiver
//! is not guessed: a possibly-null store is rejected instead of deleted.

use crate::BuiltinFunction;

use super::super::ir::{Expr, FrontendIr, Stmt, TypeSchema};
use super::super::{CompileError, ParseError};
use super::context::TypeContext;
use super::state::LocalTypeState;
use super::{EntryLocalType, bound_type_from_schema};

pub(crate) enum MapAdmissionError {
    Compile(CompileError),
    Parse(ParseError),
}

pub(crate) fn classify_map_stores(
    ir: &mut FrontendIr,
    typing_mode: super::super::TypingMode,
    entry_local_types: &[EntryLocalType],
) -> Result<(), MapAdmissionError> {
    let functions = ir.functions.clone();
    let function_impls = std::mem::take(&mut ir.function_impls);
    let struct_schemas = std::mem::take(&mut ir.struct_schemas);
    let parsed_index = ir.parsed_semantic_index.take();
    let function_names = super::helpers::build_function_names(&functions);
    let function_decls = super::helpers::build_function_decl_map(&functions);
    let host_import_return_types =
        super::helpers::build_host_import_return_types(&functions, &function_impls);
    let host_import_signatures =
        super::helpers::build_host_import_signatures(&functions, &function_impls);
    let mut context = TypeContext::new(
        &function_impls,
        &function_decls,
        &struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        typing_mode,
        parsed_index.as_ref(),
    );
    let sites = parsed_index
        .as_ref()
        .map(|index| index.call_sites.as_slice())
        .unwrap_or(&[]);
    let mut root = LocalTypeState::default();
    for entry in entry_local_types {
        bind_schema(&mut root, entry.slot, entry.schema.clone(), true);
    }
    for stmt in &mut ir.stmts {
        walk_stmt(stmt, &mut root, &mut context, sites)?;
    }
    let mut rewritten = Vec::new();
    for decl in &functions {
        let Some(mut function) = function_impls.get(&decl.index).cloned() else {
            continue;
        };
        let mut state = LocalTypeState::default();
        for (source, dest) in &function.capture_copies {
            if let Some(schema) = root.schema(*source).cloned() {
                bind_schema(&mut state, *dest, Some(schema), false);
            }
        }
        for (slot, schema) in function.param_slots.iter().zip(decl.arg_schemas.iter()) {
            if let Some(schema) = schema.clone() {
                bind_schema(&mut state, *slot, Some(schema), true);
            }
        }
        for stmt in &mut function.body_stmts {
            walk_stmt(stmt, &mut state, &mut context, sites)?;
        }
        walk_expr(
            &mut function.body_expr,
            &mut state,
            &mut context,
            sites,
            function.body_expr_line,
        )?;
        rewritten.push((decl.index, function));
    }
    drop(context);
    ir.function_impls = function_impls;
    for (index, function) in rewritten {
        ir.function_impls.insert(index, function);
    }
    ir.struct_schemas = struct_schemas;
    ir.parsed_semantic_index = parsed_index;
    Ok(())
}

fn bind_schema(
    state: &mut LocalTypeState,
    slot: super::super::ir::LocalSlot,
    schema: Option<TypeSchema>,
    declared: bool,
) {
    let Some(schema) = schema else {
        return;
    };
    if matches!(schema, TypeSchema::Unknown) {
        return;
    }
    state.set_with_schema_origin(
        slot,
        bound_type_from_schema(&schema),
        Some(schema),
        declared,
    );
}

fn walk_stmt(
    stmt: &mut Stmt,
    state: &mut LocalTypeState,
    context: &mut TypeContext<'_>,
    sites: &[super::super::ir::ParsedCallSite],
) -> Result<(), MapAdmissionError> {
    match stmt {
        Stmt::Let {
            index,
            declared_schema,
            expr,
            line,
        } => {
            walk_expr(expr, state, context, sites, *line)?;
            let schema = declared_schema
                .clone()
                .or_else(|| context.infer_expr_schema(expr, state));
            bind_schema(state, *index, schema, declared_schema.is_some());
            Ok(())
        }
        Stmt::Assign {
            index, expr, line, ..
        } => {
            walk_expr(expr, state, context, sites, *line)?;
            if !state.has_declared_schema(*index)
                && let Some(schema) = context.infer_expr_schema(expr, state)
            {
                bind_schema(state, *index, Some(schema), false);
            }
            Ok(())
        }
        Stmt::Expr { expr, line } => walk_expr(expr, state, context, sites, *line),
        Stmt::ClosureLet { closure, line, .. } => {
            walk_closure(closure, state, context, sites, *line)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        } => {
            walk_expr(condition, state, context, sites, *line)?;
            let mut then_state = state.clone();
            let mut else_state = state.clone();
            for stmt in then_branch {
                walk_stmt(stmt, &mut then_state, context, sites)?;
            }
            for stmt in else_branch {
                walk_stmt(stmt, &mut else_state, context, sites)?;
            }
            Ok(())
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            line,
        } => {
            walk_stmt(init, state, context, sites)?;
            walk_expr(condition, state, context, sites, *line)?;
            walk_stmt(post, state, context, sites)?;
            let mut nested = state.clone();
            for stmt in body {
                walk_stmt(stmt, &mut nested, context, sites)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, line, ..
        } => {
            walk_expr(condition, state, context, sites, *line)?;
            let mut nested = state.clone();
            for stmt in body {
                walk_stmt(stmt, &mut nested, context, sites)?;
            }
            Ok(())
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => Ok(()),
    }
}

fn walk_closure(
    closure: &mut super::super::ir::ClosureExpr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    sites: &[super::super::ir::ParsedCallSite],
    line: u32,
) -> Result<(), MapAdmissionError> {
    let mut nested = state.clone();
    for (source, dest) in &closure.capture_copies {
        if let Some(schema) = state.schema(*source).cloned() {
            bind_schema(&mut nested, *dest, Some(schema), false);
        }
    }
    walk_expr(&mut closure.body, &mut nested, context, sites, line)
}

fn walk_expr(
    expr: &mut Expr,
    state: &mut LocalTypeState,
    context: &mut TypeContext<'_>,
    sites: &[super::super::ir::ParsedCallSite],
    line: u32,
) -> Result<(), MapAdmissionError> {
    match expr {
        Expr::Call(_, _, args, _, _) => {
            for arg in args.iter_mut() {
                walk_expr(arg, state, context, sites, line)?;
            }
            classify_store(expr, state, context, sites, line)
        }
        Expr::ModuleCall(_, _, args, _) | Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                walk_expr(arg, state, context, sites, line)?;
            }
            Ok(())
        }
        Expr::Closure(closure) => walk_closure(closure, state, context, sites, line),
        Expr::ClosureCall(closure, args) => {
            walk_closure(closure, state, context, sites, line)?;
            for arg in args {
                walk_expr(arg, state, context, sites, line)?;
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
            walk_expr(lhs, state, context, sites, line)?;
            walk_expr(rhs, state, context, sites, line)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => walk_expr(inner, state, context, sites, line),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            walk_expr(condition, state, context, sites, line)?;
            walk_expr(then_expr, state, context, sites, line)?;
            walk_expr(else_expr, state, context, sites, line)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            walk_expr(value, state, context, sites, line)?;
            for (_, arm) in arms {
                walk_expr(arm, state, context, sites, line)?;
            }
            walk_expr(default, state, context, sites, line)
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            for stmt in stmts {
                walk_stmt(stmt, &mut nested, context, sites)?;
            }
            walk_expr(expr, &mut nested, context, sites, line)
        }
        Expr::OptionalGet { container, key, .. } => {
            walk_expr(container, state, context, sites, line)?;
            walk_expr(key, state, context, sites, line)
        }
        Expr::OptionUnwrapOr { value, fallback, .. } => {
            walk_expr(value, state, context, sites, line)?;
            walk_expr(fallback, state, context, sites, line)
        }
        Expr::ResultConstruct { payload, .. } => walk_expr(payload, state, context, sites, line),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            walk_expr(value, state, context, sites, line)?;
            walk_expr(ok_expr, state, context, sites, line)?;
            walk_expr(err_expr, state, context, sites, line)
        }
        Expr::ResultQuery { value, .. }
        | Expr::ResultTry { value, .. }
        | Expr::ResultConvert { value, .. } => walk_expr(value, state, context, sites, line),
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            walk_expr(receiver, state, context, sites, line)?;
            walk_expr(argument, state, context, sites, line)
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                walk_expr(payload, state, context, sites, line)?;
            }
            Ok(())
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            walk_expr(value, state, context, sites, line)?;
            walk_expr(some_expr, state, context, sites, line)?;
            walk_expr(none_expr, state, context, sites, line)
        }
        Expr::Null
        | Expr::Unit
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Bytes(_)
        | Expr::FunctionRef(..)
        | Expr::ModuleFunctionRef(..)
        | Expr::UnresolvedFunctionRef { .. }
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => Ok(()),
    }
}

fn classify_store(
    expr: &mut Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    sites: &[super::super::ir::ParsedCallSite],
    line: u32,
) -> Result<(), MapAdmissionError> {
    let Expr::Call(index, _, args, _, semantic_id) = expr else {
        return Ok(());
    };
    if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::MapEdit)
        && args.len() == 4
        && matches!(args.get(3), Some(Expr::Bool(true)))
    {
        return classify_remove(args, *semantic_id, state, context, sites, line);
    }
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Set) || args.len() != 3 {
        return Ok(());
    }
    let receiver = context.infer_expr_schema(&args[0], state);
    let value = context.infer_expr_schema(&args[2], state);
    if !value_may_be_unit(value.as_ref()) {
        return Ok(());
    }
    if receiver_is_map_backed(receiver.as_ref()) {
        *index = BuiltinFunction::MapEdit.call_index();
        args.push(Expr::Bool(false));
        return Ok(());
    }
    if receiver_kind_unresolved(receiver.as_ref()) {
        let span = semantic_id.and_then(|id| {
            sites
                .iter()
                .find(|site| site.id == id)
                .map(|site| site.expr_span)
        });
        return Err(MapAdmissionError::Compile(CompileError::InvalidFieldAccess {
            line: Some(line),
            source_name: None,
            detail: "map store receiver schema is unresolved; a Unit or other possibly-null value cannot use set, which deletes null keys. Declare the receiver as map<T>, an object, or a named record".to_string(),
            span,
        }));
    }
    Ok(())
}

fn classify_remove(
    args: &[Expr],
    semantic_id: Option<super::super::ir::SemanticNodeId>,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    sites: &[super::super::ir::ParsedCallSite],
    line: u32,
) -> Result<(), MapAdmissionError> {
    let receiver = context.infer_expr_schema(&args[0], state);
    if matches!(receiver, Some(TypeSchema::Map(_))) {
        return Ok(());
    }
    let span = semantic_id.and_then(|id| {
        sites
            .iter()
            .find(|site| site.id == id)
            .map(|site| site.expr_span)
    });
    Err(MapAdmissionError::Parse(
        ParseError {
            line: usize::try_from(line).unwrap_or(1),
            message: "E_MAP_REMOVE_RECORD: cannot remove a field from a record, object, Option, or Result; removal requires an explicit map<T>".to_string(),
            span,
            code: Some("E_MAP_REMOVE_RECORD".to_string()),
        },
    ))
}

pub(crate) fn map_admission_source_error(error: MapAdmissionError) -> super::super::SourceError {
    match error {
        MapAdmissionError::Compile(error) => super::super::SourceError::Compile(error),
        MapAdmissionError::Parse(error) => super::super::SourceError::Parse(error),
    }
}

fn receiver_is_map_backed(schema: Option<&TypeSchema>) -> bool {
    matches!(
        schema,
        Some(
            TypeSchema::Map(_)
                | TypeSchema::Object(_)
                | TypeSchema::Named(_, _)
                | TypeSchema::Option(_)
                | TypeSchema::Result { .. }
        )
    )
}

fn receiver_kind_unresolved(schema: Option<&TypeSchema>) -> bool {
    matches!(
        schema,
        None | Some(TypeSchema::Unknown | TypeSchema::GenericParam(_))
    )
}

fn value_may_be_unit(schema: Option<&TypeSchema>) -> bool {
    match schema {
        None
        | Some(
            TypeSchema::Unit
                | TypeSchema::Null
                | TypeSchema::GenericParam(_)
                | TypeSchema::Unknown
                | TypeSchema::Optional(_),
        ) => true,
        Some(_) => false,
    }
}
