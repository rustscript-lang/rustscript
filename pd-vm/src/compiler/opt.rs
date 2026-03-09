use std::collections::HashMap;

use crate::builtins::{BuiltinFunction, CallableParam, CallableParamType, CallableSignature};
use crate::bytecode::ValueType;

use super::CompileError;
use super::ir::{ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SimpleType {
    Null,
    Int,
    Float,
    Bool,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundType {
    Unknown,
    Null,
    Int,
    Float,
    Bool,
    String,
    Array,
    ArrayOf(Option<SimpleType>),
    Map,
    MapOf(Option<SimpleType>),
}

impl BoundType {
    fn type_name(self) -> Option<&'static str> {
        match self {
            BoundType::Unknown => None,
            BoundType::Null => Some("null"),
            BoundType::Int => Some("int"),
            BoundType::Float => Some("float"),
            BoundType::Bool => Some("bool"),
            BoundType::String => Some("string"),
            BoundType::Array | BoundType::ArrayOf(_) => Some("array"),
            BoundType::Map | BoundType::MapOf(_) => Some("map"),
        }
    }

    fn simple_type(self) -> Option<SimpleType> {
        match self {
            BoundType::Null => Some(SimpleType::Null),
            BoundType::Int => Some(SimpleType::Int),
            BoundType::Float => Some(SimpleType::Float),
            BoundType::Bool => Some(SimpleType::Bool),
            BoundType::String => Some(SimpleType::String),
            _ => None,
        }
    }

    fn from_simple(value: SimpleType) -> Self {
        match value {
            SimpleType::Null => BoundType::Null,
            SimpleType::Int => BoundType::Int,
            SimpleType::Float => BoundType::Float,
            SimpleType::Bool => BoundType::Bool,
            SimpleType::String => BoundType::String,
        }
    }
}

impl From<BoundType> for ValueType {
    fn from(value: BoundType) -> Self {
        match value {
            BoundType::Unknown => ValueType::Unknown,
            BoundType::Null => ValueType::Null,
            BoundType::Int => ValueType::Int,
            BoundType::Float => ValueType::Float,
            BoundType::Bool => ValueType::Bool,
            BoundType::String => ValueType::String,
            BoundType::Array | BoundType::ArrayOf(_) => ValueType::Array,
            BoundType::Map | BoundType::MapOf(_) => ValueType::Map,
        }
    }
}

impl From<ValueType> for BoundType {
    fn from(value: ValueType) -> Self {
        match value {
            ValueType::Unknown => BoundType::Unknown,
            ValueType::Null => BoundType::Null,
            ValueType::Int => BoundType::Int,
            ValueType::Float => BoundType::Float,
            ValueType::Bool => BoundType::Bool,
            ValueType::String => BoundType::String,
            ValueType::Array => BoundType::Array,
            ValueType::Map => BoundType::Map,
        }
    }
}

fn merge_container_element_types(lhs: Option<SimpleType>, rhs: Option<SimpleType>) -> BoundType {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) if lhs == rhs => BoundType::ArrayOf(Some(lhs)),
        (None, Some(rhs)) | (Some(rhs), None) => BoundType::ArrayOf(Some(rhs)),
        (None, None) => BoundType::ArrayOf(None),
        _ => BoundType::Array,
    }
}

fn merge_map_element_types(lhs: Option<SimpleType>, rhs: Option<SimpleType>) -> BoundType {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) if lhs == rhs => BoundType::MapOf(Some(lhs)),
        (None, Some(rhs)) | (Some(rhs), None) => BoundType::MapOf(Some(rhs)),
        (None, None) => BoundType::MapOf(None),
        _ => BoundType::Map,
    }
}

fn merge_bound_types(lhs: BoundType, rhs: BoundType) -> BoundType {
    if lhs == rhs {
        return lhs;
    }

    match (lhs, rhs) {
        (BoundType::ArrayOf(lhs), BoundType::ArrayOf(rhs)) => {
            merge_container_element_types(lhs, rhs)
        }
        (BoundType::Array, BoundType::ArrayOf(_)) | (BoundType::ArrayOf(_), BoundType::Array) => {
            BoundType::Array
        }
        (BoundType::MapOf(lhs), BoundType::MapOf(rhs)) => merge_map_element_types(lhs, rhs),
        (BoundType::Map, BoundType::MapOf(_)) | (BoundType::MapOf(_), BoundType::Map) => {
            BoundType::Map
        }
        _ => BoundType::Unknown,
    }
}

fn are_compatible_bound_types(lhs: BoundType, rhs: BoundType) -> bool {
    lhs == BoundType::Unknown
        || rhs == BoundType::Unknown
        || merge_bound_types(lhs, rhs) != BoundType::Unknown
}

#[derive(Clone, Debug)]
enum InferredCallable {
    Function(u16),
    Closure(ClosureExpr),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LocalTypeState {
    by_slot: HashMap<LocalSlot, BoundType>,
    callables: HashMap<LocalSlot, InferredCallable>,
}

impl LocalTypeState {
    pub(crate) fn get(&self, slot: LocalSlot) -> BoundType {
        self.by_slot
            .get(&slot)
            .copied()
            .unwrap_or(BoundType::Unknown)
    }

    fn callable(&self, slot: LocalSlot) -> Option<&InferredCallable> {
        self.callables.get(&slot)
    }

    pub(crate) fn set(&mut self, slot: LocalSlot, ty: BoundType) {
        if ty == BoundType::Unknown {
            self.by_slot.remove(&slot);
        } else {
            self.by_slot.insert(slot, ty);
        }
        self.callables.remove(&slot);
    }

    fn bind_callable(&mut self, slot: LocalSlot, callable: InferredCallable) {
        self.by_slot.remove(&slot);
        self.callables.insert(slot, callable);
    }

    pub(crate) fn bind_function(&mut self, slot: LocalSlot, index: u16) {
        self.bind_callable(slot, InferredCallable::Function(index));
    }

    pub(crate) fn bind_closure(&mut self, slot: LocalSlot, closure: &ClosureExpr) {
        self.bind_callable(slot, InferredCallable::Closure(closure.clone()));
    }

    fn copy_binding_from(
        &mut self,
        source: &LocalTypeState,
        source_slot: LocalSlot,
        slot: LocalSlot,
    ) {
        if let Some(callable) = source.callable(source_slot).cloned() {
            self.bind_callable(slot, callable);
        } else {
            self.set(slot, source.get(source_slot));
        }
    }

    pub(crate) fn merge_from_branches(&mut self, lhs: &LocalTypeState, rhs: &LocalTypeState) {
        self.by_slot.clear();
        self.callables.clear();
        for slot in lhs.by_slot.keys().chain(rhs.by_slot.keys()) {
            let l = lhs.get(*slot);
            let r = rhs.get(*slot);
            let merged = merge_bound_types(l, r);
            if merged != BoundType::Unknown {
                self.by_slot.insert(*slot, merged);
            }
        }
        for slot in lhs.callables.keys().chain(rhs.callables.keys()) {
            match (lhs.callable(*slot), rhs.callable(*slot)) {
                (
                    Some(InferredCallable::Function(lhs_index)),
                    Some(InferredCallable::Function(rhs_index)),
                ) if lhs_index == rhs_index => {
                    self.callables
                        .insert(*slot, InferredCallable::Function(*lhs_index));
                }
                _ => {}
            }
        }
    }
}

fn stabilize_loop_state<F>(state: &mut LocalTypeState, mut run_iteration: F)
where
    F: FnMut(&mut LocalTypeState),
{
    let zero_iteration = state.clone();
    let mut first_iteration = state.clone();
    run_iteration(&mut first_iteration);
    let mut second_iteration = first_iteration.clone();
    run_iteration(&mut second_iteration);

    let mut stable_iteration = LocalTypeState::default();
    stable_iteration.merge_from_branches(&first_iteration, &second_iteration);
    state.merge_from_branches(&zero_iteration, &stable_iteration);
}

fn try_stabilize_loop_state<E, F>(state: &mut LocalTypeState, mut run_iteration: F) -> Result<(), E>
where
    F: FnMut(&mut LocalTypeState) -> Result<(), E>,
{
    let zero_iteration = state.clone();
    let mut first_iteration = state.clone();
    run_iteration(&mut first_iteration)?;
    let mut second_iteration = first_iteration.clone();
    run_iteration(&mut second_iteration)?;

    let mut stable_iteration = LocalTypeState::default();
    stable_iteration.merge_from_branches(&first_iteration, &second_iteration);
    state.merge_from_branches(&zero_iteration, &stable_iteration);
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TypeInferenceResult {
    pub local_types: Vec<ValueType>,
}

#[derive(Clone, Debug)]
pub(crate) struct HostCallableSignature {
    pub(crate) name: String,
    pub(crate) params: Vec<CallableParam>,
}

struct TypeContext<'a> {
    function_impls: &'a HashMap<u16, FunctionImpl>,
    function_names: &'a HashMap<u16, String>,
    host_import_return_types: &'a HashMap<u16, BoundType>,
    host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    active_functions: Vec<u16>,
    observed_function_param_types: HashMap<u16, Vec<BoundType>>,
    function_param_conflicts: HashMap<u16, String>,
}

impl<'a> TypeContext<'a> {
    fn new(
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

    fn function_name(&self, index: u16) -> &str {
        self.function_names
            .get(&index)
            .map(String::as_str)
            .unwrap_or("<anonymous>")
    }

    fn function_requires_strict_add_types(&self, index: u16) -> bool {
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

    fn observe_function_arg_types(&mut self, index: u16, args: &[Expr], state: &LocalTypeState) {
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

    fn infer_expr_type(&mut self, expr: &Expr, state: &LocalTypeState) -> BoundType {
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
                value: _,
                arms,
                default,
                ..
            } => {
                let mut arm_type = BoundType::Unknown;
                for (_pattern, arm_expr) in arms {
                    let ty = self.infer_expr_type(arm_expr, state);
                    arm_type = if arm_type == BoundType::Unknown {
                        ty
                    } else if arm_type == ty {
                        arm_type
                    } else {
                        BoundType::Unknown
                    };
                }
                let default_ty = self.infer_expr_type(default, state);
                if arm_type != BoundType::Unknown && arm_type == default_ty {
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

    fn infer_call_like_expr_type(&mut self, expr: &Expr, state: &LocalTypeState) -> BoundType {
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

    fn infer_builtin_call_like_expr_type(
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

    fn infer_concat_return_type(
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

    fn infer_slice_return_type(&mut self, source: &Expr, state: &LocalTypeState) -> BoundType {
        match self.infer_expr_type(source, state) {
            BoundType::String => BoundType::String,
            BoundType::Array => BoundType::Array,
            BoundType::ArrayOf(element_type) => BoundType::ArrayOf(element_type),
            _ => BoundType::Unknown,
        }
    }

    fn infer_array_push_return_type(
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

    fn infer_set_return_type(
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

    fn infer_get_return_type(&mut self, container: &Expr, state: &LocalTypeState) -> BoundType {
        match self.infer_expr_type(container, state) {
            BoundType::String => BoundType::String,
            BoundType::ArrayOf(Some(element_type)) | BoundType::MapOf(Some(element_type)) => {
                BoundType::from_simple(element_type)
            }
            _ => BoundType::Unknown,
        }
    }

    fn infer_keys_return_type(&mut self, container: &Expr, state: &LocalTypeState) -> BoundType {
        match self.infer_expr_type(container, state) {
            BoundType::Array | BoundType::ArrayOf(_) => BoundType::ArrayOf(Some(SimpleType::Int)),
            BoundType::Map | BoundType::MapOf(_) => BoundType::Array,
            _ => BoundType::Unknown,
        }
    }

    fn infer_same_numeric_return_type(&mut self, expr: &Expr, state: &LocalTypeState) -> BoundType {
        match self.infer_expr_type(expr, state) {
            BoundType::Int => BoundType::Int,
            BoundType::Float => BoundType::Float,
            _ => BoundType::Unknown,
        }
    }

    fn infer_numeric_pair_return_type(
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

    fn infer_numeric_triplet_return_type(
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

    fn infer_function_return(
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

    fn infer_closure_return(
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

    fn infer_callable_body(
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

    fn validate_call_argument_types(
        &mut self,
        expr: &Expr,
        state: &LocalTypeState,
        line_context: Option<u32>,
    ) -> Result<(), CompileError> {
        match expr {
            Expr::Call(index, args) => {
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    self.validate_builtin_argument_types(builtin, args, state, line_context)
                } else if let Some(signature) = self.host_import_signatures.get(index).cloned() {
                    self.validate_host_argument_types(&signature, args, state, line_context)
                } else {
                    Ok(())
                }
            }
            Expr::LocalCall(slot, args) => match state.callable(*slot).cloned() {
                Some(InferredCallable::Function(index)) => {
                    if let Some(builtin) = BuiltinFunction::from_call_index(index) {
                        self.validate_builtin_argument_types(builtin, args, state, line_context)
                    } else if let Some(signature) = self.host_import_signatures.get(&index).cloned()
                    {
                        self.validate_host_argument_types(&signature, args, state, line_context)
                    } else {
                        Ok(())
                    }
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn validate_builtin_argument_types(
        &mut self,
        builtin: BuiltinFunction,
        args: &[Expr],
        state: &LocalTypeState,
        line_context: Option<u32>,
    ) -> Result<(), CompileError> {
        validate_signature_overloads(
            &display_name_for_builtin(builtin),
            "builtin",
            builtin.callable_signatures(),
            args,
            state,
            self,
            line_context,
        )
    }

    fn validate_host_argument_types(
        &mut self,
        signature: &HostCallableSignature,
        args: &[Expr],
        state: &LocalTypeState,
        line_context: Option<u32>,
    ) -> Result<(), CompileError> {
        validate_host_signature(
            &signature.name,
            &signature.params,
            args,
            state,
            self,
            line_context,
        )
    }

    fn apply_stmts(&mut self, stmts: &[Stmt], state: &mut LocalTypeState) {
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

    fn callable_binding_from_expr(
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

    fn bind_expr_to_slot(
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

pub(super) fn legalize_builtins_and_bind_types(mut ir: FrontendIr) -> FrontendIr {
    let function_names = build_function_names(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
    );
    legalize_stmts(&mut ir.stmts, &mut top_state, &mut context);
    let observed_function_param_types = context.observed_function_param_types.clone();

    let function_impls = ir.function_impls.clone();
    for (index, function_impl) in ir.function_impls.iter_mut() {
        legalize_function_impl(
            *index,
            function_impl,
            &function_impls,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            &observed_function_param_types,
        );
    }

    ir
}

pub(super) fn infer_types(ir: &FrontendIr) -> TypeInferenceResult {
    let function_names = build_function_names(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut local_types = vec![ValueType::Unknown; ir.locals];
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
    );
    collect_stmt_types(&ir.stmts, &mut top_state, &mut local_types, &mut context);
    let observed_function_param_types = context.observed_function_param_types.clone();

    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        collect_function_types(
            decl.index,
            function_impl,
            &mut local_types,
            &ir.function_impls,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            &observed_function_param_types,
        );
    }

    TypeInferenceResult { local_types }
}

pub(super) fn validate_if_else_type_consistency(ir: &FrontendIr) -> Result<(), CompileError> {
    let function_names = build_function_names(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
    );
    validate_stmts(&ir.stmts, &mut top_state, None, &mut context, false)?;

    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        validate_function_impl(decl.index, function_impl, &mut context)?;
    }

    Ok(())
}

pub(crate) fn infer_expr_type(expr: &Expr, state: &LocalTypeState) -> BoundType {
    let empty_impls: HashMap<u16, FunctionImpl> = HashMap::new();
    let empty_imports: HashMap<u16, BoundType> = HashMap::new();
    let empty_signatures: HashMap<u16, HostCallableSignature> = HashMap::new();
    infer_expr_type_with_function_impls_and_imports(
        expr,
        state,
        &empty_impls,
        &empty_imports,
        &empty_signatures,
    )
}

pub(crate) fn infer_expr_type_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> BoundType {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
    );
    context.infer_expr_type(expr, state)
}

pub(crate) fn apply_stmts_with_function_impls_and_imports(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
    );
    context.apply_stmts(stmts, state);
}

fn legalize_function_impl(
    function_index: u16,
    function_impl: &mut FunctionImpl,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_names: &HashMap<u16, String>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
    observed_function_param_types: &HashMap<u16, Vec<BoundType>>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        function_names,
        host_import_return_types,
        host_import_signatures,
    );
    seed_function_param_state(
        &mut state,
        &function_impl.param_slots,
        observed_function_param_slice(observed_function_param_types, function_index),
    );
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    legalize_stmts(&mut function_impl.body_stmts, &mut state, &mut context);
    let _ = legalize_expr(&mut function_impl.body_expr, &state, &mut context);
}

fn validate_function_impl(
    function_index: u16,
    function_impl: &FunctionImpl,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    if let Some(detail) = context
        .function_param_conflicts
        .get(&function_index)
        .cloned()
    {
        return Err(CompileError::FunctionParameterTypeConflict { line: None, detail });
    }
    let mut state = LocalTypeState::default();
    let strict_function_add_types = context
        .observed_function_param_types
        .contains_key(&function_index)
        && context.function_requires_strict_add_types(function_index);
    seed_function_param_state(
        &mut state,
        &function_impl.param_slots,
        observed_function_param_slice(&context.observed_function_param_types, function_index),
    );
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    validate_stmts(
        &function_impl.body_stmts,
        &mut state,
        None,
        context,
        strict_function_add_types,
    )?;
    let _ = validate_expr(
        &function_impl.body_expr,
        &state,
        None,
        context,
        strict_function_add_types,
    )?;
    Ok(())
}

fn legalize_stmts(stmts: &mut [Stmt], state: &mut LocalTypeState, context: &mut TypeContext<'_>) {
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
                let _ = legalize_expr(&mut closure.body, state, context);
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let expr_state = state.clone();
                let ty = legalize_expr(expr, &expr_state, context);
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, .. } => {
                let _ = legalize_expr(expr, state, context);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = legalize_expr(condition, state, context);
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                legalize_stmts(then_branch, &mut then_state, context);
                legalize_stmts(else_branch, &mut else_state, context);
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                legalize_stmts(std::slice::from_mut(init), state, context);
                stabilize_loop_state(state, |iterated| {
                    let _ = legalize_expr(condition, iterated, context);
                    legalize_stmts(body, iterated, context);
                    legalize_stmts(std::slice::from_mut(post), iterated, context);
                });
            }
            Stmt::While {
                condition, body, ..
            } => {
                stabilize_loop_state(state, |iterated| {
                    let _ = legalize_expr(condition, iterated, context);
                    legalize_stmts(body, iterated, context);
                });
            }
        }
    }
}

fn validate_stmts(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    line_context: Option<u32>,
    context: &mut TypeContext<'_>,
    strict_function_add_types: bool,
) -> Result<(), CompileError> {
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
                let _ = validate_expr(&closure.body, state, line_context, context, false)?;
            }
            Stmt::Let { index, expr, line } | Stmt::Assign { index, expr, line } => {
                let expr_state = state.clone();
                let ty = validate_expr(
                    expr,
                    &expr_state,
                    Some(*line),
                    context,
                    strict_function_add_types,
                )?;
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, line } => {
                let _ =
                    validate_expr(expr, state, Some(*line), context, strict_function_add_types)?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let _ = validate_expr(
                    condition,
                    state,
                    Some(*line),
                    context,
                    strict_function_add_types,
                )?;
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                validate_stmts(
                    then_branch,
                    &mut then_state,
                    Some(*line),
                    context,
                    strict_function_add_types,
                )?;
                validate_stmts(
                    else_branch,
                    &mut else_state,
                    Some(*line),
                    context,
                    strict_function_add_types,
                )?;
                validate_branch_state_merge(Some(*line), &then_state, &else_state)?;
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                validate_stmts(
                    std::slice::from_ref(init),
                    state,
                    Some(*line),
                    context,
                    strict_function_add_types,
                )?;
                try_stabilize_loop_state(state, |iterated| {
                    let _ = validate_expr(
                        condition,
                        iterated,
                        Some(*line),
                        context,
                        strict_function_add_types,
                    )?;
                    validate_stmts(
                        body,
                        iterated,
                        Some(*line),
                        context,
                        strict_function_add_types,
                    )?;
                    validate_stmts(
                        std::slice::from_ref(post),
                        iterated,
                        Some(*line),
                        context,
                        strict_function_add_types,
                    )
                })?;
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                try_stabilize_loop_state(state, |iterated| {
                    let _ = validate_expr(
                        condition,
                        iterated,
                        Some(*line),
                        context,
                        strict_function_add_types,
                    )?;
                    validate_stmts(
                        body,
                        iterated,
                        Some(*line),
                        context,
                        strict_function_add_types,
                    )
                })?;
            }
        }
    }

    Ok(())
}

fn bind_expr_result_to_slot(
    state: &mut LocalTypeState,
    slot: LocalSlot,
    expr: &Expr,
    expr_state: &LocalTypeState,
    ty: BoundType,
    context: &mut TypeContext<'_>,
) {
    if let Some(callable) = context.callable_binding_from_expr(expr, expr_state) {
        state.bind_callable(slot, callable);
    } else {
        state.set(slot, ty);
    }
}

fn build_function_names(functions: &[FunctionDecl]) -> HashMap<u16, String> {
    functions
        .iter()
        .map(|decl| (decl.index, decl.name.clone()))
        .collect()
}

fn build_host_import_return_types(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<u16, BoundType> {
    functions
        .iter()
        .filter(|decl| !function_impls.contains_key(&decl.index))
        .map(|decl| (decl.index, BoundType::from(decl.return_type)))
        .collect()
}

pub(crate) fn build_host_import_signatures(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<u16, HostCallableSignature> {
    functions
        .iter()
        .filter(|decl| !function_impls.contains_key(&decl.index))
        .filter_map(|decl| {
            known_host_signature(&decl.name).map(|signature| (decl.index, signature))
        })
        .collect()
}

fn known_host_signature(name: &str) -> Option<HostCallableSignature> {
    if let Some(callable) = crate::builtins::default_host_callable(name) {
        return Some(HostCallableSignature {
            name: callable.name.to_string(),
            params: callable.signature.params.to_vec(),
        });
    }

    let function = edge_abi::function_by_name(name)?;
    Some(HostCallableSignature {
        name: name.to_string(),
        params: function
            .param_names
            .iter()
            .copied()
            .zip(function.param_types.iter().copied())
            .map(|(param_name, param_type)| CallableParam {
                name: param_name,
                ty: callable_param_type_from_abi(param_type),
                optional: false,
            })
            .collect(),
    })
}

fn callable_param_type_from_abi(value: edge_abi::AbiParamType) -> CallableParamType {
    match value {
        edge_abi::AbiParamType::Any => CallableParamType::Any,
        edge_abi::AbiParamType::Null => CallableParamType::Null,
        edge_abi::AbiParamType::Int => CallableParamType::Int,
        edge_abi::AbiParamType::Float => CallableParamType::Float,
        edge_abi::AbiParamType::Bool => CallableParamType::Bool,
        edge_abi::AbiParamType::String => CallableParamType::String,
        edge_abi::AbiParamType::Array => CallableParamType::Array,
        edge_abi::AbiParamType::Map => CallableParamType::Map,
        edge_abi::AbiParamType::Number => CallableParamType::Number,
    }
}

fn merge_observed_function_param_type(
    current: BoundType,
    next: BoundType,
) -> Result<BoundType, (BoundType, BoundType)> {
    if current == BoundType::Unknown {
        return Ok(next);
    }
    if next == BoundType::Unknown || current == next {
        return Ok(current);
    }
    let merged = merge_bound_types(current, next);
    if merged != BoundType::Unknown {
        Ok(merged)
    } else {
        Err((current, next))
    }
}

fn function_body_contains_param_add(
    param_slots: &[LocalSlot],
    stmts: &[Stmt],
    expr: &Expr,
) -> bool {
    stmts
        .iter()
        .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        || expr_contains_param_add(expr, param_slots)
}

fn stmt_contains_param_add(stmt: &Stmt, param_slots: &[LocalSlot]) -> bool {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => false,
        Stmt::ClosureLet { closure, .. } => expr_contains_param_add(&closure.body, param_slots),
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            expr_contains_param_add(expr, param_slots)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_contains_param_add(condition, param_slots)
                || then_branch
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
                || else_branch
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_contains_param_add(init, param_slots)
                || expr_contains_param_add(condition, param_slots)
                || stmt_contains_param_add(post, param_slots)
                || body
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        }
        Stmt::While {
            condition, body, ..
        } => {
            expr_contains_param_add(condition, param_slots)
                || body
                    .iter()
                    .any(|stmt| stmt_contains_param_add(stmt, param_slots))
        }
    }
}

fn expr_contains_param_add(expr: &Expr, param_slots: &[LocalSlot]) -> bool {
    match expr {
        Expr::Add(lhs, rhs) => {
            expr_uses_param(lhs, param_slots)
                || expr_uses_param(rhs, param_slots)
                || expr_contains_param_add(lhs, param_slots)
                || expr_contains_param_add(rhs, param_slots)
        }
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::Call(_, args) | Expr::LocalCall(_, args) => args
            .iter()
            .any(|arg| expr_contains_param_add(arg, param_slots)),
        Expr::ClosureCall(closure, args) => {
            expr_contains_param_add(&closure.body, param_slots)
                || args
                    .iter()
                    .any(|arg| expr_contains_param_add(arg, param_slots))
        }
        Expr::Closure(closure) => expr_contains_param_add(&closure.body, param_slots),
        Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => {
            expr_contains_param_add(lhs, param_slots) || expr_contains_param_add(rhs, param_slots)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => expr_contains_param_add(inner, param_slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_contains_param_add(condition, param_slots)
                || expr_contains_param_add(then_expr, param_slots)
                || expr_contains_param_add(else_expr, param_slots)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            expr_contains_param_add(value, param_slots)
                || arms
                    .iter()
                    .any(|(_, arm_expr)| expr_contains_param_add(arm_expr, param_slots))
                || expr_contains_param_add(default, param_slots)
        }
        Expr::Block { stmts, expr } => function_body_contains_param_add(param_slots, stmts, expr),
    }
}

fn expr_uses_param(expr: &Expr, param_slots: &[LocalSlot]) -> bool {
    match expr {
        Expr::Var(slot) | Expr::MoveVar(slot) => param_slots.contains(slot),
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => false,
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            args.iter().any(|arg| expr_uses_param(arg, param_slots))
        }
        Expr::ClosureCall(closure, args) => {
            expr_uses_param(&closure.body, param_slots)
                || args.iter().any(|arg| expr_uses_param(arg, param_slots))
        }
        Expr::Closure(closure) => expr_uses_param(&closure.body, param_slots),
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
            expr_uses_param(lhs, param_slots) || expr_uses_param(rhs, param_slots)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => expr_uses_param(inner, param_slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_uses_param(condition, param_slots)
                || expr_uses_param(then_expr, param_slots)
                || expr_uses_param(else_expr, param_slots)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            expr_uses_param(value, param_slots)
                || arms
                    .iter()
                    .any(|(_, arm_expr)| expr_uses_param(arm_expr, param_slots))
                || expr_uses_param(default, param_slots)
        }
        Expr::Block { stmts, expr } => {
            stmts.iter().any(|stmt| stmt_uses_param(stmt, param_slots))
                || expr_uses_param(expr, param_slots)
        }
    }
}

fn stmt_uses_param(stmt: &Stmt, param_slots: &[LocalSlot]) -> bool {
    match stmt {
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => false,
        Stmt::ClosureLet { closure, .. } => expr_uses_param(&closure.body, param_slots),
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            expr_uses_param(expr, param_slots)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_uses_param(condition, param_slots)
                || then_branch
                    .iter()
                    .any(|stmt| stmt_uses_param(stmt, param_slots))
                || else_branch
                    .iter()
                    .any(|stmt| stmt_uses_param(stmt, param_slots))
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            stmt_uses_param(init, param_slots)
                || expr_uses_param(condition, param_slots)
                || stmt_uses_param(post, param_slots)
                || body.iter().any(|stmt| stmt_uses_param(stmt, param_slots))
        }
        Stmt::While {
            condition, body, ..
        } => {
            expr_uses_param(condition, param_slots)
                || body.iter().any(|stmt| stmt_uses_param(stmt, param_slots))
        }
    }
}

fn observed_function_param_slice(
    observed: &HashMap<u16, Vec<BoundType>>,
    function_index: u16,
) -> Option<&[BoundType]> {
    observed.get(&function_index).map(Vec::as_slice)
}

fn seed_function_param_state(
    state: &mut LocalTypeState,
    param_slots: &[LocalSlot],
    observed: Option<&[BoundType]>,
) {
    for (param_index, slot) in param_slots.iter().enumerate() {
        let ty = observed
            .and_then(|types| types.get(param_index))
            .copied()
            .unwrap_or(BoundType::Unknown);
        state.set(*slot, ty);
    }
}

fn observe_direct_function_call_types(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    let function_index = match expr {
        Expr::Call(index, _) if context.function_impls.contains_key(index) => Some(*index),
        Expr::LocalCall(slot, _) => match state.callable(*slot).cloned() {
            Some(InferredCallable::Function(index))
                if context.function_impls.contains_key(&index) =>
            {
                Some(index)
            }
            _ => None,
        },
        _ => None,
    };

    let Some(function_index) = function_index else {
        return Ok(());
    };

    let args = match expr {
        Expr::Call(_, args) | Expr::LocalCall(_, args) => args,
        _ => return Ok(()),
    };
    context.observe_function_arg_types(function_index, args, state);
    if let Some(detail) = context
        .function_param_conflicts
        .get(&function_index)
        .cloned()
    {
        return Err(CompileError::FunctionParameterTypeConflict {
            line: line_context,
            detail,
        });
    }
    Ok(())
}

fn validate_signature_overloads(
    callable_name: &str,
    callable_kind: &str,
    signatures: &[CallableSignature],
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    line_context: Option<u32>,
) -> Result<(), CompileError> {
    let actual = args
        .iter()
        .map(|arg| context.infer_expr_type(arg, state))
        .collect::<Vec<_>>();
    if signatures
        .iter()
        .any(|signature| signature_matches_actual(signature, &actual))
    {
        return Ok(());
    }

    Err(CompileError::CallableArgumentTypeMismatch {
        line: line_context,
        detail: format!(
            "{callable_kind} '{callable_name}' does not accept argument types ({}); expected {}",
            format_actual_arg_types(&actual),
            format_signature_overloads(callable_name, signatures),
        ),
    })
}

fn validate_host_signature(
    callable_name: &str,
    params: &[CallableParam],
    args: &[Expr],
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
    line_context: Option<u32>,
) -> Result<(), CompileError> {
    let actual = args
        .iter()
        .map(|arg| context.infer_expr_type(arg, state))
        .collect::<Vec<_>>();
    if params_match_actual(params, &actual) {
        return Ok(());
    }

    Err(CompileError::CallableArgumentTypeMismatch {
        line: line_context,
        detail: format!(
            "host function '{callable_name}' does not accept argument types ({}); expected {}({})",
            format_actual_arg_types(&actual),
            callable_name,
            format_param_types(params),
        ),
    })
}

fn signature_matches_actual(signature: &CallableSignature, actual: &[BoundType]) -> bool {
    params_match_actual(signature.params, actual)
}

fn params_match_actual(params: &[CallableParam], actual: &[BoundType]) -> bool {
    let required = params.iter().take_while(|param| !param.optional).count();
    if actual.len() < required || actual.len() > params.len() {
        return false;
    }
    params
        .iter()
        .take(actual.len())
        .zip(actual.iter().copied())
        .all(|(expected, actual)| param_accepts_bound_type(expected.ty, actual))
}

fn param_accepts_bound_type(expected: CallableParamType, actual: BoundType) -> bool {
    if actual == BoundType::Unknown {
        return true;
    }
    match expected {
        CallableParamType::Any => true,
        CallableParamType::Null => actual == BoundType::Null,
        CallableParamType::Int => actual == BoundType::Int,
        CallableParamType::Float => actual == BoundType::Float,
        CallableParamType::Bool => actual == BoundType::Bool,
        CallableParamType::String => actual == BoundType::String,
        CallableParamType::Array => {
            matches!(actual, BoundType::Array | BoundType::ArrayOf(_))
        }
        CallableParamType::Map => matches!(actual, BoundType::Map | BoundType::MapOf(_)),
        CallableParamType::Number => is_numeric_bound_type(actual),
    }
}

fn format_signature_overloads(name: &str, signatures: &[CallableSignature]) -> String {
    signatures
        .iter()
        .map(|signature| format!("{name}({})", format_param_types(signature.params)))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn format_param_types(params: &[CallableParam]) -> String {
    params
        .iter()
        .map(|param| {
            if param.optional {
                format!("{}?: {}", param.name, param.ty.label())
            } else {
                format!("{}: {}", param.name, param.ty.label())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_actual_arg_types(actual: &[BoundType]) -> String {
    actual
        .iter()
        .copied()
        .map(bound_type_label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn display_name_for_builtin(builtin: BuiltinFunction) -> String {
    match builtin {
        BuiltinFunction::Len => "len".to_string(),
        BuiltinFunction::Slice => "slice".to_string(),
        BuiltinFunction::Concat => "concat".to_string(),
        BuiltinFunction::ArrayNew => "array_new".to_string(),
        BuiltinFunction::ArrayPush => "array_push".to_string(),
        BuiltinFunction::MapNew => "map_new".to_string(),
        BuiltinFunction::Get => "get".to_string(),
        BuiltinFunction::Set => "set".to_string(),
        BuiltinFunction::Keys => "keys".to_string(),
        BuiltinFunction::Count => "count".to_string(),
        BuiltinFunction::TypeOf => "type".to_string(),
        BuiltinFunction::Assert => "assert".to_string(),
        _ => builtin.name().replacen('_', "::", 1),
    }
}

fn is_numeric_bound_type(value: BoundType) -> bool {
    matches!(value, BoundType::Int | BoundType::Float)
}

fn legalize_expr(
    expr: &mut Expr,
    state: &LocalTypeState,
    context: &mut TypeContext<'_>,
) -> BoundType {
    match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            legalize_expr_children(expr, state, context);
            context.infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => {
            legalize_expr_children(expr, state, context);
            context.infer_call_like_expr_type(expr, state)
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
            let lhs_ty = legalize_expr(lhs, state, context);
            let rhs_ty = legalize_expr(rhs, state, context);
            infer_binary_type(expr, lhs_ty, rhs_ty)
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = legalize_expr(inner, state, context);
            infer_unary_type(expr, inner_ty)
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            legalize_expr(inner, state, context)
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = legalize_expr(condition, state, context);
            let then_ty = legalize_expr(then_expr, state, context);
            let else_ty = legalize_expr(else_expr, state, context);
            if then_ty == else_ty {
                then_ty
            } else {
                BoundType::Unknown
            }
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            let _ = legalize_expr(value, state, context);
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = legalize_expr(arm_expr, state, context);
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = legalize_expr(default, state, context);
            if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            legalize_stmts(stmts, &mut nested, context);
            legalize_expr(expr, &nested, context)
        }
    }
}

fn validate_expr(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    context: &mut TypeContext<'_>,
    strict_function_add_types: bool,
) -> Result<BoundType, CompileError> {
    Ok(match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => validate_expr(
            inner,
            state,
            line_context,
            context,
            strict_function_add_types,
        )?,
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            validate_expr_children(
                expr,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            observe_direct_function_call_types(expr, state, line_context, context)?;
            context.validate_call_argument_types(expr, state, line_context)?;
            context.infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => {
            validate_expr_children(
                expr,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            context.validate_call_argument_types(expr, state, line_context)?;
            context.infer_call_like_expr_type(expr, state)
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
            let lhs_ty =
                validate_expr(lhs, state, line_context, context, strict_function_add_types)?;
            let rhs_ty =
                validate_expr(rhs, state, line_context, context, strict_function_add_types)?;
            let inferred = infer_binary_type(expr, lhs_ty, rhs_ty);
            if strict_function_add_types
                && matches!(expr, Expr::Add(_, _))
                && inferred == BoundType::Unknown
            {
                return Err(CompileError::BinaryOperandTypeMismatch {
                    line: line_context,
                    detail: format!(
                        "cannot infer '+' operand types in function body: {} vs {}",
                        bound_type_label(lhs_ty),
                        bound_type_label(rhs_ty)
                    ),
                });
            }
            inferred
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = validate_expr(
                inner,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            infer_unary_type(expr, inner_ty)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = validate_expr(
                condition,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            let then_ty = validate_expr(
                then_expr,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            let else_ty = validate_expr(
                else_expr,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            ensure_compatible_if_else_types(line_context, "expression result", then_ty, else_ty)?;
            if then_ty == else_ty {
                then_ty
            } else {
                BoundType::Unknown
            }
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            let _ = validate_expr(
                value,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = validate_expr(
                    arm_expr,
                    state,
                    line_context,
                    context,
                    strict_function_add_types,
                )?;
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = validate_expr(
                default,
                state,
                line_context,
                context,
                strict_function_add_types,
            )?;
            if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            validate_stmts(
                stmts,
                &mut nested,
                line_context,
                context,
                strict_function_add_types,
            )?;
            validate_expr(
                expr,
                &nested,
                line_context,
                context,
                strict_function_add_types,
            )?
        }
    })
}

fn validate_expr_children(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    context: &mut TypeContext<'_>,
    strict_function_add_types: bool,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                let _ =
                    validate_expr(arg, state, line_context, context, strict_function_add_types)?;
            }
        }
        Expr::Closure(closure) => {
            let _ = validate_expr(&closure.body, state, line_context, context, false)?;
        }
        Expr::ClosureCall(closure, args) => {
            let _ = validate_expr(&closure.body, state, line_context, context, false)?;
            for arg in args {
                let _ =
                    validate_expr(arg, state, line_context, context, strict_function_add_types)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn legalize_expr_children(expr: &mut Expr, state: &LocalTypeState, context: &mut TypeContext<'_>) {
    match expr {
        Expr::Call(index, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
            if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                fold_builtin_call(expr, builtin, state);
            }
        }
        Expr::LocalCall(_, args) => {
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
        }
        Expr::Closure(closure) => {
            let _ = legalize_expr(&mut closure.body, state, context);
        }
        Expr::ClosureCall(closure, args) => {
            let _ = legalize_expr(&mut closure.body, state, context);
            for arg in args.iter_mut() {
                let _ = legalize_expr(arg, state, context);
            }
        }
        _ => {}
    }
}

fn fold_builtin_call(expr: &mut Expr, builtin: BuiltinFunction, state: &LocalTypeState) {
    let Expr::Call(_, args) = expr else {
        return;
    };
    match builtin {
        BuiltinFunction::TypeOf => {
            if args.len() == 1 {
                let inferred = infer_expr_type(&args[0], state);
                if let Some(name) = inferred.type_name() {
                    *expr = Expr::String(name.to_string());
                }
            }
        }
        BuiltinFunction::Len => {
            if args.len() == 1
                && let Some(len) = infer_static_len(&args[0])
            {
                *expr = Expr::Int(len as i64);
            }
        }
        BuiltinFunction::Concat => {
            if args.len() == 2
                && let (Expr::String(lhs), Expr::String(rhs)) = (&args[0], &args[1])
            {
                *expr = Expr::String(format!("{lhs}{rhs}"));
            }
        }
        _ => {}
    }
}

fn infer_static_len(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::String(text) => Some(text.chars().count()),
        Expr::Call(index, args) => {
            let builtin = BuiltinFunction::from_call_index(*index)?;
            match builtin {
                BuiltinFunction::ArrayNew if args.is_empty() => Some(0),
                BuiltinFunction::MapNew if args.is_empty() => Some(0),
                BuiltinFunction::ArrayPush if args.len() == 2 => {
                    infer_static_len(&args[0]).map(|base| base.saturating_add(1))
                }
                BuiltinFunction::Set if args.len() == 3 => infer_static_len(&args[0]),
                _ => None,
            }
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            infer_static_len(inner)
        }
        _ => None,
    }
}

fn infer_binary_type(expr: &Expr, lhs: BoundType, rhs: BoundType) -> BoundType {
    match expr {
        Expr::Add(_, _) => {
            if lhs == BoundType::String || rhs == BoundType::String {
                BoundType::String
            } else if let Some(array_ty) = infer_array_concat_type(lhs, rhs) {
                array_ty
            } else if lhs == BoundType::Int && rhs == BoundType::Int {
                BoundType::Int
            } else if (lhs == BoundType::Int || lhs == BoundType::Float)
                && (rhs == BoundType::Int || rhs == BoundType::Float)
            {
                BoundType::Float
            } else {
                BoundType::Unknown
            }
        }
        Expr::Sub(_, _) | Expr::Mul(_, _) | Expr::Div(_, _) | Expr::Mod(_, _) => {
            if lhs == BoundType::Int && rhs == BoundType::Int {
                BoundType::Int
            } else if (lhs == BoundType::Int || lhs == BoundType::Float)
                && (rhs == BoundType::Int || rhs == BoundType::Float)
            {
                BoundType::Float
            } else {
                BoundType::Unknown
            }
        }
        Expr::And(_, _) | Expr::Or(_, _) | Expr::Eq(_, _) | Expr::Lt(_, _) | Expr::Gt(_, _) => {
            BoundType::Bool
        }
        _ => BoundType::Unknown,
    }
}

fn infer_array_concat_type(lhs: BoundType, rhs: BoundType) -> Option<BoundType> {
    match (lhs, rhs) {
        (BoundType::ArrayOf(lhs), BoundType::ArrayOf(rhs)) => {
            Some(merge_container_element_types(lhs, rhs))
        }
        (BoundType::Array, BoundType::Array)
        | (BoundType::Array, BoundType::ArrayOf(_))
        | (BoundType::ArrayOf(_), BoundType::Array) => Some(BoundType::Array),
        _ => None,
    }
}

fn infer_unary_type(expr: &Expr, inner: BoundType) -> BoundType {
    match expr {
        Expr::Neg(_) => match inner {
            BoundType::Int | BoundType::Float => inner,
            _ => BoundType::Unknown,
        },
        Expr::Not(_) => BoundType::Bool,
        _ => BoundType::Unknown,
    }
}

fn ensure_compatible_if_else_types(
    line: Option<u32>,
    context: &str,
    lhs: BoundType,
    rhs: BoundType,
) -> Result<(), CompileError> {
    if are_compatible_bound_types(lhs, rhs) {
        return Ok(());
    }
    Err(CompileError::IfElseBranchTypeMismatch {
        line,
        detail: format!(
            "if/else branches produced incompatible {context}: {} vs {}",
            bound_type_label(lhs),
            bound_type_label(rhs)
        ),
    })
}

fn validate_branch_state_merge(
    line: Option<u32>,
    lhs: &LocalTypeState,
    rhs: &LocalTypeState,
) -> Result<(), CompileError> {
    for slot in lhs.by_slot.keys().chain(rhs.by_slot.keys()) {
        let left = lhs.get(*slot);
        let right = rhs.get(*slot);
        if are_compatible_bound_types(left, right) {
            continue;
        }
        return Err(CompileError::IfElseBranchTypeMismatch {
            line,
            detail: format!(
                "if/else branches assign incompatible types to local slot {}: {} vs {}",
                slot,
                bound_type_label(left),
                bound_type_label(right)
            ),
        });
    }
    Ok(())
}

fn bound_type_label(ty: BoundType) -> &'static str {
    match ty {
        BoundType::Unknown => "unknown",
        BoundType::Null => "null",
        BoundType::Int => "int",
        BoundType::Float => "float",
        BoundType::Bool => "bool",
        BoundType::String => "string",
        BoundType::Array | BoundType::ArrayOf(_) => "array",
        BoundType::Map | BoundType::MapOf(_) => "map",
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_function_types(
    function_index: u16,
    function_impl: &FunctionImpl,
    local_types: &mut [ValueType],
    function_impls: &HashMap<u16, FunctionImpl>,
    function_names: &HashMap<u16, String>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
    observed_function_param_types: &HashMap<u16, Vec<BoundType>>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        function_names,
        host_import_return_types,
        host_import_signatures,
    );
    for (param_index, slot) in function_impl.param_slots.iter().enumerate() {
        let ty = observed_function_param_slice(observed_function_param_types, function_index)
            .and_then(|types| types.get(param_index))
            .copied()
            .unwrap_or(BoundType::Unknown);
        record_local_type(local_types, *slot, ty);
        state.set(*slot, ty);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let ty = state.get(*source_slot);
        record_local_type(local_types, *captured_slot, ty);
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    collect_stmt_types(
        &function_impl.body_stmts,
        &mut state,
        local_types,
        &mut context,
    );
    let _ = context.infer_expr_type(&function_impl.body_expr, &state);
}

fn collect_stmt_types(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    local_types: &mut [ValueType],
    context: &mut TypeContext<'_>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                record_local_type(local_types, *index, BoundType::Null);
                state.set(*index, BoundType::Null);
            }
            Stmt::ClosureLet { closure, .. } => {
                collect_closure_capture_types(closure, state, local_types);
                let _ = context.infer_expr_type(&closure.body, state);
            }
            Stmt::FuncDecl { index, .. } => {
                if let Some(function_impl) = context.function_impls.get(index) {
                    for (source_slot, captured_slot) in &function_impl.capture_copies {
                        let ty = state.get(*source_slot);
                        record_local_type(local_types, *captured_slot, ty);
                        let source_state = state.clone();
                        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
                    }
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                let expr_state = state.clone();
                let ty = context.infer_expr_type(expr, &expr_state);
                record_local_type(local_types, *index, ty);
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, .. } => {
                let _ = context.infer_expr_type(expr, state);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = context.infer_expr_type(condition, state);
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                collect_stmt_types(then_branch, &mut then_state, local_types, context);
                collect_stmt_types(else_branch, &mut else_state, local_types, context);
                state.merge_from_branches(&then_state, &else_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                collect_stmt_types(std::slice::from_ref(init), state, local_types, context);
                stabilize_loop_state(state, |iterated| {
                    let _ = context.infer_expr_type(condition, iterated);
                    collect_stmt_types(body, iterated, local_types, context);
                    collect_stmt_types(std::slice::from_ref(post), iterated, local_types, context);
                });
            }
            Stmt::While {
                condition, body, ..
            } => {
                stabilize_loop_state(state, |iterated| {
                    let _ = context.infer_expr_type(condition, iterated);
                    collect_stmt_types(body, iterated, local_types, context);
                });
            }
        }
    }
}

fn collect_closure_capture_types(
    closure: &ClosureExpr,
    state: &LocalTypeState,
    local_types: &mut [ValueType],
) {
    for (source_slot, captured_slot) in &closure.capture_copies {
        record_local_type(local_types, *captured_slot, state.get(*source_slot));
    }
}

fn record_local_type(local_types: &mut [ValueType], slot: LocalSlot, ty: BoundType) {
    let Some(entry) = local_types.get_mut(slot as usize) else {
        return;
    };
    let next = ValueType::from(ty);
    *entry = match (*entry, next) {
        (ValueType::Unknown, next) => next,
        (current, next) if current == next => current,
        _ => ValueType::Unknown,
    };
}
