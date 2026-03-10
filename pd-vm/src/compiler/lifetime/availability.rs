use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;

use super::super::ParseError;
use super::super::ir::{ClosureExpr, Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt};
use super::liveness::{LivenessRewriter, LocalSlotAllocator};
mod captures;
mod consumption;
mod field_moves;

use self::consumption::{
    compute_function_consumed_param_positions, extract_passthrough_return_slot,
};

#[derive(Clone, PartialEq, Eq)]
struct FlowState {
    reachable: bool,
    definite: Vec<bool>,
    possible: Vec<bool>,
    copyable_locals: Vec<bool>,
    movable_locals: Vec<bool>,
    collection_aliases: Vec<HashSet<u32>>,
    moved_local_definite: Vec<bool>,
    moved_local_possible: Vec<bool>,
    moved_definite: HashSet<MovedFieldPath>,
    moved_possible: HashSet<MovedFieldPath>,
    copyable_fields: HashSet<MovedFieldPath>,
}

#[derive(Default)]
struct LoopControlFlow {
    break_state: Option<FlowState>,
    continue_state: Option<FlowState>,
}

struct ForLoopParts<'a> {
    init: &'a Stmt,
    condition: &'a Expr,
    post: &'a Stmt,
    body: &'a [Stmt],
    line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MovedFieldPath {
    root: LocalSlot,
    key: MovedFieldKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MovedFieldKey {
    String(String),
    Index(i64),
    Dynamic,
    Slice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CaptureBindingMode {
    Copy,
    Borrow,
    BorrowMut,
    Move,
}

impl FlowState {
    fn reachable(local_count: usize) -> Self {
        Self {
            reachable: true,
            definite: vec![false; local_count],
            possible: vec![false; local_count],
            copyable_locals: vec![false; local_count],
            movable_locals: vec![false; local_count],
            collection_aliases: vec![HashSet::new(); local_count],
            moved_local_definite: vec![false; local_count],
            moved_local_possible: vec![false; local_count],
            moved_definite: HashSet::new(),
            moved_possible: HashSet::new(),
            copyable_fields: HashSet::new(),
        }
    }

    fn reachable_with_definite(local_count: usize, definite_locals: &[LocalSlot]) -> Self {
        let mut state = Self::reachable(local_count);
        for slot in definite_locals {
            let slot = *slot as usize;
            if slot >= local_count {
                continue;
            }
            state.definite[slot] = true;
            state.possible[slot] = true;
        }
        state
    }
}

pub(super) fn enforce_local_availability(
    mut ir: FrontendIr,
    entry_definite_locals: &[LocalSlot],
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
) -> Result<FrontendIr, ParseError> {
    let initial_impls = std::mem::take(&mut ir.function_impls);

    let bootstrap_analyzer = AvailabilityAnalyzer::new(
        ir.locals,
        &ir.local_bindings,
        &initial_impls,
        enable_local_move_semantics,
    );
    let mut rewritten_impls = HashMap::with_capacity(initial_impls.len());
    for (index, function_impl) in initial_impls {
        let rewritten = bootstrap_analyzer.analyze_function_impl(function_impl)?;
        rewritten_impls.insert(index, rewritten);
    }

    let analyzer = AvailabilityAnalyzer::new(
        ir.locals,
        &ir.local_bindings,
        &rewritten_impls,
        enable_local_move_semantics,
    );
    let entry_state = FlowState::reachable_with_definite(ir.locals, entry_definite_locals);
    let (rewritten_stmts, _) = analyzer.analyze_block(&ir.stmts, entry_state, true)?;
    ir.stmts = rewritten_stmts;
    ir.function_impls = rewritten_impls;

    if clear_dead_locals {
        let liveness = LivenessRewriter::new(ir.locals, &ir.local_bindings, &ir.function_impls);
        ir.stmts = liveness.rewrite_program_block(&ir.stmts);
        for function_impl in ir.function_impls.values_mut() {
            *function_impl = liveness.rewrite_function_impl(function_impl.clone());
        }
    }

    if ir.locals > (u8::MAX as usize + 1) {
        let allocator = LocalSlotAllocator::new(ir.locals, &ir.local_bindings, &ir.function_impls);
        ir = allocator.allocate(ir)?;
    }
    Ok(ir)
}

struct AvailabilityAnalyzer {
    local_count: usize,
    local_names: HashMap<LocalSlot, String>,
    function_impls: HashMap<u16, FunctionImpl>,
    collection_passthrough_params: HashMap<u16, usize>,
    function_consumed_params: HashMap<u16, HashSet<usize>>,
    next_collection_alias_id: Cell<u32>,
    enable_local_move_semantics: bool,
}

impl AvailabilityAnalyzer {
    fn new(
        local_count: usize,
        local_bindings: &[(String, LocalSlot)],
        function_impls: &HashMap<u16, FunctionImpl>,
        enable_local_move_semantics: bool,
    ) -> Self {
        let mut local_names = HashMap::with_capacity(local_bindings.len());
        for (name, index) in local_bindings {
            local_names.insert(*index, name.clone());
        }
        let mut collection_passthrough_params = HashMap::new();
        for (index, function_impl) in function_impls {
            let Some(return_slot) = self::extract_passthrough_return_slot(function_impl) else {
                continue;
            };
            let Some(param_index) = function_impl
                .param_slots
                .iter()
                .position(|slot| *slot == return_slot)
            else {
                continue;
            };
            collection_passthrough_params.insert(*index, param_index);
        }
        let function_consumed_params =
            compute_function_consumed_param_positions(function_impls, enable_local_move_semantics);
        Self {
            local_count,
            local_names,
            function_impls: function_impls.clone(),
            collection_passthrough_params,
            function_consumed_params,
            next_collection_alias_id: Cell::new(1),
            enable_local_move_semantics,
        }
    }

    fn analyze_function_impl(
        &self,
        function_impl: FunctionImpl,
    ) -> Result<FunctionImpl, ParseError> {
        let FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts,
            body_expr,
        } = function_impl;
        let mut state = FlowState::reachable(self.local_count);
        for slot in &param_slots {
            self.mark_available(&mut state, *slot, 1)?;
        }
        for (_, captured_slot) in &capture_copies {
            self.mark_available(&mut state, *captured_slot, 1)?;
        }
        let (rewritten_body, body_state) = self.analyze_block(&body_stmts, state, true)?;
        self.analyze_expr(&body_expr, &body_state, 1)?;
        Ok(FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts: rewritten_body,
            body_expr,
        })
    }

    fn analyze_block(
        &self,
        stmts: &[Stmt],
        state: FlowState,
        rewrite_clears: bool,
    ) -> Result<(Vec<Stmt>, FlowState), ParseError> {
        self.analyze_block_with_loop_control(stmts, state, rewrite_clears, None)
    }

    fn analyze_block_with_loop_control(
        &self,
        stmts: &[Stmt],
        mut state: FlowState,
        rewrite_clears: bool,
        mut loop_control: Option<&mut LoopControlFlow>,
    ) -> Result<(Vec<Stmt>, FlowState), ParseError> {
        let mut rewritten = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            if !state.reachable {
                rewritten.push(stmt.clone());
                continue;
            }

            let before = state.clone();
            let (rewritten_stmt, next_state) = if let Some(control) = loop_control.as_deref_mut() {
                self.analyze_stmt(stmt, state, rewrite_clears, Some(control))?
            } else {
                self.analyze_stmt(stmt, state, rewrite_clears, None)?
            };
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
                    rewritten.push(Stmt::Drop {
                        index: slot as LocalSlot,
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
        loop_control: Option<&mut LoopControlFlow>,
    ) -> Result<(Stmt, FlowState), ParseError> {
        match stmt {
            Stmt::Noop { .. } | Stmt::Drop { .. } => Ok((stmt.clone(), state)),
            Stmt::FuncDecl { index, line, .. } => {
                let mut out = state.clone();
                if out.reachable
                    && let Some(function_impl) = self.function_impls.get(index)
                {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        self.require_available(*source_slot, &out, *line)?;
                        self.require_local_not_moved(*source_slot, &out, *line)?;
                        self.require_local_not_partially_moved(*source_slot, &out, *line)?;
                        self.mark_available(&mut out, *captured_slot, *line)?;
                        let capture_mode =
                            self.function_capture_mode_for_slot(function_impl, *captured_slot);
                        self.apply_capture_binding_effect(
                            &mut out,
                            *source_slot,
                            *captured_slot,
                            capture_mode,
                        );
                    }
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Let { index, expr, line } => {
                let mut out = self.analyze_expr(expr, &state, *line)?;
                let mut rewritten_expr = expr.clone();
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                    self.clear_local_moved_state(&mut out, *index);
                    self.handle_local_rebind_field_moves(&mut out, *index, expr);
                    self.handle_local_rebind_collection_aliases(&mut out, *index, expr);
                    let is_copyable = self.is_definitely_copyable_expr(expr, &out);
                    self.set_local_copyable_state(&mut out, *index, is_copyable);
                    let is_movable = self.is_definitely_movable_local_expr(expr, &out);
                    self.set_local_movable_state(&mut out, *index, is_movable);
                    rewritten_expr =
                        self.rewrite_local_source_move_on_rebind(&mut out, *index, expr);
                    rewritten_expr = self.rewrite_runtime_field_move_expr(&rewritten_expr, &state);
                }
                Ok((
                    Stmt::Let {
                        index: *index,
                        expr: rewritten_expr,
                        line: *line,
                    },
                    out,
                ))
            }
            Stmt::Assign { index, expr, line } => {
                self.require_assignable(*index, &state, *line)?;
                let mut out = self.analyze_expr(expr, &state, *line)?;
                let mut rewritten_expr = expr.clone();
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                    self.clear_local_moved_state(&mut out, *index);
                    self.handle_local_rebind_field_moves(&mut out, *index, expr);
                    self.handle_local_rebind_collection_aliases(&mut out, *index, expr);
                    let is_copyable = self.is_definitely_copyable_expr(expr, &out);
                    self.set_local_copyable_state(&mut out, *index, is_copyable);
                    let is_movable = self.is_definitely_movable_local_expr(expr, &out);
                    self.set_local_movable_state(&mut out, *index, is_movable);
                    rewritten_expr =
                        self.rewrite_local_source_move_on_rebind(&mut out, *index, expr);
                    rewritten_expr = self.rewrite_runtime_field_move_expr(&rewritten_expr, &state);
                }
                Ok((
                    Stmt::Assign {
                        index: *index,
                        expr: rewritten_expr,
                        line: *line,
                    },
                    out,
                ))
            }
            Stmt::ClosureLet { line, closure } => {
                let mut out = state.clone();
                if out.reachable {
                    self.analyze_closure(closure, &out, *line)?;
                    for (source_slot, captured_slot) in &closure.capture_copies {
                        self.mark_available(&mut out, *captured_slot, *line)?;
                        let capture_mode =
                            self.closure_capture_mode_for_slot(closure, *captured_slot);
                        self.apply_capture_binding_effect(
                            &mut out,
                            *source_slot,
                            *captured_slot,
                            capture_mode,
                        );
                    }
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Expr { expr, line } => {
                let out = self.analyze_expr(expr, &state, *line)?;
                let rewritten_expr = self.rewrite_runtime_field_move_expr(expr, &state);
                Ok((
                    Stmt::Expr {
                        expr: rewritten_expr,
                        line: *line,
                    },
                    out,
                ))
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let cond_state = self.analyze_expr(condition, &state, *line)?;
                let (rewritten_then, then_state, rewritten_else, else_state) =
                    if let Some(control) = loop_control {
                        let (rewritten_then, then_state) = self.analyze_block_with_loop_control(
                            then_branch,
                            cond_state.clone(),
                            rewrite_clears,
                            Some(&mut *control),
                        )?;
                        let (rewritten_else, else_state) = self.analyze_block_with_loop_control(
                            else_branch,
                            cond_state,
                            rewrite_clears,
                            Some(&mut *control),
                        )?;
                        (rewritten_then, then_state, rewritten_else, else_state)
                    } else {
                        let (rewritten_then, then_state) =
                            self.analyze_block(then_branch, cond_state.clone(), rewrite_clears)?;
                        let (rewritten_else, else_state) =
                            self.analyze_block(else_branch, cond_state, rewrite_clears)?;
                        (rewritten_then, then_state, rewritten_else, else_state)
                    };
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
            } => self.analyze_for_loop(
                ForLoopParts {
                    init,
                    condition,
                    post,
                    body,
                    line: *line,
                },
                state,
                rewrite_clears,
            ),
            Stmt::While {
                condition,
                body,
                line,
            } => self.analyze_while_loop(condition, body, *line, state, rewrite_clears),
            Stmt::Break { .. } => {
                if let Some(control) = loop_control {
                    self.merge_optional_state(&mut control.break_state, &state);
                }
                let mut out = state;
                out.reachable = false;
                Ok((stmt.clone(), out))
            }
            Stmt::Continue { .. } => {
                if let Some(control) = loop_control {
                    self.merge_optional_state(&mut control.continue_state, &state);
                }
                let mut out = state;
                out.reachable = false;
                Ok((stmt.clone(), out))
            }
        }
    }

    fn merge_optional_state(&self, merged: &mut Option<FlowState>, next: &FlowState) {
        if !next.reachable {
            return;
        }
        *merged = Some(match merged.take() {
            Some(existing) => self.merge_states(existing, next.clone()),
            None => next.clone(),
        });
    }

    fn analyze_while_loop(
        &self,
        condition: &Expr,
        body: &[Stmt],
        line: u32,
        state: FlowState,
        rewrite_clears: bool,
    ) -> Result<(Stmt, FlowState), ParseError> {
        let alias_seed = self.next_collection_alias_id.get();
        let mut loop_entry = state.clone();
        loop {
            self.next_collection_alias_id.set(alias_seed);
            let cond_state = self.analyze_expr(condition, &loop_entry, line)?;
            let mut loop_control = LoopControlFlow::default();
            let (_, body_state) = self.analyze_block_with_loop_control(
                body,
                cond_state.clone(),
                false,
                Some(&mut loop_control),
            )?;
            let mut backedge_state = body_state;
            if let Some(continue_state) = loop_control.continue_state {
                backedge_state = self.merge_states(backedge_state, continue_state);
            }
            let next_loop_entry = self.merge_states(state.clone(), backedge_state);
            if next_loop_entry == loop_entry {
                break;
            }
            loop_entry = next_loop_entry;
        }

        self.next_collection_alias_id.set(alias_seed);
        let cond_state = self.analyze_expr(condition, &loop_entry, line)?;
        let mut loop_control = LoopControlFlow::default();
        let (rewritten_body, _) = self.analyze_block_with_loop_control(
            body,
            cond_state.clone(),
            rewrite_clears,
            Some(&mut loop_control),
        )?;
        let out = if let Some(break_state) = loop_control.break_state {
            self.merge_states(cond_state, break_state)
        } else {
            cond_state
        };

        let rewritten = Stmt::While {
            condition: condition.clone(),
            body: rewritten_body,
            line,
        };
        Ok((rewritten, out))
    }

    fn analyze_for_loop(
        &self,
        for_loop: ForLoopParts<'_>,
        state: FlowState,
        rewrite_clears: bool,
    ) -> Result<(Stmt, FlowState), ParseError> {
        let ForLoopParts {
            init,
            condition,
            post,
            body,
            line,
        } = for_loop;
        let (rewritten_init, init_state) = self.analyze_stmt(init, state, rewrite_clears, None)?;
        let alias_seed = self.next_collection_alias_id.get();
        let mut loop_entry = init_state.clone();

        loop {
            self.next_collection_alias_id.set(alias_seed);
            let cond_state = self.analyze_expr(condition, &loop_entry, line)?;
            let mut loop_control = LoopControlFlow::default();
            let (_, body_state) = self.analyze_block_with_loop_control(
                body,
                cond_state.clone(),
                false,
                Some(&mut loop_control),
            )?;
            let mut post_entry = body_state;
            if let Some(continue_state) = loop_control.continue_state {
                post_entry = self.merge_states(post_entry, continue_state);
            }
            let (_, post_state) = self.analyze_stmt(post, post_entry, false, None)?;
            let next_loop_entry = self.merge_states(init_state.clone(), post_state);
            if next_loop_entry == loop_entry {
                break;
            }
            loop_entry = next_loop_entry;
        }

        self.next_collection_alias_id.set(alias_seed);
        let cond_state = self.analyze_expr(condition, &loop_entry, line)?;
        let mut loop_control = LoopControlFlow::default();
        let (rewritten_body, body_state) = self.analyze_block_with_loop_control(
            body,
            cond_state.clone(),
            rewrite_clears,
            Some(&mut loop_control),
        )?;
        let mut post_entry = body_state;
        if let Some(continue_state) = loop_control.continue_state {
            post_entry = self.merge_states(post_entry, continue_state);
        }
        let (rewritten_post, _) = self.analyze_stmt(post, post_entry, rewrite_clears, None)?;
        let out = if let Some(break_state) = loop_control.break_state {
            self.merge_states(cond_state, break_state)
        } else {
            cond_state
        };

        let rewritten = Stmt::For {
            init: Box::new(rewritten_init),
            condition: condition.clone(),
            post: Box::new(rewritten_post),
            body: rewritten_body,
            line,
        };
        Ok((rewritten, out))
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
                self.require_local_not_moved(*index, state, line)?;
                self.require_local_not_partially_moved(*index, state, line)?;
                Ok(state.clone())
            }
            Expr::MoveVar(index) => {
                self.require_available(*index, state, line)?;
                self.require_local_not_moved(*index, state, line)?;
                self.require_local_not_partially_moved(*index, state, line)?;
                let mut out = state.clone();
                self.mark_local_moved(&mut out, *index);
                Ok(out)
            }
            Expr::MoveField { root, key } => {
                self.require_available(*root, state, line)?;
                self.require_local_not_moved(*root, state, line)?;
                let field_key = MovedFieldKey::String(key.clone());
                self.require_field_available(*root, &field_key, state, line)?;
                let mut out = state.clone();
                self.mark_field_moved(&mut out, *root, field_key);
                Ok(out)
            }
            Expr::MoveIndex { root, index } => {
                self.require_available(*root, state, line)?;
                self.require_local_not_moved(*root, state, line)?;
                let field_key = MovedFieldKey::Index(*index);
                self.require_field_available(*root, &field_key, state, line)?;
                let mut out = state.clone();
                self.mark_field_moved(&mut out, *root, field_key);
                Ok(out)
            }
            Expr::Call(index, args) => {
                if !self.enable_local_move_semantics {
                    if let Some(root_slot) = self.extract_collection_mutation_root(*index, args) {
                        let mut out = self.analyze_args(args, state, line)?;
                        self.apply_interprocedural_consumed_call_effects(*index, args, &mut out);
                        self.require_collection_mutation_permitted(root_slot, &out, line)?;
                        return Ok(out);
                    }
                    let mut out = self.analyze_args(args, state, line)?;
                    self.apply_interprocedural_consumed_call_effects(*index, args, &mut out);
                    return Ok(out);
                }
                if let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
                {
                    let mut out = self.analyze_projection_args(args, state, line)?;
                    self.require_field_available(root_slot, &field_key, &out, line)?;
                    if !self.is_copyable_field(root_slot, &field_key, &out) {
                        self.mark_field_moved(&mut out, root_slot, field_key);
                    }
                    self.apply_interprocedural_consumed_call_effects(*index, args, &mut out);
                    Ok(out)
                } else if let Some(root_slot) = self.extract_collection_mutation_root(*index, args)
                {
                    let mut out =
                        if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::Set) {
                            self.analyze_projection_args(args, state, line)?
                        } else {
                            self.analyze_args(args, state, line)?
                        };
                    self.apply_interprocedural_consumed_call_effects(*index, args, &mut out);
                    self.require_collection_mutation_permitted(root_slot, &out, line)?;
                    Ok(out)
                } else {
                    let mut out = self.analyze_args(args, state, line)?;
                    self.apply_interprocedural_consumed_call_effects(*index, args, &mut out);
                    Ok(out)
                }
            }
            Expr::LocalCall(index, args) => {
                self.require_available(*index, state, line)?;
                self.analyze_args(args, state, line)
            }
            Expr::Closure(closure) => {
                self.analyze_closure(closure, state, line)?;
                let mut out = state.clone();
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.mark_available(&mut out, *captured_slot, line)?;
                    let capture_mode = self.closure_capture_mode_for_slot(closure, *captured_slot);
                    self.apply_capture_binding_effect(
                        &mut out,
                        *source_slot,
                        *captured_slot,
                        capture_mode,
                    );
                }
                Ok(out)
            }
            Expr::ClosureCall(closure, args) => {
                let mut out = self.analyze_args(args, state, line)?;
                self.analyze_closure(closure, &out, line)?;
                for (source_slot, captured_slot) in &closure.capture_copies {
                    self.mark_available(&mut out, *captured_slot, line)?;
                    let capture_mode = self.closure_capture_mode_for_slot(closure, *captured_slot);
                    self.apply_capture_binding_effect(
                        &mut out,
                        *source_slot,
                        *captured_slot,
                        capture_mode,
                    );
                }
                Ok(out)
            }
            Expr::Add(lhs, rhs) => {
                // `+` is commonly used for string concatenation in the subset.
                // Treat local/field reads in concat/add operands as copied to keep
                // ergonomics reasonable (`a + a`, `p.a + p.a`).
                let lhs_state = self.analyze_expr_to_owned(lhs, state, line)?;
                self.analyze_expr_to_owned(rhs, &lhs_state, line)
            }
            Expr::Sub(lhs, rhs)
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
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.analyze_expr_to_owned(inner, state, line)
            }
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
            Expr::ToOwned(inner) => self.analyze_expr_to_owned(inner, state, line),
            Expr::Block { stmts, expr } => {
                let (_, block_state) = self.analyze_block(stmts, state.clone(), false)?;
                self.analyze_expr(expr, &block_state, line)
            }
        }
    }

    fn analyze_expr_to_owned(
        &self,
        inner: &Expr,
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        if !self.enable_local_move_semantics {
            return self.analyze_expr(inner, state, line);
        }
        if let Expr::Var(index) = inner {
            self.require_available(*index, state, line)?;
            self.require_local_not_moved(*index, state, line)?;
            self.require_local_not_partially_moved(*index, state, line)?;
            return Ok(state.clone());
        }
        if let Expr::Call(index, args) = inner
            && let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
        {
            let out = self.analyze_projection_args(args, state, line)?;
            self.require_field_available(root_slot, &field_key, &out, line)?;
            return Ok(out);
        }
        self.analyze_expr(inner, state, line)
    }

    fn require_available(
        &self,
        index: LocalSlot,
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
            code: Some("E_LOCAL_UNAVAILABLE".to_string()),
            line: line as usize,
            message: format!(
                "local '{display}' may be unavailable on this control-flow path; initialize it before use"
            ),
        })
    }

    fn require_assignable(
        &self,
        index: LocalSlot,
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

    fn require_local_not_moved(
        &self,
        index: LocalSlot,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        if !self.enable_local_move_semantics {
            return Ok(());
        }
        let slot = index as usize;
        if slot >= self.local_count {
            return Ok(());
        }
        if !state.moved_local_possible[slot] {
            return Ok(());
        }
        let display = self.display_local_name(index);
        Err(ParseError {
            span: None,
            code: Some("E_LOCAL_MOVED".to_string()),
            line: line as usize,
            message: format!(
                "local '{display}' was moved earlier; use '{display}.copy()' to copy it before moving"
            ),
        })
    }

    fn require_local_not_partially_moved(
        &self,
        index: LocalSlot,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        if !self.enable_local_move_semantics {
            return Ok(());
        }
        if !self.local_names.contains_key(&index) {
            return Ok(());
        }
        if !self.moved_possible_for_root(state, index).any(|_| true) {
            return Ok(());
        }
        let display = self.display_local_name(index);
        Err(ParseError {
            span: None,
            code: Some("E_LOCAL_PARTIALLY_MOVED".to_string()),
            line: line as usize,
            message: format!(
                "local '{display}' is partially moved; access remaining fields/elements directly or reinitialize moved children before using '{display}' as a whole"
            ),
        })
    }

    fn mark_available(
        &self,
        state: &mut FlowState,
        index: LocalSlot,
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

    fn should_move_local_on_rebind_source(&self, index: LocalSlot, state: &FlowState) -> bool {
        if !self.enable_local_move_semantics {
            return false;
        }
        let slot = index as usize;
        if slot >= self.local_count {
            return false;
        }
        if !state.movable_locals[slot] {
            return false;
        }
        // Collection locals use alias tracking in the current model.
        state.collection_aliases[slot].is_empty()
    }

    fn rewrite_local_source_move_on_rebind(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        expr: &Expr,
    ) -> Expr {
        if !self.enable_local_move_semantics {
            return expr.clone();
        }
        let Expr::Var(source) = expr else {
            return expr.clone();
        };
        if *source == target {
            return expr.clone();
        }
        if self.should_move_local_on_rebind_source(*source, state) {
            self.mark_local_moved(state, *source);
            return Expr::MoveVar(*source);
        }
        expr.clone()
    }

    fn rewrite_runtime_field_move_expr(&self, expr: &Expr, state: &FlowState) -> Expr {
        if !self.enable_local_move_semantics {
            return expr.clone();
        }
        let Expr::Call(index, args) = expr else {
            return expr.clone();
        };
        if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Get) {
            return expr.clone();
        }
        let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args) else {
            return expr.clone();
        };
        if self.is_copyable_field(root_slot, &field_key, state) {
            return expr.clone();
        }
        match field_key {
            MovedFieldKey::String(key) => Expr::MoveField {
                root: root_slot,
                key,
            },
            MovedFieldKey::Index(index) => Expr::MoveIndex {
                root: root_slot,
                index,
            },
            MovedFieldKey::Dynamic | MovedFieldKey::Slice => expr.clone(),
        }
    }

    fn mark_local_moved(&self, state: &mut FlowState, index: LocalSlot) {
        let slot = index as usize;
        if slot >= self.local_count {
            return;
        }
        state.moved_local_definite[slot] = true;
        state.moved_local_possible[slot] = true;
    }

    fn apply_interprocedural_consumed_call_effects(
        &self,
        call_index: u16,
        args: &[Expr],
        state: &mut FlowState,
    ) {
        if !self.enable_local_move_semantics {
            return;
        }
        let Some(consumed_arg_positions) = self.function_consumed_params.get(&call_index) else {
            return;
        };
        for position in consumed_arg_positions {
            let Some(arg_expr) = args.get(*position) else {
                continue;
            };
            let source_slot = match arg_expr {
                Expr::Var(slot) | Expr::MoveVar(slot) => *slot,
                _ => continue,
            };
            if self.should_move_local_on_rebind_source(source_slot, state) {
                self.mark_local_moved(state, source_slot);
            }
        }
    }

    fn clear_local_moved_state(&self, state: &mut FlowState, index: LocalSlot) {
        let slot = index as usize;
        if slot >= self.local_count {
            return;
        }
        state.moved_local_definite[slot] = false;
        state.moved_local_possible[slot] = false;
    }

    fn set_local_copyable_state(&self, state: &mut FlowState, index: LocalSlot, is_copyable: bool) {
        let slot = index as usize;
        if slot < self.local_count {
            state.copyable_locals[slot] = is_copyable;
        }
    }

    fn set_local_movable_state(&self, state: &mut FlowState, index: LocalSlot, is_movable: bool) {
        let slot = index as usize;
        if slot < self.local_count {
            state.movable_locals[slot] = is_movable;
        }
    }

    fn is_definitely_movable_local_expr(&self, expr: &Expr, state: &FlowState) -> bool {
        if !self.enable_local_move_semantics {
            return false;
        }
        match expr {
            Expr::String(_) => true,
            Expr::Var(index) => state
                .movable_locals
                .get(*index as usize)
                .copied()
                .unwrap_or(false),
            Expr::MoveVar(index) => state
                .movable_locals
                .get(*index as usize)
                .copied()
                .unwrap_or(false),
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.is_definitely_movable_local_expr(then_expr, state)
                    && self.is_definitely_movable_local_expr(else_expr, state)
            }
            Expr::Match { arms, default, .. } => {
                arms.iter()
                    .all(|(_, arm_expr)| self.is_definitely_movable_local_expr(arm_expr, state))
                    && self.is_definitely_movable_local_expr(default, state)
            }
            Expr::Block { expr, .. } => self.is_definitely_movable_local_expr(expr, state),
            _ => false,
        }
    }

    fn merge_states(&self, lhs: FlowState, rhs: FlowState) -> FlowState {
        match (lhs.reachable, rhs.reachable) {
            (false, false) => FlowState {
                reachable: false,
                definite: vec![false; self.local_count],
                possible: vec![false; self.local_count],
                copyable_locals: vec![false; self.local_count],
                movable_locals: vec![false; self.local_count],
                collection_aliases: vec![HashSet::new(); self.local_count],
                moved_local_definite: vec![false; self.local_count],
                moved_local_possible: vec![false; self.local_count],
                moved_definite: HashSet::new(),
                moved_possible: HashSet::new(),
                copyable_fields: HashSet::new(),
            },
            (true, false) => lhs,
            (false, true) => rhs,
            (true, true) => {
                let mut definite = vec![false; self.local_count];
                let mut possible = vec![false; self.local_count];
                let mut copyable_locals = vec![false; self.local_count];
                let mut movable_locals = vec![false; self.local_count];
                let mut collection_aliases = vec![HashSet::new(); self.local_count];
                let mut moved_local_definite = vec![false; self.local_count];
                let mut moved_local_possible = vec![false; self.local_count];
                for idx in 0..self.local_count {
                    definite[idx] = lhs.definite[idx] && rhs.definite[idx];
                    possible[idx] = lhs.possible[idx] || rhs.possible[idx];
                    copyable_locals[idx] = lhs.copyable_locals[idx] && rhs.copyable_locals[idx];
                    movable_locals[idx] = lhs.movable_locals[idx] && rhs.movable_locals[idx];
                    collection_aliases[idx] = lhs.collection_aliases[idx]
                        .union(&rhs.collection_aliases[idx])
                        .copied()
                        .collect::<HashSet<_>>();
                    moved_local_definite[idx] =
                        lhs.moved_local_definite[idx] && rhs.moved_local_definite[idx];
                    moved_local_possible[idx] =
                        lhs.moved_local_possible[idx] || rhs.moved_local_possible[idx];
                }
                let moved_possible = lhs
                    .moved_possible
                    .union(&rhs.moved_possible)
                    .cloned()
                    .collect::<HashSet<_>>();
                let moved_definite = lhs
                    .moved_definite
                    .intersection(&rhs.moved_definite)
                    .cloned()
                    .collect::<HashSet<_>>();
                let copyable_fields = lhs
                    .copyable_fields
                    .intersection(&rhs.copyable_fields)
                    .cloned()
                    .collect::<HashSet<_>>();
                FlowState {
                    reachable: true,
                    definite,
                    possible,
                    copyable_locals,
                    movable_locals,
                    collection_aliases,
                    moved_local_definite,
                    moved_local_possible,
                    moved_definite,
                    moved_possible,
                    copyable_fields,
                }
            }
        }
    }
}

fn is_simple_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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
