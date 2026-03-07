use super::super::ir::{Expr, FrontendIr, Stmt};

pub(super) fn remap_frontend_ir_line_numbers(ir: &mut FrontendIr, prelude_lines: usize) {
    let offset = u32::try_from(prelude_lines).unwrap_or(u32::MAX);
    for stmt in &mut ir.stmts {
        remap_stmt_line_numbers(stmt, offset);
    }
    for function in ir.function_impls.values_mut() {
        for stmt in &mut function.body_stmts {
            remap_stmt_line_numbers(stmt, offset);
        }
        remap_expr_line_numbers(&mut function.body_expr, offset);
    }
}

fn remap_line(line: &mut u32, offset: u32) {
    *line = (*line).saturating_sub(offset).max(1);
}

fn remap_stmt_line_numbers(stmt: &mut Stmt, offset: u32) {
    match stmt {
        Stmt::Noop { line }
        | Stmt::Break { line }
        | Stmt::Continue { line }
        | Stmt::Drop { line, .. }
        | Stmt::ClosureLet { line, .. }
        | Stmt::FuncDecl { line, .. } => remap_line(line, offset),
        Stmt::Let { expr, line, .. }
        | Stmt::Assign { expr, line, .. }
        | Stmt::Expr { expr, line } => {
            remap_line(line, offset);
            remap_expr_line_numbers(expr, offset);
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        } => {
            remap_line(line, offset);
            remap_expr_line_numbers(condition, offset);
            for stmt in then_branch {
                remap_stmt_line_numbers(stmt, offset);
            }
            for stmt in else_branch {
                remap_stmt_line_numbers(stmt, offset);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            line,
        } => {
            remap_line(line, offset);
            remap_stmt_line_numbers(init, offset);
            remap_expr_line_numbers(condition, offset);
            remap_stmt_line_numbers(post, offset);
            for stmt in body {
                remap_stmt_line_numbers(stmt, offset);
            }
        }
        Stmt::While {
            condition,
            body,
            line,
        } => {
            remap_line(line, offset);
            remap_expr_line_numbers(condition, offset);
            for stmt in body {
                remap_stmt_line_numbers(stmt, offset);
            }
        }
    }
}

fn remap_expr_line_numbers(expr: &mut Expr, offset: u32) {
    match expr {
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                remap_expr_line_numbers(arg, offset);
            }
        }
        Expr::ClosureCall(closure, args) => {
            remap_closure_line_numbers(closure, offset);
            for arg in args {
                remap_expr_line_numbers(arg, offset);
            }
        }
        Expr::Closure(closure) => remap_closure_line_numbers(closure, offset),
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
            remap_expr_line_numbers(lhs, offset);
            remap_expr_line_numbers(rhs, offset);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            remap_expr_line_numbers(inner, offset);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            remap_expr_line_numbers(condition, offset);
            remap_expr_line_numbers(then_expr, offset);
            remap_expr_line_numbers(else_expr, offset);
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            remap_expr_line_numbers(value, offset);
            for (_, arm_expr) in arms {
                remap_expr_line_numbers(arm_expr, offset);
            }
            remap_expr_line_numbers(default, offset);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                remap_stmt_line_numbers(stmt, offset);
            }
            remap_expr_line_numbers(expr, offset);
        }
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => {}
    }
}

fn remap_closure_line_numbers(closure: &mut crate::compiler::ir::ClosureExpr, offset: u32) {
    remap_expr_line_numbers(&mut closure.body, offset);
}

