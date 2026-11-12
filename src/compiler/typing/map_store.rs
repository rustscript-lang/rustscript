//! Classify map stores from the checked walk, then rewrite by semantic id.
//!
//! The decision uses the state already bound by collection: closure parameters,
//! match bindings, branch merges and loop stabilization. This pass does not
//! solve schemas again and does not read final slot aggregates as source
//! position types.

use std::collections::{HashMap, HashSet};

use crate::BuiltinFunction;

use super::super::ir::{
    ClosureExpr, Expr, FrontendIr, FunctionImpl, SemanticNodeId, Stmt, TypeSchema,
};
use super::super::source_map::{SourceId, Span};
use super::super::{CompileError, ParseError};
use super::EntryLocalType;
use super::context::{MapStoreAction, TypeContext};
use super::state::LocalTypeState;

pub(crate) enum MapAdmissionError {
    Compile(CompileError),
    Parse(ParseError),
}

/// Which producers receive RustScript map-store policy.
#[derive(Clone, Debug)]
pub(crate) enum MapStoreScope {
    /// Every spanned call in this IR belongs to one RustScript producer.
    RustScript,
    /// Only these source ids are RustScript. Other producers keep Set.
    Sources(HashSet<SourceId>),
    /// Leave legacy Set deletion unchanged.
    Legacy,
}

pub(crate) fn classify_map_stores(
    ir: &mut FrontendIr,
    typing_mode: super::super::TypingMode,
    entry_local_types: &[EntryLocalType],
    scope: MapStoreScope,
) -> Result<(), MapAdmissionError> {
    let decisions = super::collect_checked_map_store_decisions(ir, typing_mode, entry_local_types);
    let sites = ir
        .parsed_semantic_index
        .as_ref()
        .map(|index| index.call_sites.clone())
        .unwrap_or_default();
    for stmt in &mut ir.stmts {
        apply_stmt(stmt, &decisions, &sites, &scope)?;
    }
    for function in ir.function_impls.values_mut() {
        apply_function(function, &decisions, &sites, &scope)?;
    }
    Ok(())
}

pub(super) fn note_checked_map_store(
    expr: &Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) {
    let Expr::Call(index, _, args, _, Some(id)) = expr else {
        return;
    };
    let action = match BuiltinFunction::from_call_index(*index) {
        Some(BuiltinFunction::MapEdit) if args.len() == 4 => decide_edit(args, state, context),
        Some(BuiltinFunction::Set) if args.len() == 3 => decide_set(args, state, context),
        _ => None,
    };
    if let Some(action) = action {
        context.map_store_decisions.insert(*id, action);
    }
}

fn decide_set(
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> Option<MapStoreAction> {
    let receiver = context.infer_expr_schema(&args[0], state);
    if is_logical_carrier(receiver.as_ref()) {
        return Some(MapStoreAction::RejectCarrier);
    }
    let value = context.infer_expr_schema(&args[2], state);
    if !value_may_be_unit(value.as_ref()) {
        return None;
    }
    if is_map_object_or_named(receiver.as_ref()) {
        return Some(MapStoreAction::RewriteInsert);
    }
    if receiver_kind_unresolved(receiver.as_ref()) {
        return Some(MapStoreAction::RejectUnresolved);
    }
    None
}

fn decide_edit(
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> Option<MapStoreAction> {
    let receiver = context.infer_expr_schema(&args[0], state);
    if matches!(args.get(3), Some(Expr::Bool(true))) {
        return if matches!(receiver, Some(TypeSchema::Map(_))) {
            None
        } else {
            Some(MapStoreAction::RejectRemove)
        };
    }
    if is_logical_carrier(receiver.as_ref()) {
        return Some(MapStoreAction::RejectCarrier);
    }
    let value = context.infer_expr_schema(&args[2], state);
    if is_map_object_or_named(receiver.as_ref()) && !value_may_be_unit(value.as_ref()) {
        return Some(MapStoreAction::NormalizeSet);
    }
    None
}

fn apply_function(
    function: &mut FunctionImpl,
    decisions: &HashMap<SemanticNodeId, MapStoreAction>,
    sites: &[super::super::ir::ParsedCallSite],
    scope: &MapStoreScope,
) -> Result<(), MapAdmissionError> {
    for stmt in &mut function.body_stmts {
        apply_stmt(stmt, decisions, sites, scope)?;
    }
    apply_expr(
        &mut function.body_expr,
        decisions,
        sites,
        scope,
        function.body_expr_line,
    )
}

fn apply_stmt(
    stmt: &mut Stmt,
    decisions: &HashMap<SemanticNodeId, MapStoreAction>,
    sites: &[super::super::ir::ParsedCallSite],
    scope: &MapStoreScope,
) -> Result<(), MapAdmissionError> {
    let line = super::helpers::stmt_line(stmt);
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => {}
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            apply_expr(expr, decisions, sites, scope, line)?;
        }
        Stmt::ClosureLet { closure, .. } => apply_closure(closure, decisions, sites, scope, line)?,
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            apply_expr(condition, decisions, sites, scope, line)?;
            for stmt in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                apply_stmt(stmt, decisions, sites, scope)?;
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            apply_stmt(init, decisions, sites, scope)?;
            apply_expr(condition, decisions, sites, scope, line)?;
            apply_stmt(post, decisions, sites, scope)?;
            for stmt in body {
                apply_stmt(stmt, decisions, sites, scope)?;
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            apply_expr(condition, decisions, sites, scope, line)?;
            for stmt in body {
                apply_stmt(stmt, decisions, sites, scope)?;
            }
        }
    }
    Ok(())
}

fn apply_closure(
    closure: &mut ClosureExpr,
    decisions: &HashMap<SemanticNodeId, MapStoreAction>,
    sites: &[super::super::ir::ParsedCallSite],
    scope: &MapStoreScope,
    line: u32,
) -> Result<(), MapAdmissionError> {
    apply_expr(&mut closure.body, decisions, sites, scope, line)
}

fn apply_expr(
    expr: &mut Expr,
    decisions: &HashMap<SemanticNodeId, MapStoreAction>,
    sites: &[super::super::ir::ParsedCallSite],
    scope: &MapStoreScope,
    line: u32,
) -> Result<(), MapAdmissionError> {
    match expr {
        Expr::Call(_, _, args, _, _) => {
            for arg in args.iter_mut() {
                apply_expr(arg, decisions, sites, scope, line)?;
            }
            apply_call(expr, decisions, sites, scope, line)?;
        }
        Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                apply_expr(arg, decisions, sites, scope, line)?;
            }
        }
        Expr::Closure(closure) => apply_closure(closure, decisions, sites, scope, line)?,
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
            apply_expr(lhs, decisions, sites, scope, line)?;
            apply_expr(rhs, decisions, sites, scope, line)?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            apply_expr(inner, decisions, sites, scope, line)?;
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            apply_expr(condition, decisions, sites, scope, line)?;
            apply_expr(then_expr, decisions, sites, scope, line)?;
            apply_expr(else_expr, decisions, sites, scope, line)?;
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            apply_expr(value, decisions, sites, scope, line)?;
            for (_, arm) in arms {
                apply_expr(arm, decisions, sites, scope, line)?;
            }
            apply_expr(default, decisions, sites, scope, line)?;
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                apply_stmt(stmt, decisions, sites, scope)?;
            }
            apply_expr(expr, decisions, sites, scope, line)?;
        }
        Expr::OptionalGet { container, key, .. } => {
            apply_expr(container, decisions, sites, scope, line)?;
            apply_expr(key, decisions, sites, scope, line)?;
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            apply_expr(value, decisions, sites, scope, line)?;
            apply_expr(fallback, decisions, sites, scope, line)?;
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                apply_expr(payload, decisions, sites, scope, line)?;
            }
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            apply_expr(value, decisions, sites, scope, line)?;
            apply_expr(some_expr, decisions, sites, scope, line)?;
            apply_expr(none_expr, decisions, sites, scope, line)?;
        }
        Expr::OptionMethod {
            receiver, argument, ..
        }
        | Expr::ResultMethod {
            receiver, argument, ..
        } => {
            apply_expr(receiver, decisions, sites, scope, line)?;
            apply_expr(argument, decisions, sites, scope, line)?;
        }
        Expr::ResultConstruct { payload, .. } => {
            apply_expr(payload, decisions, sites, scope, line)?;
        }
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            apply_expr(value, decisions, sites, scope, line)?;
            apply_expr(ok_expr, decisions, sites, scope, line)?;
            apply_expr(err_expr, decisions, sites, scope, line)?;
        }
        Expr::ResultQuery { value, .. }
        | Expr::ResultTry { value, .. }
        | Expr::ResultConvert { value, .. } => {
            apply_expr(value, decisions, sites, scope, line)?;
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
        | Expr::MoveIndex { .. } => {}
    }
    Ok(())
}

fn apply_call(
    expr: &mut Expr,
    decisions: &HashMap<SemanticNodeId, MapStoreAction>,
    sites: &[super::super::ir::ParsedCallSite],
    scope: &MapStoreScope,
    line: u32,
) -> Result<(), MapAdmissionError> {
    let Expr::Call(index, _, args, _, semantic_id) = expr else {
        return Ok(());
    };
    let Some(id) = *semantic_id else {
        return Ok(());
    };
    let Some(action) = decisions.get(&id).copied() else {
        return Ok(());
    };
    if !scope_allows(scope, sites, id) {
        return Ok(());
    }
    let span = span_for(sites, id);
    match action {
        MapStoreAction::RewriteInsert => {
            *index = BuiltinFunction::MapEdit.call_index();
            args.push(Expr::Bool(false));
            Ok(())
        }
        MapStoreAction::NormalizeSet => {
            *index = BuiltinFunction::Set.call_index();
            if args.len() == 4 {
                args.pop();
            }
            Ok(())
        }
        MapStoreAction::RejectCarrier => Err(MapAdmissionError::Compile(
            CompileError::InvalidFieldAccess {
                line: Some(line),
                source_name: None,
                detail: "cannot store into an Option or Result by index or field; use Option/Result methods, or store into a map<T>, object, or named record".to_string(),
                span,
            },
        )),
        MapStoreAction::RejectUnresolved => Err(MapAdmissionError::Compile(
            CompileError::InvalidFieldAccess {
                line: Some(line),
                source_name: None,
                detail: "map store receiver schema is unresolved; a Unit or other possibly-null value cannot use set, which deletes null keys. Declare the receiver as map<T>, an object, or a named record".to_string(),
                span,
            },
        )),
        MapStoreAction::RejectRemove => Err(MapAdmissionError::Parse(ParseError {
            line: usize::try_from(line).unwrap_or(1),
            message: "E_MAP_REMOVE_RECORD: cannot remove a field from a record, object, Option, or Result; removal requires an explicit map<T>".to_string(),
            span,
            code: Some("E_MAP_REMOVE_RECORD".to_string()),
        })),
    }
}

fn scope_allows(
    scope: &MapStoreScope,
    sites: &[super::super::ir::ParsedCallSite],
    id: SemanticNodeId,
) -> bool {
    match scope {
        MapStoreScope::Legacy => false,
        MapStoreScope::RustScript => true,
        MapStoreScope::Sources(ids) => {
            span_for(sites, id).is_some_and(|span| ids.contains(&span.source_id))
        }
    }
}

fn span_for(sites: &[super::super::ir::ParsedCallSite], id: SemanticNodeId) -> Option<Span> {
    sites
        .iter()
        .find(|site| site.id == id)
        .map(|site| site.expr_span)
}

pub(crate) fn map_admission_source_error(error: MapAdmissionError) -> super::super::SourceError {
    match error {
        MapAdmissionError::Compile(error) => super::super::SourceError::Compile(error),
        MapAdmissionError::Parse(error) => super::super::SourceError::Parse(error),
    }
}

fn is_logical_carrier(schema: Option<&TypeSchema>) -> bool {
    matches!(
        schema,
        Some(TypeSchema::Option(_) | TypeSchema::Result { .. })
    )
}

fn is_map_object_or_named(schema: Option<&TypeSchema>) -> bool {
    matches!(
        schema,
        Some(TypeSchema::Map(_) | TypeSchema::Object(_) | TypeSchema::Named(_, _))
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
