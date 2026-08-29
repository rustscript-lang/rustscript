//! Interpreter instance state.
//!
//! [`Instance`] owns everything that describes one execution position inside a
//! program: the instruction pointer, operand stack, locals, frames, capture
//! cells, callable ownership, queued callback traffic, waiting/yield state,
//! and instance-only counters. It has no program reference of its own; the
//! immutable [`Program`](crate::bytecode::Program) and the backend
//! [`Engine`](super::engine::Engine) live beside it, so one program can drive
//! many independent instances and a reset only touches this struct.
//!
//! Lifecycle: [`Instance::new`] starts a fresh halted instance; [`Instance::reset`]
//! rewinds run state while keeping configuration and host bindings (owned by
//! the facade); [`Instance::drop_cleanup`] releases interpreter-owned values
//! with drop-contract accounting.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};

use crate::bytecode::{CallableValue, MAX_FRAME_LOCAL_COUNT, Program, SharedCaptureCell, Value};
use crate::vm::host::WaitingHostOp;
use crate::vm::invocation::{InvocationPhase, InvocationState};
use crate::vm::map_iter::MapIteratorState;
use crate::vm::{DEFAULT_MAX_SCRIPT_CALL_DEPTH, VmYieldReason};

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrameContinuation {
    Halt,
    ResumeBytecode { return_ip: usize },
    ReturnToHost,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct ExecutionFrame {
    pub(crate) continuation: FrameContinuation,
    pub(crate) operand_stack_base: usize,
    pub(crate) local_base: usize,
    pub(crate) local_count: usize,
    pub(crate) prototype_id: Option<u32>,
}

impl ExecutionFrame {
    pub(crate) fn root(local_count: usize) -> Self {
        Self {
            continuation: FrameContinuation::Halt,
            operand_stack_base: 0,
            local_base: 0,
            local_count,
            prototype_id: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedCallable {
    pub(crate) callable: Value,
    pub(crate) args: Vec<Value>,
    pub(crate) subscription: Option<Arc<AtomicBool>>,
}

/// Interpreter-owned execution state.
///
/// Thread safety: `Instance` is `!Sync` (it owns mutable interpreter state)
/// and is not shared; the VM facade owns exactly one instance. It is not
/// clonable: cloning would silently duplicate stack/frame/wait state.
pub(crate) struct Instance {
    pub(crate) ip: usize,
    pub(crate) stack: Vec<Value>,
    pub(crate) locals: Vec<Value>,
    pub(crate) capture_cells: HashMap<usize, SharedCaptureCell>,
    pub(crate) shared_capture_slots: HashSet<usize>,
    pub(crate) execution_frames: Vec<ExecutionFrame>,
    pub(crate) active_local_base_cache: usize,
    pub(crate) active_operand_stack_base_cache: usize,
    pub(crate) call_depth: usize,
    pub(crate) run_depth: usize,
    pub(crate) max_script_call_depth: usize,
    pub(crate) host_return: Option<Value>,
    pub(crate) queued_callables: VecDeque<QueuedCallable>,
    pub(crate) completed_callable_results: VecDeque<Value>,
    pub(crate) owned_callables: Vec<Weak<CallableValue>>,
    pub(crate) callback_registry_flags: Vec<Weak<AtomicBool>>,
    pub(crate) draining_queued_callables: bool,
    pub(crate) shutdown: bool,
    pub(super) waiting_host_op: Option<WaitingHostOp>,
    pub(crate) last_yield_reason: Option<VmYieldReason>,
    pub(crate) invocation: Option<InvocationState>,
    pub(crate) map_iterators: Vec<Vec<Option<MapIteratorState>>>,
    pub(crate) drop_contract_events_enabled: bool,
    pub(crate) drop_contract_events: u64,
    pub(crate) operand_hint_hit_count: u64,
    pub(crate) operand_hint_miss_count: u64,
    pub(crate) typed_builtin_fast_path_count: u64,
    pub(crate) projection_fast_path_count: u64,
    pub(crate) generic_builtin_call_count: u64,
    pub(crate) scalar_superinstruction_count: u64,
    pub(crate) local_type_hint_hit_count: u64,
}

impl Instance {
    /// Creates a halted instance positioned at program entry.
    pub(crate) fn new(program: &Program) -> Self {
        let local_count = if program.local_count <= MAX_FRAME_LOCAL_COUNT {
            program.local_count
        } else {
            0
        };
        Self {
            ip: 0,
            stack: Vec::new(),
            locals: vec![Value::Null; local_count],
            capture_cells: HashMap::new(),
            shared_capture_slots: HashSet::new(),
            execution_frames: vec![ExecutionFrame::root(local_count)],
            active_local_base_cache: 0,
            active_operand_stack_base_cache: 0,
            call_depth: 0,
            run_depth: 0,
            max_script_call_depth: DEFAULT_MAX_SCRIPT_CALL_DEPTH,
            host_return: None,
            queued_callables: VecDeque::new(),
            completed_callable_results: VecDeque::new(),
            owned_callables: Vec::new(),
            callback_registry_flags: Vec::new(),
            draining_queued_callables: false,
            shutdown: false,
            waiting_host_op: None,
            last_yield_reason: None,
            invocation: None,
            map_iterators: Vec::new(),
            drop_contract_events_enabled: false,
            drop_contract_events: 0,
            operand_hint_hit_count: 0,
            operand_hint_miss_count: 0,
            typed_builtin_fast_path_count: 0,
            projection_fast_path_count: 0,
            generic_builtin_call_count: 0,
            scalar_superinstruction_count: 0,
            local_type_hint_hit_count: 0,
        }
    }

    /// Rewinds run-scoped interpreter state for a fresh execution of the same
    /// program. Host bindings, backend configuration, and compiled artifacts
    /// (owned outside this struct) are preserved.
    pub(crate) fn reset(&mut self, program: &Program) {
        self.invalidate_callback_registries();
        self.ip = 0;
        self.drop_contract_events = 0;
        self.last_yield_reason = None;
        self.clear_stack_with_drop_contract();
        self.capture_cells.clear();
        self.shared_capture_slots.clear();
        self.clear_locals_with_drop_contract();
        self.owned_callables.clear();
        self.locals.resize(program.local_count, Value::Null);
        self.initialize_root_callable_bindings(program);
        self.call_depth = 0;
        self.run_depth = 0;
        self.execution_frames.clear();
        self.execution_frames
            .push(ExecutionFrame::root(program.local_count));
        self.active_local_base_cache = 0;
        self.active_operand_stack_base_cache = 0;
        self.host_return = None;
        self.queued_callables.clear();
        self.completed_callable_results.clear();
        self.draining_queued_callables = false;
        self.shutdown = false;
        self.waiting_host_op = None;
        self.drop_invocation_state();
        self.invocation = None;
        self.map_iterators.clear();
        self.clear_interpreter_metrics();
    }

    pub(crate) fn is_reusable(&self, program: &Program) -> bool {
        if self.run_depth != 0
            || !self.stack.is_empty()
            || !self.capture_cells.is_empty()
            || !self.shared_capture_slots.is_empty()
            || self.active_local_base_cache != 0
            || self.active_operand_stack_base_cache != 0
            || self.call_depth != 0
            || self.host_return.is_some()
            || !self.queued_callables.is_empty()
            || !self.completed_callable_results.is_empty()
            || self.draining_queued_callables
            || self.shutdown
            || self.waiting_host_op.is_some()
            || self.last_yield_reason.is_some()
            || self.map_iterators.iter().flatten().any(Option::is_some)
        {
            return false;
        }

        let reset_frame = self
            .execution_frames
            .as_slice()
            .first()
            .is_some_and(|root| {
                self.execution_frames.len() == 1
                    && root.continuation == FrameContinuation::Halt
                    && root.operand_stack_base == 0
                    && root.local_base == 0
                    && root.local_count == program.local_count
                    && root.prototype_id.is_none()
            });
        let halted = self.execution_frames.is_empty();
        if (!reset_frame && !halted) || (reset_frame && self.ip != 0) {
            return false;
        }
        if self.locals.len() != program.local_count {
            return false;
        }

        let mut root_bindings = HashMap::new();
        for binding in &program.root_callable_bindings {
            if binding.local_slot as usize >= program.local_count
                || program
                    .callable_prototypes
                    .get(binding.prototype_id as usize)
                    .is_none()
            {
                continue;
            }
            root_bindings.insert(binding.local_slot as usize, binding.prototype_id);
        }
        for (slot, value) in self.locals.iter().enumerate() {
            match root_bindings.get(&slot) {
                Some(prototype_id) => {
                    if !matches!(value, Value::Callable(callable) if callable.prototype_id == *prototype_id)
                    {
                        return false;
                    }
                }
                None if !matches!(value, Value::Null) => return false,
                None => {}
            }
        }

        match self.invocation.as_ref() {
            None => true,
            Some(state) => {
                matches!(state.phase, InvocationPhase::Fused)
                    && !state.emit_yield_pending
                    && state.pending_error.is_none()
                    && state.cancel_reason.is_none()
            }
        }
    }

    /// Releases interpreter-owned values with drop-contract accounting. Used by
    /// the facade's `Drop` (and by `shutdown`).
    pub(crate) fn drop_cleanup(&mut self) {
        self.drop_invocation_state();
        self.clear_stack_with_drop_contract();
        self.capture_cells.clear();
        self.shared_capture_slots.clear();
        self.clear_locals_with_drop_contract();
    }

    /// Drops pending invocation stream values with drop-contract accounting and
    /// rewinds the invocation state to a fresh, fused position.
    pub(crate) fn drop_invocation_state(&mut self) {
        let Some(state) = self.invocation.as_mut() else {
            return;
        };
        let value = match std::mem::replace(&mut state.phase, InvocationPhase::Fused) {
            InvocationPhase::EventPending(value) | InvocationPhase::CompletePending(value) => {
                Some(value)
            }
            _ => None,
        };
        state.emit_yield_pending = false;
        state.pending_error = None;
        state.cancel_reason = None;
        if let Some(value) = value {
            self.drop_value_with_contract(value);
        }
    }

    pub(crate) fn invalidate_callback_registries(&mut self) {
        for active in self
            .callback_registry_flags
            .drain(..)
            .filter_map(|flag| flag.upgrade())
        {
            active.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    pub(crate) fn register_callback_registry(&mut self, active: &Arc<AtomicBool>) {
        self.callback_registry_flags.push(Arc::downgrade(active));
    }

    pub(crate) fn initialize_root_callable_bindings(&mut self, program: &Program) {
        let bindings = program.root_callable_bindings.clone();
        for binding in bindings {
            let Some(kind) = program
                .callable_prototypes
                .get(binding.prototype_id as usize)
                .map(|prototype| prototype.kind)
            else {
                continue;
            };
            if binding.local_slot as usize >= self.locals.len() {
                continue;
            }
            let callable = Arc::new(CallableValue {
                prototype_id: binding.prototype_id,
                kind,
                env: None,
            });
            self.owned_callables.push(Arc::downgrade(&callable));
            self.locals[binding.local_slot as usize] = Value::Callable(callable);
        }
    }

    pub(crate) fn clear_interpreter_metrics(&mut self) {
        self.operand_hint_hit_count = 0;
        self.operand_hint_miss_count = 0;
        self.typed_builtin_fast_path_count = 0;
        self.projection_fast_path_count = 0;
        self.generic_builtin_call_count = 0;
        self.scalar_superinstruction_count = 0;
        self.local_type_hint_hit_count = 0;
    }

    pub(crate) fn clear_stack_with_drop_contract(&mut self) {
        let drained = self.stack.drain(..).collect::<Vec<_>>();
        for value in drained {
            self.drop_value_with_contract(value);
        }
    }

    pub(crate) fn clear_locals_with_drop_contract(&mut self) {
        for slot in 0..self.locals.len() {
            let previous = std::mem::replace(&mut self.locals[slot], Value::Null);
            self.drop_value_with_contract(previous);
        }
    }

    pub(crate) fn drop_value_with_contract(&mut self, value: Value) {
        if self.drop_contract_events_enabled {
            self.count_value_drop_contract(&value);
        }
    }

    pub(crate) fn count_value_drop_contract(&mut self, value: &Value) {
        match value {
            Value::Null => {}
            Value::Array(values) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
                for item in values.iter() {
                    self.count_value_drop_contract(item);
                }
            }
            Value::Map(entries) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
                for (key, value) in entries.iter() {
                    self.count_value_drop_contract(key);
                    self.count_value_drop_contract(value);
                }
            }
            Value::Int(_)
            | Value::Float(_)
            | Value::Bool(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Callable(_) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
            }
        }
    }
}
