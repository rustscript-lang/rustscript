use std::collections::HashMap;

use crate::builtins::BuiltinFunction;

use super::super::CompileError;
use super::super::ir::{ClosureExpr, Expr, FunctionImpl, LocalSlot, Stmt};
use super::helpers::{
    bind_expr_result_to_slot, bound_type_label, display_name_for_builtin,
    function_body_contains_param_add, infer_binary_type, infer_unary_type, is_numeric_bound_type,
    merge_observed_function_param_type,
};
use super::state::{
    BoundType, HostCallableSignature, InferredCallable, LocalTypeState, SimpleType,
    merge_container_element_types, stabilize_loop_state,
};
use super::validate::{validate_host_signature, validate_signature_overloads};

pub(super) struct TypeContext<'a> {
    pub(super) function_impls: &'a HashMap<u16, FunctionImpl>,
    pub(super) function_names: &'a HashMap<u16, String>,
    pub(super) host_import_return_types: &'a HashMap<u16, BoundType>,
    pub(super) host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    pub(super) active_functions: Vec<u16>,
    pub(super) observed_function_param_types: HashMap<u16, Vec<BoundType>>,
    pub(super) function_param_conflicts: HashMap<u16, String>,
}

impl<'a> TypeContext<'a> {
    pub(super) fn new(
        function_impls: &'a HashMap<u16, FunctionImpl>,
        function_names: &'a HashMap<u16, String>,
        host_import_return_types: &'a HashMap<u16, BoundType>,
        host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    ) -> Self {
        Self {
            function_impls,
            function_names,
            host_import_return_types,
            host_import_signatures,
            active_functions: Vec::new(),
            observed_function_param_types: HashMap::new(),
            function_param_conflicts: HashMap::new(),
        }
    }

    pub(super) fn function_name(&self, index: u16) -> &str {
        self.function_names
            .get(&index)
            .map(String::as_str)
            .unwrap_or("<anonymous>")
    }

    pub(super) fn function_requires_strict_add_types(&self, index: u16) -> bool {
        let Some(function_impl) = self.function_impls.get(&index) else {
            return false;
        };
        function_impl.capture_copies.is_empty()
            && function_body_contains_param_add(
                &function_impl.param_slots,
                &function_impl.body_stmts,
                &function_impl.body_expr,
            )
    }

    pub(super) fn observe_function_arg_types(
        &mut self,
        index: u16,
        args: &[Expr],
        state: &LocalTypeState,
    ) {
        let function_name = self.function_name(index).to_string();
        let actual = args
            .iter()
            .map(|arg| self.infer_expr_type(arg, state))
            .collect::<Vec<_>>();
        let mut merged_types = self
            .observed_function_param_types
            .remove(&index)
            .unwrap_or_else(|| vec![BoundType::Unknown; actual.len()]);
        if merged_types.len() < actual.len() {
            merged_types.resize(actual.len(), BoundType::Unknown);
        }
        for (arg_index, actual_ty) in actual.into_iter().enumerate() {
            let current = merged_types[arg_index];
            match merge_observed_function_param_type(current, actual_ty) {
                Ok(merged) => merged_types[arg_index] = merged,
                Err((lhs, rhs)) => {
                    if self.function_requires_strict_add_types(index) {
                        self.function_param_conflicts.entry(index).or_insert_with(|| {
                            format!(
                                "function '{}' is called with conflicting inferred types for arg{}: {} vs {}",
                                function_name,
                                arg_index + 1,
                                bound_type_label(lhs),
                                bound_type_label(rhs)
                            )
                        });
                    } else {
                        merged_types[arg_index] = BoundType::Unknown;
                    }
                }
            }
        }
        self.observed_function_param_types
            .insert(index, merged_types);
    }

    pub(super) fn infer_expr_type(&mut self, expr: &Expr, state: &LocalTypeState) -> BoundType {
        match expr {
            Expr::Null => BoundType::Null,
            Expr::Int(_) => BoundType::Int,
            Expr::Float(_) => BoundType::Float,
            Expr::Bool(_) => BoundType::Bool,
            Expr::String(_) => BoundType::String,
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.infer_expr_type(inner, state)
            }
            Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
            Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
                self.infer_call_like_expr_type(expr, state)
            }
            Expr::ClosureCall(_, _) => self.infer_call_like_expr_type(expr, state),
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
                let lhs_ty = self.infer_expr_type(lhs, state);
                let rhs_ty = self.infer_expr_type(rhs, state);
                infer_binary_type(expr, lhs_ty, rhs_ty)
            }
            Expr::Neg(inner) | Expr::Not(inner) => {
                let inner_ty = self.infer_expr_type(inner, state);
                infer_unary_type(expr, inner_ty)
            }
            Expr::IfElse {
                condition: _,
                then_expr,
                else_expr,
            } => {
                let then_ty = self.infer_expr_type(then_expr, state);
                let else_ty = self.infer_expr_type(else_expr, state);
                if then_ty == else_ty {
                    then_ty
                } else {
                    BoundType::Unknown
                }
            }
            Expr::Match {
                value_slot,
                value,
                arms,
                default,
                ..
            } => {
                let mut nested = state.clone();
                let value_ty = self.infer_expr_type(value, state);
                bind_expr_result_to_slot(&mut nested, *value_slot, value, state, value_ty, self);
                let mut arm_type = BoundType::Unknown;
                for (_pattern, arm_expr) in arms {
                    let ty = self.infer_expr_type(arm_expr, &nested);
                    arm_type = if arm_type == BoundType::Unknown {
                        ty
                    } else if arm_type == ty {
                        arm_type
                    } else {
                        BoundType::Unknown
                    };
                }
                let default_ty = self.infer_expr_type(default, &nested);
                if arms.is_empty() {
                    default_ty
                } else if arm_type != BoundType::Unknown && arm_type == default_ty {
                    arm_type
                } else {
                    BoundType::Unknown
                }
            }
            Expr::Block { stmts, expr } => {
                let mut nested = state.clone();
                self.apply_stmts(stmts, &mut nested);
                self.infer_expr_type(expr, &nested)
            }
        }
    }

    pub(super) fn infer_call_like_expr_type(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        match expr {
            Expr::Call(index, args) => {
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    self.infer_builtin_call_like_expr_type(builtin, args, state)
                } else {
                    let inferred = self.infer_function_return(*index, args, state);
                    if inferred != BoundType::Unknown {
                        inferred
                    } else {
                        self.host_import_return_types
                            .get(index)
                            .copied()
                            .unwrap_or(BoundType::Unknown)
                    }
                }
            }
            Expr::LocalCall(slot, args) => match state.callable(*slot).cloned() {
                Some(InferredCallable::Function(index)) => {
                    let inferred = self.infer_function_return(index, args, state);
                    if inferred != BoundType::Unknown {
                        inferred
                    } else {
                        self.host_import_return_types
                            .get(&index)
                            .copied()
                            .unwrap_or(BoundType::Unknown)
                    }
                }
                Some(InferredCallable::Closure(closure)) => {
                    self.infer_closure_return(&closure, args, state)
                }
                None => BoundType::Unknown,
            },
            Expr::ClosureCall(closure, args) => self.infer_closure_return(closure, args, state),
            Expr::Closure(_) | Expr::FunctionRef(_) => BoundType::Unknown,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_builtin_call_like_expr_type(
        &mut self,
        builtin: BuiltinFunction,
        args: &[Expr],
        state: &LocalTypeState,
    ) -> BoundType {
        match builtin {
            BuiltinFunction::ArrayNew => BoundType::ArrayOf(None),
            BuiltinFunction::MapNew => BoundType::MapOf(None),
            BuiltinFunction::Concat if args.len() == 2 => {
                self.infer_concat_return_type(&args[0], &args[1], state)
            }
            BuiltinFunction::Slice if args.len() == 3 => {
                self.infer_slice_return_type(&args[0], state)
            }
            BuiltinFunction::ArrayPush if args.len() == 2 => {
                self.infer_array_push_return_type(&args[0], &args[1], state)
            }
            BuiltinFunction::Set if args.len() == 3 => {
                self.infer_set_return_type(&args[0], &args[2], state)
            }
            BuiltinFunction::Get if args.len() == 2 => self.infer_get_return_type(&args[0], state),
            BuiltinFunction::Keys if args.len() == 1 => {
                self.infer_keys_return_type(&args[0], state)
            }
            BuiltinFunction::ReSplit => BoundType::ArrayOf(Some(SimpleType::String)),
            BuiltinFunction::ReCaptures => BoundType::Array,
            BuiltinFunction::MathAbs
            | BuiltinFunction::MathFloor
            | BuiltinFunction::MathCeil
            | BuiltinFunction::MathRound
            | BuiltinFunction::MathTrunc
            | BuiltinFunction::MathSignum
                if args.len() == 1 =>
            {
                self.infer_same_numeric_return_type(&args[0], state)
            }
            BuiltinFunction::MathMin | BuiltinFunction::MathMax if args.len() == 2 => {
                self.infer_numeric_pair_return_type(&args[0], &args[1], state)
            }
            BuiltinFunction::MathClamp if args.len() == 3 => {
                self.infer_numeric_triplet_return_type(&args[0], &args[1], &args[2], state)
            }
            _ => BoundType::from(builtin.static_return_type()),
        }
    }

    pub(super) fn infer_concat_return_type(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        let lhs_ty = self.infer_expr_type(lhs, state);
        let rhs_ty = self.infer_expr_type(rhs, state);
        match (lhs_ty, rhs_ty) {
            (BoundType::String, BoundType::String) => BoundType::String,
            (BoundType::ArrayOf(lhs), BoundType::ArrayOf(rhs)) => {
                merge_container_element_types(lhs, rhs)
            }
            (BoundType::Array, BoundType::Array)
            | (BoundType::Array, BoundType::ArrayOf(_))
            | (BoundType::ArrayOf(_), BoundType::Array) => BoundType::Array,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_slice_return_type(
        &mut self,
        source: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        match self.infer_expr_type(source, state) {
            BoundType::String => BoundType::String,
            BoundType::Array => BoundType::Array,
            BoundType::ArrayOf(element_type) => BoundType::ArrayOf(element_type),
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_array_push_return_type(
        &mut self,
        array: &Expr,
        value: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        let array_ty = self.infer_expr_type(array, state);
        let value_simple = self.infer_expr_type(value, state).simple_type();
        match (array_ty, value_simple) {
            (BoundType::ArrayOf(None), Some(value_ty)) => BoundType::ArrayOf(Some(value_ty)),
            (BoundType::ArrayOf(Some(existing)), Some(value_ty)) if existing == value_ty => {
                BoundType::ArrayOf(Some(existing))
            }
            (BoundType::ArrayOf(_), _) => BoundType::Array,
            (BoundType::Array, _) => BoundType::Array,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_set_return_type(
        &mut self,
        container: &Expr,
        value: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        let container_ty = self.infer_expr_type(container, state);
        let value_simple = self.infer_expr_type(value, state).simple_type();
        match (container_ty, value_simple) {
            (BoundType::ArrayOf(None), Some(value_ty)) => BoundType::ArrayOf(Some(value_ty)),
            (BoundType::ArrayOf(Some(existing)), Some(value_ty)) if existing == value_ty => {
                BoundType::ArrayOf(Some(existing))
            }
            (BoundType::ArrayOf(_), _) => BoundType::Array,
            (BoundType::Array, _) => BoundType::Array,
            (BoundType::MapOf(None), Some(value_ty)) => BoundType::MapOf(Some(value_ty)),
            (BoundType::MapOf(Some(existing)), Some(value_ty)) if existing == value_ty => {
                BoundType::MapOf(Some(existing))
            }
            (BoundType::MapOf(_), _) => BoundType::Map,
            (BoundType::Map, _) => BoundType::Map,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_get_return_type(
        &mut self,
        container: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        match self.infer_expr_type(container, state) {
            BoundType::String => BoundType::String,
            BoundType::ArrayOf(Some(element_type)) | BoundType::MapOf(Some(element_type)) => {
                BoundType::from_simple(element_type)
            }
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_keys_return_type(
        &mut self,
        container: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        match self.infer_expr_type(container, state) {
            BoundType::Array | BoundType::ArrayOf(_) => BoundType::ArrayOf(Some(SimpleType::Int)),
            BoundType::Map | BoundType::MapOf(_) => BoundType::Array,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_same_numeric_return_type(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        match self.infer_expr_type(expr, state) {
            BoundType::Int => BoundType::Int,
            BoundType::Float => BoundType::Float,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_numeric_pair_return_type(
        &mut self,
        lhs: &Expr,
        rhs: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        let lhs_ty = self.infer_expr_type(lhs, state);
        let rhs_ty = self.infer_expr_type(rhs, state);
        if lhs_ty == BoundType::Int && rhs_ty == BoundType::Int {
            BoundType::Int
        } else if is_numeric_bound_type(lhs_ty) && is_numeric_bound_type(rhs_ty) {
            BoundType::Float
        } else {
            BoundType::Unknown
        }
    }

    pub(super) fn infer_numeric_triplet_return_type(
        &mut self,
        first: &Expr,
        second: &Expr,
        third: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        let first_ty = self.infer_expr_type(first, state);
        let second_ty = self.infer_expr_type(second, state);
        let third_ty = self.infer_expr_type(third, state);
        if first_ty == BoundType::Int && second_ty == BoundType::Int && third_ty == BoundType::Int {
            BoundType::Int
        } else if is_numeric_bound_type(first_ty)
            && is_numeric_bound_type(second_ty)
            && is_numeric_bound_type(third_ty)
        {
            BoundType::Float
        } else {
            BoundType::Unknown
        }
    }

    pub(super) fn infer_function_return(
        &mut self,
        index: u16,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> BoundType {
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return BoundType::Unknown;
        };
        self.observe_function_arg_types(index, args, caller_state);
        if self.active_functions.contains(&index) {
            return BoundType::Unknown;
        }
        self.active_functions.push(index);
        let result = self.infer_callable_body(
            &function_impl.param_slots,
            &function_impl.capture_copies,
            &function_impl.body_stmts,
            &function_impl.body_expr,
            args,
            caller_state,
        );
        self.active_functions.pop();
        result
    }

    pub(super) fn infer_closure_return(
        &mut self,
        closure: &ClosureExpr,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> BoundType {
        self.infer_callable_body(
            &closure.param_slots,
            &closure.capture_copies,
            &[],
            &closure.body,
            args,
            caller_state,
        )
    }

    pub(super) fn infer_callable_body(
        &mut self,
        param_slots: &[LocalSlot],
        capture_copies: &[(LocalSlot, LocalSlot)],
        body_stmts: &[Stmt],
        body_expr: &Expr,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> BoundType {
        if param_slots.len() != args.len() {
            return BoundType::Unknown;
        }
        let mut nested = LocalTypeState::default();
        for (source_slot, captured_slot) in capture_copies {
            nested.copy_binding_from(caller_state, *source_slot, *captured_slot);
        }
        for (arg, slot) in args.iter().zip(param_slots.iter()) {
            self.bind_expr_to_slot(&mut nested, *slot, arg, caller_state);
        }
        self.apply_stmts(body_stmts, &mut nested);
        self.infer_expr_type(body_expr, &nested)
    }

    pub(super) fn validate_call_argument_types(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
        line_context: Option<u32>,
        source_name: Option<&str>,
    ) -> Result<(), CompileError> {
        match expr {
            Expr::Call(index, args) => {
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    self.validate_builtin_argument_types(
                        builtin,
                        args,
                        state,
                        line_context,
                        source_name,
                    )
                } else if let Some(signature) = self.host_import_signatures.get(index).cloned() {
                    self.validate_host_argument_types(
                        &signature,
                        args,
                        state,
                        line_context,
                        source_name,
                    )
                } else {
                    Ok(())
                }
            }
            Expr::LocalCall(slot, args) => match state.callable(*slot).cloned() {
                Some(InferredCallable::Function(index)) => {
                    if let Some(builtin) = BuiltinFunction::from_call_index(index) {
                        self.validate_builtin_argument_types(
                            builtin,
                            args,
                            state,
                            line_context,
                            source_name,
                        )
                    } else if let Some(signature) = self.host_import_signatures.get(&index).cloned()
                    {
                        self.validate_host_argument_types(
                            &signature,
                            args,
                            state,
                            line_context,
                            source_name,
                        )
                    } else {
                        Ok(())
                    }
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    pub(super) fn validate_builtin_argument_types(
        &mut self,
        builtin: BuiltinFunction,
        args: &[Expr],
        state: &LocalTypeState,
        line_context: Option<u32>,
        source_name: Option<&str>,
    ) -> Result<(), CompileError> {
        validate_signature_overloads(
            &display_name_for_builtin(builtin),
            "builtin",
            builtin.callable_signatures(),
            args,
            state,
            self,
            line_context,
            source_name,
        )
    }

    pub(super) fn validate_host_argument_types(
        &mut self,
        signature: &HostCallableSignature,
        args: &[Expr],
        state: &LocalTypeState,
        line_context: Option<u32>,
        source_name: Option<&str>,
    ) -> Result<(), CompileError> {
        validate_host_signature(
            &signature.name,
            &signature.params,
            args,
            state,
            self,
            line_context,
            source_name,
        )
    }

    pub(super) fn apply_stmts(&mut self, stmts: &[Stmt], state: &mut LocalTypeState) {
        for stmt in stmts {
            match stmt {
                Stmt::Noop { .. }
                | Stmt::FuncDecl { .. }
                | Stmt::Break { .. }
                | Stmt::Continue { .. } => {}
                Stmt::Drop { index, .. } => {
                    state.set(*index, BoundType::Null);
                }
                Stmt::ClosureLet { closure, .. } => {
                    let _ = self.infer_expr_type(&closure.body, state);
                }
                Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                    let expr_state = state.clone();
                    self.bind_expr_to_slot(state, *index, expr, &expr_state);
                }
                Stmt::Expr { expr, .. } => {
                    let _ = self.infer_expr_type(expr, state);
                }
                Stmt::IfElse {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    let _ = self.infer_expr_type(condition, state);
                    let mut then_state = state.clone();
                    let mut else_state = state.clone();
                    self.apply_stmts(then_branch, &mut then_state);
                    self.apply_stmts(else_branch, &mut else_state);
                    state.merge_from_branches(&then_state, &else_state);
                }
                Stmt::For {
                    init,
                    condition,
                    post,
                    body,
                    ..
                } => {
                    self.apply_stmts(std::slice::from_ref(init), state);
                    stabilize_loop_state(state, |iterated| {
                        let _ = self.infer_expr_type(condition, iterated);
                        self.apply_stmts(body, iterated);
                        self.apply_stmts(std::slice::from_ref(post), iterated);
                    });
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    stabilize_loop_state(state, |iterated| {
                        let _ = self.infer_expr_type(condition, iterated);
                        self.apply_stmts(body, iterated);
                    });
                }
            }
        }
    }

    pub(super) fn callable_binding_from_expr(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> Option<InferredCallable> {
        match expr {
            Expr::Closure(closure) => Some(InferredCallable::Closure(closure.clone())),
            Expr::FunctionRef(index) => Some(InferredCallable::Function(*index)),
            Expr::Var(slot) | Expr::MoveVar(slot) => state.callable(*slot).cloned(),
            _ => None,
        }
    }

    pub(super) fn bind_expr_to_slot(
        &mut self,
        state: &mut LocalTypeState,
        slot: LocalSlot,
        expr: &Expr,
        expr_state: &LocalTypeState,
    ) -> BoundType {
        if let Some(callable) = self.callable_binding_from_expr(expr, expr_state) {
            state.bind_callable(slot, callable);
            BoundType::Unknown
        } else {
            let ty = self.infer_expr_type(expr, expr_state);
            state.set(slot, ty);
            ty
        }
    }
}
