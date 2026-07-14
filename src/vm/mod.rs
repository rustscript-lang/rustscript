use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub(crate) mod aot;
pub mod diagnostics;
mod epoch;
mod fuel;
mod host;
pub(crate) mod jit;
pub(crate) mod native;
mod store;
mod superinstructions;
#[cfg(test)]
mod tests;
pub use self::aot::AotArtifactError;
pub use self::epoch::{EpochCheckpoint, EpochHandle};
pub use self::fuel::FuelCheckpoint;
pub use self::host::{
    CallOutcome, CallReturn, HostArgsFunction, HostAsyncBridge, HostBindingPlan, HostFunction,
    HostFunctionRegistry, HostOpId, HostStackFunction, StaticHostArgsFunction, StaticHostFunction,
    StaticHostStackFunction,
};
use self::host::{HostCallExecOutcome, VmHostFunction, WaitingHostOp};
pub use crate::bytecode::{HostImport, OpCode, Program, Value, ValueType};
use crate::bytecode::{StableHasher, hash_value};
pub use store::Store;

#[derive(Clone, Copy, Debug)]
pub(crate) enum NumericValue {
    Int(i64),
    Float(f64),
}

impl Value {
    pub(crate) fn as_int(&self) -> Result<i64, VmError> {
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
    InvalidEpochCheckInterval(u32),
    InterruptionModeConflict {
        active: &'static str,
        requested: &'static str,
    },
    FuelOverflow,
    OutOfFuel {
        needed: u64,
        remaining: u64,
    },
    EpochDeadlineReached {
        current: u64,
        deadline: u64,
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
            VmError::InvalidEpochCheckInterval(value) => {
                write!(f, "invalid epoch check interval {value}, expected >= 1")
            }
            VmError::InterruptionModeConflict { active, requested } => write!(
                f,
                "{requested} interruption cannot be enabled while {active} interruption is active"
            ),
            VmError::FuelOverflow => write!(f, "fuel arithmetic overflow"),
            VmError::OutOfFuel { needed, remaining } => write!(
                f,
                "out of fuel: needed {needed} units, remaining {remaining}"
            ),
            VmError::EpochDeadlineReached { current, deadline } => write!(
                f,
                "epoch deadline reached: current epoch {current}, deadline {deadline}"
            ),
        }
    }
}

impl std::error::Error for VmError {}

pub type VmResult<T> = Result<T, VmError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmStatus {
    Halted,
    Yielded,
    Waiting(HostOpId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmYieldReason {
    Fuel,
    Epoch,
    Host,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterpreterMetrics {
    pub operand_hint_hit_count: u64,
    pub operand_hint_miss_count: u64,
    pub typed_builtin_fast_path_count: u64,
    pub projection_fast_path_count: u64,
    pub generic_builtin_call_count: u64,
    pub scalar_superinstruction_count: u64,
    pub local_type_hint_hit_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum InterruptMode {
    None = 0,
    Fuel = 1,
    Epoch = 2,
}

impl InterruptMode {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fuel => "fuel",
            Self::Epoch => "epoch",
        }
    }
}
type RuntimePrintSink = dyn FnMut(String) + Send;

type PackedOperandTypes = u8;

const NO_OPERAND_TYPE_HINT: PackedOperandTypes = 0;
const INT_INT_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Int, ValueType::Int);
const FLOAT_FLOAT_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Float, ValueType::Float);
const BOOL_BOOL_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Bool, ValueType::Bool);
const STRING_STRING_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::String, ValueType::String);
const BYTES_BYTES_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Bytes, ValueType::Bytes);
const NULL_NULL_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Null, ValueType::Null);
const INT_UNARY_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Int, ValueType::Unknown);
const FLOAT_UNARY_OPERAND_TYPE_HINT: PackedOperandTypes =
    pack_operand_types(ValueType::Float, ValueType::Unknown);

pub struct Vm {
    program: Arc<Program>,
    #[allow(dead_code)]
    program_constants_ptr: usize,
    #[allow(dead_code)]
    program_constants_len: usize,
    #[allow(dead_code)]
    native_helper_fn: usize,
    #[allow(dead_code)]
    native_interrupt_helper_fn: usize,
    program_cache_key: u64,
    program_cache_key_ready: bool,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    operand_type_hints: Option<Arc<[PackedOperandTypes]>>,
    decoded_instruction_data: Arc<crate::bytecode::DecodedInstructionData>,
    host_functions: Vec<VmHostFunction>,
    host_function_symbols: HashMap<String, u16>,
    builtin_overrides: HashMap<u16, u16>,
    resolved_calls: Vec<u16>,
    resolved_calls_dirty: bool,
    call_depth: usize,
    aot_program: Option<aot::CompiledProgram>,
    aot_exec_count: u64,
    jit: jit::TraceJitEngine,
    native_traces: HashMap<usize, jit::NativeTrace>,
    native_trace_exec_count: u64,
    jit_trace_exit_count: u64,
    jit_native_loop_back_count: u64,
    jit_native_link_handoff_count: u64,
    jit_native_link_dispatch_depth: u32,
    jit_helper_fallback_count: u64,
    jit_native_bridge_stats_enabled: bool,
    jit_native_bridge_counts: HashMap<&'static str, u64>,
    async_bridge: Option<Box<dyn HostAsyncBridge>>,
    runtime_print_sink: Option<Box<RuntimePrintSink>>,
    waiting_host_op: Option<WaitingHostOp>,
    next_host_op_id: HostOpId,
    pub(crate) io_state: crate::builtins::runtime::IoState,
    regex_cache: crate::builtins::runtime::regex::RegexCache,
    epoch_handle: EpochHandle,
    #[allow(dead_code)]
    epoch_counter_ptr: usize,
    interrupt_mode: InterruptMode,
    fuel_remaining: u64,
    fuel_check_interval: u32,
    fuel_ops_until_check: u32,
    epoch_deadline: u64,
    epoch_deadline_delta: u64,
    epoch_rearm_pending: bool,
    last_yield_reason: Option<VmYieldReason>,
    drop_contract_events_enabled: bool,
    drop_contract_events: u64,
    operand_hint_hit_count: u64,
    operand_hint_miss_count: u64,
    typed_builtin_fast_path_count: u64,
    projection_fast_path_count: u64,
    generic_builtin_call_count: u64,
    scalar_superinstruction_count: u64,
    local_type_hint_hit_count: u64,
}

pub(crate) enum ExecOutcome {
    Continue,
    Halted,
    Yielded,
    Waiting(HostOpId),
}

#[inline(always)]
fn logical_shr_i64(value: i64, amount: u32) -> i64 {
    ((value as u64) >> amount) as i64
}

#[inline(always)]
const fn pack_operand_types(lhs: ValueType, rhs: ValueType) -> PackedOperandTypes {
    lhs as u8 | ((rhs as u8) << 4)
}

#[inline(always)]
const fn unpack_operand_type(raw: u8) -> ValueType {
    match raw & 0x0F {
        1 => ValueType::Null,
        2 => ValueType::Int,
        3 => ValueType::Float,
        4 => ValueType::Bool,
        5 => ValueType::String,
        6 => ValueType::Bytes,
        7 => ValueType::Array,
        8 => ValueType::Map,
        _ => ValueType::Unknown,
    }
}

#[inline(always)]
const fn unpack_operand_types(hint: PackedOperandTypes) -> (ValueType, ValueType) {
    (unpack_operand_type(hint), unpack_operand_type(hint >> 4))
}

#[inline(always)]
pub(crate) fn checked_int_div(lhs: i64, rhs: i64) -> VmResult<i64> {
    if rhs == 0 {
        return Err(VmError::DivisionByZero);
    }
    if lhs == i64::MIN && rhs == -1 {
        return Err(VmError::IntegerOverflow("division"));
    }
    Ok(lhs / rhs)
}

#[inline(always)]
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
    hash_type_map(program.type_map.as_ref(), &mut hasher);
    hasher.finish()
}

fn hash_type_map(type_map: Option<&crate::bytecode::TypeMap>, state: &mut impl Hasher) {
    let Some(type_map) = type_map else {
        0u8.hash(state);
        return;
    };

    1u8.hash(state);
    type_map.strict_types.hash(state);
    type_map.local_types.hash(state);
    hash_local_schemas(&type_map.local_schemas, state);
    type_map.callable_slots.hash(state);
    type_map.optional_slots.hash(state);
    let mut operand_entries = type_map
        .operand_types
        .iter()
        .map(|(offset, pair)| (*offset, *pair))
        .collect::<Vec<_>>();
    operand_entries.sort_unstable_by_key(|(offset, _)| *offset);
    operand_entries.hash(state);
}

fn hash_local_schemas(schemas: &[Option<crate::compiler::TypeSchema>], state: &mut impl Hasher) {
    schemas.len().hash(state);
    for schema in schemas {
        match schema {
            Some(schema) => {
                1u8.hash(state);
                hash_type_schema(schema, state);
            }
            None => 0u8.hash(state),
        }
    }
}

fn hash_type_schema(schema: &crate::compiler::TypeSchema, state: &mut impl Hasher) {
    use crate::compiler::TypeSchema;

    match schema {
        TypeSchema::Unknown => 0u8.hash(state),
        TypeSchema::Null => 1u8.hash(state),
        TypeSchema::Int => 2u8.hash(state),
        TypeSchema::Float => 3u8.hash(state),
        TypeSchema::Number => 4u8.hash(state),
        TypeSchema::Bool => 5u8.hash(state),
        TypeSchema::String => 6u8.hash(state),
        TypeSchema::Bytes => 7u8.hash(state),
        TypeSchema::Optional(inner) => {
            16u8.hash(state);
            hash_type_schema(inner, state);
        }
        TypeSchema::GenericParam(name) => {
            8u8.hash(state);
            name.hash(state);
        }
        TypeSchema::Named(name, type_args) => {
            9u8.hash(state);
            name.hash(state);
            type_args.len().hash(state);
            for arg in type_args {
                hash_type_schema(arg, state);
            }
        }
        TypeSchema::Array(item) => {
            10u8.hash(state);
            hash_type_schema(item, state);
        }
        TypeSchema::ArrayTuple(items) => {
            11u8.hash(state);
            items.len().hash(state);
            for item in items {
                hash_type_schema(item, state);
            }
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            12u8.hash(state);
            prefix.len().hash(state);
            for item in prefix {
                hash_type_schema(item, state);
            }
            hash_type_schema(rest, state);
        }
        TypeSchema::Map(item) => {
            13u8.hash(state);
            hash_type_schema(item, state);
        }
        TypeSchema::Object(fields) => {
            14u8.hash(state);
            let mut entries = fields.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|lhs, rhs| lhs.0.cmp(rhs.0));
            for (key, value) in entries {
                key.hash(state);
                hash_type_schema(value, state);
            }
        }
        TypeSchema::Callable { params, result } => {
            15u8.hash(state);
            params.len().hash(state);
            for param in params {
                hash_type_schema(param, state);
            }
            hash_type_schema(result, state);
        }
    }
}

impl Vm {
    pub fn new(program: Program) -> Self {
        Self::new_shared_with_jit_config(Arc::new(program), jit::JitConfig::default())
    }

    pub fn new_with_jit_config(program: Program, jit_config: jit::JitConfig) -> Self {
        Self::new_shared_with_jit_config(Arc::new(program), jit_config)
    }

    pub fn new_shared(program: Arc<Program>) -> Self {
        Self::new_shared_with_jit_config(program, jit::JitConfig::default())
    }

    pub fn new_shared_with_jit_config(program: Arc<Program>, jit_config: jit::JitConfig) -> Self {
        let program_constants_ptr = program.constants.as_ptr();
        let program_constants_len = program.constants.len();
        let local_count = program.local_count;
        let operand_type_hints = program.shared_operand_type_hints();
        let decoded_instruction_data = program.shared_decoded_instruction_data();
        let epoch_handle = EpochHandle::default();
        let epoch_counter_ptr = epoch_handle.as_ptr() as usize;
        Self {
            program,
            program_constants_ptr: program_constants_ptr as usize,
            program_constants_len,
            native_helper_fn: native::helper_entry_address(),
            native_interrupt_helper_fn: native::interrupt_helper_entry_address(),
            program_cache_key: 0,
            program_cache_key_ready: false,
            ip: 0,
            stack: Vec::new(),
            locals: vec![Value::Null; local_count],
            operand_type_hints,
            decoded_instruction_data,
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            call_depth: 0,
            aot_program: None,
            aot_exec_count: 0,
            jit: jit::TraceJitEngine::new(jit_config),
            native_traces: HashMap::new(),
            native_trace_exec_count: 0,
            jit_trace_exit_count: 0,
            jit_native_loop_back_count: 0,
            jit_native_link_handoff_count: 0,
            jit_native_link_dispatch_depth: 0,
            jit_helper_fallback_count: 0,
            jit_native_bridge_stats_enabled: false,
            jit_native_bridge_counts: HashMap::new(),
            async_bridge: None,
            runtime_print_sink: None,
            waiting_host_op: None,
            next_host_op_id: 1,
            io_state: crate::builtins::runtime::IoState::default(),
            regex_cache: crate::builtins::runtime::regex::RegexCache::default(),
            epoch_handle,
            epoch_counter_ptr,
            interrupt_mode: InterruptMode::None,
            fuel_remaining: 0,
            fuel_check_interval: 1,
            fuel_ops_until_check: 1,
            epoch_deadline: 0,
            epoch_deadline_delta: 0,
            epoch_rearm_pending: false,
            last_yield_reason: None,
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

    fn ensure_program_cache_key(&mut self) -> u64 {
        if !self.program_cache_key_ready {
            self.program_cache_key = compute_program_cache_key(&self.program);
            self.program_cache_key_ready = true;
        }
        self.program_cache_key
    }

    #[inline(always)]
    fn fuel_metering_enabled(&self) -> bool {
        self.interrupt_mode == InterruptMode::Fuel
    }

    #[inline(always)]
    fn epoch_interruption_enabled(&self) -> bool {
        self.interrupt_mode == InterruptMode::Epoch
    }

    #[inline(always)]
    fn interruption_enabled(&self) -> bool {
        self.interrupt_mode != InterruptMode::None
    }

    /// Returns the maximum number of compiled regular expressions retained by this VM.
    ///
    /// New VMs default to 512 entries. A capacity of zero disables caching.
    pub fn regex_cache_capacity(&self) -> usize {
        self.regex_cache.capacity()
    }

    /// Changes this VM's compiled regular-expression cache capacity.
    ///
    /// Shrinking evicts least-recently-used entries immediately. Setting zero clears
    /// all entries and disables caching until a positive capacity is configured.
    pub fn set_regex_cache_capacity(&mut self, capacity: usize) {
        self.regex_cache.set_capacity(capacity);
    }

    pub fn regex_cache_entry_count(&self) -> usize {
        self.regex_cache.len()
    }

    pub fn regex_cache_compile_count(&self) -> u64 {
        self.regex_cache.compile_count()
    }

    pub fn regex_cache_hit_count(&self) -> u64 {
        self.regex_cache.hit_count()
    }

    pub(crate) fn cached_regex(
        &mut self,
        pattern: &str,
    ) -> Result<std::sync::Arc<regex::Regex>, regex::Error> {
        self.regex_cache.get_or_compile(pattern)
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

    pub fn interpreter_metrics_snapshot(&self) -> InterpreterMetrics {
        InterpreterMetrics {
            operand_hint_hit_count: self.operand_hint_hit_count,
            operand_hint_miss_count: self.operand_hint_miss_count,
            typed_builtin_fast_path_count: self.typed_builtin_fast_path_count,
            projection_fast_path_count: self.projection_fast_path_count,
            generic_builtin_call_count: self.generic_builtin_call_count,
            scalar_superinstruction_count: self.scalar_superinstruction_count,
            local_type_hint_hit_count: self.local_type_hint_hit_count,
        }
    }

    pub fn clear_interpreter_metrics(&mut self) {
        self.operand_hint_hit_count = 0;
        self.operand_hint_miss_count = 0;
        self.typed_builtin_fast_path_count = 0;
        self.projection_fast_path_count = 0;
        self.generic_builtin_call_count = 0;
        self.scalar_superinstruction_count = 0;
        self.local_type_hint_hit_count = 0;
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

    #[allow(dead_code)]
    pub(in crate::vm) fn record_native_bridge_hit(&mut self, bridge_name: &'static str) {
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
        self.last_yield_reason = None;
        self.epoch_rearm_pending = false;
        self.clear_fuel();
        self.clear_epoch_deadline();
        self.clear_stack_with_drop_contract();
        self.clear_locals_with_drop_contract();
        crate::builtins::runtime::close_all_handles(self);
        self.call_depth = 0;
        self.waiting_host_op = None;
        self.next_host_op_id = 1;
        self.io_state = crate::builtins::runtime::IoState::default();
        self.clear_interpreter_metrics();
    }

    #[inline(always)]
    pub(super) fn local_numeric_value(&self, index: u8) -> Option<NumericValue> {
        match self.locals.get(index as usize)? {
            Value::Int(value) => Some(NumericValue::Int(*value)),
            Value::Float(value) => Some(NumericValue::Float(*value)),
            _ => None,
        }
    }

    pub fn drop_contract_event_count(&self) -> u64 {
        self.drop_contract_events
    }

    pub fn set_drop_contract_events_enabled(&mut self, enabled: bool) {
        if self.drop_contract_events_enabled != enabled {
            self.native_traces.clear();
        }
        self.drop_contract_events_enabled = enabled;
        if !enabled {
            self.drop_contract_events = 0;
        }
    }

    pub fn drop_contract_events_enabled(&self) -> bool {
        self.drop_contract_events_enabled
    }

    fn interruption_mode_conflict(&self, requested: InterruptMode) -> VmError {
        VmError::InterruptionModeConflict {
            active: self.interrupt_mode.label(),
            requested: requested.label(),
        }
    }

    fn reset_interrupt_countdown(&mut self) {
        self.fuel_ops_until_check = self.fuel_check_interval.max(1);
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
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.clear_stack_with_drop_contract();
        self.clear_locals_with_drop_contract();
        crate::builtins::runtime::close_all_handles(self);
    }
}

impl Vm {
    pub(super) fn pop_value(&mut self) -> VmResult<Value> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }

    pub(super) fn can_fuse_call_ret_pattern(&self) -> bool {
        let code = &self.program.code;
        self.ip < code.len() && code[self.ip] == OpCode::Ret as u8
    }

    pub(super) fn clear_stack_with_drop_contract(&mut self) {
        let drained = self.stack.drain(..).collect::<Vec<_>>();
        for value in drained {
            self.drop_value_with_contract(value);
        }
    }

    pub(super) fn clear_locals_with_drop_contract(&mut self) {
        for slot in 0..self.locals.len() {
            let previous = std::mem::replace(&mut self.locals[slot], Value::Null);
            self.drop_value_with_contract(previous);
        }
    }

    pub(super) fn drop_value_with_contract(&mut self, value: Value) {
        if self.drop_contract_events_enabled {
            self.count_value_drop_contract(&value);
        }
    }

    pub(super) fn count_value_drop_contract(&mut self, value: &Value) {
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
            | Value::Bytes(_) => {
                self.drop_contract_events = self.drop_contract_events.saturating_add(1);
            }
        }
    }

    #[inline(always)]
    pub(in crate::vm) fn charge_interrupt_tick(&mut self) -> VmResult<()> {
        match self.interrupt_mode {
            InterruptMode::None => Ok(()),
            InterruptMode::Fuel => self.charge_fuel_tick(),
            InterruptMode::Epoch => self.charge_epoch_tick(),
        }
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub(in crate::vm) fn charge_aot_call_boundary_interrupt(&mut self) -> VmResult<()> {
        match self.interrupt_mode {
            InterruptMode::None => Ok(()),
            InterruptMode::Fuel => self.charge_fuel(1),
            InterruptMode::Epoch => {
                let current = self.current_epoch();
                if current >= self.epoch_deadline {
                    return Err(VmError::EpochDeadlineReached {
                        current,
                        deadline: self.epoch_deadline,
                    });
                }
                Ok(())
            }
        }
    }

    pub(super) fn peek_value(&self) -> VmResult<&Value> {
        self.stack.last().ok_or(VmError::StackUnderflow)
    }

    pub(super) fn pop_int(&mut self) -> VmResult<i64> {
        self.pop_value()?.as_int()
    }

    pub(super) fn pop_numeric(&mut self) -> VmResult<NumericValue> {
        self.pop_value()?.as_numeric()
    }

    pub(super) fn pop_bool(&mut self) -> VmResult<bool> {
        self.pop_value()?.as_bool()
    }

    pub(super) fn pop_float_exact(&mut self) -> VmResult<f64> {
        match self.pop_value()? {
            Value::Float(value) => Ok(value),
            _ => Err(VmError::TypeMismatch("float")),
        }
    }

    #[inline(always)]
    pub(super) fn operand_type_hint(&self, ip: usize) -> PackedOperandTypes {
        self.operand_type_hints
            .as_deref()
            .map_or(NO_OPERAND_TYPE_HINT, |hints| hints[ip])
    }

    #[inline(always)]
    pub(super) fn operand_value_types(&self, ip: usize) -> (ValueType, ValueType) {
        unpack_operand_types(self.operand_type_hint(ip))
    }

    #[inline(always)]
    pub(super) fn local_type_hint(&self, index: u8) -> ValueType {
        self.program
            .type_map
            .as_ref()
            .and_then(|type_map| type_map.local_types.get(index as usize))
            .copied()
            .unwrap_or(ValueType::Unknown)
    }

    #[inline(always)]
    pub(super) fn record_local_type_hint_hit(&mut self) {
        self.local_type_hint_hit_count = self.local_type_hint_hit_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_scalar_superinstruction(&mut self) {
        self.scalar_superinstruction_count = self.scalar_superinstruction_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_typed_builtin_fast_path(&mut self) {
        self.typed_builtin_fast_path_count = self.typed_builtin_fast_path_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_projection_fast_path(&mut self) {
        self.projection_fast_path_count = self.projection_fast_path_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_generic_builtin_call(&mut self) {
        self.generic_builtin_call_count = self.generic_builtin_call_count.saturating_add(1);
    }

    #[inline(always)]
    fn record_operand_hint_hit(&mut self) {
        self.operand_hint_hit_count = self.operand_hint_hit_count.saturating_add(1);
    }

    #[inline(always)]
    fn record_operand_hint_miss(&mut self) {
        self.operand_hint_miss_count = self.operand_hint_miss_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn unary_not_op(&mut self) -> VmResult<()> {
        let value = self.pop_bool()?;
        self.stack.push(Value::Bool(!value));
        Ok(())
    }

    pub(super) fn int_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.stack.push(Value::Int(lhs.wrapping_add(rhs)));
        Ok(())
    }

    pub(super) fn float_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.stack.push(Value::Float(lhs + rhs));
        Ok(())
    }

    pub(super) fn string_concat_op(&mut self) -> VmResult<()> {
        let rhs = match self.pop_value()? {
            Value::String(value) => value,
            _ => return Err(VmError::TypeMismatch("string")),
        };
        let lhs = match self.pop_value()? {
            Value::String(value) => value,
            _ => return Err(VmError::TypeMismatch("string")),
        };
        let mut out = String::with_capacity(lhs.len() + rhs.len());
        out.push_str(lhs.as_str());
        out.push_str(rhs.as_str());
        self.stack.push(Value::string(out));
        Ok(())
    }

    pub(super) fn bytes_concat_op(&mut self) -> VmResult<()> {
        let rhs = match self.pop_value()? {
            Value::Bytes(value) => value,
            _ => return Err(VmError::TypeMismatch("bytes")),
        };
        let lhs = match self.pop_value()? {
            Value::Bytes(value) => value,
            _ => return Err(VmError::TypeMismatch("bytes")),
        };
        let mut out = crate::bytecode::unwrap_or_clone_shared(lhs);
        out.extend(crate::bytecode::unwrap_or_clone_shared(rhs));
        self.stack.push(Value::bytes(out));
        Ok(())
    }

    pub(super) fn int_binary_numeric_op(
        &mut self,
        op: impl FnOnce(i64, i64) -> VmResult<i64>,
    ) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.stack.push(Value::Int(op(lhs, rhs)?));
        Ok(())
    }

    pub(super) fn float_binary_numeric_op(
        &mut self,
        op: impl FnOnce(f64, f64) -> VmResult<f64>,
    ) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.stack.push(Value::Float(op(lhs, rhs)?));
        Ok(())
    }

    pub(super) fn int_neg_op(&mut self) -> VmResult<()> {
        let value = self.pop_int()?;
        self.stack.push(Value::Int(value.wrapping_neg()));
        Ok(())
    }

    pub(super) fn float_neg_op(&mut self) -> VmResult<()> {
        let value = self.pop_float_exact()?;
        self.stack.push(Value::Float(-value));
        Ok(())
    }

    pub(super) fn int_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn float_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn bool_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_bool()?;
        let lhs = self.pop_bool()?;
        self.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn string_eq_op(&mut self) -> VmResult<()> {
        let rhs = match self.pop_value()? {
            Value::String(value) => value,
            _ => return Err(VmError::TypeMismatch("string")),
        };
        let lhs = match self.pop_value()? {
            Value::String(value) => value,
            _ => return Err(VmError::TypeMismatch("string")),
        };
        self.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn null_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_value()?;
        let lhs = self.pop_value()?;
        match (lhs, rhs) {
            (Value::Null, Value::Null) => {
                self.stack.push(Value::Bool(true));
                Ok(())
            }
            _ => Err(VmError::TypeMismatch("null")),
        }
    }

    pub(super) fn int_compare_op(&mut self, op: impl FnOnce(i64, i64) -> bool) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.stack.push(Value::Bool(op(lhs, rhs)));
        Ok(())
    }

    pub(super) fn float_compare_op(&mut self, op: impl FnOnce(f64, f64) -> bool) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.stack.push(Value::Bool(op(lhs, rhs)));
        Ok(())
    }

    pub(super) fn binary_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_value()?;
        let lhs = self.pop_value()?;
        match (lhs, rhs) {
            (Value::Int(lhs), Value::Int(rhs)) => {
                self.stack.push(Value::Int(lhs.wrapping_add(rhs)))
            }
            (Value::Int(lhs), Value::Float(rhs)) => self.stack.push(Value::Float(lhs as f64 + rhs)),
            (Value::Float(lhs), Value::Int(rhs)) => self.stack.push(Value::Float(lhs + rhs as f64)),
            (Value::Float(lhs), Value::Float(rhs)) => self.stack.push(Value::Float(lhs + rhs)),
            (Value::String(lhs), Value::String(rhs)) => {
                let mut out = String::with_capacity(lhs.len() + rhs.len());
                out.push_str(lhs.as_str());
                out.push_str(rhs.as_str());
                self.stack.push(Value::string(out));
            }
            (Value::Bytes(lhs), Value::Bytes(rhs)) => {
                let mut out = crate::bytecode::unwrap_or_clone_shared(lhs);
                out.extend(crate::bytecode::unwrap_or_clone_shared(rhs));
                self.stack.push(Value::bytes(out));
            }
            (Value::Array(lhs), Value::Array(rhs)) => {
                let mut out = crate::bytecode::unwrap_or_clone_shared(lhs);
                out.extend(crate::bytecode::unwrap_or_clone_shared(rhs));
                self.stack.push(Value::array(out));
            }
            _ => {
                return Err(VmError::TypeMismatch(
                    "number/string or bytes/bytes or array/array",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn binary_numeric_op(
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

    pub(super) fn compare_numeric_op(
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

    pub(super) fn pop_shift_amount(&mut self) -> VmResult<u32> {
        let value = self.pop_int()?;
        if !(0..=63).contains(&value) {
            return Err(VmError::InvalidShift(value));
        }
        Ok(value as u32)
    }

    #[inline(always)]
    pub(super) fn store_local_with_drop_contract(
        &mut self,
        index: u8,
        value: Value,
    ) -> VmResult<()> {
        let slot = self
            .locals
            .get_mut(index as usize)
            .ok_or(VmError::InvalidLocal(index))?;
        let previous = std::mem::replace(slot, value);
        self.drop_value_with_contract(previous);
        Ok(())
    }

    pub(super) fn read_u8(&mut self) -> VmResult<u8> {
        if self.ip >= self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        let value = self.program.code[self.ip];
        self.ip += 1;
        Ok(value)
    }

    pub(super) fn read_u16(&mut self) -> VmResult<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub(super) fn read_u32(&mut self) -> VmResult<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub(super) fn read_bytes(&mut self, count: usize) -> VmResult<[u8; 4]> {
        if self.ip + count > self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        let mut buf = [0u8; 4];
        buf[..count].copy_from_slice(&self.program.code[self.ip..self.ip + count]);
        self.ip += count;
        Ok(buf)
    }

    pub(super) fn jump_to(&mut self, target: usize) -> VmResult<()> {
        if target >= self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        self.ip = target;
        Ok(())
    }
}

impl Vm {
    pub(super) fn notify_debugger_status(
        &mut self,
        debugger: &mut Option<&mut crate::debugger::Debugger>,
        status: VmStatus,
    ) {
        if let Some(active_debugger) = debugger.as_deref_mut() {
            active_debugger.on_vm_status(self, status);
        }
    }

    pub(super) fn handle_debugger_error(
        &mut self,
        debugger: &mut Option<&mut crate::debugger::Debugger>,
        err: &VmError,
    ) -> bool {
        match err {
            VmError::OutOfFuel { .. } | VmError::EpochDeadlineReached { .. } => {
                if let Some(active_debugger) = debugger.as_deref_mut() {
                    return active_debugger.on_vm_error(self, err);
                }
                false
            }
            _ => false,
        }
    }

    #[inline(always)]
    pub(super) fn yielded_interrupt_reason(err: &VmError) -> Option<VmYieldReason> {
        match err {
            VmError::OutOfFuel { .. } => Some(VmYieldReason::Fuel),
            VmError::EpochDeadlineReached { .. } => Some(VmYieldReason::Epoch),
            _ => None,
        }
    }

    pub(super) fn outcome_to_status(outcome: ExecOutcome) -> Option<VmStatus> {
        match outcome {
            ExecOutcome::Continue => None,
            ExecOutcome::Halted => Some(VmStatus::Halted),
            ExecOutcome::Yielded => Some(VmStatus::Yielded),
            ExecOutcome::Waiting(op_id) => Some(VmStatus::Waiting(op_id)),
        }
    }

    pub(super) fn finish_outcome(
        &mut self,
        debugger: &mut Option<&mut crate::debugger::Debugger>,
        outcome: ExecOutcome,
    ) -> Option<VmStatus> {
        match outcome {
            ExecOutcome::Continue => {}
            ExecOutcome::Halted | ExecOutcome::Waiting(_) => self.last_yield_reason = None,
            ExecOutcome::Yielded => {
                if self.last_yield_reason.is_none() {
                    self.last_yield_reason = Some(VmYieldReason::Host);
                }
            }
        }
        let status = Self::outcome_to_status(outcome)?;
        self.notify_debugger_status(debugger, status);
        Some(status)
    }

    pub(super) fn run_internal(
        &mut self,
        mut debugger: Option<&mut crate::debugger::Debugger>,
        allow_jit: bool,
    ) -> VmResult<VmStatus> {
        self.ensure_call_bindings()?;
        if let Some(waiting) = self.waiting_host_op {
            self.last_yield_reason = None;
            let status = VmStatus::Waiting(waiting.op_id);
            self.notify_debugger_status(&mut debugger, status);
            return Ok(status);
        }
        self.last_yield_reason = None;

        loop {
            if self.epoch_rearm_pending {
                self.rearm_epoch_after_yield_if_needed();
            }
            if let Some(active_debugger) = debugger.as_deref_mut() {
                active_debugger.on_instruction(self);
            }

            if allow_jit && self.has_aot_program() && !self.drop_contract_events_enabled() {
                let outcome = match self.execute_aot_entry() {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        if let Some(reason) = Self::yielded_interrupt_reason(&err) {
                            self.mark_interrupt_yield(reason);
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

            if allow_jit
                && self.jit_config().enabled
                && self.builtin_overrides.is_empty()
                && !self.drop_contract_events_enabled()
            {
                let trace_id = {
                    let program = &self.program;
                    self.jit
                        .observe_hot_entry(self.ip, self.stack.len(), program)
                };
                if let Some(trace_id) = trace_id {
                    let outcome = match self.execute_jit_entry(trace_id) {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            if let Some(reason) = Self::yielded_interrupt_reason(&err) {
                                self.mark_interrupt_yield(reason);
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

            if self.ip >= self.program.code.len() {
                return Err(VmError::BytecodeBounds);
            }

            if self.interruption_enabled()
                && let Err(err) = self.charge_interrupt_tick()
            {
                if let Some(reason) = Self::yielded_interrupt_reason(&err) {
                    self.mark_interrupt_yield(reason);
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
            let allow_superinstructions = debugger.is_none() && !self.interruption_enabled();
            let outcome =
                match self.execute_interpreter_instruction(opcode, allow_superinstructions) {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        if let Some(reason) = Self::yielded_interrupt_reason(&err) {
                            self.mark_interrupt_yield(reason);
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

    pub(super) fn execute_interpreter_instruction(
        &mut self,
        opcode: u8,
        allow_superinstructions: bool,
    ) -> VmResult<ExecOutcome> {
        match opcode {
            x if x == OpCode::Nop as u8 => {}
            x if x == OpCode::Ret as u8 => return Ok(ExecOutcome::Halted),
            x if x == OpCode::Ldc as u8 => {
                let opcode_ip = self.ip - 1;
                let value = if let Some(value) = self.decoded_ldc_value_at(opcode_ip).cloned() {
                    self.ip += 4;
                    value
                } else {
                    let index = self.read_u32()?;
                    self.program
                        .constants
                        .get(index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidConstant(index))?
                };
                self.stack.push(value);
            }
            x if x == OpCode::Add as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_add_op()?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_add_op()?
                    }
                    STRING_STRING_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.string_concat_op()?
                    }
                    BYTES_BYTES_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.bytes_concat_op()?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.binary_add_op()?
                    }
                }
            }
            x if x == OpCode::Sub as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_binary_numeric_op(|lhs, rhs| Ok(lhs.wrapping_sub(rhs)))?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_binary_numeric_op(|lhs, rhs| Ok(lhs - rhs))?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.binary_numeric_op(
                            |lhs, rhs| Ok(lhs.wrapping_sub(rhs)),
                            |lhs, rhs| Ok(lhs - rhs),
                        )?
                    }
                }
            }
            x if x == OpCode::Mul as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_binary_numeric_op(|lhs, rhs| Ok(lhs.wrapping_mul(rhs)))?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_binary_numeric_op(|lhs, rhs| Ok(lhs * rhs))?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.binary_numeric_op(
                            |lhs, rhs| Ok(lhs.wrapping_mul(rhs)),
                            |lhs, rhs| Ok(lhs * rhs),
                        )?
                    }
                }
            }
            x if x == OpCode::Div as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_binary_numeric_op(checked_int_div)?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_binary_numeric_op(|lhs, rhs| Ok(lhs / rhs))?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.binary_numeric_op(checked_int_div, |lhs, rhs| Ok(lhs / rhs))?
                    }
                }
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
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_binary_numeric_op(checked_int_rem)?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_binary_numeric_op(|lhs, rhs| Ok(lhs % rhs))?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.binary_numeric_op(checked_int_rem, |lhs, rhs| Ok(lhs % rhs))?
                    }
                }
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
            x if x == OpCode::Not as u8 => self.unary_not_op()?,
            x if x == OpCode::Neg as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_UNARY_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_neg_op()?
                    }
                    FLOAT_UNARY_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_neg_op()?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        match self.pop_numeric()? {
                            NumericValue::Int(value) => {
                                self.stack.push(Value::Int(value.wrapping_neg()))
                            }
                            NumericValue::Float(value) => self.stack.push(Value::Float(-value)),
                        }
                    }
                }
            }
            x if x == OpCode::Ceq as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_eq_op()?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_eq_op()?
                    }
                    BOOL_BOOL_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.bool_eq_op()?
                    }
                    STRING_STRING_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.string_eq_op()?
                    }
                    NULL_NULL_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.null_eq_op()?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        let rhs = self.pop_value()?;
                        let lhs = self.pop_value()?;
                        self.stack.push(Value::Bool(lhs == rhs));
                    }
                }
            }
            x if x == OpCode::Clt as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_compare_op(|lhs, rhs| lhs < rhs)?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_compare_op(|lhs, rhs| lhs < rhs)?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.compare_numeric_op(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?
                    }
                }
            }
            x if x == OpCode::Cgt as u8 => {
                let ip = self.ip - 1;
                match self.operand_type_hint(ip) {
                    INT_INT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.int_compare_op(|lhs, rhs| lhs > rhs)?
                    }
                    FLOAT_FLOAT_OPERAND_TYPE_HINT => {
                        self.record_operand_hint_hit();
                        self.float_compare_op(|lhs, rhs| lhs > rhs)?
                    }
                    _ => {
                        self.record_operand_hint_miss();
                        self.compare_numeric_op(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?
                    }
                }
            }
            x if x == OpCode::Br as u8 => {
                let opcode_ip = self.ip - 1;
                let target = if let Some(target) = self.decoded_jump_target_at(opcode_ip) {
                    self.ip += 4;
                    target
                } else {
                    self.read_u32()? as usize
                };
                self.jump_to(target)?;
            }
            x if x == OpCode::Brfalse as u8 => {
                let opcode_ip = self.ip - 1;
                let target = if let Some(target) = self.decoded_jump_target_at(opcode_ip) {
                    self.ip += 4;
                    target
                } else {
                    self.read_u32()? as usize
                };
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
                let opcode_ip = self.ip - 1;
                let index = if let Some(index) = self.decoded_local_index_at(opcode_ip) {
                    self.ip += 1;
                    index
                } else {
                    self.read_u8()?
                };
                if self.try_fuse_scalar_sequence(index, allow_superinstructions)? {
                    return Ok(ExecOutcome::Continue);
                }
                let value = self
                    .locals
                    .get(index as usize)
                    .cloned()
                    .ok_or(VmError::InvalidLocal(index))?;
                self.stack.push(value);
            }
            x if x == OpCode::Stloc as u8 => {
                let opcode_ip = self.ip - 1;
                let index = if let Some(index) = self.decoded_local_index_at(opcode_ip) {
                    self.ip += 1;
                    index
                } else {
                    self.read_u8()?
                };
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
                            if self.interruption_enabled() {
                                self.charge_interrupt_tick()?;
                            }
                            self.ip = self.ip.saturating_add(1);
                            return Ok(ExecOutcome::Halted);
                        }
                    }
                    HostCallExecOutcome::Halted => return Ok(ExecOutcome::Halted),
                    HostCallExecOutcome::Yielded => {
                        self.last_yield_reason = Some(VmYieldReason::Host);
                        return Ok(ExecOutcome::Yielded);
                    }
                    HostCallExecOutcome::Pending(op_id) => return Ok(ExecOutcome::Waiting(op_id)),
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

    pub fn set_local(&mut self, index: u8, value: Value) -> VmResult<()> {
        self.store_local_with_drop_contract(index, value)
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
}
