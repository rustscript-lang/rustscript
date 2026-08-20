use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;
use crate::bytecode::CaptureBindingMode;
use crate::host_api::HostParamPassing;

use super::super::ParseError;
use super::super::ir::{
    ClosureExpr, Expr, FrontendIr, FunctionImpl, LocalSlot, ResolvedHostCall, Stmt,
};
use super::EntryLocalAvailability;
use super::liveness::{LivenessRewriter, LocalSlotAllocator, persistent_capture_slots};
mod captures;
mod consumption;
mod field_moves;

use self::consumption::{
    compute_function_consumed_param_positions, extract_passthrough_return_slot,
};

const LOCAL_SLOT_ALLOCATOR_COMPAT_THRESHOLD: usize = 8;

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

    fn reachable_with_entry_locals(
        local_count: usize,
        entry_locals: &[EntryLocalAvailability],
    ) -> Self {
        let mut state = Self::reachable(local_count);
        for entry in entry_locals {
            let slot = entry.slot as usize;
            if slot >= local_count {
                continue;
            }
            state.definite[slot] = true;
            state.possible[slot] = true;
            state.copyable_locals[slot] = entry.copyable;
            state.movable_locals[slot] = entry.movable;
            state.moved_local_definite[slot] = entry.moved;
            state.moved_local_possible[slot] = entry.moved;
        }
        state
    }
}

pub(super) fn enforce_local_availability(
    mut ir: FrontendIr,
    entry_locals: &[EntryLocalAvailability],
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
    owned_local_slots: &[bool],
) -> Result<FrontendIr, ParseError> {
    // Pad the post-legalize ownership metadata to the analyzer's local space.
    // Availability runs pre-compaction, so the logical slot indices align with
    // the schemas the typing pass recorded.
    let mut owned_slots = vec![false; ir.locals];
    for (slot, is_owned) in owned_local_slots.iter().enumerate().take(ir.locals) {
        owned_slots[slot] = *is_owned;
    }
    let initial_impls = std::mem::take(&mut ir.function_impls);

    let bootstrap_analyzer = AvailabilityAnalyzer::new(
        ir.locals,
        &ir.local_bindings,
        &initial_impls,
        enable_local_move_semantics,
        &owned_slots,
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
        &owned_slots,
    );
    let entry_state = FlowState::reachable_with_entry_locals(ir.locals, entry_locals);
    let (rewritten_stmts, _) = analyzer.analyze_block(&ir.stmts, entry_state, true)?;
    ir.stmts = rewritten_stmts;
    ir.function_impls = rewritten_impls;

    if clear_dead_locals {
        let liveness = LivenessRewriter::new(
            ir.locals,
            &ir.local_bindings,
            &ir.function_impls,
            &owned_slots,
        );
        let persistent_slots = persistent_capture_slots(&ir.stmts, &ir.function_impls);
        ir.stmts = liveness.rewrite_program_block(&ir.stmts);
        for function_impl in ir.function_impls.values_mut() {
            *function_impl =
                liveness.rewrite_function_impl(function_impl.clone(), &persistent_slots);
        }
    }

    // Preserve source-local slot identity for tiny programs. Once the frontend
    // grows past the compat threshold, compact onto the minimal physical slot
    // set while still rejecting programs that need more than 256 simultaneous
    // locals.
    Ok(ir)
}

/// Compact the flat local slot space onto the minimal physical slot set.
///
/// Kept separate from `enforce_local_availability` so callers can run the
/// callable-materialization classification on the *pre-compaction* IR: the
/// classifier tracks named-function values through slot flows, and merged
/// physical slots would collapse distinct flows into one slot, producing
/// spurious dynamic-target facts. Pre-compaction slots are the true
/// frame-relative value identities, so the classification is strictly more
/// precise on the unallocated IR.
pub(crate) fn allocate_local_slots(mut ir: FrontendIr) -> Result<FrontendIr, ParseError> {
    if ir.locals > LOCAL_SLOT_ALLOCATOR_COMPAT_THRESHOLD {
        let allocator = LocalSlotAllocator::new(ir.locals, &ir.local_bindings, &ir.function_impls);
        ir = allocator.allocate(ir)?;
    }
    Ok(ir)
}

pub(crate) fn function_capture_binding_mode(
    function_impl: &FunctionImpl,
    captured_slot: LocalSlot,
) -> CaptureBindingMode {
    AvailabilityAnalyzer::new(0, &[], &HashMap::new(), false, &[])
        .runtime_function_capture_mode_for_slot(function_impl, captured_slot)
}

pub(crate) fn closure_capture_binding_mode(
    closure: &ClosureExpr,
    captured_slot: LocalSlot,
) -> CaptureBindingMode {
    AvailabilityAnalyzer::new(0, &[], &HashMap::new(), false, &[])
        .runtime_closure_capture_mode_for_slot(closure, captured_slot)
}

struct AvailabilityAnalyzer {
    local_count: usize,
    local_names: HashMap<LocalSlot, String>,
    function_impls: HashMap<u16, FunctionImpl>,
    collection_passthrough_params: HashMap<u16, usize>,
    function_consumed_params: HashMap<u16, HashSet<usize>>,
    next_collection_alias_id: Cell<u32>,
    enable_local_move_semantics: bool,
    /// Per-logical-slot resource-ownership metadata (pre-compaction indices):
    /// a slot is owned when its post-legalize schema contains a resource
    /// anywhere. Owned slots are move-only and cannot be copied or borrowed
    /// outside exact host-call arguments.
    owned_local_slots: Vec<bool>,
}

impl AvailabilityAnalyzer {
    fn new(
        local_count: usize,
        local_bindings: &[(String, LocalSlot)],
        function_impls: &HashMap<u16, FunctionImpl>,
        enable_local_move_semantics: bool,
        owned_local_slots: &[bool],
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
        let mut owned = vec![false; local_count];
        for (slot, is_owned) in owned_local_slots.iter().enumerate().take(local_count) {
            owned[slot] = *is_owned;
        }
        Self {
            local_count,
            local_names,
            function_impls: function_impls.clone(),
            collection_passthrough_params,
            function_consumed_params,
            next_collection_alias_id: Cell::new(1),
            enable_local_move_semantics,
            owned_local_slots: owned,
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
            body_expr_line,
        } = function_impl;
        let mut state = FlowState::reachable(self.local_count);
        for slot in &param_slots {
            self.mark_available(&mut state, *slot, 1)?;
            // Resource-typed parameters are move-only inside the body: they
            // can be returned (moved out), passed by ownership, or borrowed
            // through exact host-call arguments, but never copied.
            if self.is_owned_slot(*slot) {
                state.copyable_locals[*slot as usize] = false;
                state.movable_locals[*slot as usize] = true;
            }
        }
        for (_, captured_slot) in &capture_copies {
            self.mark_available(&mut state, *captured_slot, 1)?;
        }
        let (rewritten_body, body_state) = self.analyze_block(&body_stmts, state, true)?;
        let rewritten_body_expr = self.rewrite_function_return_expr(&body_expr, &body_state)?;
        self.analyze_expr(&body_expr, &body_state, 1)?;
        Ok(FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts: rewritten_body,
            body_expr: rewritten_body_expr,
            body_expr_line,
        })
    }

    /// Rewrites a function's tail expression for resource ownership.
    ///
    /// Returning a resource-owning local must move it out of the frame: the
    /// bytecode then carries a `MoveVar` (ldloc + DetachLocal) so the frame
    /// exit never releases the same owner again. Nested tail positions
    /// (if/match branches, block tails) get the same treatment through the
    /// generic ownership rewrite.
    fn rewrite_function_return_expr(
        &self,
        expr: &Expr,
        state: &FlowState,
    ) -> Result<Expr, ParseError> {
        if let Expr::Var(slot) = expr
            && self.is_owned_slot(*slot)
        {
            self.require_available(*slot, state, 1)?;
            self.require_local_not_moved(*slot, state, 1)?;
            self.require_local_not_partially_moved(*slot, state, 1)?;
            return Ok(Expr::MoveVar(*slot));
        }
        self.rewrite_expr_for_ownership(expr)
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
            Stmt::FuncDecl {
                index,
                has_impl,
                line,
                ..
            } => {
                let mut out = state.clone();
                if *has_impl
                    && out.reachable
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
                            capture_mode.0,
                            capture_mode.1,
                            *line,
                        )?;
                    }
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Let {
                index,
                declared_schema,
                expr,
                line,
            } => {
                let mut initializer_state = state.clone();
                if let Expr::Closure(closure) = expr
                    && closure
                        .capture_copies
                        .iter()
                        .any(|(source, _)| source == index)
                {
                    self.mark_available(&mut initializer_state, *index, *line)?;
                }
                let mut out = self.analyze_expr(expr, &initializer_state, *line)?;
                let mut rewritten_expr = expr.clone();
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                    self.clear_local_moved_state(&mut out, *index);
                    self.handle_local_rebind_field_moves(&mut out, *index, expr);
                    self.handle_local_rebind_collection_aliases(&mut out, *index, expr);
                    let (is_copyable, is_movable) = if self.is_owned_slot(*index) {
                        // Resource-owning bindings are move-only by schema,
                        // not by the literal shape of their initializer.
                        (false, true)
                    } else {
                        (
                            self.is_definitely_copyable_expr(expr, &out),
                            self.is_definitely_movable_local_expr(expr, &out),
                        )
                    };
                    self.set_local_copyable_state(&mut out, *index, is_copyable);
                    self.set_local_movable_state(&mut out, *index, is_movable);
                    rewritten_expr =
                        self.rewrite_local_source_move_on_rebind(&mut out, *index, expr);
                    rewritten_expr = self.rewrite_expr_for_ownership(&rewritten_expr)?;
                    rewritten_expr = self.rewrite_runtime_field_move_expr(&rewritten_expr, &state);
                }
                Ok((
                    Stmt::Let {
                        index: *index,
                        declared_schema: declared_schema.clone(),
                        expr: rewritten_expr,
                        line: *line,
                    },
                    out,
                ))
            }
            Stmt::Assign {
                kind,
                index,
                expr,
                line,
            } => {
                self.require_assignable(*index, &state, *line)?;
                let mut out = self.analyze_expr(expr, &state, *line)?;
                let mut rewritten_expr = expr.clone();
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                    self.clear_local_moved_state(&mut out, *index);
                    self.handle_local_rebind_field_moves(&mut out, *index, expr);
                    self.handle_local_rebind_collection_aliases(&mut out, *index, expr);
                    let (is_copyable, is_movable) = if self.is_owned_slot(*index) {
                        (false, true)
                    } else {
                        (
                            self.is_definitely_copyable_expr(expr, &out),
                            self.is_definitely_movable_local_expr(expr, &out),
                        )
                    };
                    self.set_local_copyable_state(&mut out, *index, is_copyable);
                    self.set_local_movable_state(&mut out, *index, is_movable);
                    rewritten_expr =
                        self.rewrite_local_source_move_on_rebind(&mut out, *index, expr);
                    rewritten_expr = self.rewrite_expr_for_ownership(&rewritten_expr)?;
                    rewritten_expr = self.rewrite_runtime_field_move_expr(&rewritten_expr, &state);
                }
                Ok((
                    Stmt::Assign {
                        kind: kind.clone(),
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
                            capture_mode.0,
                            capture_mode.1,
                            *line,
                        )?;
                    }
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Expr { expr, line } => {
                let mut out = self.analyze_expr(expr, &state, *line)?;
                // A bare value read at statement level consumes owned locals
                // (the value is discarded, so the handle must not stay
                // available for a second use).
                self.mark_owned_value_reads_moved(expr, &mut out);
                let rewritten_expr = self.rewrite_expr_for_ownership(expr)?;
                let rewritten_expr = self.rewrite_runtime_field_move_expr(&rewritten_expr, &state);
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
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. } => Ok(state.clone()),
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
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                let container_state = self.analyze_expr(container, state, line)?;
                let mut out = self.analyze_expr(key, &container_state, line)?;
                self.mark_available(&mut out, *container_slot, line)?;
                self.mark_available(&mut out, *key_slot, line)?;
                Ok(out)
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                let mut value_state = self.analyze_expr(value, state, line)?;
                self.mark_available(&mut value_state, *value_slot, line)?;
                let then_state = self.analyze_expr(fallback, &value_state, line)?;
                Ok(self.merge_states(then_state, value_state))
            }
            // Resolved module calls (pre-merge only) analyze their arguments;
            // interprocedural effects apply to the post-merge flat call.
            Expr::ModuleCall(_, _, args) => self.analyze_args(args, state, line),
            Expr::Call(index, _, args, resolution, _) => {
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
                // Catalog-resolved host calls carry the exact ordered passing
                // modes; ownership transfer (TakeOwned) moves the source
                // local/field, while Borrow/BorrowMut produce read-only
                // call-scoped temporaries that never consume the owner.
                if let Some(resolution) = resolution {
                    return self.analyze_resolved_call_args(args, resolution, state, line);
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
                    // Inserting an owned local into an aggregate transfers
                    // ownership of the handle into the collection/field.
                    self.apply_owned_aggregate_insertion_effect(*index, args, &mut out);
                    self.require_collection_mutation_permitted(root_slot, &out, line)?;
                    Ok(out)
                } else {
                    let mut out = self.analyze_args(args, state, line)?;
                    self.apply_interprocedural_consumed_call_effects(*index, args, &mut out);
                    // Inserting an owned local into an aggregate (array/map
                    // literals lower to ArrayPush/Set on a fresh collection)
                    // transfers ownership of the handle into the aggregate.
                    self.apply_owned_aggregate_insertion_effect(*index, args, &mut out);
                    Ok(out)
                }
            }
            Expr::LocalCall(index, _, args) => {
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
                        capture_mode.0,
                        capture_mode.1,
                        line,
                    )?;
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
                        capture_mode.0,
                        capture_mode.1,
                        line,
                    )?;
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
                // Outside an exact host-call argument a borrow wrapper is an
                // escape: resources cannot be aliased across a statement, and
                // the compiler never clones their underlying handle.
                if self.expr_contains_owned_local(inner) {
                    let display = self.display_owned_expr_local(inner);
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_BORROW_ESCAPE".to_string()),
                        line: line as usize,
                        message: format!(
                            "borrow of resource value '{display}' must be passed directly as an argument to a host function call; resources cannot escape a call as borrows"
                        ),
                    });
                }
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
                let mut out = self.merge_states(then_state, else_state);
                // Branch values flow into the merged result: reading an owned
                // local as a branch value transfers its ownership into the
                // merged value, so the source becomes moved on every path.
                self.mark_owned_value_reads_moved(then_expr, &mut out);
                self.mark_owned_value_reads_moved(else_expr, &mut out);
                Ok(out)
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
                for (pattern, arm_expr) in arms {
                    let mut arm_state_in = value_state.clone();
                    if let Some(binding_slot) = pattern.binding_slot() {
                        self.mark_available(&mut arm_state_in, binding_slot, line)?;
                    }
                    let arm_state = self.analyze_expr(arm_expr, &arm_state_in, line)?;
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
                for (_, arm_expr) in arms {
                    self.mark_owned_value_reads_moved(arm_expr, &mut out);
                }
                self.mark_owned_value_reads_moved(default, &mut out);
                if out.reachable {
                    self.mark_available(&mut out, *result_slot, line)?;
                }
                Ok(out)
            }
            Expr::ToOwned(inner) => {
                // `.copy()` on a resource-containing value would duplicate the
                // underlying handle; the core has no generic resource copy, so
                // this is a structured compile error rather than a silent
                // degradation to a plain read.
                if self.expr_contains_owned_local(inner) {
                    let display = self.display_owned_expr_local(inner);
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_COPY_RESOURCE".to_string()),
                        line: line as usize,
                        message: format!(
                            "cannot copy resource value '{display}'; resources are move-only and do not support '.copy()'"
                        ),
                    });
                }
                self.analyze_expr_to_owned(inner, state, line)
            }
            Expr::Block { stmts, expr } => {
                let (_, block_state) = self.analyze_block(stmts, state.clone(), false)?;
                let mut out = self.analyze_expr(expr, &block_state, line)?;
                self.mark_owned_value_reads_moved(expr, &mut out);
                Ok(out)
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
        if let Expr::Call(index, _, args, _, _) = inner
            && let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
        {
            let out = self.analyze_projection_args(args, state, line)?;
            self.require_field_available(root_slot, &field_key, &out, line)?;
            return Ok(out);
        }
        self.analyze_expr(inner, state, line)
    }

    /// Whether a logical local slot carries a resource anywhere in its
    /// post-legalize schema (direct or nested).
    fn is_owned_slot(&self, index: LocalSlot) -> bool {
        self.owned_local_slots
            .get(index as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Analyzes the arguments of a catalog-resolved host call against its
    /// exact ordered passing modes.
    ///
    /// `TakeOwned` arguments transfer ownership: the source local/field is
    /// marked moved (definite and possible) so any later use on the same path
    /// fails with a use-after-move diagnostic. `Borrow`/`BorrowMut` arguments
    /// are call-scoped read-only temporaries: the owner is never consumed and
    /// repeated borrows of the same local are fine. `Value` arguments are
    /// plain reads.
    fn analyze_resolved_call_args(
        &self,
        args: &[Expr],
        resolution: &ResolvedHostCall,
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        let mut out = state.clone();
        for (position, arg) in args.iter().enumerate() {
            out = match resolution.passing.get(position).copied() {
                Some(HostParamPassing::TakeOwned) => {
                    self.legalize_take_owned_arg(arg, &out, line)?
                }
                Some(HostParamPassing::Borrow) | Some(HostParamPassing::BorrowMut) => {
                    // The parser wraps borrowed arguments in Borrow/BorrowMut;
                    // unwrap them here into a non-consuming read so the
                    // generic borrow arm (which rejects resource escapes)
                    // never sees them.
                    match arg {
                        Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                            self.analyze_expr_to_owned(inner, &out, line)?
                        }
                        other => self.analyze_expr_to_owned(other, &out, line)?,
                    }
                }
                _ => self.analyze_expr(arg, &out, line)?,
            };
        }
        Ok(out)
    }

    /// Flow effect of a `TakeOwned` argument: the source local or literal-key
    /// field is consumed (marked moved). Fresh values (nested call results,
    /// literals) flow directly into the argument slot and have no local
    /// ownership effect. Anything else is a structurally rejected source.
    fn legalize_take_owned_arg(
        &self,
        arg: &Expr,
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        match arg {
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                self.require_available(*slot, state, line)?;
                self.require_local_not_moved(*slot, state, line)?;
                self.require_local_not_partially_moved(*slot, state, line)?;
                let mut out = state.clone();
                self.mark_local_moved(&mut out, *slot);
                Ok(out)
            }
            Expr::Call(index, _, args, _, _)
                if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::Get) =>
            {
                let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
                else {
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_TAKEOWNED_SOURCE".to_string()),
                        line: line as usize,
                        message: "TakeOwned host-call arguments must be a local, a literal-key field/index access, or a fresh call result; this argument cannot transfer ownership".to_string(),
                    });
                };
                if matches!(field_key, MovedFieldKey::Dynamic | MovedFieldKey::Slice) {
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_TAKEOWNED_SOURCE".to_string()),
                        line: line as usize,
                        message: "TakeOwned host-call arguments cannot use a dynamic key or slice access; use a literal field/index to transfer ownership".to_string(),
                    });
                }
                self.require_available(root_slot, state, line)?;
                self.require_local_not_moved(root_slot, state, line)?;
                self.require_field_available(root_slot, &field_key, state, line)?;
                let mut out = state.clone();
                self.mark_field_moved(&mut out, root_slot, field_key);
                Ok(out)
            }
            other => self.analyze_expr(other, state, line),
        }
    }

    /// Flow effect of inserting an owned local into an aggregate: `Set`
    /// (field/map write) and `ArrayPush` value arguments transfer the handle
    /// into the aggregate, so the source local becomes moved.
    fn apply_owned_aggregate_insertion_effect(
        &self,
        call_index: u16,
        args: &[Expr],
        state: &mut FlowState,
    ) {
        if !self.enable_local_move_semantics {
            return;
        }
        let value_position = match BuiltinFunction::from_call_index(call_index) {
            Some(BuiltinFunction::Set) if args.len() == 3 => Some(2),
            Some(BuiltinFunction::ArrayPush) if args.len() == 2 => Some(1),
            _ => None,
        };
        let Some(position) = value_position else {
            return;
        };
        let Some(Expr::Var(slot) | Expr::MoveVar(slot)) = args.get(position) else {
            return;
        };
        if self.is_owned_slot(*slot) {
            self.mark_local_moved(state, *slot);
        }
    }

    /// Marks owned locals/fields read as the *value* of an expression as
    /// moved. Covers direct value reads (`Var`, literal field/index access)
    /// and nested value positions (if/match branches, block tails). Call
    /// arguments are handled by their own passing rules and are intentionally
    /// not walked here.
    fn mark_owned_value_reads_moved(&self, expr: &Expr, state: &mut FlowState) {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                if self.is_owned_slot(*slot) {
                    self.mark_local_moved(state, *slot);
                }
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                let _ = root;
            }
            Expr::Call(index, _, args, _, _) => {
                if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::Get)
                    && let Some((root_slot, field_key)) =
                        self.extract_moved_field_access(*index, args)
                    && !self.is_copyable_field(root_slot, &field_key, state)
                {
                    self.mark_field_moved(state, root_slot, field_key);
                }
            }
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.mark_owned_value_reads_moved(then_expr, state);
                self.mark_owned_value_reads_moved(else_expr, state);
            }
            Expr::Match { arms, default, .. } => {
                for (_, arm_expr) in arms {
                    self.mark_owned_value_reads_moved(arm_expr, state);
                }
                self.mark_owned_value_reads_moved(default, state);
            }
            Expr::Block { expr, .. } => self.mark_owned_value_reads_moved(expr, state),
            _ => {}
        }
    }

    /// Whether an expression reads an owned local anywhere (directly or
    /// through projections, aggregates, or nested calls).
    fn expr_contains_owned_local(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => self.is_owned_slot(*slot),
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                self.is_owned_slot(*root)
            }
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => {
                self.is_owned_slot(*container_slot)
                    || self.is_owned_slot(*key_slot)
                    || self.expr_contains_owned_local(container)
                    || self.expr_contains_owned_local(key)
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => {
                self.is_owned_slot(*value_slot)
                    || self.expr_contains_owned_local(value)
                    || self.expr_contains_owned_local(fallback)
            }
            Expr::Call(_, _, args, _, _)
            | Expr::LocalCall(_, _, args)
            | Expr::ModuleCall(_, _, args) => {
                args.iter().any(|arg| self.expr_contains_owned_local(arg))
            }
            Expr::Closure(closure) => {
                closure
                    .capture_copies
                    .iter()
                    .any(|(source, _)| self.is_owned_slot(*source))
                    || self.expr_contains_owned_local(&closure.body)
            }
            Expr::ClosureCall(closure, args) => {
                args.iter().any(|arg| self.expr_contains_owned_local(arg))
                    || closure
                        .capture_copies
                        .iter()
                        .any(|(source, _)| self.is_owned_slot(*source))
                    || self.expr_contains_owned_local(&closure.body)
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
                self.expr_contains_owned_local(lhs) || self.expr_contains_owned_local(rhs)
            }
            Expr::Neg(inner) | Expr::Not(inner) => self.expr_contains_owned_local(inner),
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.expr_contains_owned_local(inner)
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.expr_contains_owned_local(condition)
                    || self.expr_contains_owned_local(then_expr)
                    || self.expr_contains_owned_local(else_expr)
            }
            Expr::Match {
                value,
                arms,
                default,
                ..
            } => {
                self.expr_contains_owned_local(value)
                    || arms
                        .iter()
                        .any(|(_, arm_expr)| self.expr_contains_owned_local(arm_expr))
                    || self.expr_contains_owned_local(default)
            }
            Expr::Block { stmts, expr } => {
                stmts
                    .iter()
                    .any(|stmt| self.stmt_contains_owned_local(stmt))
                    || self.expr_contains_owned_local(expr)
            }
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::Bytes(_)
            | Expr::String(_)
            | Expr::FunctionRef(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. } => false,
        }
    }

    fn stmt_contains_owned_local(&self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Drop { .. } => false,
            Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                self.expr_contains_owned_local(expr)
            }
            Stmt::ClosureLet { closure, .. } => {
                closure
                    .capture_copies
                    .iter()
                    .any(|(source, _)| self.is_owned_slot(*source))
                    || self.expr_contains_owned_local(&closure.body)
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.expr_contains_owned_local(condition)
                    || then_branch
                        .iter()
                        .any(|nested| self.stmt_contains_owned_local(nested))
                    || else_branch
                        .iter()
                        .any(|nested| self.stmt_contains_owned_local(nested))
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.stmt_contains_owned_local(init)
                    || self.expr_contains_owned_local(condition)
                    || self.stmt_contains_owned_local(post)
                    || body
                        .iter()
                        .any(|nested| self.stmt_contains_owned_local(nested))
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.expr_contains_owned_local(condition)
                    || body
                        .iter()
                        .any(|nested| self.stmt_contains_owned_local(nested))
            }
        }
    }

    /// The named local an owned-bearing expression reads, for diagnostics.
    fn display_owned_expr_local(&self, expr: &Expr) -> String {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => self.display_local_name(*slot),
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                self.display_local_name(*root)
            }
            Expr::Call(index, _, args, _, _)
                if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::Get) =>
            {
                args.first()
                    .and_then(|arg| match arg {
                        Expr::Var(slot) => Some(self.display_local_name(*slot)),
                        _ => None,
                    })
                    .unwrap_or_else(|| "resource value".to_string())
            }
            _ => "resource value".to_string(),
        }
    }

    /// Recursively rewrites an expression tree for resource ownership:
    ///
    /// * catalog-resolved host-call arguments are rewritten per their exact
    ///   ordered passing mode (`TakeOwned` moves the source local/field,
    ///   `Borrow`/`BorrowMut` unwrap into a plain read);
    /// * owned locals read as value positions (if/match branches, block
    ///   tails, statement values) become `MoveVar`;
    /// * `Set`/`ArrayPush` value arguments that are owned locals become
    ///   `MoveVar` (aggregate insertion transfers ownership).
    ///
    /// Plain (non-resource) programs are structurally preserved.
    fn rewrite_expr_for_ownership(&self, expr: &Expr) -> Result<Expr, ParseError> {
        self.rewrite_expr_ownership_inner(expr, false)
    }

    fn rewrite_expr_ownership_inner(
        &self,
        expr: &Expr,
        in_call_arg: bool,
    ) -> Result<Expr, ParseError> {
        match expr {
            Expr::Call(index, type_args, args, resolution, source_node_id) => {
                let mut rewritten_args = Vec::with_capacity(args.len());
                for arg in args {
                    rewritten_args.push(self.rewrite_expr_ownership_inner(arg, true)?);
                }
                if let Some(resolution) = resolution.as_deref() {
                    for (position, arg) in rewritten_args.iter_mut().enumerate() {
                        match resolution.passing.get(position).copied() {
                            Some(HostParamPassing::TakeOwned) => {
                                *arg = self.rewrite_take_owned_arg(arg)?;
                            }
                            Some(HostParamPassing::Borrow) | Some(HostParamPassing::BorrowMut) => {
                                *arg = self.rewrite_borrow_arg(arg);
                            }
                            _ => {}
                        }
                    }
                    return Ok(Expr::Call(
                        *index,
                        type_args.clone(),
                        rewritten_args,
                        Some(Box::new(resolution.clone())),
                        *source_node_id,
                    ));
                }
                // Legacy (non-resolved) calls keep their shape except for
                // aggregate insertion of owned locals.
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    let value_position = match builtin {
                        BuiltinFunction::Set if args.len() == 3 => Some(2),
                        BuiltinFunction::ArrayPush if args.len() == 2 => Some(1),
                        _ => None,
                    };
                    if let Some(position) = value_position
                        && let Some(Expr::Var(slot)) = rewritten_args.get(position)
                        && self.is_owned_slot(*slot)
                    {
                        rewritten_args[position] = Expr::MoveVar(*slot);
                    }
                }
                Ok(Expr::Call(
                    *index,
                    type_args.clone(),
                    rewritten_args,
                    None,
                    *source_node_id,
                ))
            }
            Expr::Var(slot) if !in_call_arg && self.is_owned_slot(*slot) => {
                Ok(Expr::MoveVar(*slot))
            }
            Expr::Var(slot) => Ok(Expr::Var(*slot)),
            Expr::MoveVar(slot) => Ok(Expr::MoveVar(*slot)),
            Expr::MoveField { root, key } => Ok(Expr::MoveField {
                root: *root,
                key: key.clone(),
            }),
            Expr::MoveIndex { root, index } => Ok(Expr::MoveIndex {
                root: *root,
                index: *index,
            }),
            Expr::OptionalGet {
                container,
                key,
                container_slot,
                key_slot,
            } => Ok(Expr::OptionalGet {
                container: Box::new(self.rewrite_expr_ownership_inner(container, false)?),
                key: Box::new(self.rewrite_expr_ownership_inner(key, false)?),
                container_slot: *container_slot,
                key_slot: *key_slot,
            }),
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
            } => Ok(Expr::OptionUnwrapOr {
                value: Box::new(self.rewrite_expr_ownership_inner(value, false)?),
                value_slot: *value_slot,
                fallback: Box::new(self.rewrite_expr_ownership_inner(fallback, false)?),
            }),
            Expr::LocalCall(index, type_args, args) => Ok(Expr::LocalCall(
                *index,
                type_args.clone(),
                self.rewrite_call_args(args)?,
            )),
            Expr::ModuleCall(index, type_args, args) => Ok(Expr::ModuleCall(
                *index,
                type_args.clone(),
                self.rewrite_call_args(args)?,
            )),
            Expr::Closure(closure) => Ok(Expr::Closure(ClosureExpr {
                param_slots: closure.param_slots.clone(),
                capture_copies: closure.capture_copies.clone(),
                body: Box::new(self.rewrite_expr_ownership_inner(&closure.body, false)?),
            })),
            Expr::ClosureCall(closure, args) => Ok(Expr::ClosureCall(
                ClosureExpr {
                    param_slots: closure.param_slots.clone(),
                    capture_copies: closure.capture_copies.clone(),
                    body: Box::new(self.rewrite_expr_ownership_inner(&closure.body, false)?),
                },
                self.rewrite_call_args(args)?,
            )),
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
                let lhs = self.rewrite_expr_ownership_inner(lhs, false)?;
                let rhs = self.rewrite_expr_ownership_inner(rhs, false)?;
                Ok(match expr {
                    Expr::Add(..) => Expr::Add(Box::new(lhs), Box::new(rhs)),
                    Expr::Sub(..) => Expr::Sub(Box::new(lhs), Box::new(rhs)),
                    Expr::Mul(..) => Expr::Mul(Box::new(lhs), Box::new(rhs)),
                    Expr::Div(..) => Expr::Div(Box::new(lhs), Box::new(rhs)),
                    Expr::Mod(..) => Expr::Mod(Box::new(lhs), Box::new(rhs)),
                    Expr::And(..) => Expr::And(Box::new(lhs), Box::new(rhs)),
                    Expr::Or(..) => Expr::Or(Box::new(lhs), Box::new(rhs)),
                    Expr::Eq(..) => Expr::Eq(Box::new(lhs), Box::new(rhs)),
                    Expr::Lt(..) => Expr::Lt(Box::new(lhs), Box::new(rhs)),
                    Expr::Gt(..) => Expr::Gt(Box::new(lhs), Box::new(rhs)),
                    _ => unreachable!("binary operator arm"),
                })
            }
            Expr::Neg(inner) | Expr::Not(inner) => {
                let inner = self.rewrite_expr_ownership_inner(inner, false)?;
                Ok(match expr {
                    Expr::Neg(..) => Expr::Neg(Box::new(inner)),
                    _ => Expr::Not(Box::new(inner)),
                })
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                // Non-resource borrow/copy wrappers are preserved verbatim;
                // resource-bearing ones were already rejected during analysis.
                // The inner read keeps the call-argument context when nested
                // inside one, so a borrow of an owned local in a host-call
                // argument stays a plain read (never a MoveVar).
                let inner = self.rewrite_expr_ownership_inner(inner, in_call_arg)?;
                Ok(match expr {
                    Expr::ToOwned(..) => Expr::ToOwned(Box::new(inner)),
                    Expr::Borrow(..) => Expr::Borrow(Box::new(inner)),
                    _ => Expr::BorrowMut(Box::new(inner)),
                })
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => Ok(Expr::IfElse {
                condition: Box::new(self.rewrite_expr_ownership_inner(condition, false)?),
                then_expr: Box::new(self.rewrite_expr_ownership_inner(then_expr, false)?),
                else_expr: Box::new(self.rewrite_expr_ownership_inner(else_expr, false)?),
            }),
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                let mut rewritten_arms = Vec::with_capacity(arms.len());
                for (pattern, arm_expr) in arms {
                    rewritten_arms.push((
                        pattern.clone(),
                        self.rewrite_expr_ownership_inner(arm_expr, false)?,
                    ));
                }
                Ok(Expr::Match {
                    value_slot: *value_slot,
                    result_slot: *result_slot,
                    value: Box::new(self.rewrite_expr_ownership_inner(value, false)?),
                    arms: rewritten_arms,
                    default: Box::new(self.rewrite_expr_ownership_inner(default, false)?),
                })
            }
            Expr::Block { stmts, expr } => Ok(Expr::Block {
                // Statement-level rewrites run through the stmt handlers
                // during analysis; inner block statements are preserved.
                stmts: stmts.clone(),
                expr: Box::new(self.rewrite_expr_ownership_inner(expr, false)?),
            }),
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Bytes(_)
            | Expr::FunctionRef(..)
            | Expr::ModuleFunctionRef(..)
            | Expr::UnresolvedFunctionRef { .. } => Ok(expr.clone()),
        }
    }

    fn rewrite_call_args(&self, args: &[Expr]) -> Result<Vec<Expr>, ParseError> {
        let mut rewritten = Vec::with_capacity(args.len());
        for arg in args {
            rewritten.push(self.rewrite_expr_ownership_inner(arg, true)?);
        }
        Ok(rewritten)
    }

    /// Rewrites a `TakeOwned` argument: a local becomes `MoveVar`, a literal
    /// field/index access becomes `MoveField`/`MoveIndex`. Fresh call results
    /// stay as-is. Anything else was already rejected during analysis.
    fn rewrite_take_owned_arg(&self, arg: &Expr) -> Result<Expr, ParseError> {
        match arg {
            Expr::Var(slot) => Ok(Expr::MoveVar(*slot)),
            Expr::MoveVar(slot) => Ok(Expr::MoveVar(*slot)),
            Expr::Call(index, _, args, _, _)
                if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::Get) =>
            {
                let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
                else {
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_TAKEOWNED_SOURCE".to_string()),
                        line: 1,
                        message: "TakeOwned host-call arguments must be a local, a literal-key field/index access, or a fresh call result; this argument cannot transfer ownership".to_string(),
                    });
                };
                match field_key {
                    MovedFieldKey::String(key) => Ok(Expr::MoveField {
                        root: root_slot,
                        key,
                    }),
                    MovedFieldKey::Index(index) => Ok(Expr::MoveIndex {
                        root: root_slot,
                        index,
                    }),
                    MovedFieldKey::Dynamic | MovedFieldKey::Slice => Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_TAKEOWNED_SOURCE".to_string()),
                        line: 1,
                        message: "TakeOwned host-call arguments cannot use a dynamic key or slice access; use a literal field/index to transfer ownership".to_string(),
                    }),
                }
            }
            other => Ok(other.clone()),
        }
    }

    /// Unwraps a borrow wrapper in a host-call argument: the borrow is a
    /// call-scoped passing intent, and the underlying read is a plain
    /// non-consuming temporary.
    fn rewrite_borrow_arg(&self, arg: &Expr) -> Expr {
        match arg {
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => inner.as_ref().clone(),
            other => other.clone(),
        }
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
        let Expr::Call(index, _, args, _, _) = expr else {
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
            Expr::Bytes(_) => true,
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
