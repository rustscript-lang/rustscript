use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;

use super::super::CompileError;
use super::super::TypingMode;
use super::super::ir::{
    ClosureExpr, Expr, FunctionDecl, FunctionImpl, LocalSlot, Stmt, StructDecl, TypeSchema,
};
use super::helpers::{
    bind_expr_result_to_slot, bound_type_label, display_name_for_builtin,
    function_body_contains_param_add, infer_binary_type, infer_unary_type, is_numeric_bound_type,
    merge_observed_function_param_type, refine_state_for_match_pattern,
};
use super::state::{
    BoundType, HostCallableSignature, InferredCallable, LocalTypeState, SimpleType,
    merge_bound_types, merge_container_element_types, stabilize_loop_state,
};
use super::validate::refine_state_for_condition;
use super::validate::{
    DiagnosticSite, validate_function_argument_schemas, validate_host_signature,
    validate_json_encode_argument, validate_signature_overloads,
};

pub(super) struct TypeContext<'a> {
    pub(super) function_impls: &'a HashMap<u16, FunctionImpl>,
    pub(super) function_decls: &'a HashMap<u16, FunctionDecl>,
    pub(super) struct_schemas: &'a HashMap<String, StructDecl>,
    pub(super) function_names: &'a HashMap<u16, String>,
    pub(super) host_import_return_types: &'a HashMap<u16, BoundType>,
    pub(super) host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    pub(super) typing_mode: TypingMode,
    pub(super) active_functions: Vec<(u16, Vec<TypeSchema>)>,
    pub(super) generic_bindings: Vec<HashMap<String, TypeSchema>>,
    pub(super) observed_function_param_types: HashMap<u16, Vec<BoundType>>,
    pub(super) observed_function_param_schemas: HashMap<u16, Vec<Option<TypeSchema>>>,
    pub(super) observed_function_param_callables: HashMap<u16, Vec<Option<InferredCallable>>>,
    pub(super) observed_function_param_capture_states: HashMap<u16, Vec<Option<LocalTypeState>>>,
    pub(super) observed_function_capture_states: HashMap<u16, LocalTypeState>,
    pub(super) function_param_conflicts: HashMap<u16, String>,
    observed_return_types: HashMap<(u16, Vec<String>), BoundType>,
    observed_return_schemas: HashMap<(u16, Vec<String>), Option<TypeSchema>>,
    observed_optional_returns: HashMap<u16, bool>,
    active_observed_returns: Vec<(u16, Vec<String>)>,
    active_optional_returns: Vec<u16>,
}

struct CallableBody<'a> {
    param_slots: &'a [LocalSlot],
    param_schemas: Option<&'a [Option<TypeSchema>]>,
    capture_copies: &'a [(LocalSlot, LocalSlot)],
    body_stmts: &'a [Stmt],
    body_expr: &'a Expr,
}

impl<'a> TypeContext<'a> {
    pub(super) fn new(
        function_impls: &'a HashMap<u16, FunctionImpl>,
        function_decls: &'a HashMap<u16, FunctionDecl>,
        struct_schemas: &'a HashMap<String, StructDecl>,
        function_names: &'a HashMap<u16, String>,
        host_import_return_types: &'a HashMap<u16, BoundType>,
        host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
        typing_mode: TypingMode,
    ) -> Self {
        Self {
            function_impls,
            function_decls,
            struct_schemas,
            function_names,
            host_import_return_types,
            host_import_signatures,
            typing_mode,
            active_functions: Vec::new(),
            generic_bindings: Vec::new(),
            observed_function_param_types: HashMap::new(),
            observed_function_param_schemas: HashMap::new(),
            observed_function_param_callables: HashMap::new(),
            observed_function_param_capture_states: HashMap::new(),
            observed_function_capture_states: HashMap::new(),
            function_param_conflicts: HashMap::new(),
            observed_return_types: HashMap::new(),
            observed_return_schemas: HashMap::new(),
            observed_optional_returns: HashMap::new(),
            active_observed_returns: Vec::new(),
            active_optional_returns: Vec::new(),
        }
    }

    pub(super) fn is_strict(&self) -> bool {
        self.typing_mode.is_strict()
    }

    pub(super) fn function_name(&self, index: u16) -> &str {
        self.function_names
            .get(&index)
            .map(String::as_str)
            .unwrap_or("<anonymous>")
    }

    pub(super) fn resolve_schema(&mut self, schema: &TypeSchema) -> TypeSchema {
        self.resolve_schema_with_seen(schema, &mut HashSet::new())
    }

    pub(super) fn substitute_schema_generics(&self, schema: &TypeSchema) -> TypeSchema {
        match schema {
            TypeSchema::GenericParam(name) => self
                .resolve_generic_binding(name)
                .cloned()
                .unwrap_or_else(|| schema.clone()),
            TypeSchema::Named(name, type_args) => TypeSchema::Named(
                name.clone(),
                type_args
                    .iter()
                    .map(|arg| self.substitute_schema_generics(arg))
                    .collect(),
            ),
            TypeSchema::Array(element) => {
                TypeSchema::Array(Box::new(self.substitute_schema_generics(element)))
            }
            TypeSchema::ArrayTuple(items) => TypeSchema::ArrayTuple(
                items
                    .iter()
                    .map(|item| self.substitute_schema_generics(item))
                    .collect(),
            ),
            TypeSchema::ArrayTupleRest { prefix, rest } => TypeSchema::ArrayTupleRest {
                prefix: prefix
                    .iter()
                    .map(|item| self.substitute_schema_generics(item))
                    .collect(),
                rest: Box::new(self.substitute_schema_generics(rest)),
            },
            TypeSchema::Map(value) => {
                TypeSchema::Map(Box::new(self.substitute_schema_generics(value)))
            }
            TypeSchema::Optional(inner) => {
                TypeSchema::Optional(Box::new(self.substitute_schema_generics(inner)))
            }
            TypeSchema::Object(fields) => TypeSchema::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), self.substitute_schema_generics(value)))
                    .collect(),
            ),
            TypeSchema::Callable { params, result } => TypeSchema::Callable {
                params: params
                    .iter()
                    .map(|param| self.substitute_schema_generics(param))
                    .collect(),
                result: Box::new(self.substitute_schema_generics(result)),
            },
            _ => schema.clone(),
        }
    }

    fn resolve_schema_with_seen(
        &mut self,
        schema: &TypeSchema,
        seen: &mut HashSet<String>,
    ) -> TypeSchema {
        match schema {
            TypeSchema::GenericParam(name) => {
                let bound = self.resolve_generic_binding(name).cloned();
                bound.map_or_else(
                    || schema.clone(),
                    |bound| {
                        if bound == *schema {
                            schema.clone()
                        } else {
                            self.resolve_schema_with_seen(&bound, seen)
                        }
                    },
                )
            }
            TypeSchema::Named(name, type_args) => {
                let substituted_args = type_args
                    .iter()
                    .map(|arg| self.resolve_schema_with_seen(arg, seen))
                    .collect::<Vec<_>>();
                let Some(decl) = self.struct_schemas.get(name) else {
                    return TypeSchema::Named(name.clone(), substituted_args);
                };
                if decl.type_params.len() != substituted_args.len() {
                    return TypeSchema::Named(name.clone(), substituted_args);
                }
                let key =
                    render_schema_label(&TypeSchema::Named(name.clone(), substituted_args.clone()));
                if !seen.insert(key.clone()) {
                    return TypeSchema::Named(name.clone(), substituted_args);
                }
                self.push_generic_bindings(&decl.type_params, &substituted_args);
                let resolved = self.resolve_schema_with_seen(&decl.body_schema, seen);
                self.pop_generic_bindings();
                seen.remove(&key);
                resolved
            }
            TypeSchema::Array(element) => {
                TypeSchema::Array(Box::new(self.resolve_schema_with_seen(element, seen)))
            }
            TypeSchema::ArrayTuple(items) => TypeSchema::ArrayTuple(
                items
                    .iter()
                    .map(|item| self.resolve_schema_with_seen(item, seen))
                    .collect(),
            ),
            TypeSchema::ArrayTupleRest { prefix, rest } => TypeSchema::ArrayTupleRest {
                prefix: prefix
                    .iter()
                    .map(|item| self.resolve_schema_with_seen(item, seen))
                    .collect(),
                rest: Box::new(self.resolve_schema_with_seen(rest, seen)),
            },
            TypeSchema::Map(value) => {
                TypeSchema::Map(Box::new(self.resolve_schema_with_seen(value, seen)))
            }
            TypeSchema::Optional(inner) => {
                TypeSchema::Optional(Box::new(self.resolve_schema_with_seen(inner, seen)))
            }
            TypeSchema::Object(fields) => TypeSchema::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), self.resolve_schema_with_seen(value, seen)))
                    .collect(),
            ),
            TypeSchema::Callable { params, result } => TypeSchema::Callable {
                params: params
                    .iter()
                    .map(|param| self.resolve_schema_with_seen(param, seen))
                    .collect(),
                result: Box::new(self.resolve_schema_with_seen(result, seen)),
            },
            _ => schema.clone(),
        }
    }

    pub(super) fn bound_type_for_schema(&mut self, schema: &TypeSchema) -> BoundType {
        bound_type_from_schema(&self.resolve_schema(schema))
    }

    fn resolve_generic_binding(&self, name: &str) -> Option<&TypeSchema> {
        self.generic_bindings
            .iter()
            .rev()
            .find_map(|bindings| bindings.get(name))
    }

    fn push_generic_bindings(&mut self, type_params: &[String], type_args: &[TypeSchema]) {
        self.generic_bindings.push(
            type_params
                .iter()
                .cloned()
                .zip(type_args.iter().cloned())
                .collect(),
        );
    }

    fn pop_generic_bindings(&mut self) {
        self.generic_bindings.pop();
    }

    pub(super) fn expr_has_declared_schema(&mut self, expr: &Expr, state: &LocalTypeState) -> bool {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => state.has_declared_schema(*slot),
            Expr::OptionalGet { container, .. } => self.expr_has_declared_schema(container, state),
            Expr::OptionUnwrapOr { value, .. } => self.expr_has_declared_schema(value, state),
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.expr_has_declared_schema(inner, state)
            }
            Expr::Call(index, _, args) => match BuiltinFunction::from_call_index(*index) {
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
                arms.iter().any(|(pattern, arm_expr)| {
                    let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                    self.expr_has_declared_schema(arm_expr, &arm_state)
                }) || self.expr_has_declared_schema(default, &nested)
            }
            Expr::Block { stmts, expr } => {
                let mut nested = state.clone();
                self.apply_stmts(stmts, &mut nested);
                self.expr_has_declared_schema(expr, &nested)
            }
            _ => false,
        }
    }

    pub(super) fn expr_has_struct_schema_source(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> bool {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                state.has_declared_schema(*slot)
                    || matches!(
                        state.schema(*slot),
                        Some(TypeSchema::Named(_, _) | TypeSchema::GenericParam(_))
                    )
            }
            Expr::OptionalGet { container, .. } => {
                self.expr_has_struct_schema_source(container, state)
            }
            Expr::OptionUnwrapOr { value, .. } => self.expr_has_struct_schema_source(value, state),
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.expr_has_struct_schema_source(inner, state)
            }
            Expr::Call(index, _, args) => match BuiltinFunction::from_call_index(*index) {
                Some(BuiltinFunction::Get)
                | Some(BuiltinFunction::Set)
                | Some(BuiltinFunction::Slice)
                | Some(BuiltinFunction::Keys)
                | Some(BuiltinFunction::Len) => args
                    .first()
                    .is_some_and(|container| self.expr_has_struct_schema_source(container, state)),
                _ => false,
            },
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => {
                self.expr_has_struct_schema_source(then_expr, state)
                    || self.expr_has_struct_schema_source(else_expr, state)
            }
            Expr::Match {
                value_slot,
                value,
                arms,
                default,
                ..
            } => {
                if self.expr_has_struct_schema_source(value, state) {
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
                arms.iter().any(|(pattern, arm_expr)| {
                    let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                    self.expr_has_struct_schema_source(arm_expr, &arm_state)
                }) || self.expr_has_struct_schema_source(default, &nested)
            }
            Expr::Block { stmts, expr } => {
                let mut nested = state.clone();
                self.apply_stmts(stmts, &mut nested);
                self.expr_has_struct_schema_source(expr, &nested)
            }
            _ => false,
        }
    }

    pub(super) fn expr_is_optional(&mut self, expr: &Expr, state: &LocalTypeState) -> bool {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => state.is_optional(*slot),
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.is_optional(*root),
            Expr::OptionalGet { .. } => true,
            Expr::OptionUnwrapOr { .. } => false,
            Expr::Call(index, _, _) => {
                BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::ReFind)
                    || self.function_returns_optional(*index)
            }
            Expr::LocalCall(slot, _, _) => match state.callable(*slot) {
                Some(InferredCallable::Function(index)) => {
                    BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::ReFind)
                        || self.function_returns_optional(*index)
                }
                _ => false,
            },
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.expr_is_optional(inner, state)
            }
            Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } => self.expr_is_optional(then_expr, state) || self.expr_is_optional(else_expr, state),
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
                arms.iter().any(|(pattern, arm_expr)| {
                    let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                    self.expr_is_optional(arm_expr, &arm_state)
                }) || self.expr_is_optional(default, &nested)
            }
            Expr::Block { stmts, expr } => {
                let mut nested = state.clone();
                self.apply_stmts(stmts, &mut nested);
                self.expr_is_optional(expr, &nested)
            }
            _ => false,
        }
    }

    fn function_returns_optional(&mut self, index: u16) -> bool {
        if let Some(optional) = self.observed_optional_returns.get(&index).copied() {
            return optional;
        }
        let Some(function_decl) = self.function_decls.get(&index).cloned() else {
            return false;
        };
        if function_decl
            .return_schema
            .as_ref()
            .is_some_and(TypeSchema::is_optional)
        {
            self.observed_optional_returns.insert(index, true);
            return true;
        }
        if crate::builtins::default_host_callable(&function_decl.name)
            .is_some_and(|callable| callable.signature.return_type.contains("| null"))
        {
            self.observed_optional_returns.insert(index, true);
            return true;
        }
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return false;
        };
        if self.active_optional_returns.contains(&index) {
            return false;
        }
        self.active_optional_returns.push(index);
        let optional = self
            .build_observed_function_state(index)
            .map(|mut nested| {
                self.apply_stmts(&function_impl.body_stmts, &mut nested);
                self.expr_is_optional(&function_impl.body_expr, &nested)
            })
            .unwrap_or(false);
        self.active_optional_returns.pop();
        self.observed_optional_returns.insert(index, optional);
        optional
    }

    pub(super) fn infer_optional_expr_inner_type(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
            Expr::OptionalGet { container, key, .. } => {
                self.infer_get_return_type_from_container(container, key, state)
            }
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                let value_ty = self.infer_optional_expr_inner_type(value, state);
                let fallback_ty = self.infer_expr_type(fallback, state);
                if value_ty == BoundType::Unknown {
                    fallback_ty
                } else if fallback_ty == BoundType::Unknown || fallback_ty == value_ty {
                    value_ty
                } else {
                    merge_bound_types(value_ty, fallback_ty)
                }
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.infer_optional_expr_inner_type(inner, state)
            }
            _ => self.infer_expr_type(expr, state),
        }
    }

    pub(super) fn infer_optional_expr_inner_schema(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        match expr {
            Expr::Var(slot) | Expr::MoveVar(slot) => state.schema(*slot).cloned(),
            Expr::OptionalGet { container, key, .. } => self
                .infer_expr_schema(container, state)
                .and_then(|schema| infer_access_schema(&schema, key, self, state).ok()),
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                let value_schema = self.infer_optional_expr_inner_schema(value, state);
                let fallback_schema = self.infer_expr_schema(fallback, state);
                match (value_schema, fallback_schema) {
                    (None, rhs) => rhs,
                    (lhs, None) => lhs,
                    (Some(lhs), Some(rhs)) if lhs == rhs => Some(lhs),
                    _ => None,
                }
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.infer_optional_expr_inner_schema(inner, state)
            }
            _ => self.infer_expr_schema(expr, state),
        }
    }

    fn infer_get_return_type_from_container(
        &mut self,
        container: &Expr,
        key: &Expr,
        state: &LocalTypeState,
    ) -> BoundType {
        let container_schema = if self.expr_is_optional(container, state) {
            self.infer_optional_expr_inner_schema(container, state)
        } else {
            self.infer_expr_schema(container, state)
        };
        if let Some(schema) =
            container_schema.and_then(|schema| infer_access_schema(&schema, key, self, state).ok())
        {
            return bound_type_from_schema(&schema);
        }

        let container_ty = if self.expr_is_optional(container, state) {
            self.infer_optional_expr_inner_type(container, state)
        } else {
            self.infer_expr_type(container, state)
        };
        match container_ty {
            BoundType::String => BoundType::String,
            BoundType::Bytes => BoundType::Int,
            BoundType::ArrayOf(Some(element_type)) | BoundType::MapOf(Some(element_type)) => {
                BoundType::from_simple(element_type)
            }
            _ => BoundType::Unknown,
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
            .map(|arg| {
                (
                    self.infer_expr_type(arg, state),
                    self.infer_expr_schema(arg, state),
                    self.callable_binding_from_expr(arg, state),
                    self.callable_capture_state_from_expr(arg, state),
                )
            })
            .collect::<Vec<_>>();
        let mut merged_types = self
            .observed_function_param_types
            .remove(&index)
            .unwrap_or_else(|| vec![BoundType::Unknown; actual.len()]);
        let mut merged_schemas = self
            .observed_function_param_schemas
            .remove(&index)
            .unwrap_or_else(|| vec![None; actual.len()]);
        let mut merged_callables = self
            .observed_function_param_callables
            .remove(&index)
            .unwrap_or_else(|| vec![None; actual.len()]);
        let mut merged_capture_states = self
            .observed_function_param_capture_states
            .remove(&index)
            .unwrap_or_else(|| vec![None; actual.len()]);
        if merged_types.len() < actual.len() {
            merged_types.resize(actual.len(), BoundType::Unknown);
        }
        if merged_schemas.len() < actual.len() {
            merged_schemas.resize(actual.len(), None);
        }
        if merged_callables.len() < actual.len() {
            merged_callables.resize(actual.len(), None);
        }
        if merged_capture_states.len() < actual.len() {
            merged_capture_states.resize(actual.len(), None);
        }
        for (arg_index, (actual_ty, actual_schema, actual_callable, actual_capture_state)) in
            actual.into_iter().enumerate()
        {
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
            merged_schemas[arg_index] = merge_observed_function_param_schema(
                merged_schemas[arg_index].clone(),
                actual_schema,
            );
            merged_callables[arg_index] = merge_observed_function_param_callable(
                merged_callables[arg_index].clone(),
                actual_callable,
            );
            merged_capture_states[arg_index] = merge_observed_capture_state(
                merged_capture_states[arg_index].take(),
                actual_capture_state,
            );
        }
        self.observed_function_param_types
            .insert(index, merged_types);
        self.observed_function_param_schemas
            .insert(index, merged_schemas);
        self.observed_function_param_callables
            .insert(index, merged_callables);
        self.observed_function_param_capture_states
            .insert(index, merged_capture_states);
    }

    pub(super) fn observe_function_capture_state(
        &mut self,
        index: u16,
        capture_copies: &[(LocalSlot, LocalSlot)],
        state: &LocalTypeState,
    ) {
        let mut observed = LocalTypeState::default();
        for (source_slot, captured_slot) in capture_copies {
            observed.copy_binding_from(state, *source_slot, *captured_slot, None, false);
        }

        if let Some(existing) = self.observed_function_capture_states.remove(&index) {
            let mut merged = LocalTypeState::default();
            merged.merge_from_branches(&existing, &observed);
            self.observed_function_capture_states.insert(index, merged);
        } else {
            self.observed_function_capture_states
                .insert(index, observed);
        }
    }

    fn callable_capture_state_from_expr(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> Option<LocalTypeState> {
        let callable = self.callable_binding_from_expr(expr, state)?;
        self.capture_state_for_callable(&callable, state)
    }

    fn capture_state_for_callable(
        &self,
        callable: &InferredCallable,
        state: &LocalTypeState,
    ) -> Option<LocalTypeState> {
        let capture_copies = match callable {
            InferredCallable::Function(index) => self
                .function_impls
                .get(index)
                .map(|function_impl| function_impl.capture_copies.as_slice())?,
            InferredCallable::Closure(closure) => closure.capture_copies.as_slice(),
        };
        if capture_copies.is_empty() {
            return None;
        }
        let mut captured = LocalTypeState::default();
        for (source_slot, _) in capture_copies {
            captured.copy_binding_from(state, *source_slot, *source_slot, None, false);
        }
        Some(captured)
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
            Expr::Bytes(_) => Some(TypeSchema::Bytes),
            Expr::String(_) => Some(TypeSchema::String),
            Expr::OptionalGet { container, key, .. } => self
                .infer_expr_schema(container, state)
                .and_then(|schema| infer_access_schema(&schema, key, self, state).ok()),
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                let value_schema = self.infer_optional_expr_inner_schema(value, state);
                let fallback_schema = self.infer_expr_schema(fallback, state);
                match (value_schema, fallback_schema) {
                    (None, rhs) => rhs,
                    (lhs, None) => lhs,
                    (Some(lhs), Some(rhs)) if lhs == rhs => Some(lhs),
                    _ => None,
                }
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.infer_expr_schema(inner, state)
            }
            Expr::Var(slot) | Expr::MoveVar(slot) => state.schema(*slot).cloned(),
            Expr::Call(index, type_args, args) => {
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    self.infer_builtin_call_schema(builtin, type_args, args, state)
                } else {
                    self.infer_named_call_schema(*index, type_args, args, state)
                }
            }
            Expr::LocalCall(slot, type_args, args) => match state.callable(*slot).cloned() {
                Some(InferredCallable::Function(index)) => {
                    self.infer_named_call_schema(index, type_args, args, state)
                }
                Some(InferredCallable::Closure(closure)) => {
                    self.infer_closure_return_schema(&closure, args, state)
                }
                None => self.infer_declared_callable_call_schema(*slot, args, state),
            },
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
                for (pattern, arm_expr) in arms {
                    let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                    let current = self.infer_expr_schema(arm_expr, &arm_state);
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
            Expr::Bytes(_) => BoundType::Bytes,
            Expr::String(_) => BoundType::String,
            Expr::OptionalGet { .. } => BoundType::Unknown,
            Expr::OptionUnwrapOr {
                value, fallback, ..
            } => {
                let value_ty = self.infer_optional_expr_inner_type(value, state);
                let fallback_ty = self.infer_expr_type(fallback, state);
                if value_ty == BoundType::Unknown {
                    fallback_ty
                } else if fallback_ty == BoundType::Unknown || value_ty == fallback_ty {
                    value_ty
                } else {
                    merge_bound_types(value_ty, fallback_ty)
                }
            }
            Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                if self.expr_is_optional(inner, state) {
                    BoundType::Unknown
                } else {
                    self.infer_expr_type(inner, state)
                }
            }
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                if state.is_optional(*slot) {
                    BoundType::Unknown
                } else {
                    state.get(*slot)
                }
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                if state.is_optional(*root) {
                    BoundType::Unknown
                } else {
                    state.get(*root)
                }
            }
            Expr::FunctionRef(_) | Expr::Call(..) | Expr::LocalCall(..) | Expr::Closure(_) => {
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
                for (pattern, arm_expr) in arms {
                    let arm_state = refine_state_for_match_pattern(&nested, pattern, *value_slot);
                    let ty = self.infer_expr_type(arm_expr, &arm_state);
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
            Expr::Call(index, type_args, args) => {
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    self.infer_builtin_call_like_expr_type(builtin, type_args, args, state)
                } else {
                    if let Some(decl) = self.function_decls.get(index)
                        && let inferred =
                            infer_host_passthrough_return_type(&decl.name, args, state, self)
                        && inferred != BoundType::Unknown
                    {
                        inferred
                    } else {
                        let inferred = self.infer_function_return(*index, type_args, args, state);
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
            }
            Expr::LocalCall(slot, type_args, args) => match state.callable(*slot).cloned() {
                Some(InferredCallable::Function(index)) => {
                    if let Some(decl) = self.function_decls.get(&index)
                        && let inferred =
                            infer_host_passthrough_return_type(&decl.name, args, state, self)
                        && inferred != BoundType::Unknown
                    {
                        inferred
                    } else {
                        let inferred = self.infer_function_return(index, type_args, args, state);
                        if inferred != BoundType::Unknown {
                            inferred
                        } else {
                            self.host_import_return_types
                                .get(&index)
                                .copied()
                                .unwrap_or(BoundType::Unknown)
                        }
                    }
                }
                Some(InferredCallable::Closure(closure)) => {
                    self.infer_closure_return(&closure, args, state)
                }
                None => self
                    .infer_declared_callable_call_schema(*slot, args, state)
                    .map(|schema| self.bound_type_for_schema(&schema))
                    .unwrap_or(BoundType::Unknown),
            },
            Expr::ClosureCall(closure, args) => self.infer_closure_return(closure, args, state),
            Expr::Closure(_) | Expr::FunctionRef(_) => BoundType::Unknown,
            _ => BoundType::Unknown,
        }
    }

    pub(super) fn infer_builtin_call_like_expr_type(
        &mut self,
        builtin: BuiltinFunction,
        type_args: &[TypeSchema],
        args: &[Expr],
        state: &LocalTypeState,
    ) -> BoundType {
        if let Some(schema) = builtin_generic_return_schema(builtin, type_args) {
            return self.bound_type_for_schema(&schema);
        }
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
            BuiltinFunction::ReFind => BoundType::String,
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
        type_args: &[TypeSchema],
        args: &[Expr],
        state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        if let Some(schema) = builtin_generic_return_schema(builtin, type_args) {
            return Some(schema);
        }
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
            BuiltinFunction::ReFind => Some(TypeSchema::String),
            BuiltinFunction::Slice if args.len() == 3 => {
                match self.infer_expr_schema(&args[0], state) {
                    Some(schema) if schema.array_prefix_and_rest().is_some() => schema
                        .collapsed_array_item_schema()
                        .map(|element| TypeSchema::Array(Box::new(element))),
                    Some(TypeSchema::String) => Some(TypeSchema::String),
                    Some(TypeSchema::Bytes) => Some(TypeSchema::Bytes),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn infer_named_call_schema(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        if let Some(schema) =
            self.infer_function_return_schema(index, type_args, args, caller_state)
        {
            return Some(schema);
        }

        let decl = self.function_decls.get(&index)?;
        if let Some(schema) =
            infer_host_passthrough_return_schema(&decl.name, args, caller_state, self)
        {
            return Some(schema);
        }
        host_generic_return_schema(&decl.name, type_args)
    }

    fn infer_declared_callable_call_schema(
        &mut self,
        slot: LocalSlot,
        args: &[Expr],
        state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        let schema = state.callable_schema(slot)?.clone();
        let TypeSchema::Callable { params, result } = self.resolve_schema(&schema) else {
            return None;
        };
        if params.len() != args.len() {
            return None;
        }
        Some(*result)
    }

    fn infer_closure_return_schema(
        &mut self,
        closure: &ClosureExpr,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        let nested = self.build_callable_state(
            &closure.param_slots,
            None,
            &closure.capture_copies,
            Some(args),
            caller_state,
        )?;
        self.infer_expr_schema(&closure.body, &nested)
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
            (BoundType::Bytes, BoundType::Bytes) => BoundType::Bytes,
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
            BoundType::Bytes => BoundType::Bytes,
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
        self.infer_get_return_type_from_container(container, key, state)
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
            BoundType::Number => BoundType::Number,
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
        } else if lhs_ty == BoundType::Float || rhs_ty == BoundType::Float {
            if is_numeric_bound_type(lhs_ty) && is_numeric_bound_type(rhs_ty) {
                BoundType::Float
            } else {
                BoundType::Unknown
            }
        } else if is_numeric_bound_type(lhs_ty) && is_numeric_bound_type(rhs_ty) {
            BoundType::Number
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
        } else if first_ty == BoundType::Float
            || second_ty == BoundType::Float
            || third_ty == BoundType::Float
        {
            if is_numeric_bound_type(first_ty)
                && is_numeric_bound_type(second_ty)
                && is_numeric_bound_type(third_ty)
            {
                BoundType::Float
            } else {
                BoundType::Unknown
            }
        } else if is_numeric_bound_type(first_ty)
            && is_numeric_bound_type(second_ty)
            && is_numeric_bound_type(third_ty)
        {
            BoundType::Number
        } else {
            BoundType::Unknown
        }
    }

    pub(super) fn infer_function_return(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> BoundType {
        let Some(function_decl) = self.function_decls.get(&index).cloned() else {
            return BoundType::Unknown;
        };
        if function_decl.type_params.is_empty() {
            self.observe_function_arg_types(index, args, caller_state);
        } else if function_decl.type_params.len() != type_args.len() {
            return BoundType::Unknown;
        }
        let instance_key = (index, type_args.to_vec());
        if self.active_functions.contains(&instance_key) {
            return self.infer_observed_function_return(index, type_args);
        }
        self.active_functions.push(instance_key);
        if !function_decl.type_params.is_empty() {
            self.push_generic_bindings(&function_decl.type_params, type_args);
        }
        let result = if let Some(schema) = function_decl.return_schema.as_ref() {
            let resolved = self.resolve_schema(schema);
            bound_type_from_schema(&resolved)
        } else {
            let Some(function_impl) = self.function_impls.get(&index).cloned() else {
                if !function_decl.type_params.is_empty() {
                    self.pop_generic_bindings();
                }
                self.active_functions.pop();
                return BoundType::Unknown;
            };
            let Some(mut nested) = self.build_function_call_state(
                index,
                &function_impl,
                &function_decl,
                args,
                caller_state,
            ) else {
                if !function_decl.type_params.is_empty() {
                    self.pop_generic_bindings();
                }
                self.active_functions.pop();
                return BoundType::Unknown;
            };
            self.apply_stmts(&function_impl.body_stmts, &mut nested);
            self.infer_expr_type(&function_impl.body_expr, &nested)
        };
        if !function_decl.type_params.is_empty() {
            self.pop_generic_bindings();
        }
        self.active_functions.pop();
        result
    }

    pub(super) fn infer_function_return_schema(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        let function_decl = self.function_decls.get(&index).cloned()?;
        if !function_decl.type_params.is_empty()
            && function_decl.type_params.len() != type_args.len()
        {
            return None;
        }

        let instance_key = (index, type_args.to_vec());
        if self.active_functions.contains(&instance_key) {
            return self.infer_observed_function_return_schema(index, type_args);
        }
        self.active_functions.push(instance_key);
        if !function_decl.type_params.is_empty() {
            self.push_generic_bindings(&function_decl.type_params, type_args);
        }
        let result = if let Some(schema) = function_decl.return_schema.as_ref() {
            Some(self.resolve_schema(schema).clone_inner_if_optional())
        } else {
            self.function_impls
                .get(&index)
                .cloned()
                .and_then(|function_impl| {
                    let mut nested = self.build_function_call_state(
                        index,
                        &function_impl,
                        &function_decl,
                        args,
                        caller_state,
                    )?;
                    self.apply_stmts(&function_impl.body_stmts, &mut nested);
                    self.infer_expr_schema(&function_impl.body_expr, &nested)
                        .map(|schema| self.resolve_schema(&schema))
                })
        };
        if !function_decl.type_params.is_empty() {
            self.pop_generic_bindings();
        }
        self.active_functions.pop();
        result
    }

    pub(super) fn infer_observed_function_return(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
    ) -> BoundType {
        let instance_key = observed_return_key(index, type_args);
        if let Some(cached) = self.observed_return_types.get(&instance_key).copied() {
            return cached;
        }
        if self.active_observed_returns.contains(&instance_key) {
            return BoundType::Unknown;
        }
        let Some(function_decl) = self.function_decls.get(&index).cloned() else {
            return BoundType::Unknown;
        };
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return function_decl
                .return_schema
                .as_ref()
                .map(|schema| {
                    let resolved = self.resolve_schema(schema);
                    bound_type_from_schema(&resolved)
                })
                .unwrap_or_else(|| {
                    self.host_import_return_types
                        .get(&index)
                        .copied()
                        .unwrap_or(BoundType::Unknown)
                });
        };
        if !function_decl.type_params.is_empty()
            && function_decl.type_params.len() != type_args.len()
        {
            return BoundType::Unknown;
        }
        self.active_observed_returns.push(instance_key.clone());
        if !function_decl.type_params.is_empty() {
            self.push_generic_bindings(&function_decl.type_params, type_args);
        }
        let result = self
            .build_observed_function_state(index)
            .map(|mut nested| {
                self.apply_stmts(&function_impl.body_stmts, &mut nested);
                self.infer_expr_type(&function_impl.body_expr, &nested)
            })
            .unwrap_or(BoundType::Unknown);
        if !function_decl.type_params.is_empty() {
            self.pop_generic_bindings();
        }
        self.active_observed_returns.pop();
        self.observed_return_types.insert(instance_key, result);
        result
    }

    pub(super) fn infer_observed_function_return_schema(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
    ) -> Option<TypeSchema> {
        let instance_key = observed_return_key(index, type_args);
        if let Some(cached) = self.observed_return_schemas.get(&instance_key) {
            return cached.clone();
        }
        if self.active_observed_returns.contains(&instance_key) {
            return None;
        }
        let function_decl = self.function_decls.get(&index).cloned()?;
        if !function_decl.type_params.is_empty()
            && function_decl.type_params.len() != type_args.len()
        {
            return None;
        }
        if let Some(schema) = function_decl.return_schema.as_ref() {
            let resolved = Some(self.resolve_schema(schema).clone_inner_if_optional());
            self.observed_return_schemas
                .insert(instance_key, resolved.clone());
            return resolved;
        }
        let function_impl = self.function_impls.get(&index).cloned()?;
        self.active_observed_returns.push(instance_key.clone());
        if !function_decl.type_params.is_empty() {
            self.push_generic_bindings(&function_decl.type_params, type_args);
        }
        let result = self
            .build_observed_function_state(index)
            .and_then(|mut nested| {
                self.apply_stmts(&function_impl.body_stmts, &mut nested);
                self.infer_expr_schema(&function_impl.body_expr, &nested)
                    .map(|schema| self.resolve_schema(&schema))
            });
        if !function_decl.type_params.is_empty() {
            self.pop_generic_bindings();
        }
        self.active_observed_returns.pop();
        self.observed_return_schemas
            .insert(instance_key, result.clone());
        result
    }

    pub(super) fn infer_closure_return(
        &mut self,
        closure: &ClosureExpr,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> BoundType {
        self.infer_callable_body(
            CallableBody {
                param_slots: &closure.param_slots,
                param_schemas: None,
                capture_copies: &closure.capture_copies,
                body_stmts: &[],
                body_expr: &closure.body,
            },
            args,
            caller_state,
        )
    }

    fn infer_callable_body(
        &mut self,
        callable: CallableBody<'_>,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> BoundType {
        let Some(mut nested) = self.build_callable_state(
            callable.param_slots,
            callable.param_schemas,
            callable.capture_copies,
            Some(args),
            caller_state,
        ) else {
            return BoundType::Unknown;
        };
        self.apply_stmts(callable.body_stmts, &mut nested);
        self.infer_expr_type(callable.body_expr, &nested)
    }

    fn build_function_call_state(
        &mut self,
        index: u16,
        function_impl: &FunctionImpl,
        function_decl: &FunctionDecl,
        args: &[Expr],
        caller_state: &LocalTypeState,
    ) -> Option<LocalTypeState> {
        if function_impl.param_slots.len() != args.len() {
            return None;
        }
        let mut nested = LocalTypeState::default();
        let observed_captures = self.observed_function_capture_states.get(&index);
        for (source_slot, captured_slot) in &function_impl.capture_copies {
            let has_direct_source = caller_state.callable(*source_slot).is_some()
                || caller_state.callable_schema(*source_slot).is_some()
                || caller_state.get(*source_slot) != BoundType::Unknown
                || caller_state.schema(*source_slot).is_some();
            if has_direct_source {
                nested.copy_binding_from(caller_state, *source_slot, *captured_slot, None, false);
            } else if let Some(observed) = observed_captures {
                nested.copy_binding_from(observed, *captured_slot, *captured_slot, None, false);
            } else {
                nested.copy_binding_from(caller_state, *source_slot, *captured_slot, None, false);
            }
        }
        for (param_index, (arg, slot)) in args
            .iter()
            .zip(function_impl.param_slots.iter())
            .enumerate()
        {
            let declared_schema = function_decl
                .arg_schemas
                .get(param_index)
                .and_then(|schema| schema.as_ref());
            self.bind_expr_to_slot(&mut nested, *slot, declared_schema, arg, caller_state);
        }
        Some(nested)
    }

    pub(super) fn build_callable_state(
        &mut self,
        param_slots: &[LocalSlot],
        param_schemas: Option<&[Option<TypeSchema>]>,
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
            for (param_index, (arg, slot)) in args.iter().zip(param_slots.iter()).enumerate() {
                let declared_schema = param_schemas
                    .and_then(|schemas| schemas.get(param_index))
                    .and_then(|schema| schema.as_ref());
                self.bind_expr_to_slot(&mut nested, *slot, declared_schema, arg, caller_state);
            }
        } else if let Some(param_schemas) = param_schemas {
            for (slot, declared_schema) in param_slots.iter().zip(param_schemas.iter()) {
                if let Some(schema) = declared_schema {
                    nested.set_with_schema_origin(
                        *slot,
                        self.bound_type_for_schema(schema),
                        Some(schema.clone()),
                        true,
                    );
                } else {
                    nested.set(*slot, BoundType::Unknown);
                }
            }
        }
        Some(nested)
    }

    pub(super) fn resolve_function_arg_schemas(
        &mut self,
        index: u16,
        type_args: &[TypeSchema],
    ) -> Option<Vec<Option<TypeSchema>>> {
        let function_decl = self.function_decls.get(&index).cloned()?;
        if function_decl.type_params.len() != type_args.len() {
            return None;
        }
        if !function_decl.type_params.is_empty() {
            self.push_generic_bindings(&function_decl.type_params, type_args);
        }
        let resolved = function_decl
            .arg_schemas
            .iter()
            .map(|schema| schema.as_ref().map(|schema| self.resolve_schema(schema)))
            .collect::<Vec<_>>();
        if !function_decl.type_params.is_empty() {
            self.pop_generic_bindings();
        }
        Some(resolved)
    }

    pub(super) fn infer_callable_value_schema(
        &mut self,
        callable: &InferredCallable,
    ) -> Option<TypeSchema> {
        match callable {
            InferredCallable::Function(index) => {
                let function_decl = self.function_decls.get(index).cloned()?;
                if !function_decl.type_params.is_empty() {
                    return None;
                }
                let params = function_decl
                    .arg_schemas
                    .iter()
                    .cloned()
                    .collect::<Option<Vec<_>>>()?;
                let result = function_decl
                    .return_schema
                    .clone()
                    .or_else(|| self.infer_observed_function_return_schema(*index, &[]))?;
                Some(TypeSchema::Callable {
                    params,
                    result: Box::new(self.resolve_schema(&result)),
                })
            }
            InferredCallable::Closure(_) => None,
        }
    }

    pub(super) fn infer_callable_expr_schema(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
    ) -> Option<TypeSchema> {
        self.infer_expr_schema(expr, state).or_else(|| {
            self.callable_binding_from_expr(expr, state)
                .and_then(|callable| self.infer_callable_value_schema(&callable))
        })
    }

    fn build_observed_function_state(&mut self, index: u16) -> Option<LocalTypeState> {
        let function_impl = self.function_impls.get(&index)?;
        let function_decl = self.function_decls.get(&index)?;
        let mut nested = LocalTypeState::default();
        super::collect::seed_function_param_state(
            &mut nested,
            &function_impl.param_slots,
            Some(function_decl.arg_schemas.as_slice()),
            super::collect::observed_function_param_slice(
                &self.observed_function_param_types,
                index,
            ),
            super::collect::observed_function_param_schema_slice(
                &self.observed_function_param_schemas,
                index,
            ),
            self.observed_function_param_callables
                .get(&index)
                .map(Vec::as_slice),
            self.observed_function_param_capture_states
                .get(&index)
                .map(Vec::as_slice),
        );
        super::collect::seed_function_capture_state(
            &mut nested,
            index,
            &function_impl.capture_copies,
            &self.observed_function_capture_states,
        );
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
            Expr::Call(index, type_args, args) => {
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
                } else if let Some(function_decl) = self.function_decls.get(index).cloned() {
                    let param_schemas = self
                        .resolve_function_arg_schemas(*index, type_args)
                        .unwrap_or_else(|| function_decl.arg_schemas.clone());
                    validate_function_argument_schemas(
                        &function_decl.name,
                        "function",
                        &function_decl.args,
                        &param_schemas,
                        args,
                        state,
                        DiagnosticSite {
                            line: line_context,
                            source_name,
                        },
                        self,
                    )
                } else {
                    Ok(())
                }
            }
            Expr::LocalCall(slot, type_args, args) => match state.callable(*slot).cloned() {
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
                    } else if let Some(function_decl) = self.function_decls.get(&index).cloned() {
                        let param_schemas = self
                            .resolve_function_arg_schemas(index, type_args)
                            .unwrap_or_else(|| function_decl.arg_schemas.clone());
                        validate_function_argument_schemas(
                            &function_decl.name,
                            "function",
                            &function_decl.args,
                            &param_schemas,
                            args,
                            state,
                            DiagnosticSite {
                                line: line_context,
                                source_name,
                            },
                            self,
                        )
                    } else {
                        Ok(())
                    }
                }
                _ => {
                    let Some(schema) = state.callable_schema(*slot).cloned() else {
                        return Ok(());
                    };
                    let schema = self.resolve_schema(&schema);
                    let TypeSchema::Callable { params, .. } = schema else {
                        return Ok(());
                    };
                    let param_schemas = params.into_iter().map(Some).collect::<Vec<_>>();
                    let param_names = (0..param_schemas.len())
                        .map(|index| format!("arg{}", index + 1))
                        .collect::<Vec<_>>();
                    validate_function_argument_schemas(
                        &format!("local slot {}", slot),
                        "callable",
                        &param_names,
                        &param_schemas,
                        args,
                        state,
                        DiagnosticSite {
                            line: line_context,
                            source_name,
                        },
                        self,
                    )
                }
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
        if builtin == BuiltinFunction::JsonEncode {
            let arg = args.first().expect("json::encode arity is fixed");
            return validate_json_encode_argument(
                arg,
                state,
                self,
                DiagnosticSite {
                    line: line_context,
                    source_name,
                },
            );
        }
        validate_signature_overloads(
            &display_name_for_builtin(builtin),
            "builtin",
            builtin.callable_signatures(),
            args,
            state,
            self,
            super::validate::DiagnosticSite {
                line: line_context,
                source_name,
            },
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
        if matches!(signature.name.as_str(), "print" | "println") {
            if args
                .first()
                .and_then(|arg| self.callable_binding_from_expr(arg, state))
                .is_some()
            {
                return Err(CompileError::CallableUsedAsValue);
            }
            return Ok(());
        }
        if self.is_strict()
            && signature
                .params
                .iter()
                .any(|param| param.ty == crate::builtins::CallableParamType::Any)
        {
            return Err(CompileError::StrictTypingRequired {
                line: line_context,
                source_name: super::validate::owned_source_name(source_name),
                detail: format!(
                    "host function '{}' uses dynamically typed 'any' parameters and is not available from strict RustScript without a typed wrapper",
                    signature.name
                ),
            });
        }
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
                    declared_schema,
                    expr,
                    ..
                } => {
                    let expr_state = state.clone();
                    self.bind_expr_to_slot(
                        state,
                        *index,
                        declared_schema.as_ref(),
                        expr,
                        &expr_state,
                    );
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
                    let mut then_state = refine_state_for_condition(state, condition, true);
                    let mut else_state = refine_state_for_condition(state, condition, false);
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
            let declared_binding = declared_schema.map(TypeSchema::split_optional).or_else(|| {
                expr_state
                    .has_declared_schema(slot)
                    .then(|| {
                        (
                            expr_state.schema(slot).cloned(),
                            expr_state.is_optional(slot),
                        )
                    })
                    .and_then(|(schema, optional)| schema.map(|schema| (schema, optional)))
            });
            let slot_declared_schema = declared_binding.as_ref().map(|(schema, _)| schema.clone());
            let declared_optional = declared_binding
                .as_ref()
                .map(|(_, optional)| *optional)
                .unwrap_or(false);
            let optional = self.expr_is_optional(expr, expr_state) || declared_optional;
            let schema = slot_declared_schema.clone().or_else(|| {
                expr_state
                    .callable_schema(slot)
                    .cloned()
                    .or_else(|| self.infer_expr_schema(expr, expr_state))
            });
            let from_declared_schema =
                slot_declared_schema.is_some() || expr_state.has_declared_schema(slot);
            state.bind_callable_with_schema(
                slot,
                callable.clone(),
                schema,
                from_declared_schema,
                optional,
            );
            if let Some(capture_state) = self.capture_state_for_callable(&callable, expr_state) {
                state.copy_all_bindings_from(&capture_state);
            }
            BoundType::Unknown
        } else {
            let ty = self.infer_expr_type(expr, expr_state);
            let declared_binding = declared_schema.map(TypeSchema::split_optional).or_else(|| {
                expr_state
                    .has_declared_schema(slot)
                    .then(|| {
                        (
                            expr_state.schema(slot).cloned(),
                            expr_state.is_optional(slot),
                        )
                    })
                    .and_then(|(schema, optional)| schema.map(|schema| (schema, optional)))
            });
            let slot_declared_schema = declared_binding.as_ref().map(|(schema, _)| schema.clone());
            let declared_optional = declared_binding
                .as_ref()
                .map(|(_, optional)| *optional)
                .unwrap_or(false);
            let optional = self.expr_is_optional(expr, expr_state) || declared_optional;
            let schema = slot_declared_schema.clone().or_else(|| {
                if optional {
                    self.infer_optional_expr_inner_schema(expr, expr_state)
                } else {
                    self.infer_expr_schema(expr, expr_state)
                }
            });
            let from_declared_schema = slot_declared_schema.is_some()
                || expr_state.has_declared_schema(slot)
                || self.expr_has_declared_schema(expr, expr_state);
            let inferred_ty = if optional {
                self.infer_optional_expr_inner_type(expr, expr_state)
            } else {
                ty
            };
            let ty = slot_declared_schema
                .as_ref()
                .map(|schema| self.bound_type_for_schema(schema))
                .unwrap_or(inferred_ty);
            state.set_with_optional_schema_origin(slot, ty, schema, from_declared_schema, optional);
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

fn merge_observed_function_param_callable(
    current: Option<InferredCallable>,
    next: Option<InferredCallable>,
) -> Option<InferredCallable> {
    match (current, next) {
        (None, rhs) => rhs,
        (lhs, None) => lhs,
        (Some(InferredCallable::Function(lhs)), Some(InferredCallable::Function(rhs)))
            if lhs == rhs =>
        {
            Some(InferredCallable::Function(lhs))
        }
        (Some(InferredCallable::Closure(lhs)), Some(InferredCallable::Closure(_))) => {
            Some(InferredCallable::Closure(lhs))
        }
        _ => None,
    }
}

fn merge_observed_capture_state(
    current: Option<LocalTypeState>,
    next: Option<LocalTypeState>,
) -> Option<LocalTypeState> {
    match (current, next) {
        (None, rhs) => rhs,
        (lhs, None) => lhs,
        (Some(lhs), Some(rhs)) => {
            let mut merged = LocalTypeState::default();
            merged.merge_from_branches(&lhs, &rhs);
            Some(merged)
        }
    }
}

fn schema_from_bound_type(ty: BoundType) -> Option<TypeSchema> {
    match ty {
        BoundType::Unknown => None,
        BoundType::Null => Some(TypeSchema::Null),
        BoundType::Int => Some(TypeSchema::Int),
        BoundType::Float => Some(TypeSchema::Float),
        BoundType::Number => Some(TypeSchema::Number),
        BoundType::Bool => Some(TypeSchema::Bool),
        BoundType::String => Some(TypeSchema::String),
        BoundType::Bytes => Some(TypeSchema::Bytes),
        BoundType::Array | BoundType::ArrayOf(_) => {
            Some(TypeSchema::Array(Box::new(TypeSchema::Unknown)))
        }
        BoundType::Map | BoundType::MapOf(_) => {
            Some(TypeSchema::Map(Box::new(TypeSchema::Unknown)))
        }
    }
}

fn observed_return_key(index: u16, type_args: &[TypeSchema]) -> (u16, Vec<String>) {
    (
        index,
        type_args
            .iter()
            .map(render_schema_label)
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn bound_type_from_schema(schema: &TypeSchema) -> BoundType {
    match schema {
        TypeSchema::Unknown => BoundType::Unknown,
        TypeSchema::Null => BoundType::Null,
        TypeSchema::Int => BoundType::Int,
        TypeSchema::Float => BoundType::Float,
        TypeSchema::Number => BoundType::Number,
        TypeSchema::Bool => BoundType::Bool,
        TypeSchema::String => BoundType::String,
        TypeSchema::Bytes => BoundType::Bytes,
        TypeSchema::Optional(inner) => bound_type_from_schema(inner),
        TypeSchema::GenericParam(_) => BoundType::Unknown,
        TypeSchema::Callable { .. } => BoundType::Unknown,
        TypeSchema::Named(_, _) => BoundType::Map,
        TypeSchema::Array(_) | TypeSchema::ArrayTuple(_) | TypeSchema::ArrayTupleRest { .. } => {
            BoundType::Array
        }
        TypeSchema::Map(_) | TypeSchema::Object(_) => BoundType::Map,
    }
}

fn merge_array_schema(current: Option<TypeSchema>, next: Option<TypeSchema>) -> TypeSchema {
    let next = next.unwrap_or(TypeSchema::Unknown);
    match current {
        Some(TypeSchema::ArrayTuple(mut items)) => {
            items.push(next);
            TypeSchema::ArrayTuple(items)
        }
        Some(TypeSchema::ArrayTupleRest { prefix, rest }) => TypeSchema::ArrayTupleRest {
            prefix,
            rest: Box::new(merge_schema_value(*rest, Some(next))),
        },
        Some(TypeSchema::Array(existing)) if *existing == TypeSchema::Unknown => {
            TypeSchema::ArrayTuple(vec![next])
        }
        Some(TypeSchema::Array(existing)) => {
            TypeSchema::Array(Box::new(merge_schema_value(*existing, Some(next))))
        }
        Some(other) => other,
        None => TypeSchema::ArrayTuple(vec![next]),
    }
}

fn infer_set_schema(
    container: Option<TypeSchema>,
    key: &Expr,
    value: Option<TypeSchema>,
) -> Option<TypeSchema> {
    match container {
        Some(TypeSchema::Named(name, type_args)) => Some(TypeSchema::Named(name, type_args)),
        Some(TypeSchema::GenericParam(name)) => Some(TypeSchema::GenericParam(name)),
        Some(TypeSchema::Object(mut fields)) => {
            let Expr::String(name) = key else {
                return Some(TypeSchema::Map(Box::new(
                    value.unwrap_or(TypeSchema::Unknown),
                )));
            };
            fields.insert(name.clone(), value.unwrap_or(TypeSchema::Unknown));
            Some(TypeSchema::Object(fields))
        }
        Some(TypeSchema::Map(existing)) => Some(TypeSchema::Map(Box::new(merge_schema_value(
            *existing, value,
        )))),
        Some(TypeSchema::Array(existing)) => Some(TypeSchema::Array(Box::new(merge_schema_value(
            *existing, value,
        )))),
        Some(TypeSchema::ArrayTuple(mut items)) => {
            if let Some(index) = literal_int_index(key) {
                let value = value.unwrap_or(TypeSchema::Unknown);
                if let Some(existing) = items.get_mut(index) {
                    *existing = merge_schema_value(existing.clone(), Some(value));
                } else if index == items.len() {
                    items.push(value);
                } else {
                    return Some(TypeSchema::Array(Box::new(TypeSchema::Unknown)));
                }
                Some(TypeSchema::ArrayTuple(items))
            } else {
                let merged = merge_schema_value(
                    TypeSchema::ArrayTuple(items).collapsed_array_item_schema()?,
                    value,
                );
                Some(TypeSchema::Array(Box::new(merged)))
            }
        }
        Some(TypeSchema::ArrayTupleRest { mut prefix, rest }) => {
            if let Some(index) = literal_int_index(key) {
                if let Some(existing) = prefix.get_mut(index) {
                    *existing = merge_schema_value(existing.clone(), value);
                    return Some(TypeSchema::ArrayTupleRest { prefix, rest });
                }
                return Some(TypeSchema::ArrayTupleRest {
                    prefix,
                    rest: Box::new(merge_schema_value(*rest, value)),
                });
            }
            Some(TypeSchema::ArrayTupleRest {
                prefix,
                rest: Box::new(merge_schema_value(*rest, value)),
            })
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
    let resolved = context.resolve_schema(schema);
    if resolved.array_prefix_and_rest().is_some() {
        let key_ty = context.infer_expr_type(key, state);
        if matches!(key_ty, BoundType::Unknown | BoundType::Int) {
            if let Some(index) = literal_int_index(key) {
                return Ok(resolved
                    .array_item_schema_at(index)
                    .unwrap_or(TypeSchema::Unknown));
            }
            return Ok(resolved
                .collapsed_array_item_schema()
                .unwrap_or(TypeSchema::Unknown));
        }
        return Err(format!(
            "schema-typed array access requires an int index, got {}",
            bound_type_label(key_ty)
        ));
    }

    match resolved {
        TypeSchema::Number => Err("cannot access fields on schema type 'number'".to_string()),
        TypeSchema::GenericParam(name) => Err(format!(
            "cannot access fields on unresolved generic parameter '{}'",
            name
        )),
        TypeSchema::Named(name, _) => Err(format!("unknown struct schema '{name}'")),
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
        TypeSchema::String => {
            let key_ty = context.infer_expr_type(key, state);
            if matches!(key_ty, BoundType::Unknown | BoundType::Int) {
                Ok(TypeSchema::String)
            } else {
                Err(format!(
                    "schema-typed string access requires an int index, got {}",
                    bound_type_label(key_ty)
                ))
            }
        }
        TypeSchema::Bytes => {
            let key_ty = context.infer_expr_type(key, state);
            if matches!(key_ty, BoundType::Unknown | BoundType::Int) {
                Ok(TypeSchema::Int)
            } else {
                Err(format!(
                    "schema-typed bytes access requires an int index, got {}",
                    bound_type_label(key_ty)
                ))
            }
        }
        TypeSchema::Map(value) => Ok(*value),
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
        TypeSchema::Number => "number",
        TypeSchema::Bool => "bool",
        TypeSchema::String => "string",
        TypeSchema::Bytes => "bytes",
        TypeSchema::Optional(inner) => schema_label(inner),
        TypeSchema::GenericParam(_) => "unknown",
        TypeSchema::Callable { .. } => "function",
        TypeSchema::Named(_, _) => "map",
        TypeSchema::Array(_) | TypeSchema::ArrayTuple(_) | TypeSchema::ArrayTupleRest { .. } => {
            "array"
        }
        TypeSchema::Map(_) | TypeSchema::Object(_) => "map",
    }
}

fn merge_schema_value(current: TypeSchema, next: Option<TypeSchema>) -> TypeSchema {
    match (current, next) {
        (TypeSchema::Unknown, Some(next)) => next,
        (current, Some(next)) if current == next => current,
        (current, None) => current,
        _ => TypeSchema::Unknown,
    }
}

pub(crate) fn render_schema_label(schema: &TypeSchema) -> String {
    match schema {
        TypeSchema::Unknown => "unknown".to_string(),
        TypeSchema::Null => "null".to_string(),
        TypeSchema::Int => "int".to_string(),
        TypeSchema::Float => "float".to_string(),
        TypeSchema::Number => "number".to_string(),
        TypeSchema::Bool => "bool".to_string(),
        TypeSchema::String => "string".to_string(),
        TypeSchema::Bytes => "bytes".to_string(),
        TypeSchema::Optional(inner) => format!("{}?", render_schema_label(inner)),
        TypeSchema::GenericParam(name) => name.clone(),
        TypeSchema::Callable { params, result } => format!(
            "fn({}) -> {}",
            params
                .iter()
                .map(render_schema_label)
                .collect::<Vec<_>>()
                .join(", "),
            render_schema_label(result)
        ),
        TypeSchema::Named(name, type_args) if type_args.is_empty() => name.clone(),
        TypeSchema::Named(name, type_args) => format!(
            "{}<{}>",
            name,
            type_args
                .iter()
                .map(render_schema_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeSchema::Array(element) => format!("[{}]", render_schema_label(element)),
        TypeSchema::ArrayTuple(items) => format!(
            "[{}]",
            items
                .iter()
                .map(render_schema_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            let mut parts = prefix.iter().map(render_schema_label).collect::<Vec<_>>();
            parts.push(format!("{}...", render_schema_label(rest)));
            format!("[{}]", parts.join(", "))
        }
        TypeSchema::Map(value) => format!("map<{}>", render_schema_label(value)),
        TypeSchema::Object(fields) => {
            let mut entries = fields
                .iter()
                .map(|(name, schema)| format!("{name}: {}", render_schema_label(schema)))
                .collect::<Vec<_>>();
            entries.sort();
            format!("{{ {} }}", entries.join(", "))
        }
    }
}

fn builtin_generic_return_schema(
    builtin: BuiltinFunction,
    type_args: &[TypeSchema],
) -> Option<TypeSchema> {
    match builtin {
        BuiltinFunction::JsonDecode if type_args.len() == 1 => Some(type_args[0].clone()),
        _ => None,
    }
}

fn host_generic_return_schema(name: &str, type_args: &[TypeSchema]) -> Option<TypeSchema> {
    match name {
        "json::decode" if type_args.len() == 1 => Some(type_args[0].clone()),
        _ => None,
    }
}

fn infer_host_passthrough_return_type(
    name: &str,
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> BoundType {
    if matches!(name, "print" | "println") && args.len() == 1 {
        return context.infer_expr_type(&args[0], state);
    }
    BoundType::Unknown
}

fn infer_host_passthrough_return_schema(
    name: &str,
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> Option<TypeSchema> {
    if matches!(name, "print" | "println") && args.len() == 1 {
        return context.infer_callable_expr_schema(&args[0], state);
    }
    None
}

fn literal_int_index(key: &Expr) -> Option<usize> {
    let Expr::Int(index) = key else {
        return None;
    };
    usize::try_from(*index).ok()
}
