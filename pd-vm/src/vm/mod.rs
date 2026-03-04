use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use crate::builtins::BuiltinFunction;

pub(crate) mod builtins_impl;
pub mod diagnostics;
pub(crate) mod jit;
mod store;

pub use crate::bytecode::{HostImport, OpCode, Program, Value};
pub use store::Store;

#[derive(Clone, Copy, Debug)]
enum NumericValue {
    Int(i64),
    Float(f64),
}

impl Value {
    fn as_int(&self) -> Result<i64, VmError> {
        match self {
            Value::Int(value) => Ok(*value),
            _ => Err(VmError::TypeMismatch("int")),
        }
    }

    fn as_numeric(&self) -> Result<NumericValue, VmError> {
        match self {
            Value::Int(value) => Ok(NumericValue::Int(*value)),
            Value::Float(value) => Ok(NumericValue::Float(*value)),
            _ => Err(VmError::TypeMismatch("number")),
        }
    }

    fn as_bool(&self) -> Result<bool, VmError> {
        match self {
            Value::Bool(value) => Ok(*value),
            _ => Err(VmError::TypeMismatch("bool")),
        }
    }
}

#[derive(Debug)]
pub enum VmError {
    StackUnderflow,
    TypeMismatch(&'static str),
    DivisionByZero,
    InvalidShift(i64),
    InvalidConstant(u32),
    InvalidLocal(u8),
    InvalidCall(u16),
    InvalidCallArity {
        import: String,
        expected: u8,
        got: u8,
    },
    UnboundImport(String),
    InvalidOpcode(u8),
    BytecodeBounds,
    HostError(String),
    JitNative(String),
    InvalidFuelCheckInterval(u32),
    FuelOverflow,
    OutOfFuel {
        needed: u64,
        remaining: u64,
    },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::StackUnderflow => write!(f, "stack underflow"),
            VmError::TypeMismatch(expected) => write!(f, "type mismatch: expected {expected}"),
            VmError::DivisionByZero => write!(f, "division by zero"),
            VmError::InvalidShift(value) => {
                write!(f, "invalid shift amount {value}, expected 0..63")
            }
            VmError::InvalidConstant(index) => write!(f, "invalid constant {index}"),
            VmError::InvalidLocal(index) => write!(f, "invalid local {index}"),
            VmError::InvalidCall(index) => write!(f, "invalid call target {index}"),
            VmError::InvalidCallArity {
                import,
                expected,
                got,
            } => write!(
                f,
                "invalid call arity for import '{import}': expected {expected}, got {got}",
            ),
            VmError::UnboundImport(name) => write!(f, "unbound host import '{name}'"),
            VmError::InvalidOpcode(opcode) => write!(f, "invalid opcode {opcode}"),
            VmError::BytecodeBounds => write!(f, "bytecode bounds"),
            VmError::HostError(message) => write!(f, "host error: {message}"),
            VmError::JitNative(message) => write!(f, "jit native error: {message}"),
            VmError::InvalidFuelCheckInterval(value) => {
                write!(f, "invalid fuel check interval {value}, expected >= 1")
            }
            VmError::FuelOverflow => write!(f, "fuel arithmetic overflow"),
            VmError::OutOfFuel { needed, remaining } => write!(
                f,
                "out of fuel: needed {needed} units, remaining {remaining}"
            ),
        }
    }
}

impl std::error::Error for VmError {}

pub type VmResult<T> = Result<T, VmError>;

pub type HostOpId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuelCheckpoint {
    remaining: Option<u64>,
    check_interval: u32,
    ops_until_check: u32,
}

impl FuelCheckpoint {
    pub fn fuel(&self) -> Option<u64> {
        self.remaining
    }

    pub fn check_interval(&self) -> u32 {
        self.check_interval
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmStatus {
    Halted,
    Yielded,
    Waiting(HostOpId),
}

#[derive(Debug, PartialEq)]
pub enum CallOutcome {
    Return(Vec<Value>),
    Yield,
    Pending(HostOpId),
}

pub trait HostFunction: Send {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome>;
}

pub trait HostAsyncBridge: Send {
    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<Vec<Value>>>;
}

pub type StaticHostFunction = fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>;

type HostFactory = dyn Fn() -> Box<dyn HostFunction> + Send + Sync;

enum RegistryEntryKind {
    Factory(Box<HostFactory>),
    Static(StaticHostFunction),
}

struct RegistryEntry {
    arity: u8,
    kind: RegistryEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBindingPlan {
    import_signature: Vec<HostImport>,
    registry_slots: Vec<u16>,
    resolved_calls: Vec<u16>,
}

pub struct HostFunctionRegistry {
    entries: Vec<RegistryEntry>,
    by_name: HashMap<String, u16>,
    plan_cache: HashMap<Vec<HostImport>, HostBindingPlan>,
}

impl Default for HostFunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostFunctionRegistry {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            by_name: HashMap::new(),
            plan_cache: HashMap::new(),
        }
    }

    pub fn register<F>(&mut self, name: impl Into<String>, arity: u8, factory: F)
    where
        F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
    {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = self.entries.get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::Factory(Box::new(factory));
            self.plan_cache.clear();
            return;
        }

        let slot = self.entries.len() as u16;
        self.entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Factory(Box::new(factory)),
        });
        self.by_name.insert(name, slot);
        self.plan_cache.clear();
    }

    pub fn register_static(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        function: StaticHostFunction,
    ) {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = self.entries.get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::Static(function);
            self.plan_cache.clear();
            return;
        }

        let slot = self.entries.len() as u16;
        self.entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Static(function),
        });
        self.by_name.insert(name, slot);
        self.plan_cache.clear();
    }

    pub fn bind_vm_cached(&mut self, vm: &mut Vm) -> VmResult<()> {
        let plan = self.prepare_plan(&vm.program.imports)?;
        self.bind_vm_with_plan(vm, &plan)
    }

    pub fn prepare_plan(&mut self, imports: &[HostImport]) -> VmResult<HostBindingPlan> {
        self.plan_for_imports(imports).cloned()
    }

    fn plan_for_imports(&mut self, imports: &[HostImport]) -> VmResult<&HostBindingPlan> {
        if !self.plan_cache.contains_key(imports) {
            let mut registry_slot_to_vm_slot: HashMap<u16, u16> = HashMap::new();
            let mut registry_slots = Vec::new();
            let mut resolved_calls = Vec::with_capacity(imports.len());

            for import in imports {
                let registry_slot = self
                    .by_name
                    .get(&import.name)
                    .copied()
                    .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?;
                let entry = self
                    .entries
                    .get(registry_slot as usize)
                    .ok_or(VmError::InvalidCall(registry_slot))?;
                if entry.arity != import.arity {
                    return Err(VmError::InvalidCallArity {
                        import: import.name.clone(),
                        expected: entry.arity,
                        got: import.arity,
                    });
                }

                let vm_slot = if let Some(&existing) = registry_slot_to_vm_slot.get(&registry_slot)
                {
                    existing
                } else {
                    let slot = registry_slots.len() as u16;
                    registry_slots.push(registry_slot);
                    registry_slot_to_vm_slot.insert(registry_slot, slot);
                    slot
                };
                resolved_calls.push(vm_slot);
            }

            self.plan_cache.insert(
                imports.to_vec(),
                HostBindingPlan {
                    import_signature: imports.to_vec(),
                    registry_slots,
                    resolved_calls,
                },
            );
        }

        self.plan_cache
            .get(imports)
            .ok_or_else(|| VmError::HostError("host binding plan cache lookup failed".to_string()))
    }

    pub fn bind_vm_with_plan(&self, vm: &mut Vm, plan: &HostBindingPlan) -> VmResult<()> {
        if vm.program.imports != plan.import_signature {
            return Err(VmError::HostError(
                "host binding plan does not match vm import signature".to_string(),
            ));
        }
        if !vm.host_functions.is_empty() || !vm.host_function_symbols.is_empty() {
            return Err(VmError::HostError(
                "host binding cache requires an unbound vm".to_string(),
            ));
        }

        for &registry_slot in &plan.registry_slots {
            let entry = self
                .entries
                .get(registry_slot as usize)
                .ok_or(VmError::InvalidCall(registry_slot))?;
            match &entry.kind {
                RegistryEntryKind::Factory(factory) => {
                    vm.register_function(factory());
                }
                RegistryEntryKind::Static(function) => {
                    vm.register_static_function(*function);
                }
            }
        }
        vm.install_resolved_calls(plan.resolved_calls.clone())?;
        Ok(())
    }
}

enum VmHostFunction {
    Dynamic(Box<dyn HostFunction>),
    Static(StaticHostFunction),
}

pub struct Vm {
    program: Arc<Program>,
    program_constants_ptr: *const Value,
    program_constants_len: usize,
    program_cache_key: u64,
    program_cache_key_ready: bool,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    host_functions: Vec<VmHostFunction>,
    host_function_symbols: HashMap<String, u16>,
    builtin_overrides: HashMap<u16, u16>,
    resolved_calls: Vec<u16>,
    resolved_calls_dirty: bool,
    call_depth: usize,
    jit: jit::TraceJitEngine,
    native_traces: HashMap<usize, jit::NativeTrace>,
    native_trace_exec_count: u64,
    async_bridge: Option<Box<dyn HostAsyncBridge>>,
    waiting_host_op: Option<WaitingHostOp>,
    next_host_op_id: HostOpId,
    io_state: builtins_impl::IoState,
    fuel_remaining: Option<u64>,
    fuel_check_interval: u32,
    fuel_ops_until_check: u32,
}

enum ExecOutcome {
    Continue,
    Halted,
    Yielded,
    Waiting(HostOpId),
}

enum HostCallExecOutcome {
    Returned,
    Yielded,
    Pending(HostOpId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WaitingHostOp {
    op_id: HostOpId,
    source: WaitingHostOpSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitingHostOpSource {
    HostBridge,
    BuiltinIo,
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

fn compute_program_cache_key(program: &Program) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    program.code.hash(&mut hasher);
    program.local_count.hash(&mut hasher);
    for constant in &program.constants {
        hash_value(constant, &mut hasher);
    }
    program.imports.hash(&mut hasher);
    hasher.finish()
}

fn hash_value(value: &Value, state: &mut impl Hasher) {
    match value {
        Value::Null => {
            6u8.hash(state);
        }
        Value::Int(value) => {
            0u8.hash(state);
            value.hash(state);
        }
        Value::Float(value) => {
            1u8.hash(state);
            value.to_bits().hash(state);
        }
        Value::Bool(value) => {
            2u8.hash(state);
            value.hash(state);
        }
        Value::String(value) => {
            3u8.hash(state);
            value.hash(state);
        }
        Value::Array(values) => {
            4u8.hash(state);
            values.len().hash(state);
            for value in values {
                hash_value(value, state);
            }
        }
        Value::Map(entries) => {
            5u8.hash(state);
            entries.len().hash(state);
            for (key, value) in entries {
                hash_value(key, state);
                hash_value(value, state);
            }
        }
    }
}

impl Vm {
    pub fn new(program: Program) -> Self {
        Self::new_shared(Arc::new(program))
    }

    pub fn new_shared(program: Arc<Program>) -> Self {
        let program_constants_ptr = program.constants.as_ptr();
        let program_constants_len = program.constants.len();
        let local_count = program.local_count;
        Self {
            program,
            program_constants_ptr,
            program_constants_len,
            program_cache_key: 0,
            program_cache_key_ready: false,
            ip: 0,
            stack: Vec::new(),
            locals: vec![Value::Null; local_count],
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            call_depth: 0,
            jit: jit::TraceJitEngine::default(),
            native_traces: HashMap::new(),
            native_trace_exec_count: 0,
            async_bridge: None,
            waiting_host_op: None,
            next_host_op_id: 1,
            io_state: builtins_impl::IoState::default(),
            fuel_remaining: None,
            fuel_check_interval: 1,
            fuel_ops_until_check: 1,
        }
    }

    fn ensure_program_cache_key(&mut self) -> u64 {
        if !self.program_cache_key_ready {
            self.program_cache_key = compute_program_cache_key(&self.program);
            self.program_cache_key_ready = true;
        }
        self.program_cache_key
    }

    /// Reset VM execution state to allow rerunning the same program instance while
    /// preserving JIT artifacts and registered host bindings.
    ///
    /// Locals are reset to `Null`, stack is cleared, and instruction pointer is
    /// rewound to the program entry.
    pub fn reset_for_reuse(&mut self) {
        self.ip = 0;
        self.stack.clear();
        for local in &mut self.locals {
            *local = Value::Null;
        }
        self.call_depth = 0;
        self.waiting_host_op = None;
        self.next_host_op_id = 1;
        self.io_state = builtins_impl::IoState::default();
    }

    pub fn register_function(&mut self, function: Box<dyn HostFunction>) -> u16 {
        let index = self.host_functions.len() as u16;
        self.host_functions.push(VmHostFunction::Dynamic(function));
        self.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_function(&mut self, function: StaticHostFunction) -> u16 {
        let index = self.host_functions.len() as u16;
        self.host_functions.push(VmHostFunction::Static(function));
        self.resolved_calls_dirty = true;
        index
    }

    pub fn bind_function(&mut self, name: impl Into<String>, function: Box<dyn HostFunction>) {
        let name = name.into();
        if let Some(builtin) = BuiltinFunction::from_namespaced_name(&name) {
            self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Dynamic(function));
            return;
        }
        if let Some(&index) = self.host_function_symbols.get(&name)
            && let Some(slot) = self.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::Dynamic(function);
            self.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_function(function);
        self.host_function_symbols.insert(name, index);
        self.resolved_calls_dirty = true;
    }

    pub fn bind_static_function(&mut self, name: impl Into<String>, function: StaticHostFunction) {
        let name = name.into();
        if let Some(builtin) = BuiltinFunction::from_namespaced_name(&name) {
            self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Static(function));
            return;
        }
        if let Some(&index) = self.host_function_symbols.get(&name)
            && let Some(slot) = self.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::Static(function);
            self.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_static_function(function);
        self.host_function_symbols.insert(name, index);
        self.resolved_calls_dirty = true;
    }

    pub fn bind_builtin_override(
        &mut self,
        name: impl Into<String>,
        function: Box<dyn HostFunction>,
    ) -> VmResult<()> {
        let name = name.into();
        let builtin = BuiltinFunction::from_namespaced_name(&name).ok_or_else(|| {
            VmError::HostError(format!("unknown namespaced builtin override '{name}'"))
        })?;
        self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Dynamic(function));
        Ok(())
    }

    pub fn bind_builtin_static_override(
        &mut self,
        name: impl Into<String>,
        function: StaticHostFunction,
    ) -> VmResult<()> {
        let name = name.into();
        let builtin = BuiltinFunction::from_namespaced_name(&name).ok_or_else(|| {
            VmError::HostError(format!("unknown namespaced builtin override '{name}'"))
        })?;
        self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Static(function));
        Ok(())
    }

    fn bind_builtin_overrideslot(&mut self, builtin_call_index: u16, function: VmHostFunction) {
        if let Some(&host_slot) = self.builtin_overrides.get(&builtin_call_index)
            && let Some(slot) = self.host_functions.get_mut(host_slot as usize)
        {
            *slot = function;
            return;
        }

        let host_slot = self.host_functions.len() as u16;
        self.host_functions.push(function);
        self.builtin_overrides.insert(builtin_call_index, host_slot);
    }

    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) {
        self.async_bridge = Some(bridge);
    }

    pub fn clear_async_bridge(&mut self) {
        self.async_bridge = None;
    }

    pub fn allocate_host_op_id(&mut self) -> HostOpId {
        let op_id = self.next_host_op_id;
        self.next_host_op_id = self.next_host_op_id.wrapping_add(1).max(1);
        op_id
    }

    pub fn waiting_host_op_id(&self) -> Option<HostOpId> {
        self.waiting_host_op.map(|op| op.op_id)
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.fuel_remaining = Some(fuel);
        self.fuel_ops_until_check = self.fuel_check_interval;
    }

    pub fn clear_fuel(&mut self) {
        self.fuel_remaining = None;
        self.fuel_ops_until_check = self.fuel_check_interval;
    }

    pub fn set_fuel_check_interval(&mut self, interval: u32) -> VmResult<()> {
        if interval == 0 {
            return Err(VmError::InvalidFuelCheckInterval(interval));
        }
        self.fuel_check_interval = interval;
        self.fuel_ops_until_check = interval;
        Ok(())
    }

    pub fn fuel_check_interval(&self) -> u32 {
        self.fuel_check_interval
    }

    pub fn get_fuel(&self) -> Option<u64> {
        self.fuel_remaining
            .map(|remaining| remaining.saturating_sub(self.pending_fuel_debt()))
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        if fuel == 0 {
            return Ok(());
        }
        self.fuel_remaining = Some(match self.fuel_remaining {
            Some(remaining) => remaining.checked_add(fuel).ok_or(VmError::FuelOverflow)?,
            None => fuel,
        });
        Ok(())
    }

    pub fn recharge_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.add_fuel(fuel)
    }

    pub fn consume_fuel(&mut self, fuel: u64) -> VmResult<()> {
        self.charge_fuel(fuel)
    }

    pub fn fuel_checkpoint(&self) -> FuelCheckpoint {
        FuelCheckpoint {
            remaining: self.fuel_remaining,
            check_interval: self.fuel_check_interval,
            ops_until_check: self.fuel_ops_until_check,
        }
    }

    pub fn checkpoint(&self) -> FuelCheckpoint {
        self.fuel_checkpoint()
    }

    pub fn restore_fuel(&mut self, checkpoint: FuelCheckpoint) {
        self.fuel_remaining = checkpoint.remaining;
        self.fuel_check_interval = checkpoint.check_interval.max(1);
        self.fuel_ops_until_check = checkpoint
            .ops_until_check
            .clamp(1, self.fuel_check_interval);
    }

    pub fn restore_checkpoint(&mut self, checkpoint: FuelCheckpoint) {
        self.restore_fuel(checkpoint);
    }

    pub fn complete_host_op(&mut self, op_id: HostOpId, values: Vec<Value>) -> VmResult<()> {
        self.complete_waiting_host_op(op_id, values)
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.waiting_host_op else {
            return Poll::Ready(Ok(()));
        };

        let poll_result = match waiting.source {
            WaitingHostOpSource::HostBridge => {
                let bridge_ptr = match self.async_bridge.as_mut() {
                    Some(bridge) => bridge.as_mut() as *mut dyn HostAsyncBridge,
                    None => {
                        return Poll::Ready(Err(VmError::HostError(format!(
                            "vm waiting on host op {} without an async bridge",
                            waiting.op_id
                        ))));
                    }
                };

                // SAFETY: `bridge_ptr` points to `self.async_bridge`, and this scope does not
                // mutably access `self.async_bridge` again before `poll_op` returns.
                unsafe { (&mut *bridge_ptr).poll_op(waiting.op_id, cx) }
            }
            WaitingHostOpSource::BuiltinIo => {
                builtins_impl::poll_builtin_io_op(self, waiting.op_id, cx)
            }
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(values)) => {
                self.complete_waiting_host_op(waiting.op_id, values)?;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                self.waiting_host_op = None;
                Poll::Ready(Err(err))
            }
        }
    }

    pub async fn await_waiting_host_op(&mut self) -> VmResult<()> {
        std::future::poll_fn(|cx| self.poll_waiting_host_op(cx)).await
    }

    pub fn wait_for_host_op_blocking(&mut self) -> VmResult<()> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match self.poll_waiting_host_op(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }

    pub fn run(&mut self) -> VmResult<VmStatus> {
        self.run_internal(None, true)
    }

    pub fn run_with_debugger(
        &mut self,
        debugger: &mut crate::debugger::Debugger,
    ) -> VmResult<VmStatus> {
        self.run_internal(Some(debugger), false)
    }

    fn notify_debugger_status(
        &mut self,
        debugger: &mut Option<&mut crate::debugger::Debugger>,
        status: VmStatus,
    ) {
        if let Some(active_debugger) = debugger.as_deref_mut() {
            active_debugger.on_vm_status(self, status);
        }
    }

    fn handle_debugger_error(
        &mut self,
        debugger: &mut Option<&mut crate::debugger::Debugger>,
        err: &VmError,
    ) -> bool {
        match err {
            VmError::OutOfFuel { .. } => {
                if let Some(active_debugger) = debugger.as_deref_mut() {
                    return active_debugger.on_vm_error(self, err);
                }
                false
            }
            _ => false,
        }
    }

    fn outcome_to_status(outcome: ExecOutcome) -> Option<VmStatus> {
        match outcome {
            ExecOutcome::Continue => None,
            ExecOutcome::Halted => Some(VmStatus::Halted),
            ExecOutcome::Yielded => Some(VmStatus::Yielded),
            ExecOutcome::Waiting(op_id) => Some(VmStatus::Waiting(op_id)),
        }
    }

    fn finish_outcome(
        &mut self,
        debugger: &mut Option<&mut crate::debugger::Debugger>,
        outcome: ExecOutcome,
    ) -> Option<VmStatus> {
        let status = Self::outcome_to_status(outcome)?;
        self.notify_debugger_status(debugger, status);
        Some(status)
    }

    fn run_internal(
        &mut self,
        mut debugger: Option<&mut crate::debugger::Debugger>,
        allow_jit: bool,
    ) -> VmResult<VmStatus> {
        self.ensure_call_bindings()?;
        if let Some(waiting) = self.waiting_host_op {
            let status = VmStatus::Waiting(waiting.op_id);
            self.notify_debugger_status(&mut debugger, status);
            return Ok(status);
        }

        loop {
            if let Some(active_debugger) = debugger.as_deref_mut() {
                active_debugger.on_instruction(self);
            }

            if allow_jit {
                let trace_id = {
                    let program = &self.program;
                    self.jit.observe_hot_ip(self.ip, program)
                };
                if let Some(trace_id) = trace_id {
                    let outcome = match self.execute_jit_entry(trace_id) {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            if self.handle_debugger_error(&mut debugger, &err) {
                                continue;
                            }
                            return Err(err);
                        }
                    };
                    if let Some(status) = self.finish_outcome(&mut debugger, outcome) {
                        return Ok(status);
                    }
                    continue;
                }
            }

            if self.ip >= self.program.code.len() {
                return Err(VmError::BytecodeBounds);
            }

            if self.fuel_remaining.is_some()
                && let Err(err) = self.charge_fuel_tick()
            {
                if self.handle_debugger_error(&mut debugger, &err) {
                    continue;
                }
                return Err(err);
            }
            let opcode = self.read_u8()?;
            let outcome = match self.execute_interpreter_instruction(opcode) {
                Ok(outcome) => outcome,
                Err(err) => {
                    if self.handle_debugger_error(&mut debugger, &err) {
                        continue;
                    }
                    return Err(err);
                }
            };
            if let Some(status) = self.finish_outcome(&mut debugger, outcome) {
                return Ok(status);
            }
        }
    }

    fn execute_interpreter_instruction(&mut self, opcode: u8) -> VmResult<ExecOutcome> {
        match opcode {
            x if x == OpCode::Nop as u8 => {}
            x if x == OpCode::Ret as u8 => return Ok(ExecOutcome::Halted),
            x if x == OpCode::Ldc as u8 => {
                let index = self.read_u32()?;
                let value = self
                    .program
                    .constants
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::InvalidConstant(index))?;
                self.stack.push(value);
            }
            x if x == OpCode::Add as u8 => {
                self.binary_add_op()?;
            }
            x if x == OpCode::Sub as u8 => {
                self.binary_numeric_op(
                    |lhs, rhs| Ok(lhs.wrapping_sub(rhs)),
                    |lhs, rhs| Ok(lhs - rhs),
                )?;
            }
            x if x == OpCode::Mul as u8 => {
                self.binary_numeric_op(
                    |lhs, rhs| Ok(lhs.wrapping_mul(rhs)),
                    |lhs, rhs| Ok(lhs * rhs),
                )?;
            }
            x if x == OpCode::Div as u8 => {
                self.binary_numeric_op(
                    |lhs, rhs| {
                        if rhs == 0 {
                            return Err(VmError::DivisionByZero);
                        }
                        Ok(lhs.wrapping_div(rhs))
                    },
                    |lhs, rhs| {
                        if rhs == 0.0 {
                            return Err(VmError::DivisionByZero);
                        }
                        Ok(lhs / rhs)
                    },
                )?;
            }
            x if x == OpCode::Shl as u8 => {
                let rhs = self.pop_shift_amount()?;
                let lhs = self.pop_int()?;
                self.stack.push(Value::Int(lhs.wrapping_shl(rhs)));
            }
            x if x == OpCode::Shr as u8 => {
                let rhs = self.pop_shift_amount()?;
                let lhs = self.pop_int()?;
                self.stack.push(Value::Int(lhs.wrapping_shr(rhs)));
            }
            x if x == OpCode::Mod as u8 => {
                self.binary_numeric_op(
                    |lhs, rhs| {
                        if rhs == 0 {
                            return Err(VmError::DivisionByZero);
                        }
                        Ok(lhs.wrapping_rem(rhs))
                    },
                    |lhs, rhs| {
                        if rhs == 0.0 {
                            return Err(VmError::DivisionByZero);
                        }
                        Ok(lhs % rhs)
                    },
                )?;
            }
            x if x == OpCode::And as u8 => {
                let rhs = self.pop_bool()?;
                let lhs = self.pop_bool()?;
                self.stack.push(Value::Bool(lhs && rhs));
            }
            x if x == OpCode::Or as u8 => {
                let rhs = self.pop_bool()?;
                let lhs = self.pop_bool()?;
                self.stack.push(Value::Bool(lhs || rhs));
            }
            x if x == OpCode::Neg as u8 => {
                let value = self.pop_numeric()?;
                match value {
                    NumericValue::Int(value) => self.stack.push(Value::Int(value.wrapping_neg())),
                    NumericValue::Float(value) => self.stack.push(Value::Float(-value)),
                }
            }
            x if x == OpCode::Ceq as u8 => {
                let rhs = self.pop_value()?;
                let lhs = self.pop_value()?;
                self.stack.push(Value::Bool(lhs == rhs));
            }
            x if x == OpCode::Clt as u8 => {
                self.compare_numeric_op(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?;
            }
            x if x == OpCode::Cgt as u8 => {
                self.compare_numeric_op(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?;
            }
            x if x == OpCode::Br as u8 => {
                let target = self.read_u32()? as usize;
                self.jump_to(target)?;
            }
            x if x == OpCode::Brfalse as u8 => {
                let target = self.read_u32()? as usize;
                let condition = self.pop_bool()?;
                if !condition {
                    self.jump_to(target)?;
                }
            }
            x if x == OpCode::Pop as u8 => {
                self.pop_value()?;
            }
            x if x == OpCode::Dup as u8 => {
                let value = self.peek_value()?.clone();
                self.stack.push(value);
            }
            x if x == OpCode::Ldloc as u8 => {
                let index = self.read_u8()?;
                let value = self
                    .locals
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::InvalidLocal(index))?;
                self.stack.push(value);
            }
            x if x == OpCode::Stloc as u8 => {
                let index = self.read_u8()?;
                let value = self.pop_value()?;
                let slot = self
                    .locals
                    .get_mut(index as usize)
                    .ok_or(VmError::InvalidLocal(index))?;
                *slot = value;
            }
            x if x == OpCode::Call as u8 => {
                let call_ip = self.ip - 1;
                let index = self.read_u16()?;
                let argc_u8 = self.read_u8()?;
                match self.execute_host_call(index, argc_u8, call_ip)? {
                    HostCallExecOutcome::Returned => {}
                    HostCallExecOutcome::Yielded => return Ok(ExecOutcome::Yielded),
                    HostCallExecOutcome::Pending(op_id) => {
                        return Ok(ExecOutcome::Waiting(op_id));
                    }
                }
            }
            other => return Err(VmError::InvalidOpcode(other)),
        }
        Ok(ExecOutcome::Continue)
    }

    pub fn resume(&mut self) -> VmResult<VmStatus> {
        self.run()
    }

    pub fn stack(&self) -> &[Value] {
        &self.stack
    }

    pub fn locals(&self) -> &[Value] {
        &self.locals
    }

    pub fn program(&self) -> &Program {
        self.program.as_ref()
    }

    pub fn bound_function_count(&self) -> usize {
        self.host_functions.len()
    }

    pub fn has_bound_function(&self, name: &str) -> bool {
        self.host_function_symbols.contains_key(name)
    }

    pub fn ip(&self) -> usize {
        self.ip
    }

    pub fn debug_info(&self) -> Option<&crate::debug_info::DebugInfo> {
        self.program.debug.as_ref()
    }

    pub fn call_depth(&self) -> usize {
        self.call_depth
    }

    fn pending_fuel_debt(&self) -> u64 {
        if self.fuel_remaining.is_none() {
            return 0;
        }
        let executed_since_last_check = self
            .fuel_check_interval
            .saturating_sub(self.fuel_ops_until_check);
        u64::from(executed_since_last_check)
    }

    fn pop_value(&mut self) -> VmResult<Value> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }

    pub(in crate::vm) fn charge_fuel(&mut self, amount: u64) -> VmResult<()> {
        if amount == 0 {
            return Ok(());
        }

        let Some(remaining) = self.fuel_remaining else {
            return Ok(());
        };

        if remaining < amount {
            self.fuel_remaining = Some(remaining);
            return Err(VmError::OutOfFuel {
                needed: amount,
                remaining,
            });
        }
        self.fuel_remaining = Some(remaining - amount);
        Ok(())
    }

    pub(in crate::vm) fn charge_fuel_tick(&mut self) -> VmResult<()> {
        if self.fuel_remaining.is_none() {
            return Ok(());
        }
        if self.fuel_ops_until_check > 1 {
            self.fuel_ops_until_check -= 1;
            return Ok(());
        }

        let amount = u64::from(self.fuel_check_interval);
        self.charge_fuel(amount)?;
        self.fuel_ops_until_check = self.fuel_check_interval;
        Ok(())
    }

    fn peek_value(&self) -> VmResult<&Value> {
        self.stack.last().ok_or(VmError::StackUnderflow)
    }

    fn pop_int(&mut self) -> VmResult<i64> {
        self.pop_value()?.as_int()
    }

    fn pop_numeric(&mut self) -> VmResult<NumericValue> {
        self.pop_value()?.as_numeric()
    }

    fn pop_bool(&mut self) -> VmResult<bool> {
        self.pop_value()?.as_bool()
    }

    fn binary_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_value()?;
        let lhs = self.pop_value()?;
        match (lhs, rhs) {
            (Value::Int(lhs), Value::Int(rhs)) => {
                self.stack.push(Value::Int(lhs.wrapping_add(rhs)));
            }
            (Value::Int(lhs), Value::Float(rhs)) => {
                self.stack.push(Value::Float(lhs as f64 + rhs));
            }
            (Value::Float(lhs), Value::Int(rhs)) => {
                self.stack.push(Value::Float(lhs + rhs as f64));
            }
            (Value::Float(lhs), Value::Float(rhs)) => {
                self.stack.push(Value::Float(lhs + rhs));
            }
            (Value::String(mut lhs), Value::String(rhs)) => {
                lhs.push_str(&rhs);
                self.stack.push(Value::String(lhs));
            }
            (Value::Array(mut lhs), Value::Array(rhs)) => {
                lhs.extend(rhs);
                self.stack.push(Value::Array(lhs));
            }
            _ => {
                return Err(VmError::TypeMismatch("number/string or array/array"));
            }
        }
        Ok(())
    }

    fn binary_numeric_op(
        &mut self,
        int_op: impl FnOnce(i64, i64) -> VmResult<i64>,
        float_op: impl FnOnce(f64, f64) -> VmResult<f64>,
    ) -> VmResult<()> {
        let rhs = self.pop_numeric()?;
        let lhs = self.pop_numeric()?;
        match (lhs, rhs) {
            (NumericValue::Int(lhs), NumericValue::Int(rhs)) => {
                self.stack.push(Value::Int(int_op(lhs, rhs)?));
            }
            (lhs, rhs) => {
                let lhs = match lhs {
                    NumericValue::Int(v) => v as f64,
                    NumericValue::Float(v) => v,
                };
                let rhs = match rhs {
                    NumericValue::Int(v) => v as f64,
                    NumericValue::Float(v) => v,
                };
                self.stack.push(Value::Float(float_op(lhs, rhs)?));
            }
        }
        Ok(())
    }

    fn compare_numeric_op(
        &mut self,
        int_op: impl FnOnce(i64, i64) -> bool,
        float_op: impl FnOnce(f64, f64) -> bool,
    ) -> VmResult<()> {
        let rhs = self.pop_numeric()?;
        let lhs = self.pop_numeric()?;
        let result = match (lhs, rhs) {
            (NumericValue::Int(lhs), NumericValue::Int(rhs)) => int_op(lhs, rhs),
            (lhs, rhs) => {
                let lhs = match lhs {
                    NumericValue::Int(v) => v as f64,
                    NumericValue::Float(v) => v,
                };
                let rhs = match rhs {
                    NumericValue::Int(v) => v as f64,
                    NumericValue::Float(v) => v,
                };
                float_op(lhs, rhs)
            }
        };
        self.stack.push(Value::Bool(result));
        Ok(())
    }

    fn pop_shift_amount(&mut self) -> VmResult<u32> {
        let value = self.pop_int()?;
        if !(0..=63).contains(&value) {
            return Err(VmError::InvalidShift(value));
        }
        Ok(value as u32)
    }

    fn execute_host_call(
        &mut self,
        index: u16,
        argc_u8: u8,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let argc = argc_u8 as usize;
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(self.pop_value()?);
        }
        args.reverse();

        if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            if argc_u8 != builtin.arity() {
                return Err(VmError::InvalidCallArity {
                    import: builtin.name().to_string(),
                    expected: builtin.arity(),
                    got: argc_u8,
                });
            }
            if self.builtin_overrides.contains_key(&index) {
                return self.execute_builtin_override_call(index, args, call_ip);
            }
            match builtins_impl::execute_builtin_call(self, builtin, args)? {
                builtins_impl::BuiltinCallOutcome::Return(values) => {
                    for value in values {
                        self.stack.push(value);
                    }
                    return Ok(HostCallExecOutcome::Returned);
                }
                builtins_impl::BuiltinCallOutcome::Pending(op_id) => {
                    let resume_ip = self.call_resume_ip(call_ip)?;
                    self.set_waiting_host_op(op_id, WaitingHostOpSource::BuiltinIo)?;
                    self.ip = resume_ip;
                    return Ok(HostCallExecOutcome::Pending(op_id));
                }
            }
        }

        let resolved_index = self.resolve_call_target(index, argc_u8)?;
        self.execute_bound_host_function(resolved_index, args, call_ip)
    }

    fn execute_builtin_override_call(
        &mut self,
        builtin_call_index: u16,
        args: Vec<Value>,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let resolved_index = self
            .builtin_overrides
            .get(&builtin_call_index)
            .copied()
            .ok_or_else(|| {
                VmError::HostError(format!(
                    "missing builtin override slot for call index {builtin_call_index}"
                ))
            })?;
        self.execute_bound_host_function(resolved_index, args, call_ip)
    }

    fn execute_bound_host_function(
        &mut self,
        resolved_index: u16,
        args: Vec<Value>,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        self.call_depth += 1;
        let function_ptr =
            self.host_functions
                .get_mut(resolved_index as usize)
                .ok_or(VmError::InvalidCall(resolved_index))? as *mut VmHostFunction;
        let outcome = unsafe {
            match &mut *function_ptr {
                VmHostFunction::Dynamic(function) => function.call(self, &args),
                VmHostFunction::Static(function) => function(self, &args),
            }
        };
        self.call_depth = self.call_depth.saturating_sub(1);
        let outcome = outcome?;

        match outcome {
            CallOutcome::Return(values) => {
                for value in values {
                    self.stack.push(value);
                }
                Ok(HostCallExecOutcome::Returned)
            }
            CallOutcome::Yield => {
                for value in args {
                    self.stack.push(value);
                }
                self.ip = call_ip;
                Ok(HostCallExecOutcome::Yielded)
            }
            CallOutcome::Pending(op_id) => {
                let resume_ip = self.call_resume_ip(call_ip)?;
                self.set_waiting_host_op(op_id, WaitingHostOpSource::HostBridge)?;
                self.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
    }

    fn call_resume_ip(&self, call_ip: usize) -> VmResult<usize> {
        let resume_ip = call_ip.checked_add(4).ok_or(VmError::BytecodeBounds)?;
        if resume_ip > self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        Ok(resume_ip)
    }

    fn set_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        source: WaitingHostOpSource,
    ) -> VmResult<()> {
        if let Some(active) = self.waiting_host_op
            && active.op_id != op_id
        {
            return Err(VmError::HostError(format!(
                "vm already waiting on host op {}, cannot wait on {}",
                active.op_id, op_id
            )));
        }
        self.waiting_host_op = Some(WaitingHostOp { op_id, source });
        Ok(())
    }

    fn complete_waiting_host_op(&mut self, op_id: HostOpId, values: Vec<Value>) -> VmResult<()> {
        let waiting = self.waiting_host_op.ok_or_else(|| {
            VmError::HostError(format!(
                "host op {} completed but vm is not waiting on any op",
                op_id
            ))
        })?;
        if waiting.op_id != op_id {
            return Err(VmError::HostError(format!(
                "host op {} completed while vm waits on {}",
                op_id, waiting.op_id
            )));
        }
        self.waiting_host_op = None;
        for value in values {
            self.stack.push(value);
        }
        Ok(())
    }

    fn read_u8(&mut self) -> VmResult<u8> {
        if self.ip >= self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        let value = self.program.code[self.ip];
        self.ip += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> VmResult<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> VmResult<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_bytes(&mut self, count: usize) -> VmResult<[u8; 4]> {
        if self.ip + count > self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        let mut buf = [0u8; 4];
        buf[..count].copy_from_slice(&self.program.code[self.ip..self.ip + count]);
        self.ip += count;
        Ok(buf)
    }

    fn jump_to(&mut self, target: usize) -> VmResult<()> {
        if target >= self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        self.ip = target;
        Ok(())
    }

    fn install_resolved_calls(&mut self, resolved_calls: Vec<u16>) -> VmResult<()> {
        if self.program.imports.len() != resolved_calls.len() {
            return Err(VmError::HostError(format!(
                "resolved call cache size mismatch: expected {}, got {}",
                self.program.imports.len(),
                resolved_calls.len()
            )));
        }
        for &index in &resolved_calls {
            if index as usize >= self.host_functions.len() {
                return Err(VmError::InvalidCall(index));
            }
        }
        self.resolved_calls = resolved_calls;
        self.resolved_calls_dirty = false;
        Ok(())
    }

    fn ensure_call_bindings(&mut self) -> VmResult<()> {
        if self.program.imports.is_empty() || !self.resolved_calls_dirty {
            return Ok(());
        }

        let use_legacy_order = self.host_function_symbols.is_empty();
        let mut resolved = Vec::with_capacity(self.program.imports.len());
        for (index, import) in self.program.imports.iter().enumerate() {
            if use_legacy_order {
                if index >= self.host_functions.len() {
                    return Err(VmError::InvalidCall(index as u16));
                }
                resolved.push(index as u16);
                continue;
            }

            let bound = self
                .host_function_symbols
                .get(&import.name)
                .copied()
                .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?;
            resolved.push(bound);
        }

        self.resolved_calls = resolved;
        self.resolved_calls_dirty = false;
        Ok(())
    }

    fn resolve_call_target(&mut self, index: u16, argc: u8) -> VmResult<u16> {
        if self.program.imports.is_empty() {
            return Ok(index);
        }

        self.ensure_call_bindings()?;
        let import = self
            .program
            .imports
            .get(index as usize)
            .ok_or(VmError::InvalidCall(index))?;
        if import.arity != argc {
            return Err(VmError::InvalidCallArity {
                import: import.name.clone(),
                expected: import.arity,
                got: argc,
            });
        }

        self.resolved_calls
            .get(index as usize)
            .copied()
            .ok_or(VmError::InvalidCall(index))
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        builtins_impl::close_all_handles(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn native_cache_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn native_trace_cache_resets_when_program_changes() {
        let _guard = native_cache_test_lock()
            .lock()
            .expect("native cache test lock should succeed");
        jit::runtime::clear_native_trace_cache_for_tests();

        let source_one = r#"
            let i = 0;
            while i < 8 {
                i = i + 1;
            }
            let j = 0;
            while j < 8 {
                j = j + 1;
            }
            i + j;
        "#;
        let source_two = r#"
            let k = 0;
            while k < 8 {
                k = k + 1;
            }
            k;
        "#;

        let compiled_one = crate::compile_source(source_one).expect("source one should compile");
        let compiled_two = crate::compile_source(source_two).expect("source two should compile");

        let mut vm_one = Vm::new(compiled_one.program);
        vm_one.set_jit_config(jit::JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        });
        let status_one = vm_one.run().expect("first vm should run");
        assert_eq!(status_one, VmStatus::Halted);
        let vm_one_trace_count = vm_one.jit_native_trace_count();
        assert!(
            vm_one_trace_count > 0,
            "first vm should produce native traces"
        );

        let (cache_program_after_one, cache_entries_after_one) =
            jit::runtime::native_trace_cache_snapshot_for_tests();
        assert_eq!(
            cache_program_after_one,
            Some(vm_one.program_cache_key),
            "cache should be keyed to first program after first run"
        );
        assert_eq!(
            cache_entries_after_one, vm_one_trace_count,
            "cache entry count should match first program traces"
        );

        let mut vm_two = Vm::new(compiled_two.program);
        vm_two.set_jit_config(jit::JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        });
        assert_ne!(
            vm_one.program_cache_key, vm_two.program_cache_key,
            "test programs should have different cache keys"
        );
        let status_two = vm_two.run().expect("second vm should run");
        assert_eq!(status_two, VmStatus::Halted);
        let vm_two_trace_count = vm_two.jit_native_trace_count();
        assert!(
            vm_two_trace_count > 0,
            "second vm should produce native traces"
        );

        let (cache_program_after_two, cache_entries_after_two) =
            jit::runtime::native_trace_cache_snapshot_for_tests();
        assert_eq!(
            cache_program_after_two,
            Some(vm_two.program_cache_key),
            "cache should switch to second program key"
        );
        assert_eq!(
            cache_entries_after_two, vm_two_trace_count,
            "cache should only contain traces from the active program"
        );
    }

    #[test]
    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn native_trace_cache_reuses_entries_for_same_program() {
        let _guard = native_cache_test_lock()
            .lock()
            .expect("native cache test lock should succeed");
        jit::runtime::clear_native_trace_cache_for_tests();

        let source = r#"
            let i = 0;
            let sum = 0;
            while i < 10 {
                sum = sum + i;
                i = i + 1;
            }
            sum;
        "#;
        let compiled = crate::compile_source(source).expect("source should compile");

        let mut vm_one = Vm::new(compiled.program.clone());
        vm_one.set_jit_config(jit::JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        });
        let status_one = vm_one.run().expect("first vm should run");
        assert_eq!(status_one, VmStatus::Halted);
        let vm_one_trace_count = vm_one.jit_native_trace_count();
        assert!(
            vm_one_trace_count > 0,
            "first vm should produce native traces"
        );

        let (cache_program_after_one, cache_entries_after_one) =
            jit::runtime::native_trace_cache_snapshot_for_tests();
        assert_eq!(
            cache_program_after_one,
            Some(vm_one.program_cache_key),
            "cache should be keyed to the first program"
        );
        assert_eq!(
            cache_entries_after_one, vm_one_trace_count,
            "cache entry count should match first vm traces"
        );

        let mut vm_two = Vm::new(compiled.program);
        vm_two.set_jit_config(jit::JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        });
        assert_eq!(
            vm_two.program_cache_key, vm_one.program_cache_key,
            "same program should use identical cache key"
        );

        let status_two = vm_two.run().expect("second vm should run");
        assert_eq!(status_two, VmStatus::Halted);
        let vm_two_trace_count = vm_two.jit_native_trace_count();
        assert_eq!(
            vm_two_trace_count, vm_one_trace_count,
            "same program should compile same native trace count"
        );

        let (cache_program_after_two, cache_entries_after_two) =
            jit::runtime::native_trace_cache_snapshot_for_tests();
        assert_eq!(
            cache_program_after_two,
            Some(vm_two.program_cache_key),
            "cache key should remain the same for identical program"
        );
        assert_eq!(
            cache_entries_after_two, cache_entries_after_one,
            "cache entries should be reused instead of duplicated"
        );
    }
}
