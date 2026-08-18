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
    function_impls: HashMap<u16, FunctionImpl>,
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
        Self {
            local_count,
            clearable_slots,
            function_impls: function_impls.clone(),
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
        self.compute_live_before_block_impl(stmts, live_out, true)
    }

    /// Like `compute_live_before_block` but without the conservative
    /// dynamic-local-call fill: the slot allocator needs the actual live
    /// sets, not the drop-insertion safety margin, so a `LocalCall` does not
    /// turn every statement's live set (and therefore the interference
    /// graph) into the whole program.
    fn compute_live_before_block_precise(&self, stmts: &[Stmt], live_out: &LiveSet) -> LiveSet {
        self.compute_live_before_block_impl(stmts, live_out, false)
    }

    fn compute_live_before_block_impl(
        &self,
        stmts: &[Stmt],
        live_out: &LiveSet,
        conservative: bool,
    ) -> LiveSet {
        let mut live = live_out.clone();
        for stmt in stmts.iter().rev() {
            live = self.compute_live_before_stmt_impl(stmt, &live, conservative);
        }
        live
    }

    fn compute_live_before_stmt(&self, stmt: &Stmt, live_after: &LiveSet) -> LiveSet {
        self.compute_live_before_stmt_impl(stmt, live_after, true)
    }

    /// Like `compute_live_before_stmt` but without the conservative
    /// dynamic-local-call fill (see `compute_live_before_block_precise`).
    fn compute_live_before_stmt_precise(&self, stmt: &Stmt, live_after: &LiveSet) -> LiveSet {
        self.compute_live_before_stmt_impl(stmt, live_after, false)
    }

    fn compute_live_before_stmt_impl(
        &self,
        stmt: &Stmt,
        live_after: &LiveSet,
        conservative: bool,
    ) -> LiveSet {
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
                let uses = if conservative {
                    self.uses_expr(expr)
                } else {
                    self.uses_expr_precise(expr)
                };
                self.union_inplace(&mut live_before, &uses);
                live_before
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                let uses = if conservative {
                    self.uses_expr(expr)
                } else {
                    self.uses_expr_precise(expr)
                };
                self.union_inplace(&mut live_before, &uses);
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
                let then_live =
                    self.compute_live_before_block_impl(then_branch, live_after, conservative);
                let else_live =
                    self.compute_live_before_block_impl(else_branch, live_after, conservative);
                let mut live_before = then_live;
                self.union_inplace(&mut live_before, &else_live);
                let cond_uses = if conservative {
                    self.uses_expr(condition)
                } else {
                    self.uses_expr_precise(condition)
                };
                self.union_inplace(&mut live_before, &cond_uses);
                live_before
            }
            Stmt::While {
                condition, body, ..
            } => {
                let cond_uses = if conservative {
                    self.uses_expr(condition)
                } else {
                    self.uses_expr_precise(condition)
                };
                let mut live_cond = live_after.clone();
                self.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let body_live =
                        self.compute_live_before_block_impl(body, &live_cond, conservative);
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
                let cond_uses = if conservative {
                    self.uses_expr(condition)
                } else {
                    self.uses_expr_precise(condition)
                };
                let mut live_cond = live_after.clone();
                self.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let post_live =
                        self.compute_live_before_stmt_impl(post, &live_cond, conservative);
                    let body_live =
                        self.compute_live_before_block_impl(body, &post_live, conservative);
                    let mut next = live_after.clone();
                    self.union_inplace(&mut next, &cond_uses);
                    self.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                self.compute_live_before_stmt_impl(init, &live_cond, conservative)
            }
        }
    }

    fn uses_expr(&self, expr: &Expr) -> LiveSet {
        let mut live = self.empty_set();
        self.add_expr_uses(expr, &mut live);
        live
    }

    /// Like `uses_expr` but without the conservative dynamic-local-call
    /// fill: `Expr::LocalCall` contributes only its target slot and argument
    /// uses. The liveness *rewriter* keeps the conservative fill so captured
    /// slots are never cleared before a dynamic call executes; the slot
    /// *allocator* uses this precise variant so a single closure- or
    /// callable-variable call does not turn the whole program's live sets
    /// (and therefore the interference graph) into one complete clique.
    fn uses_expr_precise(&self, expr: &Expr) -> LiveSet {
        let mut live = self.empty_set();
        self.add_expr_uses_impl(expr, &mut live, false);
        live
    }

    fn add_expr_uses(&self, expr: &Expr, live: &mut LiveSet) {
        self.add_expr_uses_impl(expr, live, true);
    }

    fn add_expr_uses_impl(&self, expr: &Expr, live: &mut LiveSet, conservative: bool) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. } => {}
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
                self.add_expr_uses_impl(container, live, conservative);
                self.add_expr_uses_impl(key, live, conservative);
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.mark_live(live, *value_slot);
                self.add_expr_uses_impl(value, live, conservative);
                self.add_expr_uses_impl(fallback, live, conservative);
            }
            Expr::Call(_, _, args, _) => {
                // Known named script calls execute in a separate runtime frame
                // with its own local_base: the callee body footprint is
                // analyzed inside the callee frame and must not be unioned
                // into the caller live set. Arguments and caller-after-call
                // uses stay live in the caller.
                for arg in args {
                    self.add_expr_uses_impl(arg, live, conservative);
                }
            }
            // Resolved module calls (pre-merge only) contribute their
            // arguments' uses; the callee lives in another unit and its
            // footprint is folded in by the post-merge call lowering.
            Expr::ModuleCall(_, _, args) => {
                for arg in args {
                    self.add_expr_uses_impl(arg, live, conservative);
                }
            }
            Expr::LocalCall(index, _, args) => {
                self.mark_live(live, *index);
                for arg in args {
                    self.add_expr_uses_impl(arg, live, conservative);
                }
                if conservative {
                    // Local-call targets can be inline closures whose captured
                    // slots are not directly visible from the call expression.
                    // Keep locals live conservatively so closure captures are
                    // not cleared before the call executes. The allocator's
                    // precise variant (used for interference constraints)
                    // skips this fill so a dynamic call cannot collapse the
                    // whole program into one interference clique.
                    live.fill(true);
                }
            }
            Expr::Closure(closure) => {
                for (source_slot, _) in &closure.capture_copies {
                    self.mark_live(live, *source_slot);
                }
                self.add_expr_uses_impl(&closure.body, live, conservative);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.add_expr_uses_impl(arg, live, conservative);
                }
                for (source_slot, _) in &closure.capture_copies {
                    self.mark_live(live, *source_slot);
                }
                self.add_expr_uses_impl(&closure.body, live, conservative);
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
                self.add_expr_uses_impl(lhs, live, conservative);
                self.add_expr_uses_impl(rhs, live, conservative);
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => self.add_expr_uses_impl(inner, live, conservative),
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.add_expr_uses_impl(condition, live, conservative);
                self.add_expr_uses_impl(then_expr, live, conservative);
                self.add_expr_uses_impl(else_expr, live, conservative);
            }
            Expr::Match {
                value,
                arms,
                default,
                ..
            } => {
                self.add_expr_uses_impl(value, live, conservative);
                for (_, arm) in arms {
                    self.add_expr_uses_impl(arm, live, conservative);
                }
                self.add_expr_uses_impl(default, live, conservative);
            }
            Expr::Block { stmts, expr } => {
                let live_out = if conservative {
                    self.uses_expr(expr)
                } else {
                    self.uses_expr_precise(expr)
                };
                let live_before = if conservative {
                    self.compute_live_before_block(stmts, &live_out)
                } else {
                    self.compute_live_before_block_precise(stmts, &live_out)
                };
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

    /// Precise variant of `function_body_live_out` for the slot allocator
    /// (no conservative dynamic-local-call fill, see
    /// `compute_live_before_block_precise`).
    fn function_body_live_out_precise(
        &self,
        body_expr: &Expr,
        capture_copies: &[(LocalSlot, LocalSlot)],
        persistent_slots: &[LocalSlot],
    ) -> LiveSet {
        let mut live_out = self.uses_expr_precise(body_expr);
        for (_, captured_slot) in capture_copies {
            self.mark_live(&mut live_out, *captured_slot);
        }
        for slot in persistent_slots {
            self.mark_live(&mut live_out, *slot);
        }
        live_out
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
    full_footprint: LiveSet,
    /// True while collecting a closure body's constraints. Closure bodies run
    /// in their own callee frame, so the conservative dynamic-local-call
    /// cross-live (which exists to keep unknown callable targets separated at
    /// the call site) must not spread into closure collection: there it would
    /// turn the closure body's slots into a program-wide clique and destroy
    /// compaction (and can push frames past the 256-slot limit spuriously).
    in_closure_body: bool,
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
            full_footprint: vec![true; local_count],
            in_closure_body: false,
        }
    }

    pub(super) fn allocate(mut self, mut ir: FrontendIr) -> Result<FrontendIr, ParseError> {
        let persistent_slots = persistent_capture_slots(&ir.stmts, &ir.function_impls);
        let mut live_out = self.liveness.empty_set();
        for slot in &persistent_slots {
            self.liveness.mark_live(&mut live_out, *slot);
        }
        let _ = self.collect_block(&ir.stmts, &live_out, &[])?;
        for function_impl in ir.function_impls.values() {
            let mut live_after = self.liveness.function_body_live_out_precise(
                &function_impl.body_expr,
                &function_impl.capture_copies,
                &persistent_slots,
            );
            // Parameters are written by the caller at frame entry and may be
            // read at any point in the body, so every parameter must interfere
            // with every other slot in the function for the WHOLE body, not
            // only with the slots live at body entry. A local that is defined
            // after entry (and is therefore absent from the entry live set)
            // must still never be colored onto a parameter slot: when it is,
            // the callee frame reads the wrong slot while evaluating call
            // arguments and the VM callable-schema check fails
            // (`type mismatch: expected string`) even though every value is
            // correctly typed.
            //
            // This invariant is deliberately conservative. Body statements
            // *can* define parameter slots: an `Assign` may target a
            // parameter, and the liveness rewriter may emit `Drop`
            // statements for parameter slots after their last use. The
            // rule is therefore not "the body never defines a parameter
            // slot"; it is a safety rule: parameter slots are
            // caller-written frame-entry state that the callee frame may
            // read at any point (directly, through captures, or through
            // nested closures), so the allocator treats them as live for
            // the entire body no matter what the body does to them.
            // `collect_block` re-marks the current function's parameter
            // slots after every statement, so the backward sweep can never
            // let a body-local share a parameter's physical slot, while
            // non-parameter locals keep sharing physical slots exactly as
            // before.
            //
            // Closures execute in their own callee frame whose slot layout
            // is drawn from the same flat slot space, so the same full-body
            // rule applies to every closure: each closure's own parameter
            // slots are seeded into its own body live-out
            // (`collect_closure_body_constraints`) and kept live for the
            // whole closure body regardless of body Assign/Drop statements.
            // Nested closures are traversed recursively, and each closure's
            // protection is scoped to its own body: an inner closure's
            // parameters never leak into the outer closure's or the
            // enclosing function's interference sets, and vice versa,
            // because each closure body is collected against its own fresh
            // live-out.
            for slot in &function_impl.param_slots {
                self.liveness.mark_live(&mut live_after, *slot);
            }
            self.add_live_clique(&live_after);
            self.collect_expr_constraints(
                &function_impl.body_expr,
                &live_after,
                &function_impl.param_slots,
            )?;
            let body_live_in = self.collect_block(
                &function_impl.body_stmts,
                &live_after,
                &function_impl.param_slots,
            )?;
            // Parameters stay live from body entry to the end, so the entry
            // clique must keep every parameter mutually interfering as well
            // (a parameter the body never uses has no other liveness edges
            // and the colorer would otherwise alias distinct parameters onto
            // one physical slot, corrupting operand placement at every call
            // site that targets the function).
            let mut entry_live = body_live_in;
            for slot in &function_impl.param_slots {
                self.liveness.mark_live(&mut entry_live, *slot);
            }
            self.add_live_clique(&entry_live);
        }

        let (mapping, compacted_local_count) = self.color_slots()?;
        remap_frontend_ir(&mut ir, &mapping, compacted_local_count)?;
        Ok(ir)
    }

    fn collect_block(
        &mut self,
        stmts: &[Stmt],
        live_out: &LiveSet,
        protected_slots: &[LocalSlot],
    ) -> Result<LiveSet, ParseError> {
        let mut live_after = live_out.clone();
        self.add_live_clique(&live_after);
        for stmt in stmts.iter().rev() {
            let mut live_before = self
                .liveness
                .compute_live_before_stmt_precise(stmt, &live_after);
            // Parameter slots stay live for the whole body no matter what the
            // statement does to them (see `allocate`); re-mark them so the
            // interference invariants never depend on def-use precision for
            // caller-written frame-entry state.
            for slot in protected_slots {
                self.liveness.mark_live(&mut live_before, *slot);
            }
            self.add_live_clique(&live_before);
            self.add_stmt_def_edges(stmt, &live_after);
            self.collect_stmt_constraints(stmt, &live_before, &live_after, protected_slots)?;
            live_after = live_before;
        }
        Ok(live_after)
    }

    fn collect_stmt_constraints(
        &mut self,
        stmt: &Stmt,
        live_before: &LiveSet,
        live_after: &LiveSet,
        protected_slots: &[LocalSlot],
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
                self.collect_expr_constraints(expr, live_before, protected_slots)?;
            }
            Stmt::ClosureLet { closure, .. } => {
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.add_slot_live_edges(*source_slot, live_before);
                    self.add_slot_live_edges(*captured_slot, live_before);
                }
                self.collect_closure_body_constraints(closure)?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.collect_expr_constraints(condition, live_before, protected_slots)?;
                let _ = self.collect_block(then_branch, live_after, protected_slots)?;
                let _ = self.collect_block(else_branch, live_after, protected_slots)?;
            }
            Stmt::While {
                condition, body, ..
            } => {
                let cond_uses = self.liveness.uses_expr_precise(condition);
                let mut live_cond = live_after.clone();
                self.liveness.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let body_live = self
                        .liveness
                        .compute_live_before_block_precise(body, &live_cond);
                    let mut next = live_after.clone();
                    self.liveness.union_inplace(&mut next, &cond_uses);
                    self.liveness.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                self.collect_expr_constraints(condition, &live_cond, protected_slots)?;
                let _ = self.collect_block(body, &live_cond, protected_slots)?;
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                let cond_uses = self.liveness.uses_expr_precise(condition);
                let mut live_cond = live_after.clone();
                self.liveness.union_inplace(&mut live_cond, &cond_uses);
                loop {
                    let post_live = self
                        .liveness
                        .compute_live_before_stmt_precise(post, &live_cond);
                    let body_live = self
                        .liveness
                        .compute_live_before_block_precise(body, &post_live);
                    let mut next = live_after.clone();
                    self.liveness.union_inplace(&mut next, &cond_uses);
                    self.liveness.union_inplace(&mut next, &body_live);
                    if next == live_cond {
                        break;
                    }
                    live_cond = next;
                }
                let post_live_before = self
                    .liveness
                    .compute_live_before_stmt_precise(post, &live_cond);
                self.collect_expr_constraints(condition, &live_cond, protected_slots)?;
                self.collect_stmt_constraints(
                    post,
                    &post_live_before,
                    &live_cond,
                    protected_slots,
                )?;
                let _ = self.collect_block(body, &post_live_before, protected_slots)?;
                self.collect_stmt_constraints(init, live_before, &live_cond, protected_slots)?;
            }
        }
        Ok(())
    }

    fn collect_expr_constraints(
        &mut self,
        expr: &Expr,
        live: &LiveSet,
        protected_slots: &[LocalSlot],
    ) -> Result<(), ParseError> {
        let mut live_during = live.clone();
        self.liveness
            .union_inplace(&mut live_during, &self.liveness.uses_expr_precise(expr));
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. } => {}
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
                self.collect_expr_constraints(container, &live_during, protected_slots)?;
                self.collect_expr_constraints(key, &live_during, protected_slots)?;
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.add_slot_live_edges(*value_slot, &live_during);
                self.collect_expr_constraints(value, &live_during, protected_slots)?;
                self.collect_expr_constraints(fallback, &live_during, protected_slots)?;
            }
            Expr::Call(_, _, args, _) => {
                // Arguments are evaluated in the caller frame, so their
                // constraints belong here. The callee body runs in a separate
                // runtime frame with its own local_base, so caller/callee
                // cross-live edges would only needlessly separate slots that
                // frame bases already isolate.
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during, protected_slots)?;
                }
            }
            // Resolved module calls (pre-merge only) constrain their
            // arguments; the callee's footprint is folded in post-merge.
            Expr::ModuleCall(_, _, args) => {
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during, protected_slots)?;
                }
            }
            Expr::LocalCall(index, _, args) => {
                self.add_slot_live_edges(*index, &live_during);
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during, protected_slots)?;
                }
                if !self.in_closure_body {
                    // Dynamic local-call targets may be closures whose
                    // capture state is not visible from the call expression;
                    // keep the caller-side interference conservative outside
                    // closure bodies. Inside a closure body the target still
                    // runs in its own callee frame (same flat slot space,
                    // separate frame base), so this program-wide cross-live
                    // would only turn the closure body's slots into a
                    // program-wide clique, destroying compaction and
                    // spuriously failing frames near the 256-slot limit.
                    let full_footprint = self.full_footprint.clone();
                    self.add_cross_live_with_set(&live_during, &full_footprint);
                }
            }
            Expr::Closure(closure) => {
                // The closure runs in its own callee frame drawn from the
                // same flat slot space; collect its body against a fresh
                // live-out seeded with its own parameter slots so the
                // full-body parameter rule holds for closures too.
                self.collect_closure_body_constraints(closure)?;
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.collect_expr_constraints(arg, &live_during, protected_slots)?;
                }
                self.collect_closure_body_constraints(closure)?;
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
                self.collect_expr_constraints(lhs, &live_during, protected_slots)?;
                self.collect_expr_constraints(rhs, &live_during, protected_slots)?;
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => {
                self.collect_expr_constraints(inner, &live_during, protected_slots)?;
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.collect_expr_constraints(condition, &live_during, protected_slots)?;
                self.collect_expr_constraints(then_expr, &live_during, protected_slots)?;
                self.collect_expr_constraints(else_expr, &live_during, protected_slots)?;
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
                self.collect_expr_constraints(value, &live_during, protected_slots)?;
                for (pattern, arm_expr) in arms {
                    if let Some(binding_slot) = pattern.binding_slot() {
                        self.add_slot_live_edges(binding_slot, &live_during);
                    }
                    self.collect_expr_constraints(arm_expr, &live_during, protected_slots)?;
                }
                self.collect_expr_constraints(default, &live_during, protected_slots)?;
            }
            Expr::Block { stmts, expr } => {
                self.collect_expr_constraints(expr, &live_during, protected_slots)?;
                let mut block_live_out = live_during.clone();
                self.liveness
                    .union_inplace(&mut block_live_out, &self.liveness.uses_expr_precise(expr));
                let _ = self.collect_block(stmts, &block_live_out, protected_slots)?;
            }
        }
        Ok(())
    }

    /// Collect the interference constraints of a closure body the way a named
    /// function body is collected: a fresh live-out seeded ONLY with the
    /// closure's own parameter slots and its capture targets. The real tail
    /// and body uses are computed by the backward collector itself
    /// (`collect_expr_constraints` / `collect_block`); seeding the live-out
    /// with `uses_expr(closure.body)` instead would put every slot the body
    /// ever touches into the live-out, turning the whole body (and, through
    /// a dynamic `LocalCall`'s conservative fill, the whole program) into
    /// one interference clique. Nested closures recurse through
    /// `collect_expr_constraints`, and each closure's protection is scoped
    /// to its own body: an inner closure's parameters never mix with the
    /// outer closure's or the enclosing function's interference sets.
    fn collect_closure_body_constraints(
        &mut self,
        closure: &ClosureExpr,
    ) -> Result<(), ParseError> {
        let mut live_out = self.liveness.empty_set();
        // Capture targets are caller-side state the closure body may read at
        // any point (through its capture cells), so they stay live for the
        // whole closure body just like the parameters.
        for (_, captured_slot) in &closure.capture_copies {
            self.liveness.mark_live(&mut live_out, *captured_slot);
        }
        for slot in &closure.param_slots {
            self.liveness.mark_live(&mut live_out, *slot);
        }
        self.add_live_clique(&live_out);
        let saved_closure_scope = self.in_closure_body;
        self.in_closure_body = true;
        let result = match &*closure.body {
            // Mirror the named-function collection for the common block body:
            // the tail expression is collected against the seeded live-out,
            // then the statements are swept backward with the tail live-out.
            Expr::Block { stmts, expr } => {
                self.collect_expr_constraints(expr, &live_out, &closure.param_slots)?;
                let mut block_live_out = live_out.clone();
                self.liveness
                    .union_inplace(&mut block_live_out, &self.liveness.uses_expr_precise(expr));
                let _ = self.collect_block(stmts, &block_live_out, &closure.param_slots)?;
                Ok(())
            }
            other => self.collect_expr_constraints(other, &live_out, &closure.param_slots),
        };
        self.in_closure_body = saved_closure_scope;
        result
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
            | Expr::FunctionRef(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. } => {}
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
            Expr::Call(_, _, args, _) => {
                // The callee runs in its own frame even when called from a
                // closure body, so only argument slots join the caller-side
                // footprint.
                for arg in args {
                    self.collect_expr_footprint(arg, set, stack);
                }
            }
            Expr::ModuleCall(_, _, args) => {
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
        | Expr::ModuleFunctionRef(..)
        | Expr::UnresolvedFunctionRef { .. }
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
        Expr::Call(_, _, args, _) | Expr::LocalCall(_, _, args) | Expr::ModuleCall(_, _, args) => {
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
        Expr::FunctionRef(..)
        | Expr::ModuleFunctionRef(..)
        | Expr::UnresolvedFunctionRef { .. } => {}
        Expr::Call(_, _, args, _) | Expr::ModuleCall(_, _, args) => {
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

#[cfg(test)]
mod call_resolution_carrier_tests {
    use super::remap_expr_slots;
    use crate::compiler::ir::{Expr, TypeSchema};
    use crate::compiler::{ResolvedHostCall, ResolvedHostParam};
    use crate::host_api::{HostApiFingerprint, HostParamPassing};

    fn fingerprint(n: u64) -> HostApiFingerprint {
        serde_json::from_value(serde_json::Value::Number(n.into())).unwrap()
    }

    fn resolution(name: &str) -> ResolvedHostCall {
        ResolvedHostCall {
            name: name.to_string(),
            params: vec![ResolvedHostParam {
                name: "x".to_string(),
                schema: TypeSchema::Int,
            }],
            return_type: TypeSchema::Int,
            passing: vec![HostParamPassing::Borrow],
            fingerprint: fingerprint(3),
        }
    }

    #[test]
    fn slot_remap_preserves_call_resolution() {
        let mut call = Expr::Call(
            4,
            Vec::new(),
            vec![Expr::Var(7)],
            Some(Box::new(resolution("read"))),
        );
        let identity: Vec<u16> = (0..12).collect();
        remap_expr_slots(&mut call, &identity).unwrap();
        assert_eq!(call.host_call_resolution().unwrap().name, "read");
    }
}
