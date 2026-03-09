use super::*;

pub(super) fn local_slot_operand(index: LocalSlot) -> Result<u8, CompileError> {
    u8::try_from(index).map_err(|_| CompileError::LocalSlotOverflow(index))
}

pub(super) fn collect_function_frame_slots(function_impl: &FunctionImpl) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    for slot in &function_impl.param_slots {
        slots.insert(*slot);
    }
    for stmt in &function_impl.body_stmts {
        collect_stmt_slot_footprint(stmt, &mut slots);
    }
    collect_expr_slot_footprint(&function_impl.body_expr, &mut slots);
    for (_, captured_slot) in &function_impl.capture_copies {
        slots.remove(captured_slot);
    }
    let mut out = slots.into_iter().collect::<Vec<_>>();
    out.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
    out
}

pub(super) fn collect_closure_frame_slots(closure: &ClosureExpr) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    for slot in &closure.param_slots {
        slots.insert(*slot);
    }
    collect_expr_slot_footprint(&closure.body, &mut slots);
    for (_, captured_slot) in &closure.capture_copies {
        slots.remove(captured_slot);
    }
    let mut out = slots.into_iter().collect::<Vec<_>>();
    out.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
    out
}

fn collect_stmt_slot_footprint(stmt: &Stmt, slots: &mut BTreeSet<LocalSlot>) {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, .. } => {
            slots.insert(*index);
        }
        Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
            slots.insert(*index);
            collect_expr_slot_footprint(expr, slots);
        }
        Stmt::ClosureLet { closure, .. } => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Stmt::Expr { expr, .. } => collect_expr_slot_footprint(expr, slots),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_expr_slot_footprint(condition, slots);
            for stmt in then_branch {
                collect_stmt_slot_footprint(stmt, slots);
            }
            for stmt in else_branch {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_stmt_slot_footprint(init, slots);
            collect_expr_slot_footprint(condition, slots);
            collect_stmt_slot_footprint(post, slots);
            for stmt in body {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_expr_slot_footprint(condition, slots);
            for stmt in body {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
    }
}

fn collect_expr_slot_footprint(expr: &Expr, slots: &mut BTreeSet<LocalSlot>) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_) => {}
        Expr::Var(index) | Expr::MoveVar(index) => {
            slots.insert(*index);
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            slots.insert(*root);
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
        }
        Expr::LocalCall(index, args) => {
            slots.insert(*index);
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
        }
        Expr::Closure(closure) => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Expr::ClosureCall(closure, args) => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
            collect_expr_slot_footprint(&closure.body, slots);
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
            collect_expr_slot_footprint(lhs, slots);
            collect_expr_slot_footprint(rhs, slots);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => collect_expr_slot_footprint(inner, slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_expr_slot_footprint(condition, slots);
            collect_expr_slot_footprint(then_expr, slots);
            collect_expr_slot_footprint(else_expr, slots);
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            slots.insert(*value_slot);
            slots.insert(*result_slot);
            collect_expr_slot_footprint(value, slots);
            for (_, arm_expr) in arms {
                collect_expr_slot_footprint(arm_expr, slots);
            }
            collect_expr_slot_footprint(default, slots);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                collect_stmt_slot_footprint(stmt, slots);
            }
            collect_expr_slot_footprint(expr, slots);
        }
    }
}

pub(super) fn collect_named_local_debug_ranges(
    parsed: &FrontendIr,
) -> HashMap<String, LocalDebugRange> {
    let slot_ranges = collect_local_debug_ranges(&parsed.stmts, &parsed.function_impls);
    let mut named_ranges = HashMap::<String, LocalDebugRange>::new();
    for (name, slot) in &parsed.local_bindings {
        let Some(range) = slot_ranges.get(slot).copied() else {
            continue;
        };
        let entry = named_ranges.entry(name.clone()).or_default();
        entry.declared_line = merge_min_debug_line(entry.declared_line, range.declared_line);
        entry.last_line = merge_max_debug_line(entry.last_line, range.last_line);
    }
    named_ranges
}

fn collect_local_debug_ranges(
    stmts: &[Stmt],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<LocalSlot, LocalDebugRange> {
    let mut ranges = HashMap::<LocalSlot, LocalDebugRange>::new();
    for stmt in stmts {
        record_stmt_local_debug_ranges(stmt, &mut ranges);
    }
    for function_impl in function_impls.values() {
        for stmt in &function_impl.body_stmts {
            record_stmt_local_debug_ranges(stmt, &mut ranges);
        }
        let fallback_line = function_impl
            .body_stmts
            .last()
            .map(stmt_source_line)
            .unwrap_or(1);
        record_expr_local_debug_ranges(&function_impl.body_expr, fallback_line, &mut ranges);
    }
    ranges
}

fn record_stmt_local_debug_ranges(stmt: &Stmt, ranges: &mut HashMap<LocalSlot, LocalDebugRange>) {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, line } => {
            note_local_use(ranges, *index, *line);
        }
        Stmt::Let { index, expr, line } => {
            note_local_decl(ranges, *index, *line);
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::Assign { index, expr, line } => {
            note_local_use(ranges, *index, *line);
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::ClosureLet { line, closure } => {
            for (source_slot, captured_slot) in &closure.capture_copies {
                note_local_use(ranges, *source_slot, *line);
                note_local_use(ranges, *captured_slot, *line);
            }
            record_expr_local_debug_ranges(&closure.body, *line, ranges);
        }
        Stmt::Expr { expr, line } => {
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        } => {
            record_expr_local_debug_ranges(condition, *line, ranges);
            for nested in then_branch {
                record_stmt_local_debug_ranges(nested, ranges);
            }
            for nested in else_branch {
                record_stmt_local_debug_ranges(nested, ranges);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            line,
        } => {
            record_stmt_local_debug_ranges(init, ranges);
            record_expr_local_debug_ranges(condition, *line, ranges);
            record_stmt_local_debug_ranges(post, ranges);
            for nested in body {
                record_stmt_local_debug_ranges(nested, ranges);
            }
        }
        Stmt::While {
            condition,
            body,
            line,
        } => {
            record_expr_local_debug_ranges(condition, *line, ranges);
            for nested in body {
                record_stmt_local_debug_ranges(nested, ranges);
            }
        }
    }
}

fn record_expr_local_debug_ranges(
    expr: &Expr,
    line: u32,
    ranges: &mut HashMap<LocalSlot, LocalDebugRange>,
) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_) => {}
        Expr::Var(index) | Expr::MoveVar(index) => {
            note_local_use(ranges, *index, line);
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            note_local_use(ranges, *root, line);
        }
        Expr::Call(_, args) => {
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
        }
        Expr::LocalCall(index, args) => {
            note_local_use(ranges, *index, line);
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
        }
        Expr::Closure(closure) => {
            for (source_slot, captured_slot) in &closure.capture_copies {
                note_local_use(ranges, *source_slot, line);
                note_local_use(ranges, *captured_slot, line);
            }
            record_expr_local_debug_ranges(&closure.body, line, ranges);
        }
        Expr::ClosureCall(closure, args) => {
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                note_local_use(ranges, *source_slot, line);
                note_local_use(ranges, *captured_slot, line);
            }
            record_expr_local_debug_ranges(&closure.body, line, ranges);
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
            record_expr_local_debug_ranges(lhs, line, ranges);
            record_expr_local_debug_ranges(rhs, line, ranges);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            record_expr_local_debug_ranges(inner, line, ranges);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            record_expr_local_debug_ranges(condition, line, ranges);
            record_expr_local_debug_ranges(then_expr, line, ranges);
            record_expr_local_debug_ranges(else_expr, line, ranges);
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            note_local_use(ranges, *value_slot, line);
            note_local_use(ranges, *result_slot, line);
            record_expr_local_debug_ranges(value, line, ranges);
            for (_, arm_expr) in arms {
                record_expr_local_debug_ranges(arm_expr, line, ranges);
            }
            record_expr_local_debug_ranges(default, line, ranges);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                record_stmt_local_debug_ranges(stmt, ranges);
            }
            record_expr_local_debug_ranges(expr, line, ranges);
        }
    }
}

fn note_local_decl(ranges: &mut HashMap<LocalSlot, LocalDebugRange>, slot: LocalSlot, line: u32) {
    let entry = ranges.entry(slot).or_default();
    entry.declared_line = Some(
        entry
            .declared_line
            .map_or(line, |current| current.min(line)),
    );
    entry.last_line = Some(entry.last_line.map_or(line, |current| current.max(line)));
}

fn note_local_use(ranges: &mut HashMap<LocalSlot, LocalDebugRange>, slot: LocalSlot, line: u32) {
    let entry = ranges.entry(slot).or_default();
    entry.last_line = Some(entry.last_line.map_or(line, |current| current.max(line)));
}

fn merge_min_debug_line(current: Option<u32>, incoming: Option<u32>) -> Option<u32> {
    match (current, incoming) {
        (Some(lhs), Some(rhs)) => Some(lhs.min(rhs)),
        (Some(lhs), None) => Some(lhs),
        (None, Some(rhs)) => Some(rhs),
        (None, None) => None,
    }
}

fn merge_max_debug_line(current: Option<u32>, incoming: Option<u32>) -> Option<u32> {
    match (current, incoming) {
        (Some(lhs), Some(rhs)) => Some(lhs.max(rhs)),
        (Some(lhs), None) => Some(lhs),
        (None, Some(rhs)) => Some(rhs),
        (None, None) => None,
    }
}

fn stmt_source_line(stmt: &Stmt) -> u32 {
    match stmt {
        Stmt::Noop { line }
        | Stmt::Let { line, .. }
        | Stmt::Assign { line, .. }
        | Stmt::ClosureLet { line, .. }
        | Stmt::FuncDecl { line, .. }
        | Stmt::Expr { line, .. }
        | Stmt::IfElse { line, .. }
        | Stmt::For { line, .. }
        | Stmt::While { line, .. }
        | Stmt::Break { line }
        | Stmt::Continue { line }
        | Stmt::Drop { line, .. } => *line,
    }
}

pub(super) fn shift_amount_for_power_of_two(value: i64) -> Option<u32> {
    if value <= 0 {
        return None;
    }
    let as_u64 = value as u64;
    if !as_u64.is_power_of_two() {
        return None;
    }
    Some(as_u64.trailing_zeros())
}

pub(super) fn is_definitely_string_expr(expr: &Expr) -> bool {
    match expr {
        Expr::String(_) => true,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            is_definitely_string_expr(inner)
        }
        Expr::Add(lhs, rhs) => {
            (is_definitely_string_expr(lhs) && is_definitely_string_expr(rhs))
                || (is_definitely_string_expr(lhs) && eval_const_int_expr(rhs).is_some())
                || (eval_const_int_expr(lhs).is_some() && is_definitely_string_expr(rhs))
        }
        _ => false,
    }
}

pub(super) fn eval_const_int_expr(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Int(value) => Some(*value),
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            eval_const_int_expr(inner)
        }
        Expr::Neg(inner) => eval_const_int_expr(inner)?.checked_neg(),
        Expr::Add(lhs, rhs) => eval_const_int_expr(lhs)?.checked_add(eval_const_int_expr(rhs)?),
        Expr::Sub(lhs, rhs) => eval_const_int_expr(lhs)?.checked_sub(eval_const_int_expr(rhs)?),
        Expr::Mul(lhs, rhs) => eval_const_int_expr(lhs)?.checked_mul(eval_const_int_expr(rhs)?),
        Expr::Div(lhs, rhs) => {
            let rhs = eval_const_int_expr(rhs)?;
            if rhs == 0 {
                return None;
            }
            eval_const_int_expr(lhs)?.checked_div(rhs)
        }
        _ => None,
    }
}

pub(super) fn is_compiler_primitive_import(name: &str) -> bool {
    name.starts_with("__prim_")
}
