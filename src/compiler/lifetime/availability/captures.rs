use super::*;

/// Mutable state threaded through the capture-mode scan of a function or
/// closure body. The final mode matches what the runtime `BorrowMut` capture
/// model computes; `implicit_read` additionally records whether the body used
/// the captured slot through a plain by-value read with no explicit `.copy()`
/// or borrow wrapper.
///
/// `implicit_read` only affects source consumption when the final mode is
/// `Copy`: it is the availability-side signal that a movable source binding
/// was consumed by an implicit by-value read even though the runtime clones
/// the value into the capture. It has no effect on the other modes — `Move`
/// consumes the source unconditionally, and `Borrow`/`BorrowMut` never
/// consume it (mutation flows back through the shared cell) regardless of how
/// the body reads the slot. Bodies that write the slot therefore always end
/// in `BorrowMut` (or `Move` under a move context) and never consult
/// `implicit_read` for consumption.
pub(super) struct CaptureModeScan {
    mode: CaptureBindingMode,
    seen: bool,
    implicit_read: bool,
}

impl CaptureModeScan {
    fn new() -> Self {
        Self {
            mode: CaptureBindingMode::Copy,
            seen: false,
            implicit_read: false,
        }
    }
}

impl AvailabilityAnalyzer {
    pub(super) fn analyze_args(
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

    pub(super) fn analyze_projection_args(
        &self,
        args: &[Expr],
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        let mut out = state.clone();
        let Some((root, rest)) = args.split_first() else {
            return Ok(out);
        };
        out = self.analyze_projection_root_expr(root, &out, line)?;
        for arg in rest {
            out = self.analyze_expr(arg, &out, line)?;
        }
        Ok(out)
    }

    pub(super) fn analyze_projection_root_expr(
        &self,
        expr: &Expr,
        state: &FlowState,
        line: u32,
    ) -> Result<FlowState, ParseError> {
        if let Expr::Var(index) = expr {
            self.require_available(*index, state, line)?;
            self.require_local_not_moved(*index, state, line)?;
            return Ok(state.clone());
        }
        self.analyze_expr(expr, state, line)
    }

    pub(super) fn analyze_closure(
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
            self.require_local_not_partially_moved(*source_slot, state, line)?;
        }

        let mut closure_state = FlowState::reachable(self.local_count);
        for slot in &closure.param_slots {
            self.mark_available(&mut closure_state, *slot, line)?;
            // Resource-typed closure parameters are move-only inside the body.
            if self.is_owned_slot(*slot) {
                closure_state.copyable_locals[*slot as usize] = false;
                closure_state.movable_locals[*slot as usize] = true;
            }
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

    pub(super) fn apply_capture_binding_effect(
        &self,
        state: &mut FlowState,
        source_slot: LocalSlot,
        captured_slot: LocalSlot,
        capture_mode: CaptureBindingMode,
        implicit_read: bool,
        line: u32,
    ) -> Result<(), ParseError> {
        let source_idx = source_slot as usize;
        let captured_idx = captured_slot as usize;
        if source_idx < self.local_count && captured_idx < self.local_count {
            state.copyable_locals[captured_idx] = state.copyable_locals[source_idx];
            state.movable_locals[captured_idx] = state.movable_locals[source_idx];
            state.moved_local_definite[captured_idx] = state.moved_local_definite[source_idx];
            state.moved_local_possible[captured_idx] = state.moved_local_possible[source_idx];
        }
        self.copy_local_field_moves(state, source_slot, captured_slot);
        self.copy_local_collection_aliases(state, source_slot, captured_slot);
        // Owned (resource-containing) sources can never be aliased or cloned
        // by a closure: a shared borrow would let the handle escape the call
        // boundary, and the core has no generic resource clone. The only
        // legal resource capture is a move — the source becomes unusable and
        // the handle transfers into the closure cell.
        if self.is_owned_slot(source_slot) {
            match capture_mode {
                CaptureBindingMode::Borrow | CaptureBindingMode::BorrowMut => {
                    let display = self.display_local_name(source_slot);
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_BORROW_ESCAPE".to_string()),
                        line: line as usize,
                        message: format!(
                            "closure capture of resource value '{display}' must move it; a shared borrow cannot escape into a closure cell"
                        ),
                    });
                }
                CaptureBindingMode::Copy => {
                    let display = self.display_local_name(source_slot);
                    return Err(ParseError {
                        span: None,
                        code: Some("E_OWNERSHIP_COPY_RESOURCE".to_string()),
                        line: line as usize,
                        message: format!(
                            "closure capture of resource value '{display}' must move it; resources cannot be cloned into a closure cell"
                        ),
                    });
                }
                CaptureBindingMode::Move => {}
            }
        }
        // Availability and codegen consume the same capture-mode classifier.
        // Codegen only needs the mode; availability additionally applies its
        // stricter body-use model: a plain by-value use (an implicit read with
        // no explicit `.copy()` or borrow) of a movable source consumes the
        // source binding even though the runtime clones the value into the
        // capture. Shared borrow captures (`Borrow`/`BorrowMut`) leave the
        // source binding usable so mutation can flow back through the cell.
        // `implicit_read` is consulted only when the final mode is `Copy`:
        // `Move` consumes the source regardless, and the shared-borrow modes
        // never consume it no matter how the body reads the slot. Owned
        // sources always consume (they are move-only by schema).
        let consumes_source = self.is_owned_slot(source_slot)
            || match capture_mode {
                CaptureBindingMode::Move => true,
                CaptureBindingMode::Borrow | CaptureBindingMode::BorrowMut => false,
                CaptureBindingMode::Copy => implicit_read,
            };
        if consumes_source
            && self.enable_local_move_semantics
            && source_idx < self.local_count
            && (self.is_owned_slot(source_slot)
                || state.movable_locals[source_idx]
                || !state.collection_aliases[source_idx].is_empty())
        {
            self.mark_local_moved(state, source_slot);
        }
        Ok(())
    }

    /// Classifies a named-function capture for availability: returns the
    /// runtime capture mode plus whether the body contains an implicit
    /// by-value read of the captured slot.
    pub(super) fn function_capture_mode_for_slot(
        &self,
        function_impl: &FunctionImpl,
        captured_slot: LocalSlot,
    ) -> (CaptureBindingMode, bool) {
        let mut scan = CaptureModeScan::new();
        self.capture_mode_for_stmts(
            &function_impl.body_stmts,
            captured_slot,
            CaptureBindingMode::Copy,
            true,
            &mut scan,
        );
        self.capture_mode_for_expr(
            &function_impl.body_expr,
            captured_slot,
            CaptureBindingMode::Copy,
            true,
            &mut scan,
        );
        if scan.seen {
            (scan.mode, scan.implicit_read)
        } else {
            (CaptureBindingMode::Move, scan.implicit_read)
        }
    }

    /// Classifies a closure capture for availability: returns the runtime
    /// capture mode plus whether the body contains an implicit by-value read
    /// of the captured slot.
    pub(super) fn closure_capture_mode_for_slot(
        &self,
        closure: &ClosureExpr,
        captured_slot: LocalSlot,
    ) -> (CaptureBindingMode, bool) {
        let mut scan = CaptureModeScan::new();
        self.capture_mode_for_expr(
            &closure.body,
            captured_slot,
            CaptureBindingMode::Copy,
            true,
            &mut scan,
        );
        if scan.seen {
            (scan.mode, scan.implicit_read)
        } else {
            (CaptureBindingMode::Move, scan.implicit_read)
        }
    }

    pub(super) fn runtime_function_capture_mode_for_slot(
        &self,
        function_impl: &FunctionImpl,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        self.function_capture_mode_for_slot(function_impl, captured_slot)
            .0
    }

    pub(super) fn runtime_closure_capture_mode_for_slot(
        &self,
        closure: &ClosureExpr,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        self.closure_capture_mode_for_slot(closure, captured_slot).0
    }

    pub(super) fn capture_mode_for_stmts(
        &self,
        stmts: &[Stmt],
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        implicit: bool,
        scan: &mut CaptureModeScan,
    ) {
        for stmt in stmts {
            self.capture_mode_for_stmt(stmt, captured_slot, context, implicit, scan);
        }
    }

    pub(super) fn capture_mode_for_stmt(
        &self,
        stmt: &Stmt,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        implicit: bool,
        scan: &mut CaptureModeScan,
    ) {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                if *index == captured_slot {
                    scan.seen = true;
                    scan.mode = scan.mode.max(context);
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                if *index == captured_slot {
                    // Writing the captured slot makes the capture shared-mutable
                    // (or a move under a move context), covering every
                    // AssignmentKind: plain `state = other` (write-only, RHS
                    // never reads the slot) and compound `state += rhs` /
                    // `state++`, whose synthesized `Add(Var(state), rhs)` RHS
                    // read is picked up below. Since any write forces the mode
                    // to at least `BorrowMut`, that read can never make
                    // `implicit_read` affect source consumption (see
                    // `apply_capture_binding_effect`).
                    scan.seen = true;
                    let assignment_mode = if context == CaptureBindingMode::Move {
                        CaptureBindingMode::Move
                    } else {
                        CaptureBindingMode::BorrowMut
                    };
                    scan.mode = scan.mode.max(assignment_mode);
                }
                self.capture_mode_for_expr(expr, captured_slot, context, implicit, scan);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            true,
                            scan,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, implicit, scan);
            }
            Stmt::Expr { expr, .. } => {
                self.capture_mode_for_expr(expr, captured_slot, context, implicit, scan);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, implicit, scan);
                self.capture_mode_for_stmts(then_branch, captured_slot, context, implicit, scan);
                self.capture_mode_for_stmts(else_branch, captured_slot, context, implicit, scan);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.capture_mode_for_stmt(init, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(condition, captured_slot, context, implicit, scan);
                self.capture_mode_for_stmt(post, captured_slot, context, implicit, scan);
                self.capture_mode_for_stmts(body, captured_slot, context, implicit, scan);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, implicit, scan);
                self.capture_mode_for_stmts(body, captured_slot, context, implicit, scan);
            }
        }
    }

    pub(super) fn capture_mode_for_expr(
        &self,
        expr: &Expr,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        implicit: bool,
        scan: &mut CaptureModeScan,
    ) {
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
            Expr::Var(index) => {
                if *index == captured_slot {
                    scan.seen = true;
                    scan.mode = scan.mode.max(context);
                    // A bare by-value read (not wrapped in an explicit
                    // `.copy()` or borrow) is an implicit read: availability
                    // treats it as a move for movable source bindings even
                    // though the runtime clones the value into the capture.
                    if implicit && context == CaptureBindingMode::Copy {
                        scan.implicit_read = true;
                    }
                }
            }
            Expr::MoveVar(index) => {
                if *index == captured_slot {
                    scan.seen = true;
                    scan.mode = CaptureBindingMode::Move;
                }
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                if *root == captured_slot {
                    scan.seen = true;
                    scan.mode = CaptureBindingMode::Move;
                }
            }
            Expr::OptionalGet { container, key, .. } => {
                self.capture_mode_for_expr(container, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(key, captured_slot, context, implicit, scan);
            }
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                self.capture_mode_for_expr(value, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(fallback, captured_slot, context, implicit, scan);
            }
            Expr::Call(_, _, args, _, _)
            | Expr::LocalCall(_, _, args)
            | Expr::ModuleCall(_, _, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, implicit, scan);
                }
            }
            Expr::Closure(closure) => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            true,
                            scan,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, implicit, scan);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, implicit, scan);
                }
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            true,
                            scan,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, implicit, scan);
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
                self.capture_mode_for_expr(lhs, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(rhs, captured_slot, context, implicit, scan);
            }
            Expr::Neg(inner) | Expr::Not(inner) => {
                self.capture_mode_for_expr(inner, captured_slot, context, implicit, scan);
            }
            Expr::ToOwned(inner) => {
                // Explicit `.copy()`: the value is duplicated on purpose and
                // never consumes the source binding, so the inner read is not
                // an implicit read.
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Copy,
                    false,
                    scan,
                );
            }
            Expr::Borrow(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Borrow,
                    false,
                    scan,
                );
            }
            Expr::BorrowMut(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::BorrowMut,
                    false,
                    scan,
                );
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(then_expr, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(else_expr, captured_slot, context, implicit, scan);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                if *value_slot == captured_slot
                    || *result_slot == captured_slot
                    || arms
                        .iter()
                        .any(|(pattern, _)| pattern.binding_slot() == Some(captured_slot))
                {
                    scan.seen = true;
                    scan.mode = scan.mode.max(context);
                    if implicit && context == CaptureBindingMode::Copy {
                        scan.implicit_read = true;
                    }
                }
                self.capture_mode_for_expr(value, captured_slot, context, implicit, scan);
                for (_, arm_expr) in arms {
                    self.capture_mode_for_expr(arm_expr, captured_slot, context, implicit, scan);
                }
                self.capture_mode_for_expr(default, captured_slot, context, implicit, scan);
            }
            Expr::Block { stmts, expr } => {
                self.capture_mode_for_stmts(stmts, captured_slot, context, implicit, scan);
                self.capture_mode_for_expr(expr, captured_slot, context, implicit, scan);
            }
        }
    }
}
