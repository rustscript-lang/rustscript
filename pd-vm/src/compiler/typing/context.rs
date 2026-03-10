use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;

use super::super::CompileError;
use super::super::ir::{ClosureExpr, Expr, FunctionImpl, LocalSlot, Stmt, TypeSchema};
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
    pub(super) struct_schemas: &'a HashMap<String, TypeSchema>,
    pub(super) function_names: &'a HashMap<u16, String>,
    pub(super) host_import_return_types: &'a HashMap<u16, BoundType>,
    pub(super) host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    pub(super) active_functions: Vec<u16>,
    pub(super) observed_function_param_types: HashMap<u16, Vec<BoundType>>,
    pub(super) observed_function_param_schemas: HashMap<u16, Vec<Option<TypeSchema>>>,
    pub(super) function_param_conflicts: HashMap<u16, String>,
}

impl<'a> TypeContext<'a> {
    pub(super) fn new(
        function_impls: &'a HashMap<u16, FunctionImpl>,
        struct_schemas: &'a HashMap<String, TypeSchema>,
        function_names: &'a HashMap<u16, String>,
        host_import_return_types: &'a HashMap<u16, BoundType>,
        host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    ) -> Self {
        Self {
            function_impls,
            struct_schemas,
            function_names,
            host_import_return_types,
            host_import_signatures,
            active_functions: Vec::new(),
            observed_function_param_types: HashMap::new(),
            observed_function_param_schemas: HashMap::new(),
            function_param_conflicts: HashMap::new(),
        }
    }

    pub(super) fn function_name(&self, index: u16) -> &str {
        self.function_names
            .get(&index)
            .map(String::as_str)
            .unwrap_or("<anonymous>")
    }

    pub(super) fn resolve_struct_schema(&self, name: &str) -> Option<&TypeSchema> {
        self.struct_schemas.get(name)
    }

    pub(super) fn resolve_schema<'b>(&'b self, schema: &'b TypeSchema) -> &'b TypeSchema {
        self.resolve_schema_with_seen(schema, &mut HashSet::new())
    }

    fn resolve_schema_with_seen<'b>(
        &'b self,
        schema: &'b TypeSchema,
        seen: &mut HashSet<&'b str>,
    ) -> &'b TypeSchema {
        let TypeSchema::Named(name) = schema else {
            return schema;
        };
        if !seen.insert(name.as_str()) {
            return schema;
        }
        self.struct_schemas
            .get(name)
            .map(|next| self.resolve_schema_with_seen(next, seen))
            .unwrap_or(schema)
    }

    pub(super) fn expr_has_declared_schema(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> bool {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                state.has_declared_schema(*slot)
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.expr_has_declared_schema(inner, state)
            }
            Expr::Call(index, args) => match BuiltinFunction::from_call_index(*index) {
                Some(BuiltinFunction::Get)
                | Some(BuiltinFunction::Set)
                | Some(BuiltinFunction::Slice)
                | Some(BuiltinFunction::Keys)
                | Some(BuiltinFunction::Len) => args
                    .first()
                    .is_some_and(|container| self.expr_has_declared_schema(container, state)),
                _ => false,
            },
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.expr_has_declared_schema(then_expr, state)
                    || self.expr_has_declared_schema(else_expr, state)
            }
            Expr::Match {
                value_slot,
                value,
                arms,
                default,
                ..
            } => {
                if self.expr_has_declared_schema(value, state) {
                    return true;
                }
                let value_ty = self.infer_expr_type(value, state);
                let mut nested = state.clone();
                bind_expr_result_to_slot(
                    &mut nested,
                    *value_slot,
                    None,
                    value,
                    state,
                    value_ty,
                    self,
                );
                arms.iter()
                    .any(|(_, arm_expr)| self.expr_has_declared_schema(arm_expr, &nested))
                    || self.expr_has_declared_schema(default, &nested)
            }
            Expr::Block { stmts, expr } => {
                let mut nested = state.clone();
                self.apply_stmts(stmts, &mut nested);
                self.expr_has_declared_schema(expr, &nested)
            }
            _ => false,
        }
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
            .map(|arg| (self.infer_expr_type(arg, state), self.infer_expr_schema(arg, state)))
            .collect::<Vec<_>>();
        let mut merged_types = self
            .observed_function_param_types
            .remove(&index)
            .unwrap_or_else(|| vec![BoundType::Unknown; actual.len()]);
        let mut merged_schemas = self
            .observed_function_param_schemas
            .remove(&index)
            .unwrap_or_else(|| vec![None; actual.len()]);
        if merged_types.len() < actual.len() {
            merged_types.resize(actual.len(), BoundType::Unknown);
        }
        if merged_schemas.len() < actual.len() {
            merged_schemas.resize(actual.len(), None);
        }
        for (arg_index, (actual_ty, actual_schema)) in actual.into_iter().enumerate() {
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
            merged_schemas[arg_index] =
                merge_observed_function_param_schema(merged_schemas[arg_index].clone(), actual_schema);
        }
        self.observed_function_param_types
            .insert(index, merged_types);
        self.observed_function_param_schemas
            .insert(index, merged_schemas);
    }

    pub(super) fn infer_expr_schema(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        match expr {
            Expr::Null => Some(TypeSchema::Null),
            Expr::Int(_) => Some(TypeSchema::Int),
            Expr::Float(_) => Some(TypeSchema::Float),
            Expr::Bool(_) => Some(TypeSchema::Bool),
            Expr::String(_) => Some(TypeSchema::String),
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.infer_expr_schema(inner, state)
            }
            Expr::Var(slot) | Expr::MoveVar(slot) => state
                .schema(*slot)
                .cloned(),
            Expr::Call(index, args) => {
                let builtin = BuiltinFunction::from_call_index(*index)?;
                self.infer_builtin_call_schema(builtin, args, state)
            }
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                let then_schema = self.infer_expr_schema(then_expr, state);
                let else_schema = self.infer_expr_schema(else_expr, state);
                match (then_schema, else_schema) {
                    (Some(TypeSchema::Null), rhs) => rhs,
                    (lhs, Some(TypeSchema::Null)) => lhs,
                    (Some(lhs), Some(rhs)) if lhs == rhs => Some(lhs),
                    _ => None,
                }
            }
            Expr::Match {
                value_slot,
                value,
                arms,
                default,
                ..
            } => {
                let value_ty = self.infer_expr_type(value, state);
                let mut nested = state.clone();
                bind_expr_result_to_slot(
                    &mut nested,
                    *value_slot,
                    None,
                    value,
                    state,
                    value_ty,
                    self,
                );
                let default_schema = self.infer_expr_schema(default, &nested);
                let mut arm_schema = None;
                for (_, arm_expr) in arms {
                    let current = self.infer_expr_schema(arm_expr, &nested);
                    match (&arm_schema, &current) {
                        (None, _) => arm_schema = current,
                        (Some(lhs), Some(rhs)) if lhs == rhs => {}
                        _ => return None,
                    }
                }
                match (arm_schema, default_schema) {
                    (None, rhs) => rhs,
                    (Some(lhs), Some(TypeSchema::Null)) => Some(lhs),
                    (Some(TypeSchema::Null), rhs) => rhs,
                    (Some(lhs), Some(rhs)) if lhs == rhs => Some(lhs),
                    _ => None,
                }
            }
            Expr::Block { stmts, expr } => {
                let mut nested = state.clone();
                self.apply_stmts(stmts, &mut nested);
                self.infer_expr_schema(expr, &nested)
            }
            _ => None,
        }
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
                bind_expr_result_to_slot(
                    &mut nested,
                    *value_slot,
                    None,
                    value,
                    state,
                    value_ty,
                    self,
                );
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
            BuiltinFunction::Get if args.len() == 2 => {
                self.infer_get_return_type(&args[0], &args[1], state)
            }
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

    fn infer_builtin_call_schema(
        &mut self,
        builtin: BuiltinFunction,
        args: &[Expr],
        state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        match builtin {
            BuiltinFunction::ArrayNew if args.is_empty() => {
                Some(TypeSchema::Array(Box::new(TypeSchema::Unknown)))
            }
            BuiltinFunction::MapNew if args.is_empty() => Some(TypeSchema::Object(HashMap::new())),
            BuiltinFunction::ArrayPush if args.len() == 2 => {
                let array_schema = self.infer_expr_schema(&args[0], state);
                let value_schema = self
                    .infer_expr_schema(&args[1], state)
                    .or_else(|| schema_from_bound_type(self.infer_expr_type(&args[1], state)));
                Some(merge_array_schema(array_schema, value_schema))
            }
            BuiltinFunction::Set if args.len() == 3 => {
                let container_schema = self.infer_expr_schema(&args[0], state);
                let value_schema = self
                    .infer_expr_schema(&args[2], state)
                    .or_else(|| schema_from_bound_type(self.infer_expr_type(&args[2], state)));
                infer_set_schema(container_schema, &args[1], value_schema)
            }
            BuiltinFunction::Get if args.len() == 2 => self
                .infer_expr_schema(&args[0], state)
                .and_then(|schema| infer_access_schema(&schema, &args[1], self, state).ok()),
            BuiltinFunction::Slice if args.len() == 3 => match self.infer_expr_schema(&args[0], state)
            {
                Some(TypeSchema::Array(element)) => Some(TypeSchema::Array(element)),
                Some(TypeSchema::String) => Some(TypeSchema::String),
                _ => None,
            },
            _ => None,
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
        key: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        if let Some(schema) = self
            .infer_expr_schema(container, state)
            .and_then(|schema| infer_access_schema(&schema, key, self, state).ok())
        {
            return bound_type_from_schema(&schema);
        }
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
        let Some(mut nested) =
            self.build_callable_state(param_slots, capture_copies, Some(args), caller_state)
        else {
            return BoundType::Unknown;
        };
        self.apply_stmts(body_stmts, &mut nested);
        self.infer_expr_type(body_expr, &nested)
    }

    pub(super) fn build_callable_state(
        &mut self,
        param_slots: &[LocalSlot],
        capture_copies: &[(LocalSlot, LocalSlot)],
        args: Option<&[Expr]>,
        caller_state: &LocalTypeState,
    ) -> Option<LocalTypeState> {
        let mut nested = LocalTypeState::default();
        for (source_slot, captured_slot) in capture_copies {
            nested.copy_binding_from(caller_state, *source_slot, *captured_slot, None, false);
        }
        if let Some(args) = args {
            if param_slots.len() != args.len() {
                return None;
            }
            for (arg, slot) in args.iter().zip(param_slots.iter()) {
                self.bind_expr_to_slot(&mut nested, *slot, None, arg, caller_state);
            }
        }
        Some(nested)
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
                Stmt::Let {
                    index,
                    declared_struct,
                    expr,
                    ..
                } => {
                    let expr_state = state.clone();
                    let declared_schema = declared_struct
                        .as_deref()
                        .and_then(|name| self.resolve_struct_schema(name))
                        .cloned();
                    self.bind_expr_to_slot(state, *index, declared_schema.as_ref(), expr, &expr_state);
                }
                Stmt::Assign { index, expr, .. } => {
                    let expr_state = state.clone();
                    self.bind_expr_to_slot(state, *index, None, expr, &expr_state);
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
        declared_schema: Option<&TypeSchema>,
        expr: &Expr,
        expr_state: &LocalTypeState,
    ) -> BoundType {
        if let Some(callable) = self.callable_binding_from_expr(expr, expr_state) {
            state.bind_callable(slot, callable);
            BoundType::Unknown
        } else {
            let ty = self.infer_expr_type(expr, expr_state);
            let slot_declared_schema = declared_schema
                .cloned()
                .or_else(|| expr_state.has_declared_schema(slot).then(|| expr_state.schema(slot).cloned()).flatten());
            let schema = slot_declared_schema
                .clone()
                .or_else(|| self.infer_expr_schema(expr, expr_state));
            let from_declared_schema = slot_declared_schema.is_some()
                || expr_state.has_declared_schema(slot)
                || self.expr_has_declared_schema(expr, expr_state);
            let ty = slot_declared_schema
                .as_ref()
                .map(bound_type_from_schema)
                .unwrap_or(ty);
            state.set_with_schema_origin(slot, ty, schema, from_declared_schema);
            ty
        }
    }
}

fn merge_observed_function_param_schema(
    current: Option<TypeSchema>,
    next: Option<TypeSchema>,
) -> Option<TypeSchema> {
    match (current, next) {
        (None, rhs) => rhs,
        (lhs, None) => lhs,
        (Some(lhs), Some(rhs)) if lhs == rhs => Some(lhs),
        _ => None,
    }
}

fn schema_from_bound_type(ty: BoundType) -> Option<TypeSchema> {
    match ty {
        BoundType::Unknown => None,
        BoundType::Null => Some(TypeSchema::Null),
        BoundType::Int => Some(TypeSchema::Int),
        BoundType::Float => Some(TypeSchema::Float),
        BoundType::Bool => Some(TypeSchema::Bool),
        BoundType::String => Some(TypeSchema::String),
        BoundType::Array | BoundType::ArrayOf(_) => {
            Some(TypeSchema::Array(Box::new(TypeSchema::Unknown)))
        }
        BoundType::Map | BoundType::MapOf(_) => Some(TypeSchema::Map(Box::new(TypeSchema::Unknown))),
    }
}

pub(crate) fn bound_type_from_schema(schema: &TypeSchema) -> BoundType {
    match schema {
        TypeSchema::Unknown => BoundType::Unknown,
        TypeSchema::Null => BoundType::Null,
        TypeSchema::Int => BoundType::Int,
        TypeSchema::Float => BoundType::Float,
        TypeSchema::Bool => BoundType::Bool,
        TypeSchema::String => BoundType::String,
        TypeSchema::Named(_) => BoundType::Map,
        TypeSchema::Array(_) => BoundType::Array,
        TypeSchema::Map(_) | TypeSchema::Object(_) => BoundType::Map,
    }
}

fn merge_array_schema(current: Option<TypeSchema>, next: Option<TypeSchema>) -> TypeSchema {
    let existing = match current {
        Some(TypeSchema::Array(existing)) => *existing,
        _ => TypeSchema::Unknown,
    };
    let merged = match (existing.clone(), next) {
        (TypeSchema::Unknown, Some(next)) => next,
        (existing, Some(next)) if existing == next => existing,
        (existing, None) => existing,
        _ => TypeSchema::Unknown,
    };
    TypeSchema::Array(Box::new(merged))
}

fn infer_set_schema(
    container: Option<TypeSchema>,
    key: &Expr,
    value: Option<TypeSchema>,
) -> Option<TypeSchema> {
    match container {
        Some(TypeSchema::Named(name)) => Some(TypeSchema::Named(name)),
        Some(TypeSchema::Object(mut fields)) => {
            let Expr::String(name) = key else {
                return Some(TypeSchema::Map(Box::new(TypeSchema::Unknown)));
            };
            fields.insert(name.clone(), value.unwrap_or(TypeSchema::Unknown));
            Some(TypeSchema::Object(fields))
        }
        Some(TypeSchema::Map(existing)) => {
            let merged = match (*existing, value) {
                (TypeSchema::Unknown, Some(next)) => next,
                (existing, Some(next)) if existing == next => existing,
                (existing, None) => existing,
                _ => TypeSchema::Unknown,
            };
            Some(TypeSchema::Map(Box::new(merged)))
        }
        Some(TypeSchema::Array(existing)) => {
            let merged = match (*existing, value) {
                (TypeSchema::Unknown, Some(next)) => next,
                (existing, Some(next)) if existing == next => existing,
                (existing, None) => existing,
                _ => TypeSchema::Unknown,
            };
            Some(TypeSchema::Array(Box::new(merged)))
        }
        Some(other) => Some(other),
        None => match key {
            Expr::String(name) => {
                let mut fields = HashMap::new();
                fields.insert(name.clone(), value.unwrap_or(TypeSchema::Unknown));
                Some(TypeSchema::Object(fields))
            }
            Expr::Int(_) => Some(TypeSchema::Map(Box::new(
                value.unwrap_or(TypeSchema::Unknown),
            ))),
            _ => Some(TypeSchema::Map(Box::new(TypeSchema::Unknown))),
        },
    }
}

pub(super) fn infer_access_schema(
    schema: &TypeSchema,
    key: &Expr,
    context: &mut TypeContext<'_>,
    state: &LocalTypeState,
) -> Result<TypeSchema, String> {
    match context.resolve_schema(schema).clone() {
        TypeSchema::Named(name) => Err(format!("unknown struct schema '{name}'")),
        TypeSchema::Object(fields) => match key {
            Expr::String(name) => fields
                .get(name)
                .cloned()
                .ok_or_else(|| format!("field '{name}' is not declared in the object schema")),
            other => {
                let key_ty = context.infer_expr_type(other, state);
                if matches!(key_ty, BoundType::String | BoundType::Unknown) {
                    Ok(collapse_object_field_schema(&fields))
                } else {
                    Err(format!(
                        "schema-typed object access requires a string field name, got {}",
                        bound_type_label(key_ty)
                    ))
                }
            }
        },
        TypeSchema::Array(element) => {
            let key_ty = context.infer_expr_type(key, state);
            if matches!(key_ty, BoundType::Unknown | BoundType::Int) {
                Ok((*element).clone())
            } else {
                Err(format!(
                    "schema-typed array access requires an int index, got {}",
                    bound_type_label(key_ty)
                ))
            }
        }
        TypeSchema::Map(value) => Ok((*value).clone()),
        other => Err(format!(
            "cannot access fields on schema type '{}'",
            schema_label(&other)
        )),
    }
}

fn collapse_object_field_schema(fields: &HashMap<String, TypeSchema>) -> TypeSchema {
    let mut values = fields.values();
    let Some(first) = values.next() else {
        return TypeSchema::Unknown;
    };
    if values.all(|schema| schema == first) {
        first.clone()
    } else {
        TypeSchema::Unknown
    }
}

pub(super) fn schema_label(schema: &TypeSchema) -> &'static str {
    match schema {
        TypeSchema::Unknown => "unknown",
        TypeSchema::Null => "null",
        TypeSchema::Int => "int",
        TypeSchema::Float => "float",
        TypeSchema::Bool => "bool",
        TypeSchema::String => "string",
        TypeSchema::Named(_) => "map",
        TypeSchema::Array(_) => "array",
        TypeSchema::Map(_) | TypeSchema::Object(_) => "map",
    }
}
