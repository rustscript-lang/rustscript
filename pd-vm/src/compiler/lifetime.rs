use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;

use super::ParseError;
use super::ir::{ClosureExpr, Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt};

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
}

fn extract_passthrough_return_slot(function_impl: &FunctionImpl) -> Option<LocalSlot> {
    if !function_impl.body_stmts.is_empty() {
        return None;
    }
    let Expr::Var(slot) = function_impl.body_expr else {
        return None;
    };
    Some(slot)
}

pub(super) fn enforce_local_availability(
    mut ir: FrontendIr,
    clear_dead_locals: bool,
) -> Result<FrontendIr, ParseError> {
    let analyzer = AvailabilityAnalyzer::new(ir.locals, &ir.local_bindings, &ir.function_impls);
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

    if clear_dead_locals {
        let liveness = LivenessRewriter::new(ir.locals, &ir.local_bindings);
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
    collection_passthrough_params: HashMap<u16, usize>,
    next_collection_alias_id: Cell<u32>,
}

impl AvailabilityAnalyzer {
    fn new(
        local_count: usize,
        local_bindings: &[(String, LocalSlot)],
        function_impls: &HashMap<u16, FunctionImpl>,
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
        Self {
            local_count,
            local_names,
            collection_passthrough_params,
            next_collection_alias_id: Cell::new(1),
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
                    rewritten.push(Stmt::Assign {
                        index: slot as LocalSlot,
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
        loop_control: Option<&mut LoopControlFlow>,
    ) -> Result<(Stmt, FlowState), ParseError> {
        match stmt {
            Stmt::Noop { .. } | Stmt::FuncDecl { .. } => Ok((stmt.clone(), state)),
            Stmt::Let { index, expr, line } => {
                let mut out = self.analyze_expr(expr, &state, *line)?;
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                    self.clear_local_moved_state(&mut out, *index);
                    self.handle_local_rebind_field_moves(&mut out, *index, expr);
                    self.handle_local_rebind_collection_aliases(&mut out, *index, expr);
                    let is_copyable = self.is_definitely_copyable_expr(expr, &out);
                    self.set_local_copyable_state(&mut out, *index, is_copyable);
                    let is_movable = self.is_definitely_movable_local_expr(expr, &out);
                    self.set_local_movable_state(&mut out, *index, is_movable);
                    self.mark_local_source_moved_on_rebind(&mut out, *index, expr);
                }
                Ok((stmt.clone(), out))
            }
            Stmt::Assign { index, expr, line } => {
                self.require_assignable(*index, &state, *line)?;
                let mut out = self.analyze_expr(expr, &state, *line)?;
                if out.reachable {
                    self.mark_available(&mut out, *index, *line)?;
                    self.clear_local_moved_state(&mut out, *index);
                    self.handle_local_rebind_field_moves(&mut out, *index, expr);
                    self.handle_local_rebind_collection_aliases(&mut out, *index, expr);
                    let is_copyable = self.is_definitely_copyable_expr(expr, &out);
                    self.set_local_copyable_state(&mut out, *index, is_copyable);
                    let is_movable = self.is_definitely_movable_local_expr(expr, &out);
                    self.set_local_movable_state(&mut out, *index, is_movable);
                    self.mark_local_source_moved_on_rebind(&mut out, *index, expr);
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
                Ok(state.clone())
            }
            Expr::Call(index, args) => {
                if let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
                {
                    let mut out = self.analyze_args(args, state, line)?;
                    self.require_field_available(root_slot, &field_key, &out, line)?;
                    if !self.is_copyable_field(root_slot, &field_key, &out) {
                        self.mark_field_moved(&mut out, root_slot, field_key);
                    }
                    Ok(out)
                } else if let Some(root_slot) = self.extract_collection_mutation_root(*index, args)
                {
                    let out = self.analyze_args(args, state, line)?;
                    self.require_collection_mutation_permitted(root_slot, &out, line)?;
                    Ok(out)
                } else {
                    self.analyze_args(args, state, line)
                }
            }
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
        if let Expr::Var(index) = inner {
            self.require_available(*index, state, line)?;
            self.require_local_not_moved(*index, state, line)?;
            return Ok(state.clone());
        }
        if let Expr::Call(index, args) = inner
            && let Some((root_slot, field_key)) = self.extract_moved_field_access(*index, args)
        {
            let out = self.analyze_args(args, state, line)?;
            self.require_field_available(root_slot, &field_key, &out, line)?;
            return Ok(out);
        }
        self.analyze_expr(inner, state, line)
    }

    fn extract_moved_field_access(
        &self,
        call_index: u16,
        args: &[Expr],
    ) -> Option<(LocalSlot, MovedFieldKey)> {
        if BuiltinFunction::from_call_index(call_index) != Some(BuiltinFunction::Get) {
            return None;
        }
        if args.len() != 2 {
            return None;
        }
        let Expr::Var(root_slot) = args.first()? else {
            return None;
        };
        let key = self.extract_literal_moved_field_key(args.get(1)?)?;
        Some((*root_slot, key))
    }

    fn extract_set_field_write_with_value<'a>(
        &self,
        expr: &'a Expr,
    ) -> Option<(LocalSlot, MovedFieldKey, &'a Expr)> {
        let Expr::Call(index, args) = expr else {
            return None;
        };
        if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Set) {
            return None;
        }
        if args.len() != 3 {
            return None;
        }
        let Expr::Var(root_slot) = args.first()? else {
            return None;
        };
        let key = self.extract_literal_moved_field_key(args.get(1)?)?;
        let value = args.get(2)?;
        Some((*root_slot, key, value))
    }

    fn extract_collection_mutation_root(
        &self,
        call_index: u16,
        args: &[Expr],
    ) -> Option<LocalSlot> {
        let builtin = BuiltinFunction::from_call_index(call_index)?;
        let expected_arity = match builtin {
            BuiltinFunction::Set => 3,
            BuiltinFunction::ArrayPush => 2,
            _ => return None,
        };
        if args.len() != expected_arity {
            return None;
        }
        let Expr::Var(root_slot) = args.first()? else {
            return None;
        };
        Some(*root_slot)
    }

    fn extract_literal_moved_field_key(&self, expr: &Expr) -> Option<MovedFieldKey> {
        match expr {
            Expr::String(value) => Some(MovedFieldKey::String(value.clone())),
            _ => None,
        }
    }

    fn handle_local_rebind_field_moves(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        expr: &Expr,
    ) {
        if let Some((root_slot, key, value_expr)) = self.extract_set_field_write_with_value(expr)
            && root_slot == target
        {
            self.mark_field_available(state, target, &key);
            if self.is_definitely_copyable_expr(value_expr, state) {
                self.mark_copyable_field(state, target, &key);
            } else {
                self.clear_copyable_field(state, target, &key);
            }
            return;
        }

        if let Expr::Var(source) = expr {
            self.copy_local_field_moves(state, *source, target);
            return;
        }

        self.clear_local_field_moves(state, target);
        if let Some(keys) = self.collect_copyable_fields_for_expr(expr, state) {
            self.set_local_copyable_fields(state, target, &keys);
        } else {
            self.clear_local_copyable_fields(state, target);
        }
    }

    fn handle_local_rebind_collection_aliases(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        expr: &Expr,
    ) {
        if let Expr::Var(source) = expr {
            self.copy_local_collection_aliases(state, *source, target);
            return;
        }
        if let Expr::Call(index, args) = expr
            && let Some(param_index) = self.collection_passthrough_params.get(index).copied()
            && let Some(source_expr) = args.get(param_index)
            && self.is_definitely_collection_expr(source_expr, state)
        {
            if let Some(source_slot) = self.extract_collection_alias_local(source_expr) {
                self.copy_local_collection_aliases(state, source_slot, target);
            } else {
                self.set_local_collection_aliases(state, target, self.fresh_collection_aliases());
            }
            return;
        }
        match expr {
            Expr::ToOwned(inner) if self.is_definitely_collection_expr(inner, state) => {
                self.set_local_collection_aliases(state, target, self.fresh_collection_aliases());
                return;
            }
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                if let Expr::Var(source) = inner.as_ref() {
                    self.copy_local_collection_aliases(state, *source, target);
                    return;
                }
                if self.is_definitely_collection_expr(inner, state) {
                    self.set_local_collection_aliases(
                        state,
                        target,
                        self.fresh_collection_aliases(),
                    );
                    return;
                }
            }
            _ => {}
        }
        if self.is_definitely_collection_expr(expr, state) {
            self.set_local_collection_aliases(state, target, self.fresh_collection_aliases());
            return;
        }
        self.clear_local_collection_aliases(state, target);
    }

    fn extract_collection_alias_local(&self, expr: &Expr) -> Option<LocalSlot> {
        match expr {
            Expr::Var(slot) => Some(*slot),
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                if let Expr::Var(slot) = inner.as_ref() {
                    Some(*slot)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn copy_local_field_moves(&self, state: &mut FlowState, source: LocalSlot, target: LocalSlot) {
        if source == target {
            return;
        }
        self.clear_local_field_moves(state, target);
        self.clear_local_copyable_fields(state, target);
        if !self.is_trackable_local(target) {
            return;
        }

        let definite_from_source = state
            .moved_definite
            .iter()
            .filter(|entry| entry.root == source)
            .cloned()
            .collect::<Vec<_>>();
        for mut entry in definite_from_source {
            entry.root = target;
            state.moved_definite.insert(entry);
        }

        let possible_from_source = state
            .moved_possible
            .iter()
            .filter(|entry| entry.root == source)
            .cloned()
            .collect::<Vec<_>>();
        for mut entry in possible_from_source {
            entry.root = target;
            state.moved_possible.insert(entry);
        }

        let copyable_from_source = state
            .copyable_fields
            .iter()
            .filter(|entry| entry.root == source)
            .cloned()
            .collect::<Vec<_>>();
        for mut entry in copyable_from_source {
            entry.root = target;
            state.copyable_fields.insert(entry);
        }
    }

    fn copy_local_collection_aliases(
        &self,
        state: &mut FlowState,
        source: LocalSlot,
        target: LocalSlot,
    ) {
        let source_slot = source as usize;
        let target_slot = target as usize;
        if source_slot >= self.local_count || target_slot >= self.local_count {
            return;
        }
        if source_slot == target_slot {
            return;
        }
        state.collection_aliases[target_slot] = state.collection_aliases[source_slot].clone();
    }

    fn clear_local_field_moves(&self, state: &mut FlowState, target: LocalSlot) {
        state.moved_definite.retain(|entry| entry.root != target);
        state.moved_possible.retain(|entry| entry.root != target);
    }

    fn clear_local_copyable_fields(&self, state: &mut FlowState, target: LocalSlot) {
        state.copyable_fields.retain(|entry| entry.root != target);
    }

    fn clear_local_collection_aliases(&self, state: &mut FlowState, target: LocalSlot) {
        let slot = target as usize;
        if slot < self.local_count {
            state.collection_aliases[slot].clear();
        }
    }

    fn set_local_copyable_fields(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        keys: &HashSet<MovedFieldKey>,
    ) {
        self.clear_local_copyable_fields(state, target);
        if !self.is_trackable_local(target) {
            return;
        }
        for key in keys {
            state.copyable_fields.insert(MovedFieldPath {
                root: target,
                key: key.clone(),
            });
        }
    }

    fn set_local_collection_aliases(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        aliases: HashSet<u32>,
    ) {
        let slot = target as usize;
        if slot < self.local_count {
            state.collection_aliases[slot] = aliases;
        }
    }

    fn fresh_collection_aliases(&self) -> HashSet<u32> {
        let mut out = HashSet::with_capacity(1);
        let alias_id = self.next_collection_alias_id.get();
        let next = alias_id
            .checked_add(1)
            .expect("collection alias id overflow");
        self.next_collection_alias_id.set(next);
        out.insert(alias_id);
        out
    }

    fn mark_field_moved(&self, state: &mut FlowState, root: LocalSlot, key: MovedFieldKey) {
        if !self.is_trackable_local(root) {
            return;
        }
        let path = MovedFieldPath {
            root,
            key: key.clone(),
        };
        state.moved_definite.insert(path);
        state.moved_possible.insert(MovedFieldPath { root, key });
    }

    fn mark_field_available(&self, state: &mut FlowState, root: LocalSlot, key: &MovedFieldKey) {
        let path = MovedFieldPath {
            root,
            key: key.clone(),
        };
        state.moved_definite.remove(&path);
        state.moved_possible.remove(&path);
    }

    fn mark_copyable_field(&self, state: &mut FlowState, root: LocalSlot, key: &MovedFieldKey) {
        if !self.is_trackable_local(root) {
            return;
        }
        state.copyable_fields.insert(MovedFieldPath {
            root,
            key: key.clone(),
        });
    }

    fn clear_copyable_field(&self, state: &mut FlowState, root: LocalSlot, key: &MovedFieldKey) {
        state.copyable_fields.remove(&MovedFieldPath {
            root,
            key: key.clone(),
        });
    }

    fn is_copyable_field(&self, root: LocalSlot, key: &MovedFieldKey, state: &FlowState) -> bool {
        state.copyable_fields.contains(&MovedFieldPath {
            root,
            key: key.clone(),
        })
    }

    fn require_field_available(
        &self,
        root: LocalSlot,
        key: &MovedFieldKey,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        if !self.is_trackable_local(root) {
            return Ok(());
        }
        let path = MovedFieldPath {
            root,
            key: key.clone(),
        };
        if !state.moved_possible.contains(&path) {
            return Ok(());
        }
        let local_name = self
            .local_names
            .get(&root)
            .cloned()
            .unwrap_or_else(|| format!("#{root}"));
        let field_display = self.format_field_display(&local_name, key);
        Err(ParseError {
            span: None,
            code: Some("E_FIELD_MOVED".to_string()),
            line: line as usize,
            message: format!(
                "field '{field_display}' was moved earlier; use '{field_display}.copy()' to copy it before moving"
            ),
        })
    }

    fn require_collection_mutation_permitted(
        &self,
        root: LocalSlot,
        state: &FlowState,
        line: u32,
    ) -> Result<(), ParseError> {
        let root_slot = root as usize;
        if root_slot >= self.local_count {
            return Ok(());
        }
        let root_aliases = &state.collection_aliases[root_slot];
        if root_aliases.is_empty() {
            return Ok(());
        }
        let conflict = (0..self.local_count).find(|other_slot| {
            if *other_slot == root_slot || !state.possible[*other_slot] {
                return false;
            }
            !state.collection_aliases[*other_slot].is_empty()
                && state.collection_aliases[*other_slot]
                    .intersection(root_aliases)
                    .next()
                    .is_some()
        });
        let Some(conflict_slot) = conflict else {
            return Ok(());
        };
        let root_name = self.display_local_name(root);
        let alias_name = self.display_local_name(conflict_slot as LocalSlot);
        Err(ParseError {
            span: None,
            code: Some("E_MUTATE_ALIASED_COLLECTION".to_string()),
            line: line as usize,
            message: format!(
                "cannot mutate local '{root_name}' while aliased by '{alias_name}'; detach one side with '.copy()' first"
            ),
        })
    }

    fn format_field_display(&self, local_name: &str, key: &MovedFieldKey) -> String {
        match key {
            MovedFieldKey::String(value) => {
                if is_simple_ident(value) {
                    format!("{local_name}.{value}")
                } else {
                    format!("{local_name}[\"{value}\"]")
                }
            }
        }
    }

    fn is_trackable_local(&self, index: LocalSlot) -> bool {
        (index as usize) < self.local_count
    }

    fn display_local_name(&self, index: LocalSlot) -> String {
        self.local_names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("#{index}"))
    }

    fn collect_copyable_fields_for_expr(
        &self,
        expr: &Expr,
        state: &FlowState,
    ) -> Option<HashSet<MovedFieldKey>> {
        let Expr::Call(index, args) = expr else {
            return None;
        };
        let builtin = BuiltinFunction::from_call_index(*index)?;
        match builtin {
            BuiltinFunction::MapNew if args.is_empty() => Some(HashSet::new()),
            BuiltinFunction::Set if args.len() == 3 => {
                let mut keys = self.collect_copyable_fields_for_expr(&args[0], state)?;
                let key = self.extract_literal_moved_field_key(&args[1])?;
                if self.is_definitely_copyable_expr(&args[2], state) {
                    keys.insert(key);
                } else {
                    keys.remove(&key);
                }
                Some(keys)
            }
            _ => None,
        }
    }

    fn is_definitely_copyable_expr(&self, expr: &Expr, state: &FlowState) -> bool {
        match expr {
            // RustScript models string values as move-by-default. String-specific ergonomics
            // (for example `p.a + p.a`) are handled by `analyze_expr_to_owned`.
            Expr::Null | Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) => true,
            Expr::Neg(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => self.is_definitely_copyable_expr(inner, state),
            Expr::Not(_)
            | Expr::And(_, _)
            | Expr::Or(_, _)
            | Expr::Eq(_, _)
            | Expr::Lt(_, _)
            | Expr::Gt(_, _) => true,
            Expr::Add(lhs, rhs)
            | Expr::Sub(lhs, rhs)
            | Expr::Mul(lhs, rhs)
            | Expr::Div(lhs, rhs)
            | Expr::Mod(lhs, rhs) => {
                self.is_definitely_copyable_expr(lhs, state)
                    && self.is_definitely_copyable_expr(rhs, state)
            }
            Expr::Call(index, args) => self
                .extract_moved_field_access(*index, args)
                .map(|(root_slot, field_key)| self.is_copyable_field(root_slot, &field_key, state))
                .unwrap_or(false),
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.is_definitely_copyable_expr(then_expr, state)
                    && self.is_definitely_copyable_expr(else_expr, state)
            }
            Expr::Match { arms, default, .. } => {
                arms.iter()
                    .all(|(_, arm_expr)| self.is_definitely_copyable_expr(arm_expr, state))
                    && self.is_definitely_copyable_expr(default, state)
            }
            Expr::Var(index) => state
                .copyable_locals
                .get(*index as usize)
                .copied()
                .unwrap_or(false),
            _ => false,
        }
    }

    fn is_definitely_collection_expr(&self, expr: &Expr, state: &FlowState) -> bool {
        match expr {
            Expr::Var(index) => state
                .collection_aliases
                .get(*index as usize)
                .is_some_and(|aliases| !aliases.is_empty()),
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.is_definitely_collection_expr(inner, state)
            }
            Expr::Call(index, args) => match BuiltinFunction::from_call_index(*index) {
                Some(BuiltinFunction::MapNew) => args.is_empty(),
                Some(BuiltinFunction::ArrayNew) => args.is_empty(),
                Some(BuiltinFunction::Set) if args.len() == 3 => {
                    self.is_definitely_collection_expr(&args[0], state)
                }
                Some(BuiltinFunction::ArrayPush) if args.len() == 2 => {
                    self.is_definitely_collection_expr(&args[0], state)
                }
                _ => false,
            },
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.is_definitely_collection_expr(then_expr, state)
                    && self.is_definitely_collection_expr(else_expr, state)
            }
            Expr::Match { arms, default, .. } => {
                arms.iter()
                    .all(|(_, arm_expr)| self.is_definitely_collection_expr(arm_expr, state))
                    && self.is_definitely_collection_expr(default, state)
            }
            Expr::Block { expr, .. } => self.is_definitely_collection_expr(expr, state),
            _ => false,
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
            self.require_local_not_moved(*source_slot, state, line)?;
        }

        let mut closure_state = FlowState::reachable(self.local_count);
        for slot in &closure.param_slots {
            self.mark_available(&mut closure_state, *slot, line)?;
        }
        for (source_slot, captured_slot) in &closure.capture_copies {
            self.mark_available(&mut closure_state, *captured_slot, line)?;
            let source_idx = *source_slot as usize;
            let captured_idx = *captured_slot as usize;
            if source_idx < self.local_count && captured_idx < self.local_count {
                closure_state.copyable_locals[captured_idx] = state.copyable_locals[source_idx];
                closure_state.movable_locals[captured_idx] = state.movable_locals[source_idx];
                closure_state.collection_aliases[captured_idx] =
                    state.collection_aliases[source_idx].clone();
                closure_state.moved_local_definite[captured_idx] =
                    state.moved_local_definite[source_idx];
                closure_state.moved_local_possible[captured_idx] =
                    state.moved_local_possible[source_idx];
            }
            for path in state
                .moved_definite
                .iter()
                .filter(|path| path.root == *source_slot)
            {
                closure_state.moved_definite.insert(MovedFieldPath {
                    root: *captured_slot,
                    key: path.key.clone(),
                });
            }
            for path in state
                .moved_possible
                .iter()
                .filter(|path| path.root == *source_slot)
            {
                closure_state.moved_possible.insert(MovedFieldPath {
                    root: *captured_slot,
                    key: path.key.clone(),
                });
            }
            for path in state
                .copyable_fields
                .iter()
                .filter(|path| path.root == *source_slot)
            {
                closure_state.copyable_fields.insert(MovedFieldPath {
                    root: *captured_slot,
                    key: path.key.clone(),
                });
            }
        }
        self.analyze_expr(&closure.body, &closure_state, line)?;
        Ok(())
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

    fn mark_local_source_moved_on_rebind(
        &self,
        state: &mut FlowState,
        target: LocalSlot,
        expr: &Expr,
    ) {
        let Expr::Var(source) = expr else {
            return;
        };
        if *source == target {
            return;
        }
        if self.should_move_local_on_rebind_source(*source, state) {
            self.mark_local_moved(state, *source);
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
        match expr {
            Expr::String(_) => true,
            Expr::Var(index) => state
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

type LiveSet = Vec<bool>;

#[derive(Clone, Copy)]
struct DefInfo {
    slot: LocalSlot,
    explicit_null: bool,
}

struct LivenessRewriter {
    local_count: usize,
    clearable_slots: Vec<bool>,
}

impl LivenessRewriter {
    fn new(local_count: usize, local_bindings: &[(String, LocalSlot)]) -> Self {
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
        | Stmt::Continue { line } => *line,
    }
}

struct LocalSlotAllocator {
    local_count: usize,
    liveness: LivenessRewriter,
    function_impls: HashMap<u16, FunctionImpl>,
    adjacency: Vec<HashSet<usize>>,
    function_footprint_cache: HashMap<u16, LiveSet>,
    full_footprint: LiveSet,
}

impl LocalSlotAllocator {
    fn new(
        local_count: usize,
        local_bindings: &[(String, LocalSlot)],
        function_impls: &HashMap<u16, FunctionImpl>,
    ) -> Self {
        let liveness = LivenessRewriter::new(local_count, local_bindings);
        Self {
            local_count,
            liveness,
            function_impls: function_impls.clone(),
            adjacency: (0..local_count).map(|_| HashSet::new()).collect(),
            function_footprint_cache: HashMap::new(),
            full_footprint: vec![true; local_count],
        }
    }

    fn allocate(mut self, mut ir: FrontendIr) -> Result<FrontendIr, ParseError> {
        let live_out = self.liveness.empty_set();
        let _ = self.collect_block(&ir.stmts, &live_out)?;
        for function_impl in ir.function_impls.values() {
            let live_after = self.liveness.uses_expr(&function_impl.body_expr);
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
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
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
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::FunctionRef(_) => {}
            Expr::Var(index) => {
                self.add_slot_live_edges(*index, live);
            }
            Expr::Call(index, args) => {
                for arg in args {
                    self.collect_expr_constraints(arg, live)?;
                }
                if self.function_impls.contains_key(index) {
                    let mut stack = Vec::new();
                    let footprint = self.function_footprint(*index, &mut stack);
                    self.add_cross_live_with_set(live, &footprint);
                }
            }
            Expr::LocalCall(index, args) => {
                self.add_slot_live_edges(*index, live);
                for arg in args {
                    self.collect_expr_constraints(arg, live)?;
                }
                let full_footprint = self.full_footprint.clone();
                self.add_cross_live_with_set(live, &full_footprint);
            }
            Expr::Closure(_closure) => {}
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.collect_expr_constraints(arg, live)?;
                }
                let mut stack = Vec::new();
                let footprint = self.closure_footprint(closure, &mut stack);
                self.add_cross_live_with_set(live, &footprint);
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
                self.collect_expr_constraints(lhs, live)?;
                self.collect_expr_constraints(rhs, live)?;
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => {
                self.collect_expr_constraints(inner, live)?;
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.collect_expr_constraints(condition, live)?;
                self.collect_expr_constraints(then_expr, live)?;
                self.collect_expr_constraints(else_expr, live)?;
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                self.add_slot_live_edges(*value_slot, live);
                self.add_slot_live_edges(*result_slot, live);
                self.collect_expr_constraints(value, live)?;
                for (_, arm_expr) in arms {
                    self.collect_expr_constraints(arm_expr, live)?;
                }
                self.collect_expr_constraints(default, live)?;
            }
            Expr::Block { stmts, expr } => {
                self.collect_expr_constraints(expr, live)?;
                let mut block_live_out = live.clone();
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
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
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
            | Expr::String(_)
            | Expr::FunctionRef(_) => {}
            Expr::Var(index) | Expr::LocalCall(index, _) => self.mark_set_slot(set, *index),
            Expr::Call(index, args) => {
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
                for (_, arm_expr) in arms {
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
            Stmt::Let { index, .. } | Stmt::Assign { index, .. } => {
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
        Expr::Null | Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::String(_) => {}
        Expr::FunctionRef(_) => {}
        Expr::Call(_, args) => {
            for arg in args {
                remap_expr_slots(arg, mapping)?;
            }
        }
        Expr::LocalCall(index, args) => {
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
        Expr::Var(index) => {
            *index = remap_slot(*index, mapping)?;
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
            for (_, arm_expr) in arms {
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
