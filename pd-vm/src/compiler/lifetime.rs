use std::collections::HashMap;

use super::ParseError;
use super::ir::{ClosureExpr, Expr, FrontendIr, FunctionImpl, Stmt};

#[derive(Clone)]
struct FlowState {
    reachable: bool,
    definite: Vec<bool>,
    possible: Vec<bool>,
}

impl FlowState {
    fn reachable(local_count: usize) -> Self {
        Self {
            reachable: true,
            definite: vec![false; local_count],
            possible: vec![false; local_count],
        }
    }
}

pub(super) fn enforce_local_availability(mut ir: FrontendIr) -> Result<FrontendIr, ParseError> {
    let analyzer = AvailabilityAnalyzer::new(ir.locals, &ir.local_bindings);
    let (rewritten_stmts, _) =
        analyzer.analyze_block(&ir.stmts, FlowState::reachable(ir.locals), true)?;
    ir.stmts = rewritten_stmts;

    let function_impls = std::mem::take(&mut ir.function_impls);
    let mut rewritten_impls = HashMap::with_capacity(function_impls.len());
    for (index, function_impl) in function_impls {
        let rewritten = analyzer.analyze_function_impl(function_impl)?;
        rewritten_impls.insert(index, rewritten);
    }
    ir.function_impls = rewritten_impls;

    let liveness = LivenessRewriter::new(ir.locals, &ir.local_bindings);
    ir.stmts = liveness.rewrite_program_block(&ir.stmts);
    for function_impl in ir.function_impls.values_mut() {
        *function_impl = liveness.rewrite_function_impl(function_impl.clone());
    }
    Ok(ir)
}

struct AvailabilityAnalyzer {
    local_count: usize,
    local_names: HashMap<u8, String>,
}

impl AvailabilityAnalyzer {
    fn new(local_count: usize, local_bindings: &[(String, u8)]) -> Self {
        let mut local_names = HashMap::with_capacity(local_bindings.len());
        for (name, index) in local_bindings {
            local_names.insert(*index, name.clone());
        }
        Self {
            local_count,
            local_names,
        }
    }

    fn analyze_function_impl(
        &self,
        function_impl: FunctionImpl,
    ) -> Result<FunctionImpl, ParseError> {
        let FunctionImpl {
            param_slots,
            body_stmts,
            body_expr,
        } = function_impl;
        let mut state = FlowState::reachable(self.local_count);
        for slot in &param_slots {
            self.mark_available(&mut state, *slot, 1)?;
        }
        let (rewritten_body, body_state) = self.analyze_block(&body_stmts, state, true)?;
        self.analyze_expr(&body_expr, &body_state, 1)?;
        Ok(FunctionImpl {
            param_slots,
            body_stmts: rewritten_body,
            body_expr,
        })
    }

    fn analyze_block(
        &self,
        stmts: &[Stmt],
        mut state: FlowState,
        rewrite_clears: bool,
    ) -> Result<(Vec<Stmt>, FlowState), ParseError> {
        let mut rewritten = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            if !state.reachable {
                rewritten.push(stmt.clone());
                continue;
            }

            let before = state.clone();
            let (rewritten_stmt, next_state) = self.analyze_stmt(stmt, state, rewrite_clears)?;
            state = next_state;
            rewritten.push(rewritten_stmt);

            if !rewrite_clears || !before.reachable || !state.reachable {
                continue;
            }
            let clear_line = stmt_line(stmt);
            for slot in 0..self.local_count {
                let before_possible = before.possible[slot];
                let before_definite = before.definite[slot];
                let after_possible = state.possible[slot];
                let after_definite = state.definite[slot];
                let entered_uncertain =
                    after_possible && !after_definite && (!before_possible || before_definite);
                if entered_uncertain {
                    rewritten.push(Stmt::Assign {
                        index: slot as u8,
                        expr: Expr::Null,
                        line: clear_line,
                    });
                }
            }
        }
        Ok((rewritten, state))
    }

    fn analyze_stmt(
        &self,
        stmt: &Stmt,
        state: FlowState,
        rewrite_clears: bool,
    ) -> Result<(Stmt, FlowState), ParseError> {
        match stmt {
            Stmt::Noop { .. } | Stmt::FuncDecl { .. } => Ok((stmt.clone(), state)),
            Stmt::Let { index, expr, line } => {
                let mut out = self.analyze_expr(expr, &state, *line)?;
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Assign { index, expr, line } => {
                self.require_assignable(*index, &state, *line)?;
                let mut out = self.analyze_expr(expr, &state, *line)?;
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                }
                Ok((stmt.clone(), out))
            }
            Stmt::ClosureLet { line, closure } => {
                let mut out = state.clone();
                if out.reachable {
                    self.analyze_closure(closure, &out, *line)?;
                    for (_, captured_slot) in &closure.capture_copies {
                        self.mark_available(&mut out, *captured_slot, *line)?;
                    }
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Expr { expr, line } => {
                let out = self.analyze_expr(expr, &state, *line)?;
                Ok((stmt.clone(), out))
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let cond_state = self.analyze_expr(condition, &state, *line)?;
                let (rewritten_then, then_state) =
                    self.analyze_block(then_branch, cond_state.clone(), rewrite_clears)?;
                let (rewritten_else, else_state) =
                    self.analyze_block(else_branch, cond_state, rewrite_clears)?;
                let merged = self.merge_states(then_state, else_state);

                let rewritten = Stmt::IfElse {
                    condition: condition.clone(),
                    then_branch: rewritten_then,
                    else_branch: rewritten_else,
                    line: *line,
                };
                Ok((rewritten, merged))
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                let (rewritten_init, init_state) =
                    self.analyze_stmt(init.as_ref(), state.clone(), rewrite_clears)?;
                let cond_state = self.analyze_expr(condition, &init_state, *line)?;
                let (rewritten_body, body_state) =
                    self.analyze_block(body, cond_state.clone(), rewrite_clears)?;
                let (rewritten_post, post_state) =
                    self.analyze_stmt(post.as_ref(), body_state, rewrite_clears)?;

                // `for` loop condition executes before each iteration and at least once.
                // Body/post execution is optional, so only condition-side availability is guaranteed after loop.
                let mut possible = cond_state.possible.clone();
                for (possible_slot, post_possible) in
                    possible.iter_mut().zip(post_state.possible.iter())
                {
                    *possible_slot = *possible_slot || *post_possible;
                }
                let out = FlowState {
                    reachable: state.reachable && cond_state.reachable,
                    definite: cond_state.definite.clone(),
                    possible,
                };

                let rewritten = Stmt::For {
                    init: Box::new(rewritten_init),
                    condition: condition.clone(),
                    post: Box::new(rewritten_post),
                    body: rewritten_body,
                    line: *line,
                };
                Ok((rewritten, out))
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                let cond_state = self.analyze_expr(condition, &state, *line)?;
                let (rewritten_body, body_state) =
                    self.analyze_block(body, cond_state.clone(), rewrite_clears)?;

                // `while` condition executes at least once; body execution is optional.
                let mut possible = cond_state.possible.clone();
                for (possible_slot, body_possible) in
                    possible.iter_mut().zip(body_state.possible.iter())
                {
                    *possible_slot = *possible_slot || *body_possible;
                }
                let out = FlowState {
                    reachable: state.reachable && cond_state.reachable,
                    definite: cond_state.definite.clone(),
                    possible,
                };

                let rewritten = Stmt::While {
                    condition: condition.clone(),
                    body: rewritten_body,
                    line: *line,
                };
                Ok((rewritten, out))
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {
                let mut out = state;
                out.reachable = false;
                Ok((stmt.clone(), out))
            }
        }
    }

    fn analyze_expr(
        &self,
        expr: &Expr,
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        if !state.reachable {
            return Ok(state.clone());
        }
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::FunctionRef(_) => Ok(state.clone()),
            Expr::Var(index) => {
                self.require_available(*index, state, line)?;
                Ok(state.clone())
            }
            Expr::Call(_, args) => self.analyze_args(args, state, line),
            Expr::LocalCall(index, args) => {
                self.require_available(*index, state, line)?;
                self.analyze_args(args, state, line)
            }
            Expr::Closure(closure) => {
                self.analyze_closure(closure, state, line)?;
                let mut out = state.clone();
                for (_, captured_slot) in &closure.capture_copies {
                    self.mark_available(&mut out, *captured_slot, line)?;
                }
                Ok(out)
            }
            Expr::ClosureCall(closure, args) => {
                let mut out = self.analyze_args(args, state, line)?;
                self.analyze_closure(closure, &out, line)?;
                for (_, captured_slot) in &closure.capture_copies {
                    self.mark_available(&mut out, *captured_slot, line)?;
                }
                Ok(out)
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
                let lhs_state = self.analyze_expr(lhs, state, line)?;
                self.analyze_expr(rhs, &lhs_state, line)
            }
            Expr::Neg(inner) | Expr::Not(inner) => self.analyze_expr(inner, state, line),
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                let cond_state = self.analyze_expr(condition, state, line)?;
                let then_state = self.analyze_expr(then_expr, &cond_state, line)?;
                let else_state = self.analyze_expr(else_expr, &cond_state, line)?;
                Ok(self.merge_states(then_state, else_state))
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                let mut value_state = self.analyze_expr(value, state, line)?;
                self.mark_available(&mut value_state, *value_slot, line)?;

                let mut merged_state: Option<FlowState> = None;
                for (_, arm_expr) in arms {
                    let arm_state = self.analyze_expr(arm_expr, &value_state, line)?;
                    merged_state = Some(match merged_state {
                        Some(existing) => self.merge_states(existing, arm_state),
                        None => arm_state,
                    });
                }
                let default_state = self.analyze_expr(default, &value_state, line)?;
                let mut out = if let Some(existing) = merged_state {
                    self.merge_states(existing, default_state)
                } else {
                    default_state
                };
                if out.reachable {
                    self.mark_available(&mut out, *result_slot, line)?;
                }
                Ok(out)
            }
            Expr::Block { stmts, expr } => {
                let (_, block_state) = self.analyze_block(stmts, state.clone(), false)?;
                self.analyze_expr(expr, &block_state, line)
            }
        }
    }

    fn analyze_args(
        &self,
        args: &[Expr],
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        let mut out = state.clone();
        for arg in args {
            out = self.analyze_expr(arg, &out, line)?;
        }
        Ok(out)
    }

    fn analyze_closure(
        &self,
        closure: &ClosureExpr,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        if !state.reachable {
            return Ok(());
        }
        for (source_slot, _) in &closure.capture_copies {
            self.require_available(*source_slot, state, line)?;
        }

        let mut closure_state = FlowState::reachable(self.local_count);
        for slot in &closure.param_slots {
            self.mark_available(&mut closure_state, *slot, line)?;
        }
        for (_, captured_slot) in &closure.capture_copies {
            self.mark_available(&mut closure_state, *captured_slot, line)?;
        }
        self.analyze_expr(&closure.body, &closure_state, line)?;
        Ok(())
    }

    fn require_available(&self, index: u8, state: &FlowState, line: u32) -> Result<(), ParseError> {
        let slot = index as usize;
        if slot >= self.local_count {
            return Err(ParseError {
                span: None,
                code: Some("E_LOCAL_BOUNDS".to_string()),
                line: line as usize,
                message: format!("internal local slot {index} out of range"),
            });
        }
        if state.definite[slot] {
            return Ok(());
        }
        let display = self
            .local_names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("#{index}"));
        Err(ParseError {
            span: None,
            code: Some("E_LOCAL_UNAVAILABLE".to_string()),
            line: line as usize,
            message: format!(
                "local '{display}' may be unavailable on this control-flow path; initialize it before use"
            ),
        })
    }

    fn require_assignable(
        &self,
        index: u8,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        let slot = index as usize;
        if slot >= self.local_count {
            return Err(ParseError {
                span: None,
                code: Some("E_LOCAL_BOUNDS".to_string()),
                line: line as usize,
                message: format!("internal local slot {index} out of range"),
            });
        }
        if state.definite[slot] {
            return Ok(());
        }
        let display = self
            .local_names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("#{index}"));
        Err(ParseError {
            span: None,
            code: Some("E_LOCAL_UNAVAILABLE_ASSIGN".to_string()),
            line: line as usize,
            message: format!(
                "local '{display}' is not definitely declared on this control-flow path; use 'let {display} = ...' before assignment"
            ),
        })
    }

    fn mark_available(
        &self,
        state: &mut FlowState,
        index: u8,
        line: u32,
    ) -> Result<(), ParseError> {
        let slot = index as usize;
        if slot >= self.local_count {
            return Err(ParseError {
                span: None,
                code: Some("E_LOCAL_BOUNDS".to_string()),
                line: line as usize,
                message: format!("internal local slot {index} out of range"),
            });
        }
        state.definite[slot] = true;
        state.possible[slot] = true;
        Ok(())
    }

    fn merge_states(&self, lhs: FlowState, rhs: FlowState) -> FlowState {
        match (lhs.reachable, rhs.reachable) {
            (false, false) => FlowState {
                reachable: false,
                definite: vec![false; self.local_count],
                possible: vec![false; self.local_count],
            },
            (true, false) => lhs,
            (false, true) => rhs,
            (true, true) => {
                let mut definite = vec![false; self.local_count];
                let mut possible = vec![false; self.local_count];
                for idx in 0..self.local_count {
                    definite[idx] = lhs.definite[idx] && rhs.definite[idx];
                    possible[idx] = lhs.possible[idx] || rhs.possible[idx];
                }
                FlowState {
                    reachable: true,
                    definite,
                    possible,
                }
            }
        }
    }
}

type LiveSet = Vec<bool>;

#[derive(Clone, Copy)]
struct DefInfo {
    slot: u8,
    explicit_null: bool,
}

struct LivenessRewriter {
    local_count: usize,
    clearable_slots: Vec<bool>,
}

impl LivenessRewriter {
    fn new(local_count: usize, local_bindings: &[(String, u8)]) -> Self {
        let mut clearable_slots = vec![false; local_count];
        for (_, index) in local_bindings {
            let slot = *index as usize;
            if slot < local_count {
                clearable_slots[slot] = true;
            }
        }
        Self {
            local_count,
            clearable_slots,
        }
    }

    fn rewrite_program_block(&self, stmts: &[Stmt]) -> Vec<Stmt> {
        let live_out = self.empty_set();
        self.rewrite_block(stmts, &live_out, false).0
    }

    fn rewrite_function_impl(&self, function_impl: FunctionImpl) -> FunctionImpl {
        let FunctionImpl {
            param_slots,
            body_stmts,
            body_expr,
        } = function_impl;
        let live_out = self.uses_expr(&body_expr);
        let (rewritten_body, _) = self.rewrite_block(&body_stmts, &live_out, false);
        FunctionImpl {
            param_slots,
            body_stmts: rewritten_body,
            body_expr,
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
                rewritten_rev.push(Stmt::Assign {
                    index: *slot,
                    expr: Expr::Null,
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
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => (stmt.clone(), live_after.clone(), Vec::new()),
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
            Stmt::Let { index, expr, line } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                (
                    Stmt::Let {
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
            Stmt::Assign { index, expr, line } => {
                let mut live_before = live_after.clone();
                self.kill_slot(&mut live_before, *index);
                self.union_inplace(&mut live_before, &self.uses_expr(expr));
                (
                    Stmt::Assign {
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
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => live_after.clone(),
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
            | Expr::String(_)
            | Expr::FunctionRef(_) => {}
            Expr::Var(index) => self.mark_live(live, *index),
            Expr::Call(_, args) => {
                for arg in args {
                    self.add_expr_uses(arg, live);
                }
            }
            Expr::LocalCall(index, args) => {
                self.mark_live(live, *index);
                for arg in args {
                    self.add_expr_uses(arg, live);
                }
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
            Expr::Neg(inner) | Expr::Not(inner) => self.add_expr_uses(inner, live),
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
                for stmt in stmts {
                    self.add_stmt_uses(stmt, live);
                }
                self.add_expr_uses(expr, live);
            }
        }
    }

    fn add_stmt_uses(&self, stmt: &Stmt, live: &mut LiveSet) {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                self.add_expr_uses(expr, live);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (source_slot, _) in &closure.capture_copies {
                    self.mark_live(live, *source_slot);
                }
                self.add_expr_uses(&closure.body, live);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.add_expr_uses(condition, live);
                for stmt in then_branch {
                    self.add_stmt_uses(stmt, live);
                }
                for stmt in else_branch {
                    self.add_stmt_uses(stmt, live);
                }
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.add_stmt_uses(init, live);
                self.add_expr_uses(condition, live);
                self.add_stmt_uses(post, live);
                for stmt in body {
                    self.add_stmt_uses(stmt, live);
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.add_expr_uses(condition, live);
                for stmt in body {
                    self.add_stmt_uses(stmt, live);
                }
            }
        }
    }

    fn compute_clear_slots(
        &self,
        live_before: &LiveSet,
        live_after: &LiveSet,
        defs: &[DefInfo],
    ) -> Vec<u8> {
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
            .filter_map(|(slot, should_clear)| should_clear.then_some(slot as u8))
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

    fn kill_slot(&self, live: &mut LiveSet, slot: u8) {
        let slot = slot as usize;
        if slot < self.local_count {
            live[slot] = false;
        }
    }

    fn mark_live(&self, live: &mut LiveSet, slot: u8) {
        let slot = slot as usize;
        if slot < self.local_count {
            live[slot] = true;
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
        | Stmt::Continue { line } => *line,
    }
}
