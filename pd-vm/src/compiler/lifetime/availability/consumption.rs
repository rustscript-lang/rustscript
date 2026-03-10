use super::*;

pub(super) fn extract_passthrough_return_slot(function_impl: &FunctionImpl) -> Option<LocalSlot> {
    if !function_impl.body_stmts.is_empty() {
        return None;
    }
    let Expr::Var(slot) = function_impl.body_expr else {
        return None;
    };
    Some(slot)
}

pub(super) fn compute_function_consumed_param_positions(
    function_impls: &HashMap<u16, FunctionImpl>,
    enable_local_move_semantics: bool,
) -> HashMap<u16, HashSet<usize>> {
    if !enable_local_move_semantics {
        return HashMap::new();
    }

    let mut consumed_positions: HashMap<u16, HashSet<usize>> = function_impls
        .keys()
        .map(|index| (*index, HashSet::new()))
        .collect();

    loop {
        let snapshot = consumed_positions.clone();
        let mut changed = false;

        for (index, function_impl) in function_impls {
            let mut next = snapshot.get(index).cloned().unwrap_or_default();
            collect_consumed_positions_from_function(function_impl, &snapshot, &mut next);
            let entry = consumed_positions.entry(*index).or_default();
            let before = entry.len();
            entry.extend(next);
            changed |= entry.len() != before;
        }

        if !changed {
            break;
        }
    }

    consumed_positions.retain(|_, positions| !positions.is_empty());
    consumed_positions
}

pub(super) fn collect_consumed_positions_from_function(
    function_impl: &FunctionImpl,
    known_consumed_positions: &HashMap<u16, HashSet<usize>>,
    out: &mut HashSet<usize>,
) {
    for stmt in &function_impl.body_stmts {
        collect_consumed_positions_from_stmt(stmt, function_impl, known_consumed_positions, out);
    }
    collect_consumed_positions_from_expr(
        &function_impl.body_expr,
        function_impl,
        known_consumed_positions,
        out,
    );
    collect_return_rebind_consumed_positions(function_impl, out);
}

pub(super) fn collect_return_rebind_consumed_positions(
    function_impl: &FunctionImpl,
    out: &mut HashSet<usize>,
) {
    let Expr::Var(return_slot) = function_impl.body_expr else {
        return;
    };
    for (stmt_index, stmt) in function_impl.body_stmts.iter().enumerate() {
        let (target_slot, source_slot) = match stmt {
            Stmt::Let {
                index,
                expr: Expr::Var(source),
                ..
            }
            | Stmt::Assign {
                index,
                expr: Expr::Var(source),
                ..
            } => (*index, *source),
            _ => continue,
        };
        if target_slot != return_slot {
            continue;
        }
        let Some(param_position) = function_impl
            .param_slots
            .iter()
            .position(|param| *param == source_slot)
        else {
            continue;
        };
        if !slot_is_used_after_statement(function_impl, source_slot, stmt_index + 1) {
            out.insert(param_position);
        }
    }
}

pub(super) fn slot_is_used_after_statement(
    function_impl: &FunctionImpl,
    slot: LocalSlot,
    next_stmt_index: usize,
) -> bool {
    function_impl
        .body_stmts
        .iter()
        .skip(next_stmt_index)
        .any(|stmt| stmt_uses_slot(stmt, slot))
        || expr_uses_slot(&function_impl.body_expr, slot)
}

pub(super) fn stmt_uses_slot(stmt: &Stmt, slot: LocalSlot) -> bool {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {
            false
        }
        Stmt::Drop { index, .. } => *index == slot,
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            expr_uses_slot(expr, slot)
        }
        Stmt::ClosureLet { closure, .. } => {
            closure
                .capture_copies
                .iter()
                .any(|(source, _)| *source == slot)
                || expr_uses_slot(&closure.body, slot)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_uses_slot(condition, slot)
                || then_branch
                    .iter()
                    .any(|nested| stmt_uses_slot(nested, slot))
                || else_branch
                    .iter()
                    .any(|nested| stmt_uses_slot(nested, slot))
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_uses_slot(init, slot)
                || expr_uses_slot(condition, slot)
                || stmt_uses_slot(post, slot)
                || body.iter().any(|nested| stmt_uses_slot(nested, slot))
        }
        Stmt::While {
            condition, body, ..
        } => {
            expr_uses_slot(condition, slot)
                || body.iter().any(|nested| stmt_uses_slot(nested, slot))
        }
    }
}

pub(super) fn expr_uses_slot(expr: &Expr, slot: LocalSlot) -> bool {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_) => false,
        Expr::Var(index) | Expr::MoveVar(index) => *index == slot,
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => *root == slot,
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            args.iter().any(|arg| expr_uses_slot(arg, slot))
        }
        Expr::Closure(closure) => {
            closure
                .capture_copies
                .iter()
                .any(|(source, _)| *source == slot)
                || expr_uses_slot(&closure.body, slot)
        }
        Expr::ClosureCall(closure, args) => {
            args.iter().any(|arg| expr_uses_slot(arg, slot))
                || closure
                    .capture_copies
                    .iter()
                    .any(|(source, _)| *source == slot)
                || expr_uses_slot(&closure.body, slot)
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
        | Expr::Gt(lhs, rhs) => expr_uses_slot(lhs, slot) || expr_uses_slot(rhs, slot),
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => expr_uses_slot(inner, slot),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_uses_slot(condition, slot)
                || expr_uses_slot(then_expr, slot)
                || expr_uses_slot(else_expr, slot)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            expr_uses_slot(value, slot)
                || arms
                    .iter()
                    .any(|(_, arm_expr)| expr_uses_slot(arm_expr, slot))
                || expr_uses_slot(default, slot)
        }
        Expr::Block { stmts, expr } => {
            stmts.iter().any(|stmt| stmt_uses_slot(stmt, slot)) || expr_uses_slot(expr, slot)
        }
    }
}

pub(super) fn collect_consumed_positions_from_stmt(
    stmt: &Stmt,
    function_impl: &FunctionImpl,
    known_consumed_positions: &HashMap<u16, HashSet<usize>>,
    out: &mut HashSet<usize>,
) {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => {}
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            collect_consumed_positions_from_expr(
                expr,
                function_impl,
                known_consumed_positions,
                out,
            );
        }
        Stmt::ClosureLet { closure, .. } => {
            collect_consumed_positions_from_expr(
                &closure.body,
                function_impl,
                known_consumed_positions,
                out,
            );
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_consumed_positions_from_expr(
                condition,
                function_impl,
                known_consumed_positions,
                out,
            );
            for nested in then_branch {
                collect_consumed_positions_from_stmt(
                    nested,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
            for nested in else_branch {
                collect_consumed_positions_from_stmt(
                    nested,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_consumed_positions_from_stmt(
                init,
                function_impl,
                known_consumed_positions,
                out,
            );
            collect_consumed_positions_from_expr(
                condition,
                function_impl,
                known_consumed_positions,
                out,
            );
            collect_consumed_positions_from_stmt(
                post,
                function_impl,
                known_consumed_positions,
                out,
            );
            for nested in body {
                collect_consumed_positions_from_stmt(
                    nested,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_consumed_positions_from_expr(
                condition,
                function_impl,
                known_consumed_positions,
                out,
            );
            for nested in body {
                collect_consumed_positions_from_stmt(
                    nested,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
        }
    }
}

pub(super) fn collect_consumed_positions_from_expr(
    expr: &Expr,
    function_impl: &FunctionImpl,
    known_consumed_positions: &HashMap<u16, HashSet<usize>>,
    out: &mut HashSet<usize>,
) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::Var(_) => {}
        Expr::MoveVar(slot) => {
            if let Some(position) = function_impl
                .param_slots
                .iter()
                .position(|param| param == slot)
            {
                out.insert(position);
            }
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            if let Some(position) = function_impl
                .param_slots
                .iter()
                .position(|param| param == root)
            {
                out.insert(position);
            }
        }
        Expr::Call(index, args) => {
            for arg in args {
                collect_consumed_positions_from_expr(
                    arg,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
            let Some(consumed_arg_positions) = known_consumed_positions.get(index) else {
                return;
            };
            for position in consumed_arg_positions {
                let Some(arg_expr) = args.get(*position) else {
                    continue;
                };
                match arg_expr {
                    Expr::Var(slot) | Expr::MoveVar(slot) => {
                        if let Some(param_position) = function_impl
                            .param_slots
                            .iter()
                            .position(|param| param == slot)
                        {
                            out.insert(param_position);
                        }
                    }
                    Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                        if let Some(param_position) = function_impl
                            .param_slots
                            .iter()
                            .position(|param| param == root)
                        {
                            out.insert(param_position);
                        }
                    }
                    _ => {}
                }
            }
        }
        Expr::LocalCall(_, args) => {
            for arg in args {
                collect_consumed_positions_from_expr(
                    arg,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
        }
        Expr::Closure(_) => {}
        Expr::ClosureCall(closure, args) => {
            for arg in args {
                collect_consumed_positions_from_expr(
                    arg,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
            collect_consumed_positions_from_expr(
                &closure.body,
                function_impl,
                known_consumed_positions,
                out,
            );
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
            collect_consumed_positions_from_expr(lhs, function_impl, known_consumed_positions, out);
            collect_consumed_positions_from_expr(rhs, function_impl, known_consumed_positions, out);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            collect_consumed_positions_from_expr(
                inner,
                function_impl,
                known_consumed_positions,
                out,
            );
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_consumed_positions_from_expr(
                condition,
                function_impl,
                known_consumed_positions,
                out,
            );
            collect_consumed_positions_from_expr(
                then_expr,
                function_impl,
                known_consumed_positions,
                out,
            );
            collect_consumed_positions_from_expr(
                else_expr,
                function_impl,
                known_consumed_positions,
                out,
            );
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            collect_consumed_positions_from_expr(
                value,
                function_impl,
                known_consumed_positions,
                out,
            );
            for (_, arm_expr) in arms {
                collect_consumed_positions_from_expr(
                    arm_expr,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
            collect_consumed_positions_from_expr(
                default,
                function_impl,
                known_consumed_positions,
                out,
            );
        }
        Expr::Block { stmts, expr } => {
            for nested in stmts {
                collect_consumed_positions_from_stmt(
                    nested,
                    function_impl,
                    known_consumed_positions,
                    out,
                );
            }
            collect_consumed_positions_from_expr(
                expr,
                function_impl,
                known_consumed_positions,
                out,
            );
        }
    }
}
