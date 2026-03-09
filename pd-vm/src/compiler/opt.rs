use std::collections::HashMap;

use crate::builtins::{BuiltinFunction, CallableParamType, CallableSignature};
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
        (BoundType::ArrayOf(lhs), BoundType::ArrayOf(rhs)) => merge_container_element_types(lhs, rhs),
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
    lhs == BoundType::Unknown || rhs == BoundType::Unknown || merge_bound_types(lhs, rhs) != BoundType::Unknown
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
    pub(crate) params: Vec<CallableParamType>,
}

struct TypeContext<'a> {
    function_impls: &'a HashMap<u16, FunctionImpl>,
    host_import_return_types: &'a HashMap<u16, BoundType>,
    host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    active_functions: Vec<u16>,
}

impl<'a> TypeContext<'a> {
    fn new(
        function_impls: &'a HashMap<u16, FunctionImpl>,
        host_import_return_types: &'a HashMap<u16, BoundType>,
        host_import_signatures: &'a HashMap<u16, HostCallableSignature>,
    ) -> Self {
        Self {
            function_impls,
            host_import_return_types,
            host_import_signatures,
            active_functions: Vec::new(),
        }
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
            BuiltinFunction::Get if args.len() == 2 => {
                self.infer_get_return_type(&args[0], state)
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
            | BuiltinFunction::MathSignum if args.len() == 1 => {
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

    fn infer_same_numeric_return_type(
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
        if first_ty == BoundType::Int
            && second_ty == BoundType::Int
            && third_ty == BoundType::Int
        {
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
                    } else if let Some(signature) =
                        self.host_import_signatures.get(&index).cloned()
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
    let host_import_return_types = build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &host_import_return_types,
        &host_import_signatures,
    );
    legalize_stmts(&mut ir.stmts, &mut top_state, &mut context);

    let function_impls = ir.function_impls.clone();
    for function_impl in ir.function_impls.values_mut() {
        legalize_function_impl(
            function_impl,
            &function_impls,
            &host_import_return_types,
            &host_import_signatures,
        );
    }

    ir
}

pub(super) fn infer_types(ir: &FrontendIr) -> TypeInferenceResult {
    let host_import_return_types = build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut local_types = vec![ValueType::Unknown; ir.locals];
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &host_import_return_types,
        &host_import_signatures,
    );
    collect_stmt_types(&ir.stmts, &mut top_state, &mut local_types, &mut context);

    for function_impl in ir.function_impls.values() {
        collect_function_types(
            function_impl,
            &mut local_types,
            &ir.function_impls,
            &host_import_return_types,
            &host_import_signatures,
        );
    }

    TypeInferenceResult { local_types }
}

pub(super) fn validate_if_else_type_consistency(ir: &FrontendIr) -> Result<(), CompileError> {
    let host_import_return_types = build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &host_import_return_types,
        &host_import_signatures,
    );
    validate_stmts(&ir.stmts, &mut top_state, None, &mut context)?;

    for function_impl in ir.function_impls.values() {
        validate_function_impl(
            function_impl,
            &ir.function_impls,
            &host_import_return_types,
            &host_import_signatures,
        )?;
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
    let mut context = TypeContext::new(
        function_impls,
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
    let mut context = TypeContext::new(
        function_impls,
        host_import_return_types,
        host_import_signatures,
    );
    context.apply_stmts(stmts, state);
}

fn legalize_function_impl(
    function_impl: &mut FunctionImpl,
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        host_import_return_types,
        host_import_signatures,
    );
    for slot in &function_impl.param_slots {
        state.set(*slot, BoundType::Unknown);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    legalize_stmts(&mut function_impl.body_stmts, &mut state, &mut context);
    let _ = legalize_expr(&mut function_impl.body_expr, &state, &mut context);
}

fn validate_function_impl(
    function_impl: &FunctionImpl,
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> Result<(), CompileError> {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        host_import_return_types,
        host_import_signatures,
    );
    for slot in &function_impl.param_slots {
        state.set(*slot, BoundType::Unknown);
    }
    for (source_slot, captured_slot) in &function_impl.capture_copies {
        let source_state = state.clone();
        state.copy_binding_from(&source_state, *source_slot, *captured_slot);
    }
    validate_stmts(&function_impl.body_stmts, &mut state, None, &mut context)?;
    let _ = validate_expr(&function_impl.body_expr, &state, None, &mut context)?;
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
                let _ = validate_expr(&closure.body, state, line_context, context)?;
            }
            Stmt::Let { index, expr, line } | Stmt::Assign { index, expr, line } => {
                let expr_state = state.clone();
                let ty = validate_expr(expr, &expr_state, Some(*line), context)?;
                bind_expr_result_to_slot(state, *index, expr, &expr_state, ty, context);
            }
            Stmt::Expr { expr, line } => {
                let _ = validate_expr(expr, state, Some(*line), context)?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let _ = validate_expr(condition, state, Some(*line), context)?;
                let mut then_state = state.clone();
                let mut else_state = state.clone();
                validate_stmts(then_branch, &mut then_state, Some(*line), context)?;
                validate_stmts(else_branch, &mut else_state, Some(*line), context)?;
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
                validate_stmts(std::slice::from_ref(init), state, Some(*line), context)?;
                try_stabilize_loop_state(state, |iterated| {
                    let _ = validate_expr(condition, iterated, Some(*line), context)?;
                    validate_stmts(body, iterated, Some(*line), context)?;
                    validate_stmts(std::slice::from_ref(post), iterated, Some(*line), context)
                })?;
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                try_stabilize_loop_state(state, |iterated| {
                    let _ = validate_expr(condition, iterated, Some(*line), context)?;
                    validate_stmts(body, iterated, Some(*line), context)
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
    match name {
        "print" | "println" => {
            return Some(HostCallableSignature {
                name: name.to_string(),
                params: vec![CallableParamType::Any],
            });
        }
        _ => {}
    }

    let function = edge_abi::function_by_name(name)?;
    Some(HostCallableSignature {
        name: name.to_string(),
        params: function
            .param_types
            .iter()
            .copied()
            .map(callable_param_type_from_abi)
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
    params: &[CallableParamType],
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

fn params_match_actual(params: &[CallableParamType], actual: &[BoundType]) -> bool {
    params.len() == actual.len()
        && params
            .iter()
            .zip(actual.iter().copied())
            .all(|(expected, actual)| param_accepts_bound_type(*expected, actual))
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
        CallableParamType::StringOrArray => {
            matches!(
                actual,
                BoundType::String | BoundType::Array | BoundType::ArrayOf(_)
            )
        }
        CallableParamType::ArrayOrMap => {
            matches!(
                actual,
                BoundType::Array | BoundType::ArrayOf(_) | BoundType::Map | BoundType::MapOf(_)
            )
        }
        CallableParamType::StringArrayOrMap => {
            matches!(
                actual,
                BoundType::String
                    | BoundType::Array
                    | BoundType::ArrayOf(_)
                    | BoundType::Map
                    | BoundType::MapOf(_)
            )
        }
    }
}

fn format_signature_overloads(name: &str, signatures: &[CallableSignature]) -> String {
    signatures
        .iter()
        .map(|signature| format!("{name}({})", format_param_types(signature.params)))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn format_param_types(params: &[CallableParamType]) -> String {
    params
        .iter()
        .enumerate()
        .map(|(index, param)| format!("arg{}: {}", index + 1, param.label()))
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
) -> Result<BoundType, CompileError> {
    Ok(match expr {
        Expr::Null => BoundType::Null,
        Expr::Int(_) => BoundType::Int,
        Expr::Float(_) => BoundType::Float,
        Expr::Bool(_) => BoundType::Bool,
        Expr::String(_) => BoundType::String,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            validate_expr(inner, state, line_context, context)?
        }
        Expr::Var(slot) | Expr::MoveVar(slot) => state.get(*slot),
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => state.get(*root),
        Expr::FunctionRef(_) | Expr::Call(_, _) | Expr::LocalCall(_, _) | Expr::Closure(_) => {
            validate_expr_children(expr, state, line_context, context)?;
            context.validate_call_argument_types(expr, state, line_context)?;
            context.infer_call_like_expr_type(expr, state)
        }
        Expr::ClosureCall(_, _) => {
            validate_expr_children(expr, state, line_context, context)?;
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
            let lhs_ty = validate_expr(lhs, state, line_context, context)?;
            let rhs_ty = validate_expr(rhs, state, line_context, context)?;
            infer_binary_type(expr, lhs_ty, rhs_ty)
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            let inner_ty = validate_expr(inner, state, line_context, context)?;
            infer_unary_type(expr, inner_ty)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            let _ = validate_expr(condition, state, line_context, context)?;
            let then_ty = validate_expr(then_expr, state, line_context, context)?;
            let else_ty = validate_expr(else_expr, state, line_context, context)?;
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
            let _ = validate_expr(value, state, line_context, context)?;
            let mut arm_type = BoundType::Unknown;
            for (_pattern, arm_expr) in arms {
                let ty = validate_expr(arm_expr, state, line_context, context)?;
                arm_type = if arm_type == BoundType::Unknown {
                    ty
                } else if arm_type == ty {
                    arm_type
                } else {
                    BoundType::Unknown
                };
            }
            let default_ty = validate_expr(default, state, line_context, context)?;
            if arm_type != BoundType::Unknown && arm_type == default_ty {
                arm_type
            } else {
                BoundType::Unknown
            }
        }
        Expr::Block { stmts, expr } => {
            let mut nested = state.clone();
            validate_stmts(stmts, &mut nested, line_context, context)?;
            validate_expr(expr, &nested, line_context, context)?
        }
    })
}

fn validate_expr_children(
    expr: &Expr,
    state: &LocalTypeState,
    line_context: Option<u32>,
    context: &mut TypeContext<'_>,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                let _ = validate_expr(arg, state, line_context, context)?;
            }
        }
        Expr::Closure(closure) => {
            let _ = validate_expr(&closure.body, state, line_context, context)?;
        }
        Expr::ClosureCall(closure, args) => {
            let _ = validate_expr(&closure.body, state, line_context, context)?;
            for arg in args {
                let _ = validate_expr(arg, state, line_context, context)?;
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

fn collect_function_types(
    function_impl: &FunctionImpl,
    local_types: &mut [ValueType],
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) {
    let mut state = LocalTypeState::default();
    let mut context = TypeContext::new(
        function_impls,
        host_import_return_types,
        host_import_signatures,
    );
    for slot in &function_impl.param_slots {
        record_local_type(local_types, *slot, BoundType::Unknown);
        state.set(*slot, BoundType::Unknown);
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
