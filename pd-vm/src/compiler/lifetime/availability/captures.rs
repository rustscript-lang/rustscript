use super::*;

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
    ) {
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
        if capture_mode == CaptureBindingMode::Move
            && self.should_move_local_on_rebind_source(source_slot, state)
        {
            self.mark_local_moved(state, source_slot);
        }
    }

    pub(super) fn function_capture_mode_for_slot(
        &self,
        function_impl: &FunctionImpl,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        let mut mode = CaptureBindingMode::Copy;
        let mut seen = false;
        self.capture_mode_for_stmts(
            &function_impl.body_stmts,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        self.capture_mode_for_expr(
            &function_impl.body_expr,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        if seen { mode } else { CaptureBindingMode::Move }
    }

    pub(super) fn closure_capture_mode_for_slot(
        &self,
        closure: &ClosureExpr,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        let mut mode = CaptureBindingMode::Copy;
        let mut seen = false;
        self.capture_mode_for_expr(
            &closure.body,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        if seen { mode } else { CaptureBindingMode::Move }
    }

    pub(super) fn capture_mode_for_stmts(
        &self,
        stmts: &[Stmt],
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        for stmt in stmts {
            self.capture_mode_for_stmt(stmt, captured_slot, context, mode, seen);
        }
    }

    pub(super) fn capture_mode_for_stmt(
        &self,
        stmt: &Stmt,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Stmt::Expr { expr, .. } => {
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(then_branch, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(else_branch, captured_slot, context, mode, seen);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.capture_mode_for_stmt(init, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmt(post, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(body, captured_slot, context, mode, seen);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(body, captured_slot, context, mode, seen);
            }
        }
    }

    pub(super) fn capture_mode_for_expr(
        &self,
        expr: &Expr,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::FunctionRef(_) => {}
            Expr::Var(index) => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
            }
            Expr::MoveVar(index) => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = CaptureBindingMode::Move;
                }
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                if *root == captured_slot {
                    *seen = true;
                    *mode = CaptureBindingMode::Move;
                }
            }
            Expr::Call(_, args) | Expr::LocalCall(_, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, mode, seen);
                }
            }
            Expr::Closure(closure) => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, mode, seen);
                }
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
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
                self.capture_mode_for_expr(lhs, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(rhs, captured_slot, context, mode, seen);
            }
            Expr::Neg(inner) | Expr::Not(inner) => {
                self.capture_mode_for_expr(inner, captured_slot, context, mode, seen);
            }
            Expr::ToOwned(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Copy,
                    mode,
                    seen,
                );
            }
            Expr::Borrow(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Borrow,
                    mode,
                    seen,
                );
            }
            Expr::BorrowMut(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::BorrowMut,
                    mode,
                    seen,
                );
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(then_expr, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(else_expr, captured_slot, context, mode, seen);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                if *value_slot == captured_slot || *result_slot == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(value, captured_slot, context, mode, seen);
                for (_, arm_expr) in arms {
                    self.capture_mode_for_expr(arm_expr, captured_slot, context, mode, seen);
                }
                self.capture_mode_for_expr(default, captured_slot, context, mode, seen);
            }
            Expr::Block { stmts, expr } => {
                self.capture_mode_for_stmts(stmts, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
        }
    }
}
