//! Left-to-right ANF for calls that contain `?`.
//!
//! A value read or resource move that precedes a later `?` must already be a
//! local when that `?` runs. Leaving a bare argument in the call postpones the
//! read until after the hoisted `?`, so a later mutation is visible to the
//! earlier operand. Borrow and BorrowMut stay call-scoped passing modes.

use crate::builtins::BuiltinFunction;

use super::ParseError;
use super::ir::{Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt, StructDecl, TypeSchema};

pub(super) fn attach_err_cleanup_schemas(
    ir: &mut FrontendIr,
    schemas: &[Option<TypeSchema>],
    structs: &std::collections::HashMap<String, StructDecl>,
) {
    for stmt in &mut ir.stmts {
        attach_stmt_schema(stmt, schemas, structs);
    }
    for function in ir.function_impls.values_mut() {
        for stmt in &mut function.body_stmts {
            attach_stmt_schema(stmt, schemas, structs);
        }
        attach_expr_schema(&mut function.body_expr, schemas, structs);
    }
}

fn attach_stmt_schema(
    stmt: &mut Stmt,
    schemas: &[Option<TypeSchema>],
    structs: &std::collections::HashMap<String, StructDecl>,
) {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            attach_expr_schema(expr, schemas, structs);
        }
        Stmt::ClosureLet { closure, .. } => attach_expr_schema(&mut closure.body, schemas, structs),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            attach_expr_schema(condition, schemas, structs);
            for stmt in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                attach_stmt_schema(stmt, schemas, structs);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            attach_stmt_schema(init, schemas, structs);
            attach_expr_schema(condition, schemas, structs);
            attach_stmt_schema(post, schemas, structs);
            for stmt in body {
                attach_stmt_schema(stmt, schemas, structs);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            attach_expr_schema(condition, schemas, structs);
            for stmt in body {
                attach_stmt_schema(stmt, schemas, structs);
            }
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => {}
    }
}

fn attach_expr_schema(
    expr: &mut Expr,
    schemas: &[Option<TypeSchema>],
    structs: &std::collections::HashMap<String, StructDecl>,
) {
    if let Expr::ResultTry { exit_cleanup, .. } = expr {
        for (slot, schema) in exit_cleanup.iter_mut() {
            if let Some(known) = schemas
                .get(*slot as usize)
                .and_then(|schema| schema.clone())
                && known.contains_resource_with_named_types(structs)
            {
                *schema = if matches!(schema, TypeSchema::Optional(inner) if matches!(inner.as_ref(), TypeSchema::Unknown))
                {
                    // Availability marks legitimate whole-slot absence on a
                    // conditional-move Err edge; do not widen the variable's
                    // public logical type or all Resource cleanup leaves.
                    TypeSchema::Optional(Box::new(known))
                } else {
                    known
                };
            }
        }
    }
    match expr {
        Expr::OptionalGet { container, key, .. } => {
            attach_expr_schema(container, schemas, structs);
            attach_expr_schema(key, schemas, structs);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            attach_expr_schema(value, schemas, structs);
            attach_expr_schema(fallback, schemas, structs);
        }
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                attach_expr_schema(arg, schemas, structs);
            }
        }
        Expr::Closure(closure) => attach_expr_schema(&mut closure.body, schemas, structs),
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
            attach_expr_schema(lhs, schemas, structs);
            attach_expr_schema(rhs, schemas, structs);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            attach_expr_schema(inner, schemas, structs);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            attach_expr_schema(condition, schemas, structs);
            attach_expr_schema(then_expr, schemas, structs);
            attach_expr_schema(else_expr, schemas, structs);
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            attach_expr_schema(value, schemas, structs);
            for (_, arm) in arms {
                attach_expr_schema(arm, schemas, structs);
            }
            attach_expr_schema(default, schemas, structs);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                attach_stmt_schema(stmt, schemas, structs);
            }
            attach_expr_schema(expr, schemas, structs);
        }
        Expr::ResultConstruct { payload, .. } => attach_expr_schema(payload, schemas, structs),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            attach_expr_schema(value, schemas, structs);
            attach_expr_schema(ok_expr, schemas, structs);
            attach_expr_schema(err_expr, schemas, structs);
        }
        Expr::ResultQuery { value, .. } | Expr::ResultTry { value, .. } => {
            attach_expr_schema(value, schemas, structs);
        }
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
        | Expr::MoveIndex { .. } => {}
    }
}

pub(super) fn lower_partial_resource_args(ir: &mut FrontendIr) -> Result<(), ParseError> {
    let mut locals = ir.locals;
    for stmt in &mut ir.stmts {
        rewrite_stmt(stmt, &mut locals)?;
    }
    for function in ir.function_impls.values_mut() {
        rewrite_function(function, &mut locals)?;
    }
    ir.locals = locals;
    Ok(())
}

fn rewrite_function(function: &mut FunctionImpl, locals: &mut usize) -> Result<(), ParseError> {
    for stmt in &mut function.body_stmts {
        rewrite_stmt(stmt, locals)?;
    }
    rewrite_expr(&mut function.body_expr, locals)
}

fn rewrite_stmt(stmt: &mut Stmt, locals: &mut usize) -> Result<(), ParseError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            rewrite_expr(expr, locals)
        }
        Stmt::ClosureLet { closure, .. } => rewrite_expr(&mut closure.body, locals),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_expr(condition, locals)?;
            for stmt in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                rewrite_stmt(stmt, locals)?;
            }
            Ok(())
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            rewrite_stmt(init, locals)?;
            rewrite_expr(condition, locals)?;
            rewrite_stmt(post, locals)?;
            for stmt in body {
                rewrite_stmt(stmt, locals)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            rewrite_expr(condition, locals)?;
            for stmt in body {
                rewrite_stmt(stmt, locals)?;
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

fn rewrite_expr(expr: &mut Expr, locals: &mut usize) -> Result<(), ParseError> {
    rewrite_children(expr, locals)?;
    if let Some(lowered) = hoist_call(expr, locals)? {
        *expr = lowered;
    }
    Ok(())
}

fn rewrite_children(expr: &mut Expr, locals: &mut usize) -> Result<(), ParseError> {
    match expr {
        Expr::OptionalGet { container, key, .. } => {
            rewrite_expr(container, locals)?;
            rewrite_expr(key, locals)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            rewrite_expr(value, locals)?;
            rewrite_expr(fallback, locals)
        }
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                rewrite_expr(arg, locals)?;
            }
            Ok(())
        }
        Expr::Closure(closure) => rewrite_expr(&mut closure.body, locals),
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
            rewrite_expr(lhs, locals)?;
            rewrite_expr(rhs, locals)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => rewrite_expr(inner, locals),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            rewrite_expr(condition, locals)?;
            rewrite_expr(then_expr, locals)?;
            rewrite_expr(else_expr, locals)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            rewrite_expr(value, locals)?;
            for (_, arm) in arms {
                rewrite_expr(arm, locals)?;
            }
            rewrite_expr(default, locals)
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                rewrite_stmt(stmt, locals)?;
            }
            rewrite_expr(expr, locals)
        }
        Expr::ResultConstruct { payload, .. } => rewrite_expr(payload, locals),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            rewrite_expr(value, locals)?;
            rewrite_expr(ok_expr, locals)?;
            rewrite_expr(err_expr, locals)
        }
        Expr::ResultQuery { value, .. } | Expr::ResultTry { value, .. } => {
            rewrite_expr(value, locals)
        }
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

fn hoist_call(expr: &Expr, locals: &mut usize) -> Result<Option<Expr>, ParseError> {
    if call_args(expr).is_none()
        || !call_args(expr).is_some_and(|args| args.iter().any(contains_try))
    {
        return Ok(None);
    }
    let mut lets = Vec::new();
    let mut lowered = expr.clone();
    snapshot_callee(&mut lowered, &mut lets, locals)?;
    let args = call_args(&lowered)
        .expect("callee snapshot preserves the call")
        .to_vec();
    let mut final_args = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        let later_exits = args[index + 1..].iter().any(contains_try);
        final_args.push(bind_call_arg(arg, later_exits, &mut lets, locals)?);
    }
    set_call_args(&mut lowered, final_args);
    Ok(Some(Expr::Block {
        stmts: lets,
        expr: Box::new(lowered),
    }))
}

fn call_args(expr: &Expr) -> Option<&[Expr]> {
    match expr {
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => Some(args),
        _ => None,
    }
}

fn set_call_args(expr: &mut Expr, args: Vec<Expr>) {
    match expr {
        Expr::Call(_, _, slot, _, _)
        | Expr::ModuleCall(_, _, slot, _)
        | Expr::LocalCall(_, _, slot, _)
        | Expr::ClosureCall(_, slot) => *slot = args,
        _ => {}
    }
}

fn snapshot_callee(
    expr: &mut Expr,
    lets: &mut Vec<Stmt>,
    locals: &mut usize,
) -> Result<(), ParseError> {
    match expr {
        Expr::LocalCall(slot, _, _, _) => {
            let saved = allocate_slot(locals)?;
            lets.push(Stmt::Let {
                index: saved,
                declared_schema: None,
                expr: Expr::Var(*slot),
                line: 1,
            });
            *slot = saved;
        }
        Expr::ClosureCall(closure, args) => {
            let saved = allocate_slot(locals)?;
            let closure = closure.clone();
            let args = std::mem::take(args);
            lets.push(Stmt::Let {
                index: saved,
                declared_schema: None,
                expr: Expr::Closure(closure),
                line: 1,
            });
            *expr = Expr::LocalCall(saved, Vec::new(), args, None);
        }
        _ => {}
    }
    Ok(())
}

fn bind_call_arg(
    arg: &Expr,
    later_exits: bool,
    lets: &mut Vec<Stmt>,
    locals: &mut usize,
) -> Result<Expr, ParseError> {
    if let Some(preserved) = preserved_borrow(arg, lets, locals)? {
        return Ok(preserved);
    }
    if is_bare_local(arg) && !later_exits {
        return Ok(arg.clone());
    }
    let slot = allocate_slot(locals)?;
    let moved = matches!(arg, Expr::MoveVar(_));
    let schema = if moved { None } else { owned_schema(arg) };
    lets.push(Stmt::Let {
        index: slot,
        declared_schema: schema.clone(),
        expr: arg.clone(),
        line: 1,
    });
    Ok(if moved || schema.is_some() {
        Expr::MoveVar(slot)
    } else {
        Expr::Var(slot)
    })
}

fn preserved_borrow(
    arg: &Expr,
    lets: &mut Vec<Stmt>,
    locals: &mut usize,
) -> Result<Option<Expr>, ParseError> {
    let (mutable, inner) = match arg {
        Expr::Borrow(inner) => (false, inner.as_ref()),
        Expr::BorrowMut(inner) => (true, inner.as_ref()),
        _ => return Ok(None),
    };
    if is_stable_borrow_receiver(inner) {
        return Ok(Some(arg.clone()));
    }
    let slot = allocate_slot(locals)?;
    lets.push(Stmt::Let {
        index: slot,
        declared_schema: None,
        expr: inner.clone(),
        line: 1,
    });
    let receiver = Box::new(Expr::Var(slot));
    Ok(Some(if mutable {
        Expr::BorrowMut(receiver)
    } else {
        Expr::Borrow(receiver)
    }))
}

fn is_stable_borrow_receiver(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Var(_) | Expr::MoveVar(_) | Expr::MoveField { .. } | Expr::MoveIndex { .. }
    )
}

fn is_bare_local(expr: &Expr) -> bool {
    matches!(expr, Expr::Var(_) | Expr::MoveVar(_))
}

fn contains_try(expr: &Expr) -> bool {
    match expr {
        Expr::ResultTry { .. } => true,
        Expr::OptionalGet { container, key, .. } => contains_try(container) || contains_try(key),
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => contains_try(value) || contains_try(fallback),
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => args.iter().any(contains_try),
        Expr::Closure(closure) => contains_try(&closure.body),
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => contains_try(lhs) || contains_try(rhs),
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => contains_try(inner),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => contains_try(condition) || contains_try(then_expr) || contains_try(else_expr),
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            contains_try(value)
                || arms.iter().any(|(_, arm)| contains_try(arm))
                || contains_try(default)
        }
        Expr::Block { stmts, expr } => stmts.iter().any(stmt_contains_try) || contains_try(expr),
        Expr::ResultConstruct { payload, .. } => contains_try(payload),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => contains_try(value) || contains_try(ok_expr) || contains_try(err_expr),
        Expr::ResultQuery { value, .. } => contains_try(value),
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
        | Expr::MoveIndex { .. } => false,
    }
}

fn stmt_contains_try(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            contains_try(expr)
        }
        Stmt::ClosureLet { closure, .. } => contains_try(&closure.body),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            contains_try(condition)
                || then_branch.iter().any(stmt_contains_try)
                || else_branch.iter().any(stmt_contains_try)
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_contains_try(init)
                || contains_try(condition)
                || stmt_contains_try(post)
                || body.iter().any(stmt_contains_try)
        }
        Stmt::While {
            condition, body, ..
        } => contains_try(condition) || body.iter().any(stmt_contains_try),
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => false,
    }
}

fn owned_schema(expr: &Expr) -> Option<TypeSchema> {
    match expr {
        Expr::Call(index, _, args, resolved, _) => {
            if let Some(resolved) = resolved
                && resolved.return_type.contains_resource()
            {
                return Some(resolved.return_type.clone());
            }
            if *index == BuiltinFunction::ArrayNew.call_index() {
                let inner = args.iter().find_map(owned_schema)?;
                return Some(TypeSchema::Array(Box::new(inner)));
            }
            None
        }
        Expr::Block { expr, .. } => owned_schema(expr),
        _ => None,
    }
}

fn allocate_slot(locals: &mut usize) -> Result<LocalSlot, ParseError> {
    let slot = u16::try_from(*locals).map_err(|_| {
        ParseError::new("too many locals while binding a resource argument before `?`".to_string())
    })?;
    *locals += 1;
    Ok(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codegen(ir: &FrontendIr) -> crate::Program {
        let mut compiler = super::super::Compiler::new();
        compiler.set_root_local_count(ir.locals);
        compiler.set_function_impls(ir.function_impls.clone());
        compiler.set_function_decls(
            ir.functions
                .iter()
                .cloned()
                .map(|decl| (decl.index, decl))
                .collect(),
        );
        compiler.set_struct_schemas(ir.struct_schemas.clone());
        compiler.set_type_inference(super::super::typing::infer_types(
            ir,
            super::super::TypingMode::DynamicHints,
            &[],
        ));
        compiler
            .compile_program(&ir.stmts)
            .expect("NoResult codegen")
    }

    #[test]
    fn no_result_controls_add_no_try_locals_instructions_or_runtime_allocation_work() {
        for source in [
            "let x: int = 4; x + 1;",
            "fn inc(n: int) -> int { n + 1 } inc(4);",
            "fn inc(n: int) -> int { n + 1 } let f: fn(int) -> int = inc; f(4);",
            "let xs = [4, 5]; xs[0] + xs[1];",
        ] {
            let model = crate::analyze_source(source).expect("NoResult admission");
            let mut ir = model.ir().clone();
            assert!(!ir.stmts.iter().any(stmt_contains_try));
            let before_ir = format!("{ir:?}");
            let before = codegen(&ir);
            let locals = ir.locals;
            lower_partial_resource_args(&mut ir).unwrap();
            let structs = ir.struct_schemas.clone();
            attach_err_cleanup_schemas(&mut ir, &vec![None; locals], &structs);
            assert_eq!(ir.locals, locals, "no hidden Try locals: {source}");
            assert_eq!(
                format!("{ir:?}"),
                before_ir,
                "no inserted Map/call/branch nodes: {source}"
            );
            let after = codegen(&ir);
            assert_eq!(
                after.code, before.code,
                "no added instruction or allocation work: {source}"
            );
            assert_eq!(after.constants, before.constants);
            assert_eq!(after.local_count, before.local_count);
            assert!(after.ret_cleanups.is_empty());
            assert!(
                !after
                    .constants
                    .iter()
                    .any(|value| value == &crate::Value::string("tag")
                        || value == &crate::Value::string("payload"))
            );
        }
    }
}
