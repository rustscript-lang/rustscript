use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};

use super::super::ParseError;
use super::super::ir::{ClosureExpr, Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt};

type LiveSet = Vec<bool>;

#[derive(Clone, Copy)]
struct DefInfo {
    slot: LocalSlot,
    explicit_null: bool,
}

pub(super) struct LivenessRewriter {
    local_count: usize,
    clearable_slots: Vec<bool>,
    conservative_call_indices: HashSet<u16>,
    function_impls: HashMap<u16, FunctionImpl>,
    function_footprint_cache: RefCell<HashMap<u16, LiveSet>>,
    full_footprint: LiveSet,
}

impl LivenessRewriter {
    pub(super) fn new(
        local_count: usize,
        _local_bindings: &[(String, LocalSlot)],
        function_impls: &HashMap<u16, FunctionImpl>,
    ) -> Self {
        // Clear hidden and named slots alike. Hidden slots back closure captures,
        // inline-call parameters, and parser-generated temporaries, so excluding
        // them leaves stale values past their last use.
        let clearable_slots = vec![true; local_count];
        let conservative_call_indices = function_impls
            .iter()
            .filter_map(|(index, function_impl)| {
                function_impl_uses_local_call(function_impl).then_some(*index)
            })
            .collect::<HashSet<_>>();
        Self {
            local_count,
            clearable_slots,
            conservative_call_indices,
            function_impls: function_impls.clone(),
            function_footprint_cache: RefCell::new(HashMap::new()),
            full_footprint: vec![true; local_count],
        }
    }

    pub(super) fn rewrite_program_block(&self, stmts: &[Stmt]) -> Vec<Stmt> {
        let mut live_out = self.empty_set();
        for slot in persistent_capture_slots(stmts, &self.function_impls) {
            self.mark_live(&mut live_out, slot);
        }
        self.rewrite_block(stmts, &live_out, false).0
    }

    pub(super) fn rewrite_function_impl(
        &self,
        function_impl: FunctionImpl,
        persistent_slots: &[LocalSlot],
    ) -> FunctionImpl {
        let FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts,
            body_expr,
            body_expr_line,
        } = function_impl;
        let live_out = self.function_body_live_out(&body_expr, &capture_copies, persistent_slots);
        let (rewritten_body, _) = self.rewrite_block(&body_stmts, &live_out, false);
        FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts: rewritten_body,
            body_expr,
            body_expr_line,
        }
    }

    fn rewrite_block(
        &self,
        stmts: &[Stmt],
        live_out: &LiveSet,
        suppress_clears: bool,
    ) -> (Vec<Stmt>, LiveSet) {
        let mut live_after = live_out.clone();
        let mut rewritten_rev = Vec::<Stmt>::new();
        for stmt in stmts.iter().rev() {
            let (rewritten_stmt, live_before, defs) =
                self.rewrite_stmt(stmt, &live_after, suppress_clears);
            let clear_slots = if suppress_clears {
                Vec::new()
            } else {
                self.compute_clear_slots(&live_before, &live_after, &defs)
            };
            let clear_line = stmt_line(stmt);
            for slot in clear_slots.iter().rev() {
                rewritten_rev.push(Stmt::Drop {
                    index: *slot,
                    line: clear_line,
                });
            }
            rewritten_rev.push(rewritten_stmt);
            live_after = live_before;
        }
        rewritten_rev.reverse();
        (rewritten_rev, live_after)
    }

    fn rewrite_stmt(
        &self,
        stmt: &Stmt,
        live_after: &LiveSet,
        suppress_clears: bool,
    ) -> (Stmt, LiveSet, Vec<DefInfo>) {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {
                (stmt.clone(), live_after.clone(), Vec::new())
            }
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                let mut live_before = live_after.clone();
                if *has_impl && let Some(function_impl) = self.function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        self.kill_slot(&mut live_before, *captured_slot);
                        self.mark_live(&mut live_before, *source_slot);
                    }
                }
                (stmt.clone(), live_before, Vec::new())
            }
            Stmt::Drop { index, line } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                (
                    Stmt::Drop {
                        index: *index,
                        line: *line,
                    },
                    live_before,
                    vec![DefInfo {
                        slot: *index,
                        explicit_null: true,
                    }],
                )
            }
            Stmt::Expr { expr, line } => {
                let mut live_before = live_after.clone();
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                (
                    Stmt::Expr {
                        expr: expr.clone(),
                        line: *line,
                    },
                    live_before,
                    Vec::new(),
                )
            }
            Stmt::Let {
                index,
                declared_schema,
                expr,
                line,
            } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                (
                    Stmt::Let {
                        index: *index,
                        declared_schema: declared_schema.clone(),
                        expr: expr.clone(),
                        line: *line,
                    },
                    live_before,
                    vec![DefInfo {
                        slot: *index,
                        explicit_null: matches!(expr, Expr::Null),
                    }],
                )
            }
            Stmt::Assign {
                kind,
                index,
                expr,
                line,
            } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                (
                    Stmt::Assign {
                        kind: kind.clone(),
                        index: *index,
                        expr: expr.clone(),
                        line: *line,
                    },
                    live_before,
                    vec![DefInfo {
                        slot: *index,
                        explicit_null: matches!(expr, Expr::Null),
                    }],
                )
            }
            Stmt::ClosureLet { line, closure } => {
                let mut live_before = live_after.clone();
                let mut defs = Vec::with_capacity(closure.capture_copies.len());
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.kill_slot(&mut live_before, *captured_slot);
                    self.mark_live(&mut live_before, *source_slot);
                    defs.push(DefInfo {
                        slot: *captured_slot,
                        explicit_null: false,
                    });
                }
                (
                    Stmt::ClosureLet {
                        line: *line,
                        closure: closure.clone(),
                    },
                    live_before,
                    defs,
                )
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let (rewritten_then, then_live_before) =
                    self.rewrite_block(then_branch, live_after, suppress_clears);
                let (rewritten_else, else_live_before) =
                    self.rewrite_block(else_branch, live_after, suppress_clears);
                let mut live_before = then_live_before;
                self.union_inplace(&mut live_before, &else_live_before);
                self.union_inplace(&mut live_before, &self.uses_expr(condition));
                (
                    Stmt::IfElse {
                        condition: condition.clone(),
                        then_branch: rewritten_then,
                        else_branch: rewritten_else,
                        line: *line,
                    },
                    live_before,
                    Vec::new(),
                )
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                let cond_uses = self.uses_expr(condition);
                let mut live_cond = live_after.clone();
                self.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let body_live_before = self.compute_live_before_block(body, &live_cond);
                    let mut next = live_after.clone();
                    self.union_inplace(&mut next, &cond_uses);
                    self.union_inplace(&mut next, &body_live_before);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                let (rewritten_body, _) = self.rewrite_block(body, &live_cond, true);
                (
                    Stmt::While {
                        condition: condition.clone(),
                        body: rewritten_body,
                        line: *line,
                    },
                    live_cond,
                    Vec::new(),
                )
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                let cond_uses = self.uses_expr(condition);
                let mut live_cond = live_after.clone();
                self.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let post_live_before = self.compute_live_before_stmt(post, &live_cond);
                    let body_live_before = self.compute_live_before_block(body, &post_live_before);
                    let mut next = live_after.clone();
                    self.union_inplace(&mut next, &cond_uses);
                    self.union_inplace(&mut next, &body_live_before);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }

                let post_live_before = self.compute_live_before_stmt(post, &live_cond);
                let (rewritten_post, _, _) = self.rewrite_stmt(post, &live_cond, true);
                let (rewritten_body, _) = self.rewrite_block(body, &post_live_before, true);
                let (rewritten_init, live_before, _) =
                    self.rewrite_stmt(init, &live_cond, suppress_clears);
                (
                    Stmt::For {
                        init: Box::new(rewritten_init),
                        condition: condition.clone(),
                        post: Box::new(rewritten_post),
                        body: rewritten_body,
                        line: *line,
                    },
                    live_before,
                    Vec::new(),
                )
            }
        }
    }

    fn compute_live_before_block(&self, stmts: &[Stmt], live_out: &LiveSet) -> LiveSet {
        let mut live = live_out.clone();
        for stmt in stmts.iter().rev() {
            live = self.compute_live_before_stmt(stmt, &live);
        }
        live
    }

    fn compute_live_before_stmt(&self, stmt: &Stmt, live_after: &LiveSet) -> LiveSet {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => live_after.clone(),
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                let mut live_before = live_after.clone();
                if *has_impl && let Some(function_impl) = self.function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        self.kill_slot(&mut live_before, *captured_slot);
                        self.mark_live(&mut live_before, *source_slot);
                    }
                }
                live_before
            }
            Stmt::Drop { index, .. } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                live_before
            }
            Stmt::Expr { expr, .. } => {
                let mut live_before = live_after.clone();
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                live_before
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                live_before
            }
            Stmt::ClosureLet { closure, .. } => {
                let mut live_before = live_after.clone();
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.kill_slot(&mut live_before, *captured_slot);
                    self.mark_live(&mut live_before, *source_slot);
                }
                live_before
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let then_live = self.compute_live_before_block(then_branch, live_after);
                let else_live = self.compute_live_before_block(else_branch, live_after);
                let mut live_before = then_live;
                self.union_inplace(&mut live_before, &else_live);
                self.union_inplace(&mut live_before, &self.uses_expr(condition));
                live_before
            }
            Stmt::While {
                condition, body, ..
            } => {
                let cond_uses = self.uses_expr(condition);
                let mut live_cond = live_after.clone();
                self.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let body_live = self.compute_live_before_block(body, &live_cond);
                    let mut next = live_after.clone();
                    self.union_inplace(&mut next, &cond_uses);
                    self.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                live_cond
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                let cond_uses = self.uses_expr(condition);
                let mut live_cond = live_after.clone();
                self.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let post_live = self.compute_live_before_stmt(post, &live_cond);
                    let body_live = self.compute_live_before_block(body, &post_live);
                    let mut next = live_after.clone();
                    self.union_inplace(&mut next, &cond_uses);
                    self.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                self.compute_live_before_stmt(init, &live_cond)
            }
        }
    }

    fn uses_expr(&self, expr: &Expr) -> LiveSet {
        let mut live = self.empty_set();
        self.add_expr_uses(expr, &mut live);
        live
    }

    fn add_expr_uses(&self, expr: &Expr, live: &mut LiveSet) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..) => {}
            Expr::Var(index) | Expr::MoveVar(index) => self.mark_live(live, *index),
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                self.mark_live(live, *root)
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                self.mark_live(live, *container_slot);
                self.mark_live(live, *key_slot);
                self.add_expr_uses(container, live);
                self.add_expr_uses(key, live);
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.mark_live(live, *value_slot);
                self.add_expr_uses(value, live);
                self.add_expr_uses(fallback, live);
            }
            Expr::Call(index, _, args) => {
                for arg in args {
                    self.add_expr_uses(arg, live);
                }
                if self.function_impls.contains_key(index) {
                    let mut stack = Vec::new();
                    let footprint = self.function_footprint(*index, &mut stack);
                    self.union_inplace(live, &footprint);
                }
            }
            Expr::LocalCall(index, _, args) => {
                self.mark_live(live, *index);
                for arg in args {
                    self.add_expr_uses(arg, live);
                }
                // Local-call targets can be inline closures whose captured
                // slots are not directly visible from the call expression.
                // Keep locals live conservatively so closure captures are not
                // cleared before the call executes.
                live.fill(true);
            }
            Expr::Closure(closure) => {
                for (source_slot, _) in &closure.capture_copies {
                    self.mark_live(live, *source_slot);
                }
                self.add_expr_uses(&closure.body, live);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.add_expr_uses(arg, live);
                }
                for (source_slot, _) in &closure.capture_copies {
                    self.mark_live(live, *source_slot);
                }
                self.add_expr_uses(&closure.body, live);
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
                self.add_expr_uses(lhs, live);
                self.add_expr_uses(rhs, live);
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => self.add_expr_uses(inner, live),
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.add_expr_uses(condition, live);
                self.add_expr_uses(then_expr, live);
                self.add_expr_uses(else_expr, live);
            }
            Expr::Match {
                value,
                arms,
                default,
                ..
            } => {
                self.add_expr_uses(value, live);
                for (_, arm) in arms {
                    self.add_expr_uses(arm, live);
                }
                self.add_expr_uses(default, live);
            }
            Expr::Block { stmts, expr } => {
                let live_out = self.uses_expr(expr);
                let live_before = self.compute_live_before_block(stmts, &live_out);
                self.union_inplace(live, &live_before);
            }
        }
    }

    fn compute_clear_slots(
        &self,
        live_before: &LiveSet,
        live_after: &LiveSet,
        defs: &[DefInfo],
    ) -> Vec<LocalSlot> {
        let mut clear = vec![false; self.local_count];
        for slot in 0..self.local_count {
            if self.clearable_slots[slot] && live_before[slot] && !live_after[slot] {
                clear[slot] = true;
            }
        }
        for def in defs {
            let slot = def.slot as usize;
            if slot < self.local_count
                && self.clearable_slots[slot]
                && !live_after[slot]
                && !def.explicit_null
            {
                clear[slot] = true;
            }
        }
        clear
            .iter()
            .enumerate()
            .filter_map(|(slot, should_clear)| should_clear.then_some(slot as LocalSlot))
            .collect()
    }

    fn empty_set(&self) -> LiveSet {
        vec![false; self.local_count]
    }

    fn union_inplace(&self, target: &mut LiveSet, source: &LiveSet) {
        for (idx, bit) in source.iter().enumerate() {
            if *bit {
                target[idx] = true;
            }
        }
    }

    fn kill_slot(&self, live: &mut LiveSet, slot: LocalSlot) {
        let slot = slot as usize;
        if slot < self.local_count {
            live[slot] = false;
        }
    }

    fn mark_live(&self, live: &mut LiveSet, slot: LocalSlot) {
        let slot = slot as usize;
        if slot < self.local_count {
            live[slot] = true;
        }
    }

    fn function_body_live_out(
        &self,
        body_expr: &Expr,
        capture_copies: &[(LocalSlot, LocalSlot)],
        persistent_slots: &[LocalSlot],
    ) -> LiveSet {
        let mut live_out = self.uses_expr(body_expr);
        for (_, captured_slot) in capture_copies {
            self.mark_live(&mut live_out, *captured_slot);
        }
        for slot in persistent_slots {
            self.mark_live(&mut live_out, *slot);
        }
        live_out
    }

    fn function_footprint(&self, index: u16, stack: &mut Vec<u16>) -> LiveSet {
        if let Some(cached) = self.function_footprint_cache.borrow().get(&index).cloned() {
            return cached;
        }
        if stack.contains(&index) || self.conservative_call_indices.contains(&index) {
            return self.full_footprint.clone();
        }
        let Some(function_impl) = self.function_impls.get(&index) else {
            return self.empty_set();
        };

        stack.push(index);
        let mut footprint = self.empty_set();
        for slot in &function_impl.param_slots {
            self.mark_live(&mut footprint, *slot);
        }
        for (_, captured_slot) in &function_impl.capture_copies {
            self.mark_live(&mut footprint, *captured_slot);
        }
        for stmt in &function_impl.body_stmts {
            self.collect_stmt_footprint(stmt, &mut footprint, stack);
        }
        self.collect_expr_footprint(&function_impl.body_expr, &mut footprint, stack);
        stack.pop();

        self.function_footprint_cache
            .borrow_mut()
            .insert(index, footprint.clone());
        footprint
    }

    fn closure_footprint(&self, closure: &ClosureExpr, stack: &mut Vec<u16>) -> LiveSet {
        if expr_contains_local_call(&closure.body) {
            return self.full_footprint.clone();
        }

        let mut footprint = self.empty_set();
        for slot in &closure.param_slots {
            self.mark_live(&mut footprint, *slot);
        }
        for (source_slot, captured_slot) in &closure.capture_copies {
            self.mark_live(&mut footprint, *source_slot);
            self.mark_live(&mut footprint, *captured_slot);
        }
        self.collect_expr_footprint(&closure.body, &mut footprint, stack);
        footprint
    }

    fn collect_stmt_footprint(&self, stmt: &Stmt, footprint: &mut LiveSet, stack: &mut Vec<u16>) {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                if *has_impl && let Some(function_impl) = self.function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        self.mark_live(footprint, *source_slot);
                        self.mark_live(footprint, *captured_slot);
                    }
                }
            }
            Stmt::Drop { index, .. } => self.mark_live(footprint, *index),
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                self.mark_live(footprint, *index);
                self.collect_expr_footprint(expr, footprint, stack);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.mark_live(footprint, *source_slot);
                    self.mark_live(footprint, *captured_slot);
                }
            }
            Stmt::Expr { expr, .. } => self.collect_expr_footprint(expr, footprint, stack),
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.collect_expr_footprint(condition, footprint, stack);
                for nested in then_branch {
                    self.collect_stmt_footprint(nested, footprint, stack);
                }
                for nested in else_branch {
                    self.collect_stmt_footprint(nested, footprint, stack);
                }
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.collect_stmt_footprint(init, footprint, stack);
                self.collect_expr_footprint(condition, footprint, stack);
                self.collect_stmt_footprint(post, footprint, stack);
                for nested in body {
                    self.collect_stmt_footprint(nested, footprint, stack);
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.collect_expr_footprint(condition, footprint, stack);
                for nested in body {
                    self.collect_stmt_footprint(nested, footprint, stack);
                }
            }
        }
    }

    fn collect_expr_footprint(&self, expr: &Expr, footprint: &mut LiveSet, stack: &mut Vec<u16>) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..) => {}
            Expr::Var(index) | Expr::MoveVar(index) | Expr::LocalCall(index, _, _) => {
                self.mark_live(footprint, *index);
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                self.mark_live(footprint, *root);
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                self.mark_live(footprint, *container_slot);
                self.mark_live(footprint, *key_slot);
                self.collect_expr_footprint(container, footprint, stack);
                self.collect_expr_footprint(key, footprint, stack);
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.mark_live(footprint, *value_slot);
                self.collect_expr_footprint(value, footprint, stack);
                self.collect_expr_footprint(fallback, footprint, stack);
            }
            Expr::Call(index, _, args) => {
                let called = self.function_footprint(*index, stack);
                self.union_inplace(footprint, &called);
                for arg in args {
                    self.collect_expr_footprint(arg, footprint, stack);
                }
            }
            Expr::Closure(closure) => {
                for slot in &closure.param_slots {
                    self.mark_live(footprint, *slot);
                }
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.mark_live(footprint, *source_slot);
                    self.mark_live(footprint, *captured_slot);
                }
            }
            Expr::ClosureCall(closure, args) => {
                let called = self.closure_footprint(closure, stack);
                self.union_inplace(footprint, &called);
                for arg in args {
                    self.collect_expr_footprint(arg, footprint, stack);
                }
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
                self.collect_expr_footprint(lhs, footprint, stack);
                self.collect_expr_footprint(rhs, footprint, stack);
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => self.collect_expr_footprint(inner, footprint, stack),
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.collect_expr_footprint(condition, footprint, stack);
                self.collect_expr_footprint(then_expr, footprint, stack);
                self.collect_expr_footprint(else_expr, footprint, stack);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                self.mark_live(footprint, *value_slot);
                self.mark_live(footprint, *result_slot);
                self.collect_expr_footprint(value, footprint, stack);
                for (pattern, arm_expr) in arms {
                    if let Some(binding_slot) = pattern.binding_slot() {
                        self.mark_live(footprint, binding_slot);
                    }
                    self.collect_expr_footprint(arm_expr, footprint, stack);
                }
                self.collect_expr_footprint(default, footprint, stack);
            }
            Expr::Block { stmts, expr } => {
                for stmt in stmts {
                    self.collect_stmt_footprint(stmt, footprint, stack);
                }
                self.collect_expr_footprint(expr, footprint, stack);
            }
        }
    }
}

fn function_impl_uses_local_call(function_impl: &FunctionImpl) -> bool {
    function_impl
        .body_stmts
        .iter()
        .any(stmt_contains_local_call)
        || expr_contains_local_call(&function_impl.body_expr)
}

fn stmt_contains_local_call(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => false,
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            expr_contains_local_call(expr)
        }
        Stmt::ClosureLet { closure, .. } => expr_contains_local_call(&closure.body),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_local_call(condition)
                || then_branch.iter().any(stmt_contains_local_call)
                || else_branch.iter().any(stmt_contains_local_call)
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_contains_local_call(init)
                || expr_contains_local_call(condition)
                || stmt_contains_local_call(post)
                || body.iter().any(stmt_contains_local_call)
        }
        Stmt::While {
            condition, body, ..
        } => expr_contains_local_call(condition) || body.iter().any(stmt_contains_local_call),
    }
}

fn expr_contains_local_call(expr: &Expr) -> bool {
    match expr {
        Expr::LocalCall(..) => true,
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Bytes(_)
        | Expr::String(_)
        | Expr::FunctionRef(..)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::OptionalGet { container, key, .. } => {
            expr_contains_local_call(container) || expr_contains_local_call(key)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => expr_contains_local_call(value) || expr_contains_local_call(fallback),
        Expr::Call(_, _, args) => args.iter().any(expr_contains_local_call),
        Expr::Closure(closure) => expr_contains_local_call(&closure.body),
        Expr::ClosureCall(closure, args) => {
            args.iter().any(expr_contains_local_call) || expr_contains_local_call(&closure.body)
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
        | Expr::Gt(lhs, rhs) => expr_contains_local_call(lhs) || expr_contains_local_call(rhs),
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => expr_contains_local_call(inner),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_contains_local_call(condition)
                || expr_contains_local_call(then_expr)
                || expr_contains_local_call(else_expr)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            expr_contains_local_call(value)
                || arms
                    .iter()
                    .any(|(_, arm_expr)| expr_contains_local_call(arm_expr))
                || expr_contains_local_call(default)
        }
        Expr::Block { stmts, expr } => {
            stmts.iter().any(stmt_contains_local_call) || expr_contains_local_call(expr)
        }
    }
}

fn stmt_line(stmt: &Stmt) -> u32 {
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

pub(super) struct LocalSlotAllocator {
    local_count: usize,
    liveness: LivenessRewriter,
    function_impls: HashMap<u16, FunctionImpl>,
    adjacency: Vec<HashSet<usize>>,
    function_footprint_cache: HashMap<u16, LiveSet>,
    full_footprint: LiveSet,
}

impl LocalSlotAllocator {
    pub(super) fn new(
        local_count: usize,
        local_bindings: &[(String, LocalSlot)],
        function_impls: &HashMap<u16, FunctionImpl>,
    ) -> Self {
        let liveness = LivenessRewriter::new(local_count, local_bindings, function_impls);
        Self {
            local_count,
            liveness,
            function_impls: function_impls.clone(),
            adjacency: (0..local_count).map(|_| HashSet::new()).collect(),
            function_footprint_cache: HashMap::new(),
            full_footprint: vec![true; local_count],
        }
    }

    pub(super) fn allocate(mut self, mut ir: FrontendIr) -> Result<FrontendIr, ParseError> {
        let persistent_slots = persistent_capture_slots(&ir.stmts, &ir.function_impls);
        let mut live_out = self.liveness.empty_set();
        for slot in &persistent_slots {
            self.liveness.mark_live(&mut live_out, *slot);
        }
        let _ = self.collect_block(&ir.stmts, &live_out)?;
        for function_impl in ir.function_impls.values() {
            let live_after = self.liveness.function_body_live_out(
                &function_impl.body_expr,
                &function_impl.capture_copies,
                &persistent_slots,
            );
            self.add_live_clique(&live_after);
            self.collect_expr_constraints(&function_impl.body_expr, &live_after)?;
            let _ = self.collect_block(&function_impl.body_stmts, &live_after)?;
        }

        let (mapping, compacted_local_count) = self.color_slots()?;
        remap_frontend_ir(&mut ir, &mapping, compacted_local_count)?;
        Ok(ir)
    }

    fn collect_block(&mut self, stmts: &[Stmt], live_out: &LiveSet) -> Result<LiveSet, ParseError> {
        let mut live_after = live_out.clone();
        self.add_live_clique(&live_after);
        for stmt in stmts.iter().rev() {
            let live_before = self.liveness.compute_live_before_stmt(stmt, &live_after);
            self.add_live_clique(&live_before);
            self.add_stmt_def_edges(stmt, &live_after);
            self.collect_stmt_constraints(stmt, &live_before, &live_after)?;
            live_after = live_before;
        }
        Ok(live_after)
    }

    fn collect_stmt_constraints(
        &mut self,
        stmt: &Stmt,
        live_before: &LiveSet,
        live_after: &LiveSet,
    ) -> Result<(), ParseError> {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Drop { .. } => {}
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                if *has_impl && let Some(function_impl) = self.function_impls.get(index) {
                    let capture_copies = function_impl.capture_copies.clone();
                    for (source_slot, captured_slot) in capture_copies {
                        self.add_slot_live_edges(source_slot, live_before);
                        self.add_slot_live_edges(captured_slot, live_before);
                    }
                }
            }
            Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                self.collect_expr_constraints(expr, live_before)?;
            }
            Stmt::ClosureLet { closure, .. } => {
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.add_slot_live_edges(*source_slot, live_before);
                    self.add_slot_live_edges(*captured_slot, live_before);
                }
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.collect_expr_constraints(condition, live_before)?;
                let _ = self.collect_block(then_branch, live_after)?;
                let _ = self.collect_block(else_branch, live_after)?;
            }
            Stmt::While {
                condition, body, ..
            } => {
                let cond_uses = self.liveness.uses_expr(condition);
                let mut live_cond = live_after.clone();
                self.liveness.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let body_live = self.liveness.compute_live_before_block(body, &live_cond);
                    let mut next = live_after.clone();
                    self.liveness.union_inplace(&mut next, &cond_uses);
                    self.liveness.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                self.collect_expr_constraints(condition, &live_cond)?;
                let _ = self.collect_block(body, &live_cond)?;
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                let cond_uses = self.liveness.uses_expr(condition);
                let mut live_cond = live_after.clone();
                self.liveness.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let post_live = self.liveness.compute_live_before_stmt(post, &live_cond);
                    let body_live = self.liveness.compute_live_before_block(body, &post_live);
                    let mut next = live_after.clone();
                    self.liveness.union_inplace(&mut next, &cond_uses);
                    self.liveness.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                let post_live_before = self.liveness.compute_live_before_stmt(post, &live_cond);
                self.collect_expr_constraints(condition, &live_cond)?;
                self.collect_stmt_constraints(post, &post_live_before, &live_cond)?;
                let _ = self.collect_block(body, &post_live_before)?;
                self.collect_stmt_constraints(init, live_before, &live_cond)?;
            }
        }
        Ok(())
    }

    fn collect_expr_constraints(&mut self, expr: &Expr, live: &LiveSet) -> Result<(), ParseError> {
        let mut live_during = live.clone();
        self.liveness
            .union_inplace(&mut live_during, &self.liveness.uses_expr(expr));
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..) => {}
            Expr::Var(index) | Expr::MoveVar(index) => {
                self.add_slot_live_edges(*index, &live_during);
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                self.add_slot_live_edges(*root, &live_during);
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                self.add_slot_live_edges(*container_slot, &live_during);
                self.add_slot_live_edges(*key_slot, &live_during);
                self.collect_expr_constraints(container, &live_during)?;
                self.collect_expr_constraints(key, &live_during)?;
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.add_slot_live_edges(*value_slot, &live_during);
                self.collect_expr_constraints(value, &live_during)?;
                self.collect_expr_constraints(fallback, &live_during)?;
            }
            Expr::Call(index, _, args) => {
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during)?;
                }
                if self.function_impls.contains_key(index) {
                    let mut stack = Vec::new();
                    let footprint = self.function_footprint(*index, &mut stack);
                    self.add_cross_live_with_set(&live_during, &footprint);
                }
            }
            Expr::LocalCall(index, _, args) => {
                self.add_slot_live_edges(*index, &live_during);
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during)?;
                }
                let full_footprint = self.full_footprint.clone();
                self.add_cross_live_with_set(&live_during, &full_footprint);
            }
            Expr::Closure(_closure) => {}
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during)?;
                }
                let mut stack = Vec::new();
                let footprint = self.closure_footprint(closure, &mut stack);
                self.add_cross_live_with_set(&live_during, &footprint);
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
                self.collect_expr_constraints(lhs, &live_during)?;
                self.collect_expr_constraints(rhs, &live_during)?;
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => {
                self.collect_expr_constraints(inner, &live_during)?;
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.collect_expr_constraints(condition, &live_during)?;
                self.collect_expr_constraints(then_expr, &live_during)?;
                self.collect_expr_constraints(else_expr, &live_during)?;
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                self.add_slot_live_edges(*value_slot, &live_during);
                self.add_slot_live_edges(*result_slot, &live_during);
                self.collect_expr_constraints(value, &live_during)?;
                for (pattern, arm_expr) in arms {
                    if let Some(binding_slot) = pattern.binding_slot() {
                        self.add_slot_live_edges(binding_slot, &live_during);
                    }
                    self.collect_expr_constraints(arm_expr, &live_during)?;
                }
                self.collect_expr_constraints(default, &live_during)?;
            }
            Expr::Block { stmts, expr } => {
                self.collect_expr_constraints(expr, &live_during)?;
                let mut block_live_out = live_during.clone();
                self.liveness
                    .union_inplace(&mut block_live_out, &self.liveness.uses_expr(expr));
                let _ = self.collect_block(stmts, &block_live_out)?;
            }
        }
        Ok(())
    }

    fn function_footprint(&mut self, index: u16, stack: &mut Vec<u16>) -> LiveSet {
        if let Some(cached) = self.function_footprint_cache.get(&index) {
            return cached.clone();
        }
        if stack.contains(&index) {
            return self.full_footprint.clone();
        }
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return self.liveness.empty_set();
        };
        stack.push(index);
        let mut footprint = self.liveness.empty_set();
        for slot in &function_impl.param_slots {
            self.mark_set_slot(&mut footprint, *slot);
        }
        for stmt in &function_impl.body_stmts {
            self.collect_stmt_footprint(stmt, &mut footprint, stack);
        }
        self.collect_expr_footprint(&function_impl.body_expr, &mut footprint, stack);
        stack.pop();
        self.function_footprint_cache
            .insert(index, footprint.clone());
        footprint
    }

    fn closure_footprint(&mut self, closure: &ClosureExpr, stack: &mut Vec<u16>) -> LiveSet {
        let mut footprint = self.liveness.empty_set();
        for slot in &closure.param_slots {
            self.mark_set_slot(&mut footprint, *slot);
        }
        for (source_slot, captured_slot) in &closure.capture_copies {
            self.mark_set_slot(&mut footprint, *source_slot);
            self.mark_set_slot(&mut footprint, *captured_slot);
        }
        self.collect_expr_footprint(&closure.body, &mut footprint, stack);
        footprint
    }

    fn collect_stmt_footprint(&mut self, stmt: &Stmt, set: &mut LiveSet, stack: &mut Vec<u16>) {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::FuncDecl {
                index, has_impl, ..
            } => {
                if *has_impl && let Some(function_impl) = self.function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        self.mark_set_slot(set, *source_slot);
                        self.mark_set_slot(set, *captured_slot);
                    }
                }
            }
            Stmt::Drop { index, .. } => {
                self.mark_set_slot(set, *index);
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                self.mark_set_slot(set, *index);
                self.collect_expr_footprint(expr, set, stack);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.mark_set_slot(set, *source_slot);
                    self.mark_set_slot(set, *captured_slot);
                }
            }
            Stmt::Expr { expr, .. } => self.collect_expr_footprint(expr, set, stack),
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.collect_expr_footprint(condition, set, stack);
                for nested in then_branch {
                    self.collect_stmt_footprint(nested, set, stack);
                }
                for nested in else_branch {
                    self.collect_stmt_footprint(nested, set, stack);
                }
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.collect_stmt_footprint(init, set, stack);
                self.collect_expr_footprint(condition, set, stack);
                self.collect_stmt_footprint(post, set, stack);
                for nested in body {
                    self.collect_stmt_footprint(nested, set, stack);
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.collect_expr_footprint(condition, set, stack);
                for nested in body {
                    self.collect_stmt_footprint(nested, set, stack);
                }
            }
        }
    }

    fn collect_expr_footprint(&mut self, expr: &Expr, set: &mut LiveSet, stack: &mut Vec<u16>) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..) => {}
            Expr::Var(index) | Expr::MoveVar(index) | Expr::LocalCall(index, _, _) => {
                self.mark_set_slot(set, *index)
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                self.mark_set_slot(set, *root);
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                self.mark_set_slot(set, *container_slot);
                self.mark_set_slot(set, *key_slot);
                self.collect_expr_footprint(container, set, stack);
                self.collect_expr_footprint(key, set, stack);
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.mark_set_slot(set, *value_slot);
                self.collect_expr_footprint(value, set, stack);
                self.collect_expr_footprint(fallback, set, stack);
            }
            Expr::Call(index, _, args) => {
                if self.function_impls.contains_key(index) {
                    let footprint = self.function_footprint(*index, stack);
                    for (slot, used) in footprint.iter().enumerate() {
                        if *used {
                            set[slot] = true;
                        }
                    }
                }
                for arg in args {
                    self.collect_expr_footprint(arg, set, stack);
                }
            }
            Expr::Closure(closure) => {
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.mark_set_slot(set, *source_slot);
                    self.mark_set_slot(set, *captured_slot);
                }
                for slot in &closure.param_slots {
                    self.mark_set_slot(set, *slot);
                }
            }
            Expr::ClosureCall(closure, args) => {
                let footprint = self.closure_footprint(closure, stack);
                for (slot, used) in footprint.iter().enumerate() {
                    if *used {
                        set[slot] = true;
                    }
                }
                for arg in args {
                    self.collect_expr_footprint(arg, set, stack);
                }
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
                self.collect_expr_footprint(lhs, set, stack);
                self.collect_expr_footprint(rhs, set, stack);
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => self.collect_expr_footprint(inner, set, stack),
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.collect_expr_footprint(condition, set, stack);
                self.collect_expr_footprint(then_expr, set, stack);
                self.collect_expr_footprint(else_expr, set, stack);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                self.mark_set_slot(set, *value_slot);
                self.mark_set_slot(set, *result_slot);
                self.collect_expr_footprint(value, set, stack);
                for (pattern, arm_expr) in arms {
                    if let Some(binding_slot) = pattern.binding_slot() {
                        self.mark_set_slot(set, binding_slot);
                    }
                    self.collect_expr_footprint(arm_expr, set, stack);
                }
                self.collect_expr_footprint(default, set, stack);
            }
            Expr::Block { stmts, expr } => {
                for nested in stmts {
                    self.collect_stmt_footprint(nested, set, stack);
                }
                self.collect_expr_footprint(expr, set, stack);
            }
        }
    }

    fn add_stmt_def_edges(&mut self, stmt: &Stmt, live_after: &LiveSet) {
        match stmt {
            Stmt::Let { index, .. } | Stmt::Assign { index, .. } | Stmt::Drop { index, .. } => {
                self.add_slot_live_edges(*index, live_after);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (_, captured_slot) in &closure.capture_copies {
                    self.add_slot_live_edges(*captured_slot, live_after);
                }
            }
            _ => {}
        }
    }

    fn add_live_clique(&mut self, live: &LiveSet) {
        let mut members = Vec::new();
        for (idx, active) in live.iter().enumerate() {
            if *active {
                members.push(idx);
            }
        }
        for left in 0..members.len() {
            for right in (left + 1)..members.len() {
                self.add_edge(members[left], members[right]);
            }
        }
    }

    fn add_slot_live_edges(&mut self, slot: LocalSlot, live: &LiveSet) {
        let slot_idx = slot as usize;
        if slot_idx >= self.local_count {
            return;
        }
        for (idx, active) in live.iter().enumerate() {
            if *active {
                self.add_edge(slot_idx, idx);
            }
        }
    }

    fn add_cross_live_with_set(&mut self, live: &LiveSet, other: &LiveSet) {
        let mut live_members = Vec::new();
        let mut other_members = Vec::new();
        for (idx, active) in live.iter().enumerate() {
            if *active {
                live_members.push(idx);
            }
        }
        for (idx, active) in other.iter().enumerate() {
            if *active {
                other_members.push(idx);
            }
        }
        for lhs in &live_members {
            for rhs in &other_members {
                self.add_edge(*lhs, *rhs);
            }
        }
    }

    fn add_edge(&mut self, lhs: usize, rhs: usize) {
        if lhs == rhs || lhs >= self.local_count || rhs >= self.local_count {
            return;
        }
        self.adjacency[lhs].insert(rhs);
        self.adjacency[rhs].insert(lhs);
    }

    fn mark_set_slot(&self, set: &mut LiveSet, slot: LocalSlot) {
        let idx = slot as usize;
        if idx < self.local_count {
            set[idx] = true;
        }
    }

    fn color_slots(&self) -> Result<(Vec<LocalSlot>, usize), ParseError> {
        let mut nodes = (0..self.local_count).collect::<Vec<_>>();
        nodes.sort_by_key(|idx| (Reverse(self.adjacency[*idx].len()), *idx));

        let mut colors = vec![LocalSlot::MAX; self.local_count];
        let mut used = [false; (u8::MAX as usize) + 1];
        let mut max_color = 0usize;

        for node in nodes {
            used.fill(false);
            for neighbor in &self.adjacency[node] {
                let color = colors[*neighbor];
                if color != LocalSlot::MAX {
                    used[color as usize] = true;
                }
            }
            let Some(color) = used.iter().position(|occupied| !occupied) else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: format!(
                        "too many simultaneously live locals (requires more than {} slots)",
                        (u8::MAX as usize) + 1
                    ),
                });
            };
            colors[node] = color as LocalSlot;
            if color > max_color {
                max_color = color;
            }
        }

        let compacted_local_count = if self.local_count == 0 {
            0
        } else {
            max_color + 1
        };
        Ok((colors, compacted_local_count))
    }
}

fn remap_frontend_ir(
    ir: &mut FrontendIr,
    mapping: &[LocalSlot],
    compacted_local_count: usize,
) -> Result<(), ParseError> {
    for stmt in &mut ir.stmts {
        remap_stmt_slots(stmt, mapping)?;
    }
    for function_impl in ir.function_impls.values_mut() {
        for slot in &mut function_impl.param_slots {
            *slot = remap_slot(*slot, mapping)?;
        }
        for (source_slot, captured_slot) in &mut function_impl.capture_copies {
            *source_slot = remap_slot(*source_slot, mapping)?;
            *captured_slot = remap_slot(*captured_slot, mapping)?;
        }
        for stmt in &mut function_impl.body_stmts {
            remap_stmt_slots(stmt, mapping)?;
        }
        remap_expr_slots(&mut function_impl.body_expr, mapping)?;
    }

    for (_, index) in &mut ir.local_bindings {
        *index = remap_slot(*index, mapping)?;
    }
    ir.local_bindings
        .sort_by(|(lhs_name, lhs_slot), (rhs_name, rhs_slot)| {
            lhs_slot.cmp(rhs_slot).then_with(|| lhs_name.cmp(rhs_name))
        });
    ir.locals = compacted_local_count;
    Ok(())
}

pub(super) fn persistent_capture_slots(
    stmts: &[Stmt],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    collect_persistent_closure_sources_from_stmts(stmts, &mut slots);
    for function_impl in function_impls.values() {
        collect_persistent_closure_sources_from_stmts(&function_impl.body_stmts, &mut slots);
        collect_persistent_closure_sources_from_expr(&function_impl.body_expr, &mut slots);
    }
    for stmt in stmts {
        let Stmt::FuncDecl {
            index, has_impl, ..
        } = stmt
        else {
            continue;
        };
        if !has_impl {
            continue;
        }
        let Some(function_impl) = function_impls.get(index) else {
            continue;
        };
        for (source_slot, captured_slot) in &function_impl.capture_copies {
            slots.insert(*captured_slot);
            if matches!(
                super::availability::function_capture_binding_mode(function_impl, *captured_slot),
                crate::CaptureBindingMode::Borrow | crate::CaptureBindingMode::BorrowMut
            ) {
                slots.insert(*source_slot);
            }
        }
    }
    for function_impl in function_impls.values() {
        for (source_slot, captured_slot) in &function_impl.capture_copies {
            slots.insert(*captured_slot);
            if matches!(
                super::availability::function_capture_binding_mode(function_impl, *captured_slot),
                crate::CaptureBindingMode::Borrow | crate::CaptureBindingMode::BorrowMut
            ) {
                slots.insert(*source_slot);
            }
        }
    }
    slots.into_iter().collect()
}

fn collect_persistent_closure_sources(
    closure: &super::super::ir::ClosureExpr,
    slots: &mut BTreeSet<LocalSlot>,
) {
    for (source_slot, captured_slot) in &closure.capture_copies {
        slots.insert(*captured_slot);
        if matches!(
            super::availability::closure_capture_binding_mode(closure, *captured_slot),
            crate::CaptureBindingMode::Borrow | crate::CaptureBindingMode::BorrowMut
        ) {
            slots.insert(*source_slot);
        }
    }
    collect_persistent_closure_sources_from_expr(&closure.body, slots);
}

fn collect_persistent_closure_sources_from_stmts(stmts: &[Stmt], slots: &mut BTreeSet<LocalSlot>) {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Drop { .. } => {}
            Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                collect_persistent_closure_sources_from_expr(expr, slots);
            }
            Stmt::ClosureLet { closure, .. } => {
                collect_persistent_closure_sources(closure, slots);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                collect_persistent_closure_sources_from_expr(condition, slots);
                collect_persistent_closure_sources_from_stmts(then_branch, slots);
                collect_persistent_closure_sources_from_stmts(else_branch, slots);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                collect_persistent_closure_sources_from_stmts(
                    core::slice::from_ref(init.as_ref()),
                    slots,
                );
                collect_persistent_closure_sources_from_expr(condition, slots);
                collect_persistent_closure_sources_from_stmts(
                    core::slice::from_ref(post.as_ref()),
                    slots,
                );
                collect_persistent_closure_sources_from_stmts(body, slots);
            }
            Stmt::While {
                condition, body, ..
            } => {
                collect_persistent_closure_sources_from_expr(condition, slots);
                collect_persistent_closure_sources_from_stmts(body, slots);
            }
        }
    }
}

fn collect_persistent_closure_sources_from_expr(expr: &Expr, slots: &mut BTreeSet<LocalSlot>) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Bytes(_)
        | Expr::FunctionRef(..)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => {}
        Expr::OptionalGet { container, key, .. } => {
            collect_persistent_closure_sources_from_expr(container, slots);
            collect_persistent_closure_sources_from_expr(key, slots);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            collect_persistent_closure_sources_from_expr(value, slots);
            collect_persistent_closure_sources_from_expr(fallback, slots);
        }
        Expr::Call(_, _, args) | Expr::LocalCall(_, _, args) => {
            for arg in args {
                collect_persistent_closure_sources_from_expr(arg, slots);
            }
        }
        Expr::Closure(closure) => collect_persistent_closure_sources(closure, slots),
        Expr::ClosureCall(closure, args) => {
            collect_persistent_closure_sources(closure, slots);
            for arg in args {
                collect_persistent_closure_sources_from_expr(arg, slots);
            }
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
            collect_persistent_closure_sources_from_expr(lhs, slots);
            collect_persistent_closure_sources_from_expr(rhs, slots);
        }
        Expr::Neg(value)
        | Expr::Not(value)
        | Expr::ToOwned(value)
        | Expr::Borrow(value)
        | Expr::BorrowMut(value) => {
            collect_persistent_closure_sources_from_expr(value, slots);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_persistent_closure_sources_from_expr(condition, slots);
            collect_persistent_closure_sources_from_expr(then_expr, slots);
            collect_persistent_closure_sources_from_expr(else_expr, slots);
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            collect_persistent_closure_sources_from_expr(value, slots);
            for (_, arm) in arms {
                collect_persistent_closure_sources_from_expr(arm, slots);
            }
            collect_persistent_closure_sources_from_expr(default, slots);
        }
        Expr::Block { stmts, expr } => {
            collect_persistent_closure_sources_from_stmts(stmts, slots);
            collect_persistent_closure_sources_from_expr(expr, slots);
        }
    }
}

fn remap_slot(index: LocalSlot, mapping: &[LocalSlot]) -> Result<LocalSlot, ParseError> {
    let slot = index as usize;
    mapping.get(slot).copied().ok_or(ParseError {
        span: None,
        code: None,
        line: 1,
        message: "internal local slot remap out of range".to_string(),
    })
}

fn remap_stmt_slots(stmt: &mut Stmt, mapping: &[LocalSlot]) -> Result<(), ParseError> {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, .. } => {
            *index = remap_slot(*index, mapping)?;
        }
        Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
            *index = remap_slot(*index, mapping)?;
            remap_expr_slots(expr, mapping)?;
        }
        Stmt::ClosureLet { closure, .. } => {
            for (source_slot, captured_slot) in &mut closure.capture_copies {
                *source_slot = remap_slot(*source_slot, mapping)?;
                *captured_slot = remap_slot(*captured_slot, mapping)?;
            }
            remap_expr_slots(&mut closure.body, mapping)?;
        }
        Stmt::Expr { expr, .. } => {
            remap_expr_slots(expr, mapping)?;
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            remap_expr_slots(condition, mapping)?;
            for nested in then_branch {
                remap_stmt_slots(nested, mapping)?;
            }
            for nested in else_branch {
                remap_stmt_slots(nested, mapping)?;
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            remap_stmt_slots(init, mapping)?;
            remap_expr_slots(condition, mapping)?;
            remap_stmt_slots(post, mapping)?;
            for nested in body {
                remap_stmt_slots(nested, mapping)?;
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            remap_expr_slots(condition, mapping)?;
            for nested in body {
                remap_stmt_slots(nested, mapping)?;
            }
        }
    }
    Ok(())
}

fn remap_expr_slots(expr: &mut Expr, mapping: &[LocalSlot]) -> Result<(), ParseError> {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Bytes(_)
        | Expr::String(_) => {}
        Expr::FunctionRef(..) => {}
        Expr::Call(_, _, args) => {
            for arg in args {
                remap_expr_slots(arg, mapping)?;
            }
        }
        Expr::LocalCall(index, _, args) => {
            *index = remap_slot(*index, mapping)?;
            for arg in args {
                remap_expr_slots(arg, mapping)?;
            }
        }
        Expr::Closure(closure) | Expr::ClosureCall(closure, _) => {
            for slot in &mut closure.param_slots {
                *slot = remap_slot(*slot, mapping)?;
            }
            for (source_slot, captured_slot) in &mut closure.capture_copies {
                *source_slot = remap_slot(*source_slot, mapping)?;
                *captured_slot = remap_slot(*captured_slot, mapping)?;
            }
            remap_expr_slots(&mut closure.body, mapping)?;
            if let Expr::ClosureCall(_, args) = expr {
                for arg in args {
                    remap_expr_slots(arg, mapping)?;
                }
            }
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
            remap_expr_slots(lhs, mapping)?;
            remap_expr_slots(rhs, mapping)?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            remap_expr_slots(inner, mapping)?;
        }
        Expr::Var(index) | Expr::MoveVar(index) => {
            *index = remap_slot(*index, mapping)?;
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            *root = remap_slot(*root, mapping)?;
        }
        Expr::OptionalGet {
            container,
            key,
            container_slot,
            key_slot,
        } => {
            *container_slot = remap_slot(*container_slot, mapping)?;
            *key_slot = remap_slot(*key_slot, mapping)?;
            remap_expr_slots(container, mapping)?;
            remap_expr_slots(key, mapping)?;
        }
        Expr::OptionUnwrapOr {
            value,
            value_slot,
            fallback,
        } => {
            *value_slot = remap_slot(*value_slot, mapping)?;
            remap_expr_slots(value, mapping)?;
            remap_expr_slots(fallback, mapping)?;
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            remap_expr_slots(condition, mapping)?;
            remap_expr_slots(then_expr, mapping)?;
            remap_expr_slots(else_expr, mapping)?;
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            *value_slot = remap_slot(*value_slot, mapping)?;
            *result_slot = remap_slot(*result_slot, mapping)?;
            remap_expr_slots(value, mapping)?;
            for (pattern, arm_expr) in arms {
                if let crate::compiler::ir::MatchPattern::SomeBinding(binding_slot) = pattern {
                    *binding_slot = remap_slot(*binding_slot, mapping)?;
                }
                remap_expr_slots(arm_expr, mapping)?;
            }
            remap_expr_slots(default, mapping)?;
        }
        Expr::Block { stmts, expr } => {
            for nested in stmts {
                remap_stmt_slots(nested, mapping)?;
            }
            remap_expr_slots(expr, mapping)?;
        }
    }
    Ok(())
}
