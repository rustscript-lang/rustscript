use std::collections::{HashMap, HashSet};

use crate::builtins::CallableParam;
use crate::bytecode::ValueType;

use super::super::ir::{ClosureExpr, LocalSlot, TypeSchema};

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
    pub(super) fn type_name(self) -> Option<&'static str> {
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

    pub(super) fn simple_type(self) -> Option<SimpleType> {
        match self {
            BoundType::Null => Some(SimpleType::Null),
            BoundType::Int => Some(SimpleType::Int),
            BoundType::Float => Some(SimpleType::Float),
            BoundType::Bool => Some(SimpleType::Bool),
            BoundType::String => Some(SimpleType::String),
            _ => None,
        }
    }

    pub(super) fn from_simple(value: SimpleType) -> Self {
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

pub(super) fn merge_container_element_types(
    lhs: Option<SimpleType>,
    rhs: Option<SimpleType>,
) -> BoundType {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) if lhs == rhs => BoundType::ArrayOf(Some(lhs)),
        (None, Some(rhs)) | (Some(rhs), None) => BoundType::ArrayOf(Some(rhs)),
        (None, None) => BoundType::ArrayOf(None),
        _ => BoundType::Array,
    }
}

pub(super) fn merge_map_element_types(
    lhs: Option<SimpleType>,
    rhs: Option<SimpleType>,
) -> BoundType {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) if lhs == rhs => BoundType::MapOf(Some(lhs)),
        (None, Some(rhs)) | (Some(rhs), None) => BoundType::MapOf(Some(rhs)),
        (None, None) => BoundType::MapOf(None),
        _ => BoundType::Map,
    }
}

pub(super) fn merge_bound_types(lhs: BoundType, rhs: BoundType) -> BoundType {
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

pub(super) fn are_compatible_bound_types(lhs: BoundType, rhs: BoundType) -> bool {
    lhs == BoundType::Unknown
        || rhs == BoundType::Unknown
        || lhs == BoundType::Null
        || rhs == BoundType::Null
        || merge_bound_types(lhs, rhs) != BoundType::Unknown
}

#[derive(Clone, Debug)]
pub(super) enum InferredCallable {
    Function(u16),
    Closure(ClosureExpr),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LocalTypeState {
    by_slot: HashMap<LocalSlot, BoundType>,
    schemas: HashMap<LocalSlot, TypeSchema>,
    declared_schema_slots: HashSet<LocalSlot>,
    callables: HashMap<LocalSlot, InferredCallable>,
}

impl LocalTypeState {
    pub(crate) fn get(&self, slot: LocalSlot) -> BoundType {
        self.by_slot
            .get(&slot)
            .copied()
            .unwrap_or(BoundType::Unknown)
    }

    pub(super) fn callable(&self, slot: LocalSlot) -> Option<&InferredCallable> {
        self.callables.get(&slot)
    }

    pub(crate) fn schema(&self, slot: LocalSlot) -> Option<&TypeSchema> {
        self.schemas.get(&slot)
    }

    pub(crate) fn has_declared_schema(&self, slot: LocalSlot) -> bool {
        self.declared_schema_slots.contains(&slot)
    }

    pub(super) fn iter_slots(&self) -> impl Iterator<Item = LocalSlot> + '_ {
        self.by_slot.keys().copied()
    }

    pub(crate) fn set(&mut self, slot: LocalSlot, ty: BoundType) {
        self.set_with_schema_origin(slot, ty, None, false);
    }

    pub(crate) fn set_with_schema_origin(
        &mut self,
        slot: LocalSlot,
        ty: BoundType,
        schema: Option<TypeSchema>,
        from_declared_schema: bool,
    ) {
        if ty == BoundType::Unknown {
            self.by_slot.remove(&slot);
        } else {
            self.by_slot.insert(slot, ty);
        }
        if let Some(schema) = schema {
            self.schemas.insert(slot, schema);
        } else {
            self.schemas.remove(&slot);
        }
        if from_declared_schema {
            self.declared_schema_slots.insert(slot);
        } else {
            self.declared_schema_slots.remove(&slot);
        }
        self.callables.remove(&slot);
    }

    pub(super) fn bind_callable(&mut self, slot: LocalSlot, callable: InferredCallable) {
        self.by_slot.remove(&slot);
        self.schemas.remove(&slot);
        self.declared_schema_slots.remove(&slot);
        self.callables.insert(slot, callable);
    }

    pub(crate) fn bind_function(&mut self, slot: LocalSlot, index: u16) {
        self.bind_callable(slot, InferredCallable::Function(index));
    }

    pub(crate) fn bind_closure(&mut self, slot: LocalSlot, closure: &ClosureExpr) {
        self.bind_callable(slot, InferredCallable::Closure(closure.clone()));
    }

    pub(super) fn copy_binding_from(
        &mut self,
        source: &LocalTypeState,
        source_slot: LocalSlot,
        slot: LocalSlot,
        fallback_schema: Option<TypeSchema>,
        fallback_from_declared_schema: bool,
    ) {
        if let Some(callable) = source.callable(source_slot).cloned() {
            self.bind_callable(slot, callable);
        } else {
            self.set_with_schema_origin(
                slot,
                source.get(source_slot),
                source.schema(source_slot).cloned().or(fallback_schema),
                source.has_declared_schema(source_slot) || fallback_from_declared_schema,
            );
        }
    }

    pub(crate) fn merge_from_branches(&mut self, lhs: &LocalTypeState, rhs: &LocalTypeState) {
        self.by_slot.clear();
        self.schemas.clear();
        self.declared_schema_slots.clear();
        self.callables.clear();
        for slot in lhs.iter_slots().chain(rhs.iter_slots()) {
            let l = lhs.get(slot);
            let r = rhs.get(slot);
            let merged = merge_bound_types(l, r);
            if merged != BoundType::Unknown {
                self.by_slot.insert(slot, merged);
            }
            if lhs.schema(slot) == rhs.schema(slot)
                && let Some(schema) = lhs.schema(slot).cloned()
            {
                self.schemas.insert(slot, schema);
            }
            if lhs.has_declared_schema(slot) && rhs.has_declared_schema(slot) {
                self.declared_schema_slots.insert(slot);
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

pub(super) fn stabilize_loop_state<F>(state: &mut LocalTypeState, mut run_iteration: F)
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

pub(super) fn try_stabilize_loop_state<E, F>(
    state: &mut LocalTypeState,
    mut run_iteration: F,
) -> Result<(), E>
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
    pub callable_slots: Vec<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct HostCallableSignature {
    pub(crate) name: String,
    pub(crate) params: Vec<CallableParam>,
}
