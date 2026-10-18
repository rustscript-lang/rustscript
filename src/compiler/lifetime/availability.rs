use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;
use crate::bytecode::CaptureBindingMode;
use crate::host_api::HostParamPassing;

use super::super::ParseError;
use super::super::ir::{
    ClosureExpr, Expr, FrontendIr, FunctionImpl, LocalSlot, ResolvedHostCall, Stmt, TypeSchema,
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
    /// Current-invocation owners. A resource schema bit alone is not ownership:
    /// borrowed parameters and persistent captures stay false until an explicit
    /// move/detach creates a fresh current-owned local.
    current_owned: Vec<bool>,
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
            current_owned: vec![false; local_count],
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
            // Entry seeds are caller-owned until this invocation explicitly
            // detaches them. Do not infer ownership from the schema bit.
            state.current_owned[slot] = false;
        }
        state
    }
}

pub(super) fn enforce_local_availability(
    ir: FrontendIr,
    entry_locals: &[EntryLocalAvailability],
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
    owned_local_slots: &[bool],
) -> Result<FrontendIr, ParseError> {
    enforce_source_availability(
        ir,
        entry_locals,
        clear_dead_locals,
        enable_local_move_semantics,
        owned_local_slots,
        None,
        None,
        &[],
    )
}

#[allow(clippy::too_many_arguments)] // Source policy and admission provenance are independent inputs.
pub(super) fn enforce_source_availability(
    mut ir: FrontendIr,
    entry_locals: &[EntryLocalAvailability],
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
    owned_local_slots: &[bool],
    source_policies: Option<&HashMap<String, bool>>,
    sources: Option<&super::super::source_map::SourceMap>,
    rejections: &[super::super::typing::CanonicalHostRejection],
) -> Result<FrontendIr, ParseError> {
    // Pad the post-legalize ownership metadata to the analyzer's local space.
    // Availability runs pre-compaction, so the logical slot indices align with
    // the schemas the typing pass recorded.
    let mut owned_slots = vec![false; ir.locals];
    for (slot, is_owned) in owned_local_slots.iter().enumerate().take(ir.locals) {
        owned_slots[slot] = *is_owned;
    }
    let attach_owner = |mut error: ParseError, source: Option<&str>| {
        if let Some(sources) = sources {
            let id = source
                .and_then(|name| sources.source_id_by_name(name))
                .unwrap_or(0);
            if error.span.is_none() {
                error.span = ir.parsed_semantic_index.as_ref().and_then(|index| {
                    index
                        .stmt_spans
                        .iter()
                        .find(|site| site.line as usize == error.line && site.span.source_id == id)
                        .map(|site| site.span)
                });
            }
            error = error.with_line_span_from_source(sources, id);
        }
        error
    };
    let initial_impls = std::mem::take(&mut ir.function_impls);

    let mut rewritten_impls = HashMap::with_capacity(initial_impls.len());
    for (index, function_impl) in &initial_impls {
        let policy = source_policies
            .and_then(|policies| {
                ir.function_sources
                    .get(index)
                    .and_then(|source| policies.get(source))
            })
            .copied()
            .unwrap_or(enable_local_move_semantics);
        let analyzer = AvailabilityAnalyzer::new(
            ir.locals,
            &ir.local_bindings,
            &initial_impls,
            policy,
            &owned_slots,
        )
        .with_rejections(
            rejections,
            ir.parsed_semantic_index.as_ref(),
            sources.and_then(|sources| {
                ir.function_sources
                    .get(index)
                    .and_then(|source| sources.source_id_by_name(source))
            }),
        );
        let rewritten = analyzer
            .analyze_function_impl(*index, function_impl.clone())
            .map_err(|error| {
                attach_owner(error, ir.function_sources.get(index).map(String::as_str))
            })?;
        rewritten_impls.insert(*index, rewritten);
    }

    let mut state = FlowState::reachable_with_entry_locals(ir.locals, entry_locals);
    let mut rewritten_stmts = Vec::new();
    let mut rewritten_sources = Vec::new();
    let mut start = 0;
    while start < ir.stmts.len() {
        let source = ir
            .stmt_sources
            .get(start)
            .and_then(|source| source.as_deref());
        let mut end = start + 1;
        while end < ir.stmts.len()
            && ir
                .stmt_sources
                .get(end)
                .and_then(|source| source.as_deref())
                == source
        {
            end += 1;
        }
        let policy = source_policies
            .and_then(|policies| source.and_then(|source| policies.get(source)))
            .copied()
            .unwrap_or(enable_local_move_semantics);
        let analyzer = AvailabilityAnalyzer::new(
            ir.locals,
            &ir.local_bindings,
            &rewritten_impls,
            policy,
            &owned_slots,
        )
        .with_rejections(
            rejections,
            ir.parsed_semantic_index.as_ref(),
            sources.and_then(|sources| source.and_then(|source| sources.source_id_by_name(source))),
        );
        let (stmts, next) = analyzer
            .analyze_block(&ir.stmts[start..end], state, true)
            .map_err(|error| attach_owner(error, source))?;
        rewritten_sources.extend(std::iter::repeat_n(source.map(str::to_owned), stmts.len()));
        rewritten_stmts.extend(stmts);
        state = next;
        start = end;
    }
    ir.stmts = rewritten_stmts;
    ir.stmt_sources = rewritten_sources;
    ir.function_impls = rewritten_impls;

    if clear_dead_locals {
        let liveness = LivenessRewriter::new(
            ir.locals,
            &ir.local_bindings,
            &ir.function_impls,
            &owned_slots,
        );
        let persistent_slots = persistent_capture_slots(&ir.stmts, &ir.function_impls);
        (ir.stmts, ir.stmt_sources) =
            liveness.rewrite_program_block_with_sources(&ir.stmts, &ir.stmt_sources);
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
pub(crate) fn allocate_local_slots(ir: FrontendIr) -> Result<FrontendIr, ParseError> {
    allocate_local_slots_with_root_slots(ir, &[]).map(|allocation| allocation.ir)
}

pub(in crate::compiler) fn allocate_local_slots_with_root_slots(
    ir: FrontendIr,
    root_slots: &[LocalSlot],
) -> Result<super::LocalSlotAllocation, ParseError> {
    if ir.locals > LOCAL_SLOT_ALLOCATOR_COMPAT_THRESHOLD {
        let allocator = LocalSlotAllocator::new(ir.locals, &ir.local_bindings, &ir.function_impls);
        allocator.allocate(ir, root_slots)
    } else {
        // Tiny programs retain their original layout, including the entry seeds.
        let mapping = (0..ir.locals).map(|slot| slot as LocalSlot).collect();
        Ok(super::LocalSlotAllocation { ir, mapping })
    }
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
    rejections: Vec<super::super::typing::CanonicalHostRejection>,
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
            rejections: Vec::new(),
        }
    }

    fn with_rejections(
        mut self,
        rejections: &[super::super::typing::CanonicalHostRejection],
        parsed: Option<&super::super::ir::ParsedSemanticIndex>,
        source: Option<super::super::source_map::SourceId>,
    ) -> Self {
        if let (Some(parsed), Some(source)) = (parsed, source) {
            self.rejections = rejections
                .iter()
                .filter(|rejection| {
                    let site = &rejection.provenance;
                    let reason_in_range = match rejection.reason {
                        Some(
                            super::super::host_call_resolve::ArgumentRejection::Schema { argument }
                            | super::super::host_call_resolve::ArgumentRejection::Passing {
                                argument,
                                ..
                            },
                        ) => argument < site.arguments.len(),
                        None => true,
                    };
                    reason_in_range
                        && site.callee_span.source_id == source
                        && parsed
                            .call_arguments
                            .iter()
                            .filter(|args| *args == site)
                            .count()
                            == 1
                        && parsed
                            .call_sites
                            .iter()
                            .filter(|call| {
                                call.id == site.call_id
                                    && call.callee_span == site.callee_span
                                    && call.expr_span == site.expr_span
                            })
                            .count()
                            == 1
                })
                .cloned()
                .collect();
        }
        self
    }

    /// Only parser-proven direct wrappers of this recorded failed host call
    /// participate. These are admission facts, never runtime passing modes.
    fn analyze_rejected_arg(
        &self,
        id: Option<super::super::ir::SemanticNodeId>,
        position: usize,
        arg: &Expr,
        state: &FlowState,
        line: u32,
    ) -> Option<Result<FlowState, ParseError>> {
        use super::super::host_call_resolve::ArgumentRejection;
        use super::super::ir::ParsedArgumentShape;
        let mut matching = self
            .rejections
            .iter()
            .filter(|rejection| Some(rejection.provenance.call_id) == id);
        let rejection = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let site = rejection.provenance.arguments.get(position)?;
        let (shape, inner) = match arg {
            Expr::Borrow(inner) => (ParsedArgumentShape::Borrow, inner.as_ref()),
            Expr::BorrowMut(inner) => (ParsedArgumentShape::BorrowMut, inner.as_ref()),
            Expr::ToOwned(inner) => (ParsedArgumentShape::Copy, inner.as_ref()),
            _ => return None,
        };
        if shape != site.shape
            || site.span.source_id != rejection.provenance.callee_span.source_id
            || site.span.lo < rejection.provenance.callee_span.hi
            || site.span.hi >= rejection.provenance.expr_span.hi
        {
            return None;
        }
        let local = match inner {
            Expr::Var(slot) | Expr::MoveVar(slot) => Some(*slot),
            _ => None,
        };
        if local != site.local {
            return None;
        }
        match shape {
            ParsedArgumentShape::Borrow | ParsedArgumentShape::BorrowMut => {
                Some(self.analyze_expr_to_owned(inner, state, line))
            }
            ParsedArgumentShape::Copy
                if matches!(rejection.reason,
                Some(ArgumentRejection::Passing { argument, expected: HostParamPassing::TakeOwned | HostParamPassing::Borrow | HostParamPassing::BorrowMut,
                    actual: HostParamPassing::Value }) if argument == position) =>
            {
                let slot = local?;
                if !self.is_owned_slot(slot) {
                    return None;
                }
                // Same invalid copy argument owns the canonical passing error,
                // including an already moved local. Preserve state unchanged;
                // all other arguments and subsequent uses still run normally.
                Some(
                    self.require_available(slot, state, line)
                        .map(|()| state.clone()),
                )
            }
            _ => None,
        }
    }

    fn analyze_function_impl(
        &self,
        function_index: u16,
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
        let consumed = self.function_consumed_params.get(&function_index);
        for (position, slot) in param_slots.iter().enumerate() {
            self.mark_available(&mut state, *slot, 1)?;
            // Resource-typed parameters are move-only inside the body: they
            // can be returned (moved out), passed by ownership, or borrowed
            // through exact host-call arguments, but never copied.
            // Ownership for exit cleanup follows the existing consumed-param
            // contract, not the schema bit. Borrowed params stay caller-owned.
            if self.is_owned_slot(*slot) {
                state.copyable_locals[*slot as usize] = false;
                state.movable_locals[*slot as usize] = true;
                if consumed.is_some_and(|positions| positions.contains(&position)) {
                    state.current_owned[*slot as usize] = true;
                }
            }
        }
        for (_, captured_slot) in &capture_copies {
            self.mark_available(&mut state, *captured_slot, 1)?;
            // Persistent environment captures are not this invocation's
            // owners, even when the environment was created by Move.
            if self.is_owned_slot(*captured_slot) {
                state.copyable_locals[*captured_slot as usize] = false;
                state.movable_locals[*captured_slot as usize] = true;
                state.current_owned[*captured_slot as usize] = false;
            }
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
        self.rewrite_expr_for_ownership_with_state(expr, state, 1)
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
                            true,
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
                    self.note_rebind_ownership(&mut out, *index, expr);
                    rewritten_expr =
                        self.rewrite_local_source_move_on_rebind(&mut out, *index, expr);
                    rewritten_expr = self.rewrite_expr_for_ownership_with_state(
                        &rewritten_expr,
                        &initializer_state,
                        *line,
                    )?;
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
                    self.note_rebind_ownership(&mut out, *index, expr);
                    rewritten_expr =
                        self.rewrite_local_source_move_on_rebind(&mut out, *index, expr);
                    rewritten_expr =
                        self.rewrite_expr_for_ownership_with_state(&rewritten_expr, &state, *line)?;
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
            Stmt::ClosureLet { line, closure, .. } => {
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
                            true,
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
                let rewritten_expr =
                    self.rewrite_expr_for_ownership_with_state(expr, &state, *line)?;
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
                let (condition, cond_state) =
                    self.rewrite_expr_and_state(condition, false, &state, *line)?;
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
                    condition,
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
        let (condition, cond_state) =
            self.rewrite_expr_and_state(condition, false, &loop_entry, line)?;
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
            condition,
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
        let (condition, cond_state) =
            self.rewrite_expr_and_state(condition, false, &loop_entry, line)?;
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
            condition,
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
            Expr::OptionalGet { .. } => self.analyze_optional_get(expr, state, line, false),
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
                semantic_id: _,
            } => {
                let mut value_state = self.analyze_expr(value, state, line)?;
                self.mark_owned_value_reads_moved(value, &mut value_state);
                self.mark_available(&mut value_state, *value_slot, line)?;
                let mut fallback_state = self.analyze_expr(fallback, &value_state, line)?;
                self.mark_owned_value_reads_moved(fallback, &mut fallback_state);
                Ok(self.merge_states(fallback_state, value_state))
            }
            // Resolved module calls (pre-merge only) analyze their arguments;
            // interprocedural effects apply to the post-merge flat call.
            Expr::ModuleCall(_, _, args, _) => self.analyze_args(args, state, line),
            Expr::Call(index, _, args, resolution, semantic_id) => {
                if resolution.is_none()
                    && self
                        .rejections
                        .iter()
                        .any(|rejection| Some(rejection.provenance.call_id) == *semantic_id)
                {
                    let mut out = state.clone();
                    for (position, arg) in args.iter().enumerate() {
                        out = match self.analyze_rejected_arg(
                            *semantic_id,
                            position,
                            arg,
                            &out,
                            line,
                        ) {
                            Some(result) => result?,
                            None => self.analyze_expr(arg, &out, line)?,
                        };
                    }
                    return Ok(out);
                }
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
            Expr::LocalCall(index, _, args, _) => {
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
                        true,
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
                        false,
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
            | Expr::Eq(lhs, rhs)
            | Expr::Lt(lhs, rhs)
            | Expr::Gt(lhs, rhs) => {
                let lhs_state = self.analyze_expr(lhs, state, line)?;
                self.analyze_expr(rhs, &lhs_state, line)
            }
            Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
                let lhs_state = self.analyze_expr(lhs, state, line)?;
                let rhs_state = self.analyze_expr(rhs, &lhs_state, line)?;
                // The RHS transfers apply only on its executed edge.
                Ok(self.merge_states(lhs_state, rhs_state))
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
                self.mark_owned_value_reads_moved(value, &mut value_state);
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
            Expr::ResultConstruct { payload, .. } => {
                let mut out = self.analyze_expr(payload, state, line)?;
                self.mark_owned_value_reads_moved(payload, &mut out);
                Ok(out)
            }
            Expr::ResultQuery { value, .. } => self.analyze_expr_to_owned(value, state, line),
            Expr::ResultTry {
                value, value_slot, ..
            } => {
                let mut out = self.analyze_expr(value, state, line)?;
                self.mark_owned_value_reads_moved(value, &mut out);
                self.mark_available(&mut out, *value_slot, line)?;
                Ok(out)
            }
            Expr::ResultMatch {
                value,
                value_slot,
                ok_binding,
                ok_expr,
                err_binding,
                err_expr,
                ..
            } => {
                let (ok_in, err_in) = self.result_match_arm_states(
                    value,
                    *value_slot,
                    *ok_binding,
                    *err_binding,
                    state,
                    line,
                )?;
                let ok_state = self.analyze_expr(ok_expr, &ok_in, line)?;
                let err_state = self.analyze_expr(err_expr, &err_in, line)?;
                let mut out = self.merge_states(ok_state, err_state);
                self.mark_owned_value_reads_moved(ok_expr, &mut out);
                self.mark_owned_value_reads_moved(err_expr, &mut out);
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

    fn analyze_optional_get(
        &self,
        expr: &Expr,
        state: &FlowState,
        line: u32,
        copy_projection: bool,
    ) -> Result<FlowState, ParseError> {
        let Expr::OptionalGet {
            field_projection,
            container,
            key,
            container_slot,
            key_slot,
            ..
        } = expr
        else {
            unreachable!("optional lookup expression");
        };
        let mut out = if *field_projection {
            let Some((root, field_key)) = self.extract_optional_field_projection(expr) else {
                return Err(ParseError {
                    span: None,
                    code: Some("E_OPTIONAL_FIELD_PROJECTION".to_string()),
                    line: line as usize,
                    message: "optional field projection requires a direct local and a literal key"
                        .to_string(),
                });
            };
            let mut out = self.analyze_projection_root_expr(container, state, line)?;
            out = self.analyze_expr(key, &out, line)?;
            self.require_field_available(root, &field_key, &out, line)?;
            if self.enable_local_move_semantics
                && !copy_projection
                && !self.is_copyable_field(root, &field_key, &out)
            {
                self.mark_field_moved(&mut out, root, field_key);
            }
            out
        } else {
            let mut out = self.analyze_expr(container, state, line)?;
            self.mark_owned_value_reads_moved(container, &mut out);
            self.analyze_expr(key, &out, line)?
        };
        self.mark_available(&mut out, *container_slot, line)?;
        self.mark_available(&mut out, *key_slot, line)?;
        Ok(out)
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
        if self.extract_optional_field_projection(inner).is_some() {
            return self.analyze_optional_get(inner, state, line, true);
        }
        self.analyze_expr(inner, state, line)
    }

    /// Whether this slot is a current-invocation owner that Err cleanup may close.
    /// The resource schema bit is necessary but not sufficient.
    fn is_current_cleanup_owner(&self, slot: LocalSlot, state: &FlowState) -> bool {
        self.is_owned_slot(slot)
            && state
                .current_owned
                .get(slot as usize)
                .copied()
                .unwrap_or(false)
    }

    /// A rebind establishes current ownership when it detaches an owned value
    /// into this local or materializes a fresh owned result. A bare read of a
    /// caller-owned local does not.
    fn note_rebind_ownership(&self, state: &mut FlowState, target: LocalSlot, expr: &Expr) {
        let slot = target as usize;
        if slot >= self.local_count {
            return;
        }
        state.current_owned[slot] =
            self.is_owned_slot(target) && self.expr_establishes_current_ownership(expr, state);
    }

    fn expr_establishes_current_ownership(&self, expr: &Expr, state: &FlowState) -> bool {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                state
                    .current_owned
                    .get(*slot as usize)
                    .copied()
                    .unwrap_or(false)
                    || self.should_move_local_on_rebind_source(*slot, state)
            }
            Expr::Block { expr, .. } => self.expr_establishes_current_ownership(expr, state),
            Expr::Borrow(_) | Expr::BorrowMut(_) => false,
            _ => true,
        }
    }

    /// Arm entry states shared by analysis and ownership rewrite. Payload
    /// bindings receive the same current-ownership fact in both.
    fn result_match_arm_states(
        &self,
        value: &Expr,
        value_slot: LocalSlot,
        ok_binding: Option<LocalSlot>,
        err_binding: Option<LocalSlot>,
        state: &FlowState,
        line: u32,
    ) -> Result<(FlowState, FlowState), ParseError> {
        let mut value_state = self.analyze_expr(value, state, line)?;
        self.mark_owned_value_reads_moved(value, &mut value_state);
        self.mark_available(&mut value_state, value_slot, line)?;
        let mut ok_in = value_state.clone();
        if let Some(slot) = ok_binding {
            self.mark_available(&mut ok_in, slot, line)?;
            // Each selected arm installs a fresh payload, including on a
            // backedge after the previous iteration transferred that binding.
            self.clear_local_moved_state(&mut ok_in, slot);
            self.note_result_payload_owner(&mut ok_in, slot, value);
        }
        let mut err_in = value_state;
        if let Some(slot) = err_binding {
            self.mark_available(&mut err_in, slot, line)?;
            self.clear_local_moved_state(&mut err_in, slot);
            self.note_result_payload_owner(&mut err_in, slot, value);
        }
        Ok((ok_in, err_in))
    }

    fn note_result_payload_owner(
        &self,
        state: &mut FlowState,
        binding: LocalSlot,
        scrutinee: &Expr,
    ) {
        let index = binding as usize;
        if index >= self.local_count {
            return;
        }
        // This binding's post-legalize projection already resolves Named
        // bodies. A declaration-blind schema walk would lose non-generic
        // resource holders. Runtime ownership still comes from the scrutinee.
        state.current_owned[index] = self.is_owned_slot(binding)
            && self.expr_establishes_current_ownership(scrutinee, state);
    }

    /// Whether a logical local slot carries a resource anywhere in its
    /// post-legalize schema (direct or nested).
    fn is_owned_slot(&self, index: LocalSlot) -> bool {
        self.owned_local_slots
            .get(index as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Frame-owned locals that may still be present when `?` takes the Err
    /// return. The try's own operand temps are excluded so the returned Err
    /// carrier is not closed; legitimate per-edge absence is Optional.
    fn err_exit_cleanup_slots(
        &self,
        state: &FlowState,
        exclude: &[LocalSlot],
    ) -> Vec<(LocalSlot, TypeSchema)> {
        let mut slots = Vec::new();
        if !state.reachable {
            return slots;
        }
        for slot in 0..self.local_count {
            let slot = slot as LocalSlot;
            if exclude.contains(&slot) || !self.is_current_cleanup_owner(slot, state) {
                continue;
            }
            let index = slot as usize;
            if !state.possible.get(index).copied().unwrap_or(false) {
                continue;
            }
            if state
                .moved_local_definite
                .get(index)
                .copied()
                .unwrap_or(false)
            {
                continue;
            }
            // Whole-slot MoveVar detaches to Null. Preserve provenance across
            // the join, permitting absence only on this Err edge when a
            // predecessor moved this owner or never initialized a branch-local
            // owner. Schema attachment preserves this per-edge Optional view.
            let schema = if state.moved_local_possible[index] || !state.definite[index] {
                TypeSchema::Optional(Box::new(TypeSchema::Unknown))
            } else {
                TypeSchema::Unknown
            };
            slots.push((slot, schema));
        }
        slots
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
            out =
                self.analyze_call_arg(arg, resolution.passing.get(position).copied(), &out, line)?;
        }
        Ok(out)
    }

    /// Shared exact passing-mode transfer for analysis and lexical rewrite.
    fn analyze_call_arg(
        &self,
        arg: &Expr,
        passing: Option<HostParamPassing>,
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        match passing {
            Some(HostParamPassing::TakeOwned) => self.legalize_take_owned_arg(arg, state, line),
            Some(HostParamPassing::Borrow | HostParamPassing::BorrowMut) => match arg {
                Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                    self.analyze_expr_to_owned(inner, state, line)
                }
                other => self.analyze_expr_to_owned(other, state, line),
            },
            _ => self.analyze_expr(arg, state, line),
        }
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
            Expr::MoveField { root, key } => {
                if self.is_owned_slot(*root) {
                    self.mark_field_moved(state, *root, MovedFieldKey::String(key.clone()));
                }
            }
            Expr::MoveIndex { root, index } => {
                if self.is_owned_slot(*root) {
                    self.mark_field_moved(state, *root, MovedFieldKey::Index(*index));
                }
            }
            Expr::OptionalGet { container, .. } => {
                if let Some((root, key)) = self.extract_optional_field_projection(expr) {
                    if !self.is_copyable_field(root, &key, state) {
                        self.mark_field_moved(state, root, key);
                    }
                } else {
                    self.mark_owned_value_reads_moved(container, state);
                }
            }
            Expr::OptionUnwrapOr {
                value: container, ..
            } => {
                self.mark_owned_value_reads_moved(container, state);
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
                field_projection: _,
                container,
                key,
                container_slot,
                key_slot,
                semantic_id: _,
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
                semantic_id: _,
            } => {
                self.is_owned_slot(*value_slot)
                    || self.expr_contains_owned_local(value)
                    || self.expr_contains_owned_local(fallback)
            }
            Expr::Call(_, _, args, _, _)
            | Expr::LocalCall(_, _, args, _)
            | Expr::ModuleCall(_, _, args, _) => {
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
            other @ (Expr::ResultConstruct { .. }
            | Expr::ResultQuery { .. }
            | Expr::ResultMatch { .. }
            | Expr::ResultTry { .. }) => {
                let mut found = false;
                other.visit_result_children(|child| found |= self.expr_contains_owned_local(child));
                found
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

    /// Replays ownership analysis while rebuilding nested expression blocks so
    /// state-dependent projection and rebind rewrites are retained.
    fn rewrite_expr_for_ownership_with_state(
        &self,
        expr: &Expr,
        state: &FlowState,
        line: u32,
    ) -> Result<Expr, ParseError> {
        match expr {
            Expr::Block { stmts, expr } => {
                let (rewritten_stmts, block_state) =
                    self.analyze_block(stmts, state.clone(), false)?;
                let rewritten_expr =
                    self.rewrite_expr_for_ownership_with_state(expr, &block_state, line)?;
                Ok(Expr::Block {
                    stmts: rewritten_stmts,
                    expr: Box::new(rewritten_expr),
                })
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                let (condition, condition_state) =
                    self.rewrite_expr_and_state(condition, false, state, line)?;
                Ok(Expr::IfElse {
                    condition: Box::new(condition),
                    then_expr: Box::new(self.rewrite_expr_for_ownership_with_state(
                        then_expr,
                        &condition_state,
                        line,
                    )?),
                    else_expr: Box::new(self.rewrite_expr_for_ownership_with_state(
                        else_expr,
                        &condition_state,
                        line,
                    )?),
                })
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                let (rewritten_value, mut value_state) =
                    self.rewrite_expr_and_state(value, false, state, line)?;
                self.mark_owned_value_reads_moved(&rewritten_value, &mut value_state);
                self.mark_available(&mut value_state, *value_slot, line)?;
                let mut rewritten_arms = Vec::with_capacity(arms.len());
                for (pattern, arm_expr) in arms {
                    let mut arm_state = value_state.clone();
                    if let Some(binding_slot) = pattern.binding_slot() {
                        self.mark_available(&mut arm_state, binding_slot, line)?;
                    }
                    rewritten_arms.push((
                        pattern.clone(),
                        self.rewrite_expr_for_ownership_with_state(arm_expr, &arm_state, line)?,
                    ));
                }
                let rewritten_default =
                    self.rewrite_expr_for_ownership_with_state(default, &value_state, line)?;
                Ok(Expr::Match {
                    value_slot: *value_slot,
                    result_slot: *result_slot,
                    value: Box::new(rewritten_value),
                    arms: rewritten_arms,
                    default: Box::new(rewritten_default),
                })
            }
            _ => {
                let rewritten = self.rewrite_expr_for_ownership(expr, state)?;
                Ok(self.rewrite_runtime_field_move_expr(&rewritten, state))
            }
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
    fn rewrite_expr_for_ownership(
        &self,
        expr: &Expr,
        state: &FlowState,
    ) -> Result<Expr, ParseError> {
        self.rewrite_expr_ownership_inner(expr, false, state)
    }

    /// Pair emitted ownership syntax with its continuation facts. Replaying
    /// transfer rules is metadata work only; no runtime expression is copied.
    /// Use the same alias seed for rewrite and analysis, so nested replay does
    /// not spend a second set of alias IDs or change fixed-point convergence.
    fn rewrite_expr_and_state(
        &self,
        expr: &Expr,
        in_call_arg: bool,
        state: &FlowState,
        line: u32,
    ) -> Result<(Expr, FlowState), ParseError> {
        let seed = self.next_collection_alias_id.get();
        let rewritten = self.rewrite_expr_ownership_inner(expr, in_call_arg, state);
        self.next_collection_alias_id.set(seed);
        let rewritten = rewritten?;
        let out = self.analyze_expr(&rewritten, state, line)?;
        Ok((rewritten, out))
    }

    fn rewrite_expr_ownership_inner(
        &self,
        expr: &Expr,
        in_call_arg: bool,
        state: &FlowState,
    ) -> Result<Expr, ParseError> {
        match expr {
            Expr::Call(index, type_args, args, resolution, source_node_id) => {
                let mut rewritten_args = Vec::with_capacity(args.len());
                let mut arg_state = state.clone();
                for (position, arg) in args.iter().enumerate() {
                    if resolution.is_none()
                        && let Some(result) =
                            self.analyze_rejected_arg(*source_node_id, position, arg, &arg_state, 1)
                    {
                        arg_state = result?;
                        rewritten_args.push(arg.clone());
                        continue;
                    }
                    let seed = self.next_collection_alias_id.get();
                    let mut arg = self.rewrite_expr_ownership_inner(arg, true, &arg_state)?;
                    let passing = resolution
                        .as_deref()
                        .and_then(|r| r.passing.get(position))
                        .copied();
                    match passing {
                        Some(HostParamPassing::TakeOwned) => {
                            arg = self.rewrite_take_owned_arg(&arg)?
                        }
                        Some(HostParamPassing::Borrow | HostParamPassing::BorrowMut) => {
                            arg = self.rewrite_borrow_arg(&arg)
                        }
                        _ => {}
                    }
                    self.next_collection_alias_id.set(seed);
                    arg_state = if resolution.is_none()
                        && position == 0
                        && matches!(
                            BuiltinFunction::from_call_index(*index),
                            Some(BuiltinFunction::Get | BuiltinFunction::Set)
                        ) {
                        // A projection may read an available remaining child
                        // of a partially moved root; it is not a whole read.
                        self.analyze_projection_root_expr(&arg, &arg_state, 1)?
                    } else {
                        self.analyze_call_arg(&arg, passing, &arg_state, 1)?
                    };
                    rewritten_args.push(arg);
                }
                if let Some(resolution) = resolution.as_deref() {
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
                field_projection: true,
                ..
            } => Ok(self.rewrite_runtime_field_move_expr(expr, state)),
            Expr::OptionalGet {
                field_projection,
                container,
                key,
                container_slot,
                key_slot,
                semantic_id,
            } => {
                let (container, mut container_state) =
                    self.rewrite_expr_and_state(container, false, state, 1)?;
                self.mark_owned_value_reads_moved(&container, &mut container_state);
                Ok(Expr::OptionalGet {
                    field_projection: *field_projection,
                    container: Box::new(container),
                    key: Box::new(self.rewrite_expr_ownership_inner(
                        key,
                        false,
                        &container_state,
                    )?),
                    container_slot: *container_slot,
                    key_slot: *key_slot,
                    semantic_id: *semantic_id,
                })
            }
            Expr::OptionUnwrapOr {
                value,
                value_slot,
                fallback,
                semantic_id,
            } => {
                let (value, mut value_state) =
                    self.rewrite_expr_and_state(value, false, state, 1)?;
                self.mark_owned_value_reads_moved(&value, &mut value_state);
                self.mark_available(&mut value_state, *value_slot, 1)?;
                Ok(Expr::OptionUnwrapOr {
                    value: Box::new(value),
                    value_slot: *value_slot,
                    fallback: Box::new(self.rewrite_expr_ownership_inner(
                        fallback,
                        false,
                        &value_state,
                    )?),
                    semantic_id: *semantic_id,
                })
            }
            Expr::LocalCall(index, type_args, args, semantic_id) => Ok(Expr::LocalCall(
                *index,
                type_args.clone(),
                self.rewrite_call_args(args, state)?.0,
                *semantic_id,
            )),
            Expr::ModuleCall(index, type_args, args, semantic_id) => Ok(Expr::ModuleCall(
                *index,
                type_args.clone(),
                self.rewrite_call_args(args, state)?.0,
                *semantic_id,
            )),
            Expr::Closure(closure) => {
                let closure_state = self.closure_flow_state(closure, state, 0)?;
                Ok(Expr::Closure(ClosureExpr {
                    param_slots: closure.param_slots.clone(),
                    capture_copies: closure.capture_copies.clone(),
                    body: Box::new(self.rewrite_expr_ownership_inner(
                        &closure.body,
                        false,
                        &closure_state,
                    )?),
                }))
            }
            Expr::ClosureCall(closure, args) => {
                let (args, arg_state) = self.rewrite_call_args(args, state)?;
                let closure_state = self.closure_flow_state(closure, &arg_state, 0)?;
                Ok(Expr::ClosureCall(
                    ClosureExpr {
                        param_slots: closure.param_slots.clone(),
                        capture_copies: closure.capture_copies.clone(),
                        body: Box::new(self.rewrite_expr_ownership_inner(
                            &closure.body,
                            false,
                            &closure_state,
                        )?),
                    },
                    args,
                ))
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
                let copy = matches!(expr, Expr::Add(..));
                let observation = matches!(
                    expr,
                    Expr::Eq(..) | Expr::Lt(..) | Expr::Gt(..) | Expr::And(..) | Expr::Or(..)
                );
                let (lhs, lhs_state) = if copy {
                    let seed = self.next_collection_alias_id.get();
                    let lhs = self.rewrite_expr_ownership_inner(lhs, true, state)?;
                    self.next_collection_alias_id.set(seed);
                    // Add's existing projection-copy transfer must not become
                    // a consuming Get merely because its RHS needs the state.
                    let out = self.analyze_expr_to_owned(&lhs, state, 1)?;
                    (lhs, out)
                } else {
                    // Only bare observations use the plain-read context.
                    // Nested projections and explicit calls keep their own
                    // consuming transfer rules, matching source analysis.
                    self.rewrite_expr_and_state(
                        lhs,
                        observation && matches!(lhs.as_ref(), Expr::Var(_)),
                        state,
                        1,
                    )?
                };
                let rhs = self.rewrite_expr_ownership_inner(
                    rhs,
                    copy || (observation && matches!(rhs.as_ref(), Expr::Var(_))),
                    &lhs_state,
                )?;
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
                let inner = self.rewrite_expr_ownership_inner(inner, false, state)?;
                Ok(match expr {
                    Expr::Neg(..) => Expr::Neg(Box::new(inner)),
                    _ => Expr::Not(Box::new(inner)),
                })
            }
            Expr::ToOwned(inner) if self.extract_optional_field_projection(inner).is_some() => {
                // An explicit non-resource copy checks availability without
                // removing the projected child from its original record.
                Ok(expr.clone())
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                // Non-resource borrow/copy wrappers are preserved verbatim;
                // resource-bearing ones were already rejected during analysis.
                // The inner read keeps the call-argument context when nested
                // inside one, so a borrow of an owned local in a host-call
                // argument stays a plain read (never a MoveVar).
                let inner = self.rewrite_expr_ownership_inner(inner, in_call_arg, state)?;
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
            } => {
                let (condition, condition_state) =
                    self.rewrite_expr_and_state(condition, false, state, 1)?;
                Ok(Expr::IfElse {
                    condition: Box::new(condition),
                    then_expr: Box::new(self.rewrite_expr_ownership_inner(
                        then_expr,
                        false,
                        &condition_state,
                    )?),
                    else_expr: Box::new(self.rewrite_expr_ownership_inner(
                        else_expr,
                        false,
                        &condition_state,
                    )?),
                })
            }
            Expr::Match { .. } => self.rewrite_expr_for_ownership_with_state(expr, state, 1),
            Expr::ResultConstruct {
                ok,
                payload,
                result_schema,
                span,
                semantic_id,
            } => Ok(Expr::ResultConstruct {
                ok: *ok,
                payload: Box::new(self.rewrite_expr_ownership_inner(payload, false, state)?),
                result_schema: result_schema.clone(),
                span: *span,
                semantic_id: *semantic_id,
            }),
            Expr::ResultQuery {
                value,
                ok,
                span,
                semantic_id,
            } => Ok(Expr::ResultQuery {
                // Tag reads must not detach a resource-bearing carrier. The
                // call-arg flag keeps an owned local as `Var` instead of `MoveVar`.
                value: Box::new(self.rewrite_expr_ownership_inner(value, true, state)?),
                ok: *ok,
                span: *span,
                semantic_id: *semantic_id,
            }),
            Expr::ResultTry {
                value,
                value_slot,
                payload_slot,
                operand_schema,
                return_schema,
                span,
                semantic_id,
                ..
            } => {
                let value = self.rewrite_expr_ownership_inner(value, false, state)?;
                // Err returns after evaluating and consuming the operand. Replay
                // the existing transfer rules on the emitted operand so detached
                // sources are excluded while fresh frame-owned locals remain.
                // This only derives metadata; the operand still executes once.
                let mut operand_state = self.analyze_expr(&value, state, 1)?;
                self.mark_owned_value_reads_moved(&value, &mut operand_state);
                Ok(Expr::ResultTry {
                    value: Box::new(value),
                    value_slot: *value_slot,
                    payload_slot: *payload_slot,
                    operand_schema: operand_schema.clone(),
                    return_schema: return_schema.clone(),
                    exit_cleanup: self
                        .err_exit_cleanup_slots(&operand_state, &[*value_slot, *payload_slot]),
                    span: *span,
                    semantic_id: *semantic_id,
                })
            }
            Expr::ResultMatch {
                value,
                value_slot,
                ok_binding,
                ok_expr,
                err_binding,
                err_expr,
                scrutinee_schema,
                span,
                semantic_id,
            } => {
                let line = 1u32;
                let value = self.rewrite_expr_ownership_inner(value, false, state)?;
                let (ok_in, err_in) = self.result_match_arm_states(
                    &value,
                    *value_slot,
                    *ok_binding,
                    *err_binding,
                    state,
                    line,
                )?;
                Ok(Expr::ResultMatch {
                    value: Box::new(value),
                    value_slot: *value_slot,
                    ok_binding: *ok_binding,
                    ok_expr: Box::new(self.rewrite_expr_ownership_inner(ok_expr, false, &ok_in)?),
                    err_binding: *err_binding,
                    err_expr: Box::new(
                        self.rewrite_expr_ownership_inner(err_expr, false, &err_in)?,
                    ),
                    scrutinee_schema: scrutinee_schema.clone(),
                    span: *span,
                    semantic_id: *semantic_id,
                })
            }
            Expr::Block { .. } => self.rewrite_expr_for_ownership_with_state(expr, state, 0),
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

    fn rewrite_call_args(
        &self,
        args: &[Expr],
        state: &FlowState,
    ) -> Result<(Vec<Expr>, FlowState), ParseError> {
        let mut rewritten = Vec::with_capacity(args.len());
        let mut arg_state = state.clone();
        for arg in args {
            let (arg, next) = self.rewrite_expr_and_state(arg, true, &arg_state, 1)?;
            rewritten.push(arg);
            arg_state = next;
        }
        Ok((rewritten, arg_state))
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
        if let Some((root, key)) = self.extract_optional_field_projection(expr) {
            if self.is_copyable_field(root, &key, state) {
                return expr.clone();
            }
            let Expr::OptionalGet { container_slot, .. } = expr else {
                unreachable!("optional field projection");
            };
            // Save the missing-to-Null result, then detach the same child as
            // ordinary Get. Do not insert a missing child or clone the record.
            let mut lookup = expr.clone();
            if let Expr::OptionalGet {
                field_projection, ..
            } = &mut lookup
            {
                *field_projection = false;
            }
            let key_expr = match key {
                MovedFieldKey::String(value) => Expr::String(value),
                MovedFieldKey::Index(value) => Expr::Int(value),
                _ => unreachable!("literal projection key"),
            };
            let call = |builtin: BuiltinFunction, args| {
                Expr::Call(builtin.call_index(), vec![], args, None, None)
            };
            let has = call(
                BuiltinFunction::Has,
                vec![Expr::Var(root), key_expr.clone()],
            );
            let present = if matches!(key_expr, Expr::Int(_)) {
                Expr::IfElse {
                    condition: Box::new(Expr::Eq(
                        Box::new(call(BuiltinFunction::TypeOf, vec![Expr::Var(root)])),
                        Box::new(Expr::String("map".to_string())),
                    )),
                    then_expr: Box::new(has),
                    else_expr: Box::new(Expr::And(
                        Box::new(Expr::Not(Box::new(Expr::Lt(
                            Box::new(key_expr.clone()),
                            Box::new(Expr::Int(0)),
                        )))),
                        Box::new(Expr::Lt(
                            Box::new(key_expr.clone()),
                            Box::new(call(BuiltinFunction::Len, vec![Expr::Var(root)])),
                        )),
                    )),
                }
            } else {
                has
            };
            return Expr::Block {
                stmts: vec![
                    Stmt::Let {
                        index: *container_slot,
                        expr: lookup,
                        line: 0,
                        declared_schema: None,
                    },
                    Stmt::IfElse {
                        condition: present,
                        then_branch: vec![Stmt::Assign {
                            kind: super::super::ir::AssignmentKind::Set,
                            index: root,
                            expr: call(
                                BuiltinFunction::Set,
                                vec![Expr::Var(root), key_expr, Expr::Null],
                            ),
                            line: 0,
                        }],
                        else_branch: vec![],
                        line: 0,
                    },
                ],
                expr: Box::new(Expr::Var(*container_slot)),
            };
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
                current_owned: vec![false; self.local_count],
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
                let mut current_owned = vec![false; self.local_count];
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
                    // A branch-local owner remains ours when the other edge
                    // never initialized the slot (Null at runtime). Do not turn
                    // a real caller-owned/borrowed predecessor into ownership.
                    current_owned[idx] = (lhs.current_owned[idx]
                        && (rhs.current_owned[idx] || !rhs.possible[idx]))
                        || (rhs.current_owned[idx] && !lhs.possible[idx]);
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
                    current_owned,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::{ResolvedHostParam, TypeSchema};
    use crate::host_api::{HostApiFingerprint, ResourceTypeKey};

    fn take_resolution(key: &ResourceTypeKey) -> Box<ResolvedHostCall> {
        Box::new(ResolvedHostCall {
            name: "test::take".to_string(),
            params: vec![ResolvedHostParam {
                name: "value".to_string(),
                schema: TypeSchema::Resource(key.clone()),
            }],
            return_type: TypeSchema::Null,
            passing: vec![HostParamPassing::TakeOwned],
            fingerprint: HostApiFingerprint::from_wire(1),
            host_params: Vec::new(),
            host_return_type: crate::host_api::HostTypeSchema::Unknown,
        })
    }

    fn resource_binding(index: LocalSlot, key: &ResourceTypeKey) -> Stmt {
        Stmt::Let {
            index,
            declared_schema: Some(TypeSchema::Resource(key.clone())),
            expr: Expr::Null,
            line: 1,
        }
    }

    fn take_call(arg: Expr, key: &ResourceTypeKey) -> Expr {
        Expr::Call(99, Vec::new(), vec![arg], Some(take_resolution(key)), None)
    }

    fn resolved_capture_call(mode: HostParamPassing) -> Expr {
        resolved_capture_call_at(mode, 7)
    }

    fn resolved_capture_call_at(mode: HostParamPassing, slot: LocalSlot) -> Expr {
        let key = ResourceTypeKey::new("test.resource").expect("test key");
        Expr::Call(
            99,
            Vec::new(),
            vec![Expr::Var(slot)],
            Some(Box::new(ResolvedHostCall {
                name: "test::use".to_string(),
                params: vec![ResolvedHostParam {
                    name: "value".to_string(),
                    schema: TypeSchema::Resource(key),
                }],
                return_type: TypeSchema::Null,
                passing: vec![mode],
                fingerprint: HostApiFingerprint::from_wire(2),
                host_params: Vec::new(),
                host_return_type: crate::host_api::HostTypeSchema::Unknown,
            })),
            None,
        )
    }

    #[test]
    fn postoperand_join_retains_branch_local_owner_but_not_borrowed_predecessor() {
        let analyzer = AvailabilityAnalyzer::new(1, &[], &HashMap::new(), true, &[true]);
        let absent = FlowState::reachable(1);
        let mut owned = absent.clone();
        analyzer.mark_available(&mut owned, 0, 1).unwrap();
        owned.current_owned[0] = true;
        for (lhs, rhs) in [(owned.clone(), absent.clone()), (absent, owned.clone())] {
            let merged = analyzer.merge_states(lhs, rhs);
            assert!(merged.current_owned[0]);
            assert!(!merged.definite[0]);
            assert_eq!(
                analyzer.err_exit_cleanup_slots(&merged, &[]),
                vec![(0, TypeSchema::Optional(Box::new(TypeSchema::Unknown)))]
            );
            assert!(analyzer.err_exit_cleanup_slots(&merged, &[0]).is_empty());
        }
        let mut borrowed = owned.clone();
        borrowed.current_owned[0] = false;
        for (lhs, rhs) in [(owned.clone(), borrowed.clone()), (borrowed, owned)] {
            let merged = analyzer.merge_states(lhs, rhs);
            assert!(!merged.current_owned[0]);
            assert!(analyzer.err_exit_cleanup_slots(&merged, &[]).is_empty());
        }
    }

    fn polex_try_expr() -> Expr {
        let schema = TypeSchema::Result {
            ok: Box::new(TypeSchema::Int),
            err: Box::new(TypeSchema::Int),
        };
        let span = crate::compiler::source_map::Span {
            source_id: 0,
            lo: 0,
            hi: 1,
        };
        Expr::ResultTry {
            value: Box::new(Expr::ResultConstruct {
                ok: false,
                payload: Box::new(Expr::Int(9)),
                result_schema: schema.clone(),
                span,
                semantic_id: None,
            }),
            value_slot: 2,
            payload_slot: 3,
            operand_schema: schema.clone(),
            return_schema: schema,
            exit_cleanup: Vec::new(),
            span,
            semantic_id: None,
        }
    }

    #[test]
    fn polex_metadata_optional_container_and_exact_host_args_thread_consumption() {
        let key = ResourceTypeKey::new("test.resource").unwrap();
        let analyzer = AvailabilityAnalyzer::new(
            6,
            &[],
            &HashMap::new(),
            true,
            &[true, true, false, false, false, false],
        );
        let mut state = FlowState::reachable(6);
        for slot in [0, 1] {
            analyzer.mark_available(&mut state, slot, 1).unwrap();
            state.current_owned[slot as usize] = true;
        }
        let optional = Expr::OptionalGet {
            field_projection: false,
            container: Box::new(take_call(Expr::Var(0), &key)),
            key: Box::new(polex_try_expr()),
            container_slot: 4,
            key_slot: 5,
            semantic_id: None,
        };
        let mut resolution = take_resolution(&key);
        resolution.passing.push(HostParamPassing::Value);
        resolution.params.push(ResolvedHostParam {
            name: "next".into(),
            schema: TypeSchema::Int,
        });
        let call = Expr::Call(
            99,
            vec![],
            vec![Expr::Var(0), polex_try_expr()],
            Some(resolution),
            None,
        );
        for expr in [optional, call] {
            let rewritten = analyzer
                .rewrite_expr_ownership_inner(&expr, false, &state)
                .unwrap();
            let nested = match &rewritten {
                Expr::OptionalGet { key, .. } => key.as_ref(),
                Expr::Call(_, _, args, _, _) => &args[1],
                _ => unreachable!(),
            };
            let Expr::ResultTry { exit_cleanup, .. } = nested else {
                panic!("missing Try")
            };
            assert_eq!(*exit_cleanup, vec![(1, TypeSchema::Unknown)]);
        }
    }

    #[test]
    fn polex_for_post_cleanup_uses_continue_edge_fresh_owner() {
        let key = ResourceTypeKey::new("test.resource").unwrap();
        let analyzer =
            AvailabilityAnalyzer::new(4, &[], &HashMap::new(), true, &[true, true, false, false]);
        let mut state = FlowState::reachable(4);
        analyzer.mark_available(&mut state, 0, 1).unwrap();
        state.current_owned[0] = true;
        let init = Stmt::Noop { line: 1 };
        let condition = Expr::Bool(true);
        let post = Stmt::Expr {
            expr: polex_try_expr(),
            line: 1,
        };
        let body = vec![resource_binding(1, &key), Stmt::Continue { line: 1 }];
        let parts = ForLoopParts {
            init: &init,
            condition: &condition,
            post: &post,
            body: &body,
            line: 1,
        };
        let (rewritten, _) = analyzer.analyze_for_loop(parts, state, true).unwrap();
        let Stmt::For { post, .. } = rewritten else {
            unreachable!()
        };
        let Stmt::Expr {
            expr: Expr::ResultTry { exit_cleanup, .. },
            ..
        } = *post
        else {
            panic!("missing post Try")
        };
        assert_eq!(
            exit_cleanup,
            vec![(0, TypeSchema::Unknown), (1, TypeSchema::Unknown)]
        );
    }

    #[test]
    fn capture_mode_uses_resolved_host_parameter_ownership() {
        let move_impl = FunctionImpl {
            param_slots: Vec::new(),
            capture_copies: Vec::new(),
            body_stmts: Vec::new(),
            body_expr: resolved_capture_call(HostParamPassing::TakeOwned),
            body_expr_line: 1,
        };
        assert_eq!(
            function_capture_binding_mode(&move_impl, 7),
            CaptureBindingMode::Move
        );

        let borrow_impl = FunctionImpl {
            body_expr: resolved_capture_call(HostParamPassing::Borrow),
            ..move_impl
        };
        assert_eq!(
            function_capture_binding_mode(&borrow_impl, 7),
            CaptureBindingMode::Borrow
        );
    }

    #[test]
    fn nested_capture_propagates_resolved_take_owned_mode() {
        let inner = ClosureExpr {
            param_slots: Vec::new(),
            capture_copies: vec![(8, 9)],
            body: Box::new(resolved_capture_call(HostParamPassing::TakeOwned)),
        };
        let outer = ClosureExpr {
            param_slots: Vec::new(),
            capture_copies: vec![(7, 8)],
            body: Box::new(Expr::Closure(inner)),
        };
        assert_eq!(
            closure_capture_binding_mode(&outer, 8),
            CaptureBindingMode::Move
        );
    }

    #[test]
    fn immediate_closure_borrow_does_not_escape() {
        let key = ResourceTypeKey::new("test.resource").expect("test key");
        let closure = ClosureExpr {
            param_slots: Vec::new(),
            capture_copies: vec![(0, 1)],
            body: Box::new(resolved_capture_call_at(HostParamPassing::Borrow, 1)),
        };
        let ir = FrontendIr {
            stmts: vec![
                resource_binding(0, &key),
                Stmt::Expr {
                    expr: Expr::ClosureCall(closure, Vec::new()),
                    line: 1,
                },
            ],
            locals: 2,
            local_bindings: vec![("resource".to_string(), 0)],
            struct_schemas: HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions: Vec::new(),
            function_impls: HashMap::new(),
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        };
        enforce_local_availability(ir, &[], false, true, &[true, false])
            .expect("immediate borrowed capture should not escape");
    }

    #[test]
    fn expression_block_retains_nested_take_owned_move_rewrites() {
        let key = ResourceTypeKey::new("test.resource").expect("test key");
        let field = Expr::Call(
            BuiltinFunction::Get.call_index(),
            Vec::new(),
            vec![Expr::Var(1), Expr::String("field".to_string())],
            None,
            None,
        );
        let index = Expr::Call(
            BuiltinFunction::Get.call_index(),
            Vec::new(),
            vec![Expr::Var(2), Expr::Int(0)],
            None,
            None,
        );
        let projection = Expr::Call(
            BuiltinFunction::Get.call_index(),
            Vec::new(),
            vec![Expr::Var(3), Expr::String("field".to_string())],
            None,
            None,
        );
        let ir = FrontendIr {
            stmts: vec![
                resource_binding(0, &key),
                resource_binding(1, &key),
                resource_binding(2, &key),
                resource_binding(3, &key),
                Stmt::Expr {
                    expr: Expr::Block {
                        stmts: vec![
                            Stmt::Expr {
                                expr: take_call(Expr::Var(0), &key),
                                line: 2,
                            },
                            Stmt::Expr {
                                expr: take_call(field, &key),
                                line: 3,
                            },
                            Stmt::Expr {
                                expr: take_call(index, &key),
                                line: 4,
                            },
                            Stmt::Expr {
                                expr: projection,
                                line: 5,
                            },
                        ],
                        expr: Box::new(Expr::Int(0)),
                    },
                    line: 2,
                },
            ],
            locals: 4,
            local_bindings: vec![
                ("value".to_string(), 0),
                ("object".to_string(), 1),
                ("array".to_string(), 2),
                ("projection".to_string(), 3),
            ],
            struct_schemas: HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions: Vec::new(),
            function_impls: HashMap::new(),
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        };

        let rewritten = enforce_local_availability(ir, &[], false, true, &[true, true, true, true])
            .expect("ownership analysis should succeed");
        let Stmt::Expr {
            expr: Expr::Block { stmts: nested, .. },
            ..
        } = &rewritten.stmts[4]
        else {
            panic!("expected expression block");
        };

        let move_var = match &nested[0] {
            Stmt::Expr {
                expr: Expr::Call(_, _, args, Some(_), _),
                ..
            } => args.first(),
            other => panic!("expected resolved take call, got {other:?}"),
        };
        assert!(matches!(move_var, Some(Expr::MoveVar(0))));

        let move_field = match &nested[1] {
            Stmt::Expr {
                expr: Expr::Call(_, _, args, Some(_), _),
                ..
            } => args.first(),
            other => panic!("expected resolved take call, got {other:?}"),
        };
        assert!(matches!(move_field, Some(Expr::MoveField { root: 1, key }) if key == "field"));

        let move_index = match &nested[2] {
            Stmt::Expr {
                expr: Expr::Call(_, _, args, Some(_), _),
                ..
            } => args.first(),
            other => panic!("expected resolved take call, got {other:?}"),
        };
        assert!(matches!(
            move_index,
            Some(Expr::MoveIndex { root: 2, index: 0 })
        ));

        let projection = match &nested[3] {
            Stmt::Expr { expr, .. } => expr,
            other => panic!("expected projection expression, got {other:?}"),
        };
        assert!(matches!(
            projection,
            Expr::MoveField { root: 3, key } if key == "field"
        ));
    }
}
