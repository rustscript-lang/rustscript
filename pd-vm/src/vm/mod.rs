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
    IntegerOverflow(&'static str),
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
            VmError::IntegerOverflow(operation) => {
                write!(f, "integer overflow in {operation}")
            }
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
    program_constants_ptr: usize,
    program_constants_len: usize,
    native_helper_fn: usize,
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
    jit_native_bridge_stats_enabled: bool,
    jit_native_bridge_counts: HashMap<&'static str, u64>,
    async_bridge: Option<Box<dyn HostAsyncBridge>>,
    waiting_host_op: Option<WaitingHostOp>,
    next_host_op_id: HostOpId,
    io_state: builtins_impl::IoState,
    fuel_enabled: bool,
    fuel_remaining: u64,
    fuel_check_interval: u32,
    fuel_ops_until_check: u32,
    native_only_aot: bool,
    native_aot_fuel_check_interval: Option<u32>,
    drop_contract_events: u64,
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

#[derive(Default)]
struct StableHasher(u64);

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;

        if self.0 == 0 {
            self.0 = OFFSET_BASIS;
        }
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }
}

fn logical_shr_i64(value: i64, amount: u32) -> i64 {
    ((value as u64) >> amount) as i64
}

pub(crate) fn checked_int_div(lhs: i64, rhs: i64) -> VmResult<i64> {
    if rhs == 0 {
        return Err(VmError::DivisionByZero);
    }
    if lhs == i64::MIN && rhs == -1 {
        return Err(VmError::IntegerOverflow("division"));
    }
    Ok(lhs / rhs)
}

pub(crate) fn checked_int_rem(lhs: i64, rhs: i64) -> VmResult<i64> {
    if rhs == 0 {
        return Err(VmError::DivisionByZero);
    }
    if lhs == i64::MIN && rhs == -1 {
        return Err(VmError::IntegerOverflow("remainder"));
    }
    Ok(lhs % rhs)
}

fn compute_program_cache_key(program: &Program) -> u64 {
    let mut hasher = StableHasher::default();
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
            let mut entry_hashes = entries
                .iter()
                .map(|(key, value)| {
                    let mut entry_hasher = StableHasher::default();
                    hash_value(key, &mut entry_hasher);
                    hash_value(value, &mut entry_hasher);
                    entry_hasher.finish()
                })
                .collect::<Vec<_>>();
            entry_hashes.sort_unstable();
            for entry_hash in entry_hashes {
                entry_hash.hash(state);
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
            program_constants_ptr: program_constants_ptr as usize,
            program_constants_len,
            native_helper_fn: jit::native::helper_entry_address(),
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
            jit_native_bridge_stats_enabled: false,
            jit_native_bridge_counts: HashMap::new(),
            async_bridge: None,
            waiting_host_op: None,
            next_host_op_id: 1,
            io_state: builtins_impl::IoState::default(),
            fuel_enabled: false,
            fuel_remaining: 0,
            fuel_check_interval: 1,
            fuel_ops_until_check: 1,
            native_only_aot: false,
            native_aot_fuel_check_interval: None,
            drop_contract_events: 0,
        }
    }

    fn ensure_program_cache_key(&mut self) -> u64 {
        if !self.program_cache_key_ready {
            self.program_cache_key = compute_program_cache_key(&self.program);
            self.program_cache_key_ready = true;
        }
        self.program_cache_key
    }

    fn validate_native_aot_fuel_interval(&self, interval: u32) -> VmResult<()> {
        if let Some(expected) = self.native_aot_fuel_check_interval {
            if expected == 0 {
                return Err(VmError::JitNative(
                    "native-only AOT bundle was emitted without fuel checks".to_string(),
                ));
            }
            if interval != expected {
                return Err(VmError::JitNative(format!(
                    "native-only AOT bundles require fuel_check_interval={expected}, got {interval}",
                )));
            }
        }
        Ok(())
    }

    fn validate_native_aot_fuel_runtime(&self) -> VmResult<()> {
        if self.native_only_aot
            && self.fuel_enabled
            && self.native_aot_fuel_check_interval == Some(0)
        {
            return Err(VmError::JitNative(
                "native-only AOT bundle was emitted without fuel checks and cannot run with fuel enabled"
                    .to_string(),
            ));
        }
        Ok(())
    }

    pub fn set_jit_native_bridge_stats_enabled(&mut self, enabled: bool) {
        self.jit_native_bridge_stats_enabled = enabled;
        if !enabled {
            self.jit_native_bridge_counts.clear();
        }
    }

    pub fn jit_native_bridge_stats_enabled(&self) -> bool {
        self.jit_native_bridge_stats_enabled
    }

    pub fn clear_jit_native_bridge_stats(&mut self) {
        self.jit_native_bridge_counts.clear();
    }

    pub fn jit_native_bridge_stats_snapshot(&self) -> Vec<(&'static str, u64)> {
        let mut entries: Vec<(&'static str, u64)> = self
            .jit_native_bridge_counts
            .iter()
            .map(|(name, count)| (*name, *count))
            .collect();
        entries.sort_unstable_by_key(|(name, _)| *name);
        entries
    }

    pub(in crate::vm) fn record_jit_native_bridge_hit(&mut self, bridge_name: &'static str) {
        if !self.jit_native_bridge_stats_enabled {
            return;
        }
        let entry = self
            .jit_native_bridge_counts
            .entry(bridge_name)
            .or_insert(0);
        *entry = entry.saturating_add(1);
    }

    /// Reset VM execution state to allow rerunning the same program instance while
    /// preserving JIT artifacts and registered host bindings.
    ///
    /// Locals are reset to `Null`, stack is cleared, and instruction pointer is
    /// rewound to the program entry.
    pub fn reset_for_reuse(&mut self) {
        self.ip = 0;
        self.drop_contract_events = 0;
        self.clear_stack_with_drop_contract();
        self.clear_locals_with_drop_contract();
        self.call_depth = 0;
        self.waiting_host_op = None;
        self.next_host_op_id = 1;
        self.io_state = builtins_impl::IoState::default();
    }

    pub fn drop_contract_event_count(&self) -> u64 {
        self.drop_contract_events
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
        self.fuel_enabled = true;
        self.fuel_remaining = fuel;
        self.fuel_ops_until_check = self.fuel_check_interval;
    }

    pub fn clear_fuel(&mut self) {
        self.fuel_enabled = false;
        self.fuel_remaining = 0;
        self.fuel_ops_until_check = self.fuel_check_interval;
    }

    pub fn set_fuel_check_interval(&mut self, interval: u32) -> VmResult<()> {
        if interval == 0 {
            return Err(VmError::InvalidFuelCheckInterval(interval));
        }
        self.validate_native_aot_fuel_interval(interval)?;
        self.fuel_check_interval = interval;
        self.fuel_ops_until_check = interval;
        Ok(())
    }

    pub fn fuel_check_interval(&self) -> u32 {
        if self.native_aot_fuel_check_interval == Some(0) {
            0
        } else {
            self.fuel_check_interval
        }
    }

    pub fn aot_fuel_check_interval(&self) -> Option<u32> {
        self.native_aot_fuel_check_interval
    }

    pub fn get_fuel(&self) -> Option<u64> {
        self.fuel_enabled
            .then_some(self.fuel_remaining.saturating_sub(self.pending_fuel_debt()))
    }

    pub fn add_fuel(&mut self, fuel: u64) -> VmResult<()> {
        if fuel == 0 {
            return Ok(());
        }
        self.fuel_remaining = if self.fuel_enabled {
            self.fuel_remaining
                .checked_add(fuel)
                .ok_or(VmError::FuelOverflow)?
        } else {
            self.fuel_enabled = true;
            fuel
        };
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
            remaining: self.fuel_enabled.then_some(self.fuel_remaining),
            check_interval: self.fuel_check_interval(),
            ops_until_check: self.fuel_ops_until_check,
        }
    }

    pub fn checkpoint(&self) -> FuelCheckpoint {
        self.fuel_checkpoint()
    }

    pub fn restore_fuel(&mut self, checkpoint: FuelCheckpoint) {
        self.fuel_enabled = checkpoint.remaining.is_some();
        self.fuel_remaining = checkpoint.remaining.unwrap_or(0);
        if self.native_aot_fuel_check_interval == Some(0) {
            self.fuel_check_interval = 1;
            self.fuel_ops_until_check = 1;
            return;
        }
        self.fuel_check_interval = self
            .native_aot_fuel_check_interval
            .unwrap_or(checkpoint.check_interval.max(1));
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
                Poll::Pending => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        return Err(VmError::HostError(
                            "blocking host-op wait is unsupported on wasm32 runtime".to_string(),
                        ));
                    }
                }
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
        self.validate_native_aot_fuel_runtime()?;
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
                            if matches!(err, VmError::OutOfFuel { .. }) {
                                // Fuel exhaustion is cooperative: surface as a yield so callers
                                // can top up budget and resume without treating it as a hard fault.
                                if self.handle_debugger_error(&mut debugger, &err) {
                                    continue;
                                }
                                let status = VmStatus::Yielded;
                                self.notify_debugger_status(&mut debugger, status);
                                return Ok(status);
                            }
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

            if self.native_only_aot {
                return Err(VmError::JitNative(format!(
                    "native-only AOT bundle has no compiled trace for ip {}",
                    self.ip
                )));
            }

            if self.ip >= self.program.code.len() {
                return Err(VmError::BytecodeBounds);
            }

            if self.fuel_enabled
                && let Err(err) = self.charge_fuel_tick()
            {
                if matches!(err, VmError::OutOfFuel { .. }) {
                    // Fuel exhaustion is cooperative: surface as a yield so callers
                    // can top up budget and resume without treating it as a hard fault.
                    if self.handle_debugger_error(&mut debugger, &err) {
                        continue;
                    }
                    let status = VmStatus::Yielded;
                    self.notify_debugger_status(&mut debugger, status);
                    return Ok(status);
                }
                if self.handle_debugger_error(&mut debugger, &err) {
                    continue;
                }
                return Err(err);
            }
            let opcode = self.read_u8()?;
            let outcome = match self.execute_interpreter_instruction(opcode) {
                Ok(outcome) => outcome,
                Err(err) => {
                    if matches!(err, VmError::OutOfFuel { .. }) {
                        // Fuel exhaustion stays cooperative even when raised during
                        // instruction execution (for example, fused call+ret tail tick).
                        if self.handle_debugger_error(&mut debugger, &err) {
                            continue;
                        }
                        let status = VmStatus::Yielded;
                        self.notify_debugger_status(&mut debugger, status);
                        return Ok(status);
                    }
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
                    checked_int_div,
                    |lhs, rhs| Ok(lhs / rhs),
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
            x if x == OpCode::Lshr as u8 => {
                let rhs = self.pop_shift_amount()?;
                let lhs = self.pop_int()?;
                self.stack.push(Value::Int(logical_shr_i64(lhs, rhs)));
            }
            x if x == OpCode::Mod as u8 => {
                self.binary_numeric_op(
                    checked_int_rem,
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
            x if x == OpCode::Not as u8 => {
                self.unary_not_op()?;
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
                if !self.fuel_enabled && self.can_fuse_ldloc_copy_pattern(index) {
                    let value = self
                        .locals
                        .get(index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidLocal(index))?;
                    self.stack.push(value);
                    self.ip = self.ip.saturating_add(3);
                } else {
                    let slot = self
                        .locals
                        .get_mut(index as usize)
                        .ok_or(VmError::InvalidLocal(index))?;
                    let value = std::mem::replace(slot, Value::Null);
                    self.stack.push(value);
                }
            }
            x if x == OpCode::Stloc as u8 => {
                let index = self.read_u8()?;
                let value = self.pop_value()?;
                self.store_local_with_drop_contract(index, value)?;
            }
            x if x == OpCode::Call as u8 => {
                let call_ip = self.ip - 1;
                let index = self.read_u16()?;
                let argc_u8 = self.read_u8()?;
                let can_fuse_tail_halt = self.can_fuse_call_ret_pattern();
                match self.execute_host_call(index, argc_u8, call_ip)? {
                    HostCallExecOutcome::Returned => {
                        if can_fuse_tail_halt {
                            if self.fuel_enabled {
                                // Preserve per-instruction fuel semantics when folding `call; ret`.
                                self.charge_fuel_tick()?;
                            }
                            // Consume the trailing `ret` so a resumed run does not halt twice.
                            self.ip = self.ip.saturating_add(1);
                            return Ok(ExecOutcome::Halted);
                        }
                    }
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
        if !self.fuel_enabled {
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

    fn can_fuse_ldloc_copy_pattern(&self, index: u8) -> bool {
        let code = &self.program.code;
        if self.ip + 2 >= code.len() {
            return false;
        }
        code[self.ip] == OpCode::Dup as u8
            && code[self.ip + 1] == OpCode::Stloc as u8
            && code[self.ip + 2] == index
    }

    fn can_fuse_call_ret_pattern(&self) -> bool {
        let code = &self.program.code;
        self.ip < code.len() && code[self.ip] == OpCode::Ret as u8
    }

    fn clear_stack_with_drop_contract(&mut self) {
        let drained = self.stack.drain(..).collect::<Vec<_>>();
        for value in drained {
            self.drop_value_with_contract(value);
        }
    }

    fn clear_locals_with_drop_contract(&mut self) {
        for slot in 0..self.locals.len() {
            let previous = std::mem::replace(&mut self.locals[slot], Value::Null);
            self.drop_value_with_contract(previous);
        }
    }

    fn drop_value_with_contract(&mut self, value: Value) {
        match value {
            Value::Null => {}
            Value::Array(values) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
                for item in values {
                    self.drop_value_with_contract(item);
                }
            }
            Value::Map(entries) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
                for (key, value) in entries {
                    self.drop_value_with_contract(key);
                    self.drop_value_with_contract(value);
                }
            }
            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::String(_) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
            }
        }
    }

    pub(in crate::vm) fn charge_fuel(&mut self, amount: u64) -> VmResult<()> {
        if amount == 0 {
            return Ok(());
        }

        if !self.fuel_enabled {
            return Ok(());
        }
        let remaining = self.fuel_remaining;

        if remaining < amount {
            return Err(VmError::OutOfFuel {
                needed: amount,
                remaining,
            });
        }
        self.fuel_remaining = remaining - amount;
        Ok(())
    }

    pub(in crate::vm) fn charge_fuel_tick(&mut self) -> VmResult<()> {
        if !self.fuel_enabled {
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

    fn unary_not_op(&mut self) -> VmResult<()> {
        let value = self.pop_bool()?;
        self.stack.push(Value::Bool(!value));
        Ok(())
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

    fn store_local_with_drop_contract(&mut self, index: u8, value: Value) -> VmResult<()> {
        let slot = self
            .locals
            .get_mut(index as usize)
            .ok_or(VmError::InvalidLocal(index))?;
        let previous = std::mem::replace(slot, value);
        self.drop_value_with_contract(previous);
        Ok(())
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
                if self.native_only_aot {
                    return Err(VmError::JitNative(
                        "native-only AOT bundles do not support host CallOutcome::Yield"
                            .to_string(),
                    ));
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
        self.clear_stack_with_drop_contract();
        self.clear_locals_with_drop_contract();
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
            let mut i = 0;
            while i < 8 {
                i = i + 1;
            }
            let mut j = 0;
            while j < 8 {
                j = j + 1;
            }
            i + j;
        "#;
        let source_two = r#"
            let mut k = 0;
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
            let mut i = 0;
            let mut sum = 0;
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

    fn step_once(vm: &mut Vm) -> VmResult<ExecOutcome> {
        let opcode = vm.read_u8()?;
        vm.execute_interpreter_instruction(opcode)
    }

    #[test]
    fn interpreter_fuses_ldloc_dup_stloc_same_slot_without_fuel() {
        let program = Program::new(
            vec![],
            vec![
                OpCode::Ldloc as u8,
                0,
                OpCode::Dup as u8,
                OpCode::Stloc as u8,
                0,
                OpCode::Ret as u8,
            ],
        )
        .with_local_count(1);
        let mut vm = Vm::new(program);
        let map_value = Value::Map(vec![(Value::String("k".to_string()), Value::Int(9))]);
        vm.locals[0] = map_value.clone();

        let outcome = step_once(&mut vm).expect("ldloc should execute");
        assert!(matches!(outcome, ExecOutcome::Continue));
        assert_eq!(vm.ip, 5, "fusion should skip dup+stloc bytes");
        assert_eq!(vm.locals[0], map_value, "local value should remain in slot");
        assert_eq!(
            vm.stack(),
            &[map_value],
            "stack should receive copied value"
        );
        assert_eq!(
            vm.drop_contract_event_count(),
            0,
            "copy fusion should not synthesize drop events"
        );

        let halted = step_once(&mut vm).expect("ret should execute");
        assert!(matches!(halted, ExecOutcome::Halted));
    }

    #[test]
    fn interpreter_does_not_fuse_ldloc_dup_stloc_when_fuel_enabled() {
        let program = Program::new(
            vec![],
            vec![
                OpCode::Ldloc as u8,
                0,
                OpCode::Dup as u8,
                OpCode::Stloc as u8,
                0,
                OpCode::Ret as u8,
            ],
        )
        .with_local_count(1);
        let mut vm = Vm::new(program);
        vm.locals[0] = Value::Int(42);
        vm.set_fuel(32);

        let ldloc = step_once(&mut vm).expect("ldloc should execute");
        assert!(matches!(ldloc, ExecOutcome::Continue));
        assert_eq!(vm.ip, 2, "fuel metering path must not skip dup+stloc");
        assert_eq!(
            vm.locals[0],
            Value::Null,
            "ldloc move should clear local slot"
        );
        assert_eq!(vm.stack(), &[Value::Int(42)]);

        let dup = step_once(&mut vm).expect("dup should execute");
        assert!(matches!(dup, ExecOutcome::Continue));
        assert_eq!(vm.ip, 3);
        assert_eq!(vm.stack(), &[Value::Int(42), Value::Int(42)]);

        let stloc = step_once(&mut vm).expect("stloc should execute");
        assert!(matches!(stloc, ExecOutcome::Continue));
        assert_eq!(vm.ip, 5);
        assert_eq!(vm.locals[0], Value::Int(42));
        assert_eq!(vm.stack(), &[Value::Int(42)]);
    }

    #[test]
    fn interpreter_fuses_call_ret_without_fuel() {
        let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
        let program = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
        );
        let mut vm = Vm::new(program);
        vm.stack.push(Value::String("tail".to_string()));

        let outcome = step_once(&mut vm).expect("call should execute");
        assert!(matches!(outcome, ExecOutcome::Halted));
        assert_eq!(vm.ip, 5, "tail-call fusion should consume trailing ret");
        assert_eq!(vm.stack(), &[Value::Int(4)]);
    }

    #[test]
    fn interpreter_fuses_call_ret_when_fuel_enabled_if_tail_tick_available() {
        let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
        let program = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
        );
        let mut vm = Vm::new(program);
        vm.set_fuel(1);
        vm.stack.push(Value::String("tail".to_string()));

        // `step_once` bypasses the outer run-loop pre-tick, so this fuel only covers fused `ret`.
        let call = step_once(&mut vm).expect("call should execute");
        assert!(matches!(call, ExecOutcome::Halted));
        assert_eq!(vm.ip, 5, "tail-call fusion should consume trailing ret");
        assert_eq!(vm.stack(), &[Value::Int(4)]);
        assert_eq!(vm.get_fuel(), Some(0));
    }

    #[test]
    fn interpreter_call_ret_fusion_preserves_ip_when_tail_tick_exhausted() {
        let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
        let program = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
        );
        let mut vm = Vm::new(program);
        vm.set_fuel(0);
        vm.stack.push(Value::String("tail".to_string()));

        let err = match step_once(&mut vm) {
            Ok(_) => panic!("tail tick should fail with out-of-fuel"),
            Err(err) => err,
        };
        assert!(matches!(err, VmError::OutOfFuel { .. }));
        assert_eq!(
            vm.ip, 4,
            "ret must remain pending when tail tick cannot be charged"
        );
        assert_eq!(vm.stack(), &[Value::Int(4)]);
    }

    #[test]
    fn run_consumes_two_ticks_for_call_ret_when_fuel_enabled() {
        let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
        let program = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
        );
        let mut vm = Vm::new(program);
        vm.set_fuel(2);
        vm.stack.push(Value::String("tail".to_string()));

        let status = vm.run().expect("run should complete");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(vm.ip, 5);
        assert_eq!(vm.stack(), &[Value::Int(4)]);
        assert_eq!(
            vm.get_fuel(),
            Some(0),
            "call+ret should spend two ticks with fuel metering enabled"
        );
    }

    #[test]
    fn run_yields_before_ret_in_call_ret_sequence_when_out_of_fuel() {
        let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
        let program = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
        );
        let mut vm = Vm::new(program);
        vm.set_fuel(1);
        vm.stack.push(Value::String("tail".to_string()));

        let status = vm.run().expect("first run should yield");
        assert_eq!(status, VmStatus::Yielded);
        assert_eq!(
            vm.ip, 4,
            "fuel exhaustion should happen before trailing ret"
        );
        assert_eq!(vm.stack(), &[Value::Int(4)]);
        assert_eq!(vm.get_fuel(), Some(0));

        vm.add_fuel(1).expect("recharging fuel should succeed");
        let resumed = vm.resume().expect("resume should execute trailing ret");
        assert_eq!(resumed, VmStatus::Halted);
        assert_eq!(vm.ip, 5);
        assert_eq!(vm.stack(), &[Value::Int(4)]);
    }

    #[test]
    fn call_ret_fusion_pattern_requires_immediate_ret() {
        let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
        let with_ret = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
        );
        let mut vm_with_ret = Vm::new(with_ret);
        vm_with_ret.ip = 4;
        assert!(vm_with_ret.can_fuse_call_ret_pattern());

        let wrong_next = Program::new(
            vec![],
            vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Nop as u8],
        );
        let mut vm_wrong_next = Vm::new(wrong_next);
        vm_wrong_next.ip = 4;
        assert!(!vm_wrong_next.can_fuse_call_ret_pattern());

        let no_next = Program::new(vec![], vec![OpCode::Call as u8, call_lo, call_hi, 1]);
        let mut vm_no_next = Vm::new(no_next);
        vm_no_next.ip = 4;
        assert!(!vm_no_next.can_fuse_call_ret_pattern());
    }

    #[test]
    fn ldloc_copy_pattern_match_is_strict_to_dup_stloc_same_slot() {
        let base = Program::new(
            vec![],
            vec![
                OpCode::Ldloc as u8,
                0,
                OpCode::Dup as u8,
                OpCode::Stloc as u8,
                0,
                OpCode::Ret as u8,
            ],
        )
        .with_local_count(1);
        let mut vm = Vm::new(base);
        vm.ip = 2;
        assert!(vm.can_fuse_ldloc_copy_pattern(0));

        let mismatch_slot = Program::new(
            vec![],
            vec![
                OpCode::Ldloc as u8,
                0,
                OpCode::Dup as u8,
                OpCode::Stloc as u8,
                1,
                OpCode::Ret as u8,
            ],
        )
        .with_local_count(2);
        let mut vm_slot_mismatch = Vm::new(mismatch_slot);
        vm_slot_mismatch.ip = 2;
        assert!(!vm_slot_mismatch.can_fuse_ldloc_copy_pattern(0));

        let wrong_middle = Program::new(
            vec![],
            vec![
                OpCode::Ldloc as u8,
                0,
                OpCode::Pop as u8,
                OpCode::Stloc as u8,
                0,
                OpCode::Ret as u8,
            ],
        )
        .with_local_count(1);
        let mut vm_wrong_middle = Vm::new(wrong_middle);
        vm_wrong_middle.ip = 2;
        assert!(!vm_wrong_middle.can_fuse_ldloc_copy_pattern(0));
    }
}
