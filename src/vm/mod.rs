use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub(crate) mod aot;
pub mod diagnostics;
mod epoch;
mod fuel;
mod host;
pub(crate) mod jit;
mod map_iter;
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
pub use crate::bytecode::{
    CallableTarget, CallableValue, HostImport, OpCode, Program, Value, ValueType,
};
use crate::bytecode::{StableHasher, hash_value};
pub use store::{
    IntoScriptValue, QueuedScriptInvocation, ScriptArgs, ScriptCallback, ScriptResult, Store,
};

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
    InvalidFrameState(&'static str),
    InvalidCallable,

    InvalidCallablePrototype(u32),
    InvalidBranchTarget {
        target: usize,
    },
    CallableArityMismatch {
        prototype_id: u32,
        expected: u8,
        got: u8,
    },
    CallStackOverflow {
        limit: usize,
    },
    InvalidCallStackLimit(usize),
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
            VmError::InvalidFrameState(message) => {
                write!(f, "invalid execution frame state: {message}")
            }
            VmError::InvalidCallable => write!(f, "callvalue operand is not callable"),

            VmError::InvalidCallablePrototype(id) => {
                write!(f, "invalid callable prototype {id}")
            }
            VmError::InvalidBranchTarget { target } => {
                write!(
                    f,
                    "branch target {target} leaves the active function region"
                )
            }
            VmError::CallableArityMismatch {
                prototype_id,
                expected,
                got,
            } => write!(
                f,
                "invalid call arity for callable {prototype_id}: expected {expected}, got {got}"
            ),
            VmError::CallStackOverflow { limit } => {
                write!(f, "script call stack limit {limit} exceeded")
            }
            VmError::InvalidCallStackLimit(limit) => {
                write!(
                    f,
                    "invalid script call stack limit {limit}: expected a positive value"
                )
            }
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

pub const DEFAULT_MAX_SCRIPT_CALL_DEPTH: usize = 1024;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmFrameContinuation {
    Halt,
    ResumeBytecode { return_ip: usize },
    ReturnToHost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmExecutionFrameSnapshot {
    pub continuation: VmFrameContinuation,
    pub operand_stack_base: usize,
    pub local_base: usize,
    pub local_count: usize,
    pub prototype_id: Option<u32>,
}

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
    fn root(local_count: usize) -> Self {
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
struct QueuedCallable {
    callable: Value,
    args: Vec<Value>,
    subscription: Option<Arc<AtomicBool>>,
}

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
    capture_cells: HashMap<usize, crate::bytecode::SharedCaptureCell>,
    shared_capture_slots: HashSet<usize>,
    operand_type_hints: Option<Arc<[PackedOperandTypes]>>,
    decoded_instruction_data: Arc<crate::bytecode::DecodedInstructionData>,
    host_functions: Vec<VmHostFunction>,
    host_function_symbols: HashMap<String, u16>,
    builtin_overrides: HashMap<u16, u16>,
    resolved_calls: Vec<u16>,
    resolved_calls_dirty: bool,
    call_depth: usize,
    max_script_call_depth: usize,
    execution_frames: Vec<ExecutionFrame>,
    active_local_base_cache: usize,
    active_operand_stack_base_cache: usize,
    host_return: Option<Value>,
    queued_callables: VecDeque<QueuedCallable>,
    completed_callable_results: VecDeque<Value>,
    owned_callables: Vec<Weak<CallableValue>>,
    callback_registry_flags: Vec<Weak<AtomicBool>>,
    draining_queued_callables: bool,
    shutdown: bool,
    aot_program: Option<aot::CompiledProgram>,
    aot_exec_count: u64,
    aot_interpreter_boundary_hit: bool,
    jit: jit::TraceJitEngine,
    native_traces: Vec<Option<jit::NativeTrace>>,
    native_trace_exec_count: u64,
    jit_native_region_entry_count: u64,
    jit_native_region_edge_count: u64,
    jit_native_direct_link_count: u64,
    jit_native_direct_links_enabled: bool,
    jit_native_direct_cross_frame_enabled: bool,
    jit_native_active_direct_trace_id: usize,
    jit_native_direct_escape_streak: u16,
    jit_native_direct_region_fallback: bool,
    jit_native_compile_time_ns: u64,
    jit_native_region_compile_time_ns: u64,
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
    map_iterators: Vec<Vec<Option<map_iter::MapIteratorState>>>,
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
        9 => ValueType::Callable,
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
    crate::bytecode::BYTECODE_ABI_VERSION.hash(&mut hasher);
    native::NATIVE_CALLABLE_ABI_VERSION.hash(&mut hasher);
    program.code.hash(&mut hasher);
    program.local_count.hash(&mut hasher);
    for constant in &program.constants {
        hash_value(constant, &mut hasher);
    }
    program.imports.hash(&mut hasher);
    program.script_functions.hash(&mut hasher);
    program.function_regions.hash(&mut hasher);
    program.root_callable_bindings.hash(&mut hasher);
    program.exported_callables.hash(&mut hasher);
    program.callable_prototypes.len().hash(&mut hasher);
    for prototype in &program.callable_prototypes {
        prototype.kind.hash(&mut hasher);
        prototype.target.hash(&mut hasher);
        prototype.arity.hash(&mut hasher);
        prototype.frame_local_count.hash(&mut hasher);
        prototype.parameter_slots.hash(&mut hasher);
        prototype.capture_source_slots.hash(&mut hasher);
        prototype.capture_slots.hash(&mut hasher);
        prototype.capture_modes.hash(&mut hasher);
        prototype.self_slot.hash(&mut hasher);
        match &prototype.schema {
            Some(schema) => {
                1u8.hash(&mut hasher);
                hash_type_schema(schema, &mut hasher);
            }
            None => 0u8.hash(&mut hasher),
        }
    }
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

fn value_matches_type_schema(value: &Value, schema: &crate::compiler::TypeSchema) -> bool {
    use crate::compiler::TypeSchema;

    match schema {
        TypeSchema::Unknown | TypeSchema::GenericParam(_) => true,
        TypeSchema::Null => matches!(value, Value::Null),
        TypeSchema::Int => matches!(value, Value::Int(_)),
        TypeSchema::Float => matches!(value, Value::Float(_)),
        TypeSchema::Number => matches!(value, Value::Int(_) | Value::Float(_)),
        TypeSchema::Bool => matches!(value, Value::Bool(_)),
        TypeSchema::String => matches!(value, Value::String(_)),
        TypeSchema::Bytes => matches!(value, Value::Bytes(_)),
        TypeSchema::Optional(inner) => {
            matches!(value, Value::Null) || value_matches_type_schema(value, inner)
        }
        TypeSchema::Named(_, _) | TypeSchema::Map(_) | TypeSchema::Object(_) => {
            matches!(value, Value::Map(_))
        }
        TypeSchema::Array(_) | TypeSchema::ArrayTuple(_) | TypeSchema::ArrayTupleRest { .. } => {
            matches!(value, Value::Array(_))
        }
        TypeSchema::Callable { .. } => matches!(value, Value::Callable(_)),
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

fn inline_compatible_callable_prototype(value: &Value) -> Option<u32> {
    match value {
        Value::Callable(callable)
            if callable.kind == crate::CallableKind::FunctionItem && callable.env.is_none() =>
        {
            Some(callable.prototype_id)
        }
        _ => None,
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
        let mut vm = Self {
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
            capture_cells: HashMap::new(),
            shared_capture_slots: HashSet::new(),
            operand_type_hints,
            decoded_instruction_data,
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            builtin_overrides: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            call_depth: 0,
            max_script_call_depth: DEFAULT_MAX_SCRIPT_CALL_DEPTH,
            execution_frames: vec![ExecutionFrame::root(local_count)],
            active_local_base_cache: 0,
            active_operand_stack_base_cache: 0,
            host_return: None,
            queued_callables: VecDeque::new(),
            completed_callable_results: VecDeque::new(),
            owned_callables: Vec::new(),
            callback_registry_flags: Vec::new(),
            draining_queued_callables: false,
            shutdown: false,
            aot_program: None,
            aot_exec_count: 0,
            aot_interpreter_boundary_hit: false,
            jit: jit::TraceJitEngine::new(jit_config),
            native_traces: Vec::new(),
            native_trace_exec_count: 0,
            jit_native_region_entry_count: 0,
            jit_native_region_edge_count: 0,
            jit_native_direct_link_count: 0,
            jit_native_direct_links_enabled: true,
            jit_native_direct_cross_frame_enabled: false,
            jit_native_active_direct_trace_id: usize::MAX,
            jit_native_direct_escape_streak: 0,
            jit_native_direct_region_fallback: false,
            jit_native_compile_time_ns: 0,
            jit_native_region_compile_time_ns: 0,
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
            map_iterators: Vec::new(),
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
        };
        vm.initialize_root_callable_bindings();
        vm
    }

    fn initialize_root_callable_bindings(&mut self) {
        let bindings = self.program.root_callable_bindings.clone();
        for binding in bindings {
            let Some(kind) = self
                .program
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

    /// Returns the maximum number of simultaneously active script call frames.
    pub fn max_script_call_depth(&self) -> usize {
        self.max_script_call_depth
    }

    /// Sets the maximum number of simultaneously active script call frames.
    ///
    /// The limit must be greater than zero. Existing active frames are not unwound;
    /// the new limit is checked before the next script frame is entered.
    pub fn set_max_script_call_depth(&mut self, limit: usize) -> VmResult<()> {
        if limit == 0 {
            return Err(VmError::InvalidCallStackLimit(limit));
        }
        self.max_script_call_depth = limit;
        Ok(())
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
        self.invalidate_callback_registries();
        self.cancel_waiting_host_op();
        self.ip = 0;
        self.drop_contract_events = 0;
        self.last_yield_reason = None;
        self.epoch_rearm_pending = false;
        self.clear_fuel();
        self.clear_epoch_deadline();
        self.clear_stack_with_drop_contract();
        self.capture_cells.clear();
        self.shared_capture_slots.clear();
        self.clear_locals_with_drop_contract();
        self.owned_callables.clear();
        self.locals.resize(self.program.local_count, Value::Null);
        self.initialize_root_callable_bindings();
        crate::builtins::runtime::close_all_handles(self);
        self.call_depth = 0;
        self.execution_frames.clear();
        self.execution_frames
            .push(ExecutionFrame::root(self.program.local_count));
        self.active_local_base_cache = 0;
        self.active_operand_stack_base_cache = 0;
        self.host_return = None;
        self.queued_callables.clear();
        self.completed_callable_results.clear();
        self.owned_callables.clear();
        self.draining_queued_callables = false;
        self.shutdown = false;
        self.aot_interpreter_boundary_hit = false;
        self.waiting_host_op = None;
        self.io_state = crate::builtins::runtime::IoState::default();
        self.map_iterators.clear();
        self.jit.reset_runtime_backoff();
        self.jit.clear_call_site_profiles();
        self.clear_interpreter_metrics();
    }

    fn validate_map_iterator_slot(&self, slot: usize) -> VmResult<()> {
        if u8::try_from(slot).is_err() {
            return Err(VmError::HostError(format!(
                "invalid map iterator id {slot}; maximum is {}",
                u8::MAX
            )));
        }
        Ok(())
    }

    pub(crate) fn init_map_iterator(
        &mut self,
        slot: usize,
        map: crate::bytecode::SharedMap,
    ) -> VmResult<()> {
        self.validate_map_iterator_slot(slot)?;
        let depth = self.call_depth;
        if self.map_iterators.len() <= depth {
            self.map_iterators.resize_with(depth + 1, Vec::new);
        }
        let frame = &mut self.map_iterators[depth];
        if frame.len() <= slot {
            frame.resize_with(slot + 1, || None);
        }
        frame[slot] = Some(map_iter::MapIteratorState::new(map));
        Ok(())
    }

    pub(crate) fn advance_map_iterator(&mut self, slot: usize) -> VmResult<bool> {
        self.validate_map_iterator_slot(slot)?;
        let frame = self.map_iterators.get_mut(self.call_depth).ok_or_else(|| {
            VmError::HostError("map iterator frame is not initialized".to_string())
        })?;
        let state = frame
            .get_mut(slot)
            .and_then(Option::as_mut)
            .ok_or_else(|| VmError::HostError("map iterator is not initialized".to_string()))?;
        let has_next = state.advance();
        if !has_next {
            frame[slot] = None;
        }
        Ok(has_next)
    }

    pub(crate) fn take_map_iterator_key(&mut self, slot: usize) -> VmResult<Value> {
        self.validate_map_iterator_slot(slot)?;
        self.map_iterators
            .get_mut(self.call_depth)
            .and_then(|frame| frame.get_mut(slot))
            .and_then(Option::as_mut)
            .and_then(map_iter::MapIteratorState::take_key)
            .ok_or_else(|| VmError::HostError("map iterator has no current key".to_string()))
    }

    pub(crate) fn take_map_iterator_value(&mut self, slot: usize) -> VmResult<Value> {
        self.validate_map_iterator_slot(slot)?;
        self.map_iterators
            .get_mut(self.call_depth)
            .and_then(|frame| frame.get_mut(slot))
            .and_then(Option::as_mut)
            .and_then(map_iter::MapIteratorState::take_value)
            .ok_or_else(|| VmError::HostError("map iterator has no current value".to_string()))
    }

    pub(crate) fn close_map_iterator(&mut self, slot: usize) -> VmResult<()> {
        self.validate_map_iterator_slot(slot)?;
        if let Some(state) = self
            .map_iterators
            .get_mut(self.call_depth)
            .and_then(|frame| frame.get_mut(slot))
        {
            *state = None;
        }
        Ok(())
    }

    fn close_all_map_iterators(&mut self) {
        for frame in &mut self.map_iterators {
            for state in frame {
                state.take();
            }
        }
    }

    #[inline(always)]
    pub(super) fn active_operand_stack_base(&self) -> usize {
        self.active_operand_stack_base_cache
    }

    #[inline(always)]
    pub(super) fn active_operand_stack_len(&self) -> usize {
        self.stack
            .len()
            .saturating_sub(self.active_operand_stack_base())
    }

    #[inline(always)]
    pub(super) fn active_frame_key(&self) -> u64 {
        self.execution_frames
            .last()
            .and_then(|frame| frame.prototype_id)
            .map(u64::from)
            .unwrap_or(crate::vm::native::ROOT_FRAME_KEY)
    }

    #[inline(always)]
    pub(super) fn active_local_base(&self) -> usize {
        self.active_local_base_cache
    }

    pub(super) fn active_local_types(&self) -> Vec<ValueType> {
        self.locals[self.active_local_base()..]
            .iter()
            .map(|value| match value {
                Value::Null => ValueType::Null,
                Value::Int(_) => ValueType::Int,
                Value::Float(_) => ValueType::Float,
                Value::Bool(_) => ValueType::Bool,
                Value::String(_) => ValueType::String,
                Value::Bytes(_) => ValueType::Bytes,
                Value::Array(_) => ValueType::Array,
                Value::Map(_) => ValueType::Map,
                Value::Callable(_) => ValueType::Callable,
            })
            .collect()
    }

    pub(super) fn active_local_callable_prototypes(&self) -> Option<Vec<Option<u32>>> {
        let base = self.active_local_base();
        let mut prototypes = Vec::with_capacity(self.locals.len().saturating_sub(base));
        for (offset, value) in self.locals[base..].iter().enumerate() {
            let prototype_id = if let Some(cell) = self.capture_cells.get(&(base + offset)) {
                let value = cell.lock().ok()?;
                inline_compatible_callable_prototype(&value)
            } else {
                inline_compatible_callable_prototype(value)
            };
            prototypes.push(prototype_id);
        }
        Some(prototypes)
    }

    pub(super) fn active_frame_has_shared_capture_cells(&self) -> bool {
        if self.shared_capture_slots.is_empty() {
            return false;
        }
        let Some(frame) = self.execution_frames.last() else {
            return false;
        };
        let base = frame.local_base;
        let end = base.saturating_add(frame.local_count);
        self.shared_capture_slots
            .iter()
            .any(|absolute| base <= *absolute && *absolute < end)
    }

    fn script_frame_depth(&self) -> usize {
        self.execution_frames
            .iter()
            .filter(|frame| frame.prototype_id.is_some())
            .count()
    }

    #[inline(always)]
    fn absolute_local_index(&self, index: u8) -> VmResult<usize> {
        let absolute = self
            .active_local_base()
            .checked_add(index as usize)
            .ok_or(VmError::InvalidLocal(index))?;
        self.locals
            .get(absolute)
            .map(|_| absolute)
            .ok_or(VmError::InvalidLocal(index))
    }

    #[inline(always)]
    fn load_local_value(&self, index: u8) -> VmResult<Value> {
        let absolute = self.absolute_local_index(index)?;
        if self.capture_cells.is_empty() {
            return Ok(self.locals[absolute].clone());
        }
        self.load_local_value_with_captures(absolute, index)
    }

    #[cold]
    #[inline(never)]
    fn load_local_value_with_captures(&self, absolute: usize, index: u8) -> VmResult<Value> {
        if let Some(cell) = self.capture_cells.get(&absolute) {
            return cell
                .lock()
                .map(|value| value.clone())
                .map_err(|_| VmError::InvalidFrameState("capture cell lock is poisoned"));
        }
        self.locals
            .get(absolute)
            .cloned()
            .ok_or(VmError::InvalidLocal(index))
    }

    #[inline(always)]
    pub(super) fn local_numeric_value(&self, index: u8) -> Option<NumericValue> {
        let absolute = self.absolute_local_index(index).ok()?;
        if self.capture_cells.is_empty() {
            return match self.locals.get(absolute)? {
                Value::Int(value) => Some(NumericValue::Int(*value)),
                Value::Float(value) => Some(NumericValue::Float(*value)),
                _ => None,
            };
        }
        self.local_numeric_value_with_captures(absolute)
    }

    #[cold]
    #[inline(never)]
    fn local_numeric_value_with_captures(&self, absolute: usize) -> Option<NumericValue> {
        let captured = self
            .capture_cells
            .get(&absolute)
            .and_then(|cell| cell.lock().ok().map(|value| value.clone()));
        match captured.as_ref().or_else(|| self.locals.get(absolute))? {
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
            self.disconnect_native_regions();
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
        self.cancel_waiting_host_op();
        self.clear_stack_with_drop_contract();
        self.capture_cells.clear();
        self.shared_capture_slots.clear();
        self.clear_locals_with_drop_contract();
        crate::builtins::runtime::close_all_handles(self);
    }
}

impl Vm {
    pub(super) fn pop_value(&mut self) -> VmResult<Value> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }

    pub(crate) fn bind_callable_value(
        &mut self,
        prototype_id: u32,
        captures: Vec<Value>,
    ) -> VmResult<Value> {
        let prototype = self
            .program
            .callable_prototypes
            .get(prototype_id as usize)
            .cloned()
            .ok_or(VmError::InvalidCallablePrototype(prototype_id))?;
        if captures.len() != prototype.capture_slots.len()
            || captures.len() != prototype.capture_modes.len()
            || captures.len() != prototype.capture_source_slots.len()
        {
            return Err(VmError::InvalidFrameState(
                "callable capture layout mismatch",
            ));
        }
        let active_base = self.active_local_base();
        let mut cells = Vec::with_capacity(captures.len());
        for (((value, source), target), mode) in captures
            .into_iter()
            .zip(&prototype.capture_source_slots)
            .zip(&prototype.capture_slots)
            .zip(&prototype.capture_modes)
        {
            let self_capture = prototype.self_slot == Some(*target);
            let cell = if !self_capture
                && matches!(
                    mode,
                    crate::CaptureBindingMode::Borrow | crate::CaptureBindingMode::BorrowMut
                ) {
                let absolute = active_base
                    .checked_add(usize::from(*source))
                    .ok_or(VmError::InvalidFrameState("capture source slot overflow"))?;
                if absolute >= self.locals.len() {
                    return Err(VmError::InvalidFrameState(
                        "capture source exceeds active frame locals",
                    ));
                }
                let cell = self
                    .capture_cells
                    .entry(absolute)
                    .or_insert_with(|| Arc::new(Mutex::new(value)))
                    .clone();
                self.shared_capture_slots.insert(absolute);
                self.locals[absolute] = cell
                    .lock()
                    .map_err(|_| VmError::InvalidFrameState("capture cell lock is poisoned"))?
                    .clone();
                cell
            } else {
                Arc::new(Mutex::new(value))
            };
            cells.push(cell);
        }
        let env = if prototype.kind == crate::CallableKind::Closure || !cells.is_empty() {
            Some(Arc::new(crate::CallableEnvironment {
                cells: Mutex::new(cells),
            }))
        } else {
            None
        };
        let callable = Arc::new(CallableValue {
            prototype_id,
            kind: prototype.kind,
            env,
        });
        self.owned_callables.push(Arc::downgrade(&callable));
        Ok(Value::Callable(callable))
    }

    fn execute_call_value(
        &mut self,
        argc: u8,
        call_site_ip: Option<usize>,
    ) -> VmResult<ExecOutcome> {
        let operand_count = argc as usize + 1;
        if self.stack.len() < operand_count {
            return Err(VmError::StackUnderflow);
        }
        let operand_stack_base = self.stack.len() - operand_count;
        let mut operands = self.stack.split_off(operand_stack_base);
        let callee = operands.remove(0);
        let Value::Callable(callable) = callee else {
            return Err(VmError::InvalidCallable);
        };
        let prototype = self
            .program
            .callable_prototypes
            .get(callable.prototype_id as usize)
            .cloned()
            .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
        if prototype.arity != argc {
            return Err(VmError::CallableArityMismatch {
                prototype_id: callable.prototype_id,
                expected: prototype.arity,
                got: argc,
            });
        }
        if let Some(crate::compiler::TypeSchema::Callable { params, .. }) = &prototype.schema
            && (params.len() != operands.len()
                || !params
                    .iter()
                    .zip(&operands)
                    .all(|(schema, value)| value_matches_type_schema(value, schema)))
        {
            return Err(VmError::TypeMismatch("callable argument schema"));
        }

        match prototype.target {
            CallableTarget::ScriptFunction(function_id) => {
                if let Some(call_ip) = call_site_ip {
                    self.jit.observe_script_call_target(
                        self.active_frame_key(),
                        call_ip,
                        callable.prototype_id,
                    );
                }
                if self.call_depth >= self.max_script_call_depth {
                    return Err(VmError::CallStackOverflow {
                        limit: self.max_script_call_depth,
                    });
                }
                let function = self
                    .program
                    .script_functions
                    .get(function_id as usize)
                    .cloned()
                    .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
                if prototype.parameter_slots.len() != operands.len() {
                    return Err(VmError::CallableArityMismatch {
                        prototype_id: callable.prototype_id,
                        expected: prototype.parameter_slots.len() as u8,
                        got: argc,
                    });
                }
                let inherited_callables = self
                    .execution_frames
                    .last()
                    .map(|frame| {
                        self.locals[frame.local_base..frame.local_base + frame.local_count]
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| matches!(value, Value::Callable(_)))
                            .map(|(slot, value)| (slot, value.clone()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let local_base = self.locals.len();
                let local_count = prototype.frame_local_count;
                self.locals
                    .resize(local_base.saturating_add(local_count), Value::Null);
                for binding in &self.program.root_callable_bindings {
                    let relative = binding.local_slot as usize;
                    if relative >= local_count {
                        return Err(VmError::InvalidFrameState(
                            "root callable binding is outside the script frame",
                        ));
                    }
                    let kind = self
                        .program
                        .callable_prototypes
                        .get(binding.prototype_id as usize)
                        .map(|prototype| prototype.kind)
                        .ok_or(VmError::InvalidCallablePrototype(binding.prototype_id))?;
                    let callable = Arc::new(CallableValue {
                        prototype_id: binding.prototype_id,
                        kind,
                        env: None,
                    });
                    self.owned_callables.push(Arc::downgrade(&callable));
                    self.locals[local_base + relative] = Value::Callable(callable);
                }
                for (slot, value) in inherited_callables {
                    if slot < local_count {
                        self.locals[local_base + slot] = value;
                    }
                }
                for (slot, argument) in prototype.parameter_slots.iter().zip(operands) {
                    let relative = *slot as usize;
                    if relative >= local_count {
                        return Err(VmError::InvalidFrameState(
                            "parameter slot is outside the script frame",
                        ));
                    }
                    self.locals[local_base + relative] = argument;
                }
                if let Some(environment) = &callable.env {
                    let cells = environment
                        .cells
                        .lock()
                        .map_err(|_| VmError::InvalidFrameState("poisoned callable environment"))?;
                    if cells.len() != prototype.capture_slots.len() {
                        return Err(VmError::InvalidFrameState(
                            "callable environment layout mismatch",
                        ));
                    }
                    for ((slot, mode), cell) in prototype
                        .capture_slots
                        .iter()
                        .zip(&prototype.capture_modes)
                        .zip(cells.iter())
                    {
                        let relative = *slot as usize;
                        if relative >= local_count {
                            return Err(VmError::InvalidFrameState(
                                "capture slot is outside the script frame",
                            ));
                        }
                        let absolute = local_base + relative;
                        self.locals[absolute] = cell
                            .lock()
                            .map_err(|_| {
                                VmError::InvalidFrameState("capture cell lock is poisoned")
                            })?
                            .clone();
                        if prototype.self_slot != Some(*slot) {
                            self.capture_cells.insert(absolute, cell.clone());
                            if matches!(
                                mode,
                                crate::CaptureBindingMode::Borrow
                                    | crate::CaptureBindingMode::BorrowMut
                            ) {
                                self.shared_capture_slots.insert(absolute);
                            }
                        }
                    }
                }
                if let Some(slot) = prototype.self_slot {
                    let relative = slot as usize;
                    if relative >= local_count {
                        return Err(VmError::InvalidFrameState(
                            "self slot is outside the script frame",
                        ));
                    }
                    self.locals[local_base + relative] = Value::Callable(callable.clone());
                }
                let return_ip = self.ip;
                self.execution_frames.push(ExecutionFrame {
                    continuation: FrameContinuation::ResumeBytecode { return_ip },
                    operand_stack_base,
                    local_base,
                    local_count,
                    prototype_id: Some(callable.prototype_id),
                });
                self.active_local_base_cache = local_base;
                self.active_operand_stack_base_cache = operand_stack_base;
                self.call_depth = self.script_frame_depth();
                self.ip = function.entry_ip as usize;
                self.charge_interrupt_tick()?;
                Ok(ExecOutcome::Continue)
            }
            CallableTarget::HostImport(import_index) => {
                self.stack.extend(operands);
                let call_ip = self.ip.saturating_sub(2);
                match self.execute_host_call(import_index, argc, call_ip)? {
                    HostCallExecOutcome::Returned => Ok(ExecOutcome::Continue),
                    HostCallExecOutcome::Halted => Ok(ExecOutcome::Halted),
                    HostCallExecOutcome::Yielded => {
                        self.stack
                            .insert(operand_stack_base, Value::Callable(callable));
                        Ok(ExecOutcome::Yielded)
                    }
                    HostCallExecOutcome::Pending(op_id) => Ok(ExecOutcome::Waiting(op_id)),
                }
            }
        }
    }

    fn complete_active_frame(&mut self) -> VmResult<ExecOutcome> {
        let frame = self
            .execution_frames
            .pop()
            .ok_or(VmError::InvalidFrameState("missing active frame"))?;
        self.active_local_base_cache = self
            .execution_frames
            .last()
            .map(|frame| frame.local_base)
            .unwrap_or(0);
        self.active_operand_stack_base_cache = self
            .execution_frames
            .last()
            .map(|frame| frame.operand_stack_base)
            .unwrap_or(0);
        if self.stack.len() < frame.operand_stack_base {
            return Err(VmError::InvalidFrameState(
                "operand stack is below the active frame base",
            ));
        }
        if matches!(frame.continuation, FrameContinuation::Halt) {
            self.call_depth = self.script_frame_depth();
            return Ok(ExecOutcome::Halted);
        }

        let result = if self.stack.len() > frame.operand_stack_base {
            self.stack.pop().expect("stack length checked above")
        } else {
            Value::Null
        };
        while self.stack.len() > frame.operand_stack_base {
            let value = self.stack.pop().expect("stack length checked above");
            self.drop_value_with_contract(value);
        }
        self.call_depth = self.script_frame_depth();

        if frame.prototype_id.is_some() {
            let frame_end = frame.local_base.saturating_add(frame.local_count);
            self.capture_cells
                .retain(|absolute, _| *absolute < frame.local_base || *absolute >= frame_end);
            self.shared_capture_slots
                .retain(|absolute| *absolute < frame.local_base || *absolute >= frame_end);
        }

        if !matches!(frame.continuation, FrameContinuation::Halt) {
            let frame_end = frame
                .local_base
                .checked_add(frame.local_count)
                .ok_or(VmError::InvalidFrameState("local frame range overflow"))?;
            if frame_end != self.locals.len() {
                return Err(VmError::InvalidFrameState(
                    "active local frame does not end at the local stack tail",
                ));
            }
            let drained = self.locals.drain(frame.local_base..).collect::<Vec<_>>();
            for value in drained {
                self.drop_value_with_contract(value);
            }
        }

        if let Some(prototype_id) = frame.prototype_id
            && let Some(crate::compiler::TypeSchema::Callable { result: schema, .. }) = self
                .program
                .callable_prototypes
                .get(prototype_id as usize)
                .and_then(|prototype| prototype.schema.as_ref())
            && !value_matches_type_schema(&result, schema)
        {
            self.drop_value_with_contract(result);
            return Err(VmError::TypeMismatch("callable return schema"));
        }

        match frame.continuation {
            FrameContinuation::Halt => {
                self.stack.push(result);
                Ok(ExecOutcome::Halted)
            }
            FrameContinuation::ResumeBytecode { return_ip } => {
                self.ip = return_ip;
                self.stack.push(result);
                Ok(ExecOutcome::Continue)
            }
            FrameContinuation::ReturnToHost => {
                self.host_return = Some(result);
                Ok(ExecOutcome::Halted)
            }
        }
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
            | Value::Bytes(_)
            | Value::Callable(_) => {
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
        let absolute = self.absolute_local_index(index)?;
        self.store_local_absolute_with_drop_contract(absolute, index, value)
    }

    #[inline(always)]
    pub(super) fn store_local_absolute_with_drop_contract(
        &mut self,
        absolute: usize,
        index: u8,
        value: Value,
    ) -> VmResult<()> {
        if self.capture_cells.is_empty() {
            let slot = self
                .locals
                .get_mut(absolute)
                .ok_or(VmError::InvalidLocal(index))?;
            let previous = std::mem::replace(slot, value);
            self.drop_value_with_contract(previous);
            return Ok(());
        }
        self.store_local_with_captures(absolute, index, value)
    }

    #[cold]
    #[inline(never)]
    fn store_local_with_captures(
        &mut self,
        absolute: usize,
        index: u8,
        value: Value,
    ) -> VmResult<()> {
        if let Some(cell) = self.capture_cells.get(&absolute).cloned() {
            if Self::value_references_capture_cell(&value, &cell, &mut HashSet::new())? {
                return Err(VmError::InvalidFrameState(
                    "callable capture ownership cycle is unsupported",
                ));
            }
            let previous = {
                let mut captured = cell
                    .lock()
                    .map_err(|_| VmError::InvalidFrameState("capture cell lock is poisoned"))?;
                std::mem::replace(&mut *captured, value.clone())
            };
            self.locals[absolute] = value;
            self.drop_value_with_contract(previous);
            return Ok(());
        }
        let slot = self
            .locals
            .get_mut(absolute)
            .ok_or(VmError::InvalidLocal(index))?;
        let previous = std::mem::replace(slot, value);
        self.drop_value_with_contract(previous);
        Ok(())
    }

    fn value_references_capture_cell(
        value: &Value,
        target: &crate::bytecode::SharedCaptureCell,
        visited_cells: &mut HashSet<usize>,
    ) -> VmResult<bool> {
        match value {
            Value::Callable(callable) => {
                let Some(environment) = callable.env.as_ref() else {
                    return Ok(false);
                };
                let cells = environment.cells.lock().map_err(|_| {
                    VmError::InvalidFrameState("capture environment lock is poisoned")
                })?;
                for cell in cells.iter() {
                    if Arc::ptr_eq(cell, target) {
                        return Ok(true);
                    }
                    let identity = Arc::as_ptr(cell) as usize;
                    if visited_cells.insert(identity) {
                        let captured = cell.lock().map_err(|_| {
                            VmError::InvalidFrameState("capture cell lock is poisoned")
                        })?;
                        if Self::value_references_capture_cell(&captured, target, visited_cells)? {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
            Value::Array(values) => {
                for value in values.iter() {
                    if Self::value_references_capture_cell(value, target, visited_cells)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Value::Map(values) => {
                for (key, value) in values.iter() {
                    if Self::value_references_capture_cell(key, target, visited_cells)?
                        || Self::value_references_capture_cell(value, target, visited_cells)?
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn detach_local_with_drop_contract(&mut self, index: u8) -> VmResult<()> {
        let absolute = self.absolute_local_index(index)?;
        self.capture_cells.remove(&absolute);
        let slot = self
            .locals
            .get_mut(absolute)
            .ok_or(VmError::InvalidLocal(index))?;
        let previous = std::mem::replace(slot, Value::Null);
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
        if !self.program.function_regions.is_empty() {
            let active_prototype = self
                .execution_frames
                .last()
                .and_then(|frame| frame.prototype_id);
            let target_is_valid = if let Some(prototype_id) = active_prototype
                && !self.has_aot_program()
            {
                self.program
                    .callable_prototypes
                    .get(prototype_id as usize)
                    .and_then(|prototype| match prototype.target {
                        CallableTarget::ScriptFunction(function_id) => {
                            self.program.script_functions.get(function_id as usize)
                        }
                        CallableTarget::HostImport(_) => None,
                    })
                    .is_some_and(|function| {
                        function.entry_ip as usize <= target && target < function.end_ip as usize
                    })
            } else {
                let target_prototype = self
                    .program
                    .function_regions
                    .iter()
                    .find(|region| {
                        region.start_ip as usize <= target && target < region.end_ip as usize
                    })
                    .and_then(|region| region.prototype_id);
                active_prototype == target_prototype
            };
            if !target_is_valid {
                return Err(VmError::InvalidBranchTarget { target });
            }
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
        debugger: Option<&mut crate::debugger::Debugger>,
        allow_jit: bool,
    ) -> VmResult<VmStatus> {
        let result = self.run_internal_impl(debugger, allow_jit);
        if result.is_err() {
            self.close_all_map_iterators();
        }
        result
    }

    fn run_fast_interpreter(&mut self, allow_jit: bool) -> VmResult<Option<VmStatus>> {
        loop {
            if self.ip >= self.program.code.len() {
                return Err(VmError::BytecodeBounds);
            }
            let opcode = self.read_u8()?;
            let outcome = self.execute_interpreter_instruction(opcode, true)?;
            match outcome {
                ExecOutcome::Continue => {}
                ExecOutcome::Halted => {
                    self.last_yield_reason = None;
                    return Ok(Some(VmStatus::Halted));
                }
                ExecOutcome::Yielded => {
                    if self.last_yield_reason.is_none() {
                        self.last_yield_reason = Some(VmYieldReason::Host);
                    }
                    return Ok(Some(VmStatus::Yielded));
                }
                ExecOutcome::Waiting(op_id) => {
                    self.last_yield_reason = None;
                    return Ok(Some(VmStatus::Waiting(op_id)));
                }
            }
            if (opcode == OpCode::Call as u8 || opcode == OpCode::CallValue as u8)
                && (self.interruption_enabled()
                    || (allow_jit && (self.jit_config().enabled || self.has_aot_program())))
            {
                return Ok(None);
            }
        }
    }

    fn run_internal_impl(
        &mut self,
        mut debugger: Option<&mut crate::debugger::Debugger>,
        allow_jit: bool,
    ) -> VmResult<VmStatus> {
        self.ensure_call_bindings()?;
        self.sync_jit_non_yielding_host_imports();
        if let Some(waiting) = self.waiting_host_op {
            self.last_yield_reason = None;
            let status = VmStatus::Waiting(waiting.op_id);
            self.notify_debugger_status(&mut debugger, status);
            return Ok(status);
        }
        self.last_yield_reason = None;
        if self.epoch_rearm_pending {
            self.rearm_epoch_after_yield_if_needed();
        }
        if debugger.is_none()
            && !self.interruption_enabled()
            && (!allow_jit || (!self.jit_config().enabled && !self.has_aot_program()))
            && let Some(status) = self.run_fast_interpreter(allow_jit)?
        {
            return Ok(status);
        }

        loop {
            if self.epoch_rearm_pending {
                self.rearm_epoch_after_yield_if_needed();
            }
            if let Some(active_debugger) = debugger.as_deref_mut() {
                active_debugger.on_instruction(self);
            }

            if allow_jit
                && self.has_aot_program()
                && !self.aot_interpreter_boundary_hit
                && !self.drop_contract_events_enabled()
            {
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
                && !self.active_frame_has_shared_capture_cells()
            {
                let frame_key = self.active_frame_key();
                let trace_id = if self.jit.callable_frame_is_blocked(frame_key) {
                    None
                } else {
                    let stack_depth = self.active_operand_stack_len();
                    let entry_local_types = (frame_key != crate::vm::native::ROOT_FRAME_KEY)
                        .then(|| self.active_local_types());
                    let entry_callable_prototypes = self.active_local_callable_prototypes();
                    let program = &self.program;
                    self.jit.observe_hot_entry_with_local_types(
                        frame_key,
                        self.ip,
                        stack_depth,
                        entry_local_types.as_deref(),
                        entry_callable_prototypes.as_deref(),
                        program,
                    )
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

    #[inline(always)]
    pub(super) fn execute_interpreter_instruction(
        &mut self,
        opcode: u8,
        allow_superinstructions: bool,
    ) -> VmResult<ExecOutcome> {
        match opcode {
            x if x == OpCode::Nop as u8 => {}
            x if x == OpCode::Ret as u8 => return self.complete_active_frame(),
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
                if self.decoded_jump_target_is_valid_at(opcode_ip) {
                    self.ip = target;
                } else {
                    self.jump_to(target)?;
                }
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
                    if self.decoded_jump_target_is_valid_at(opcode_ip) {
                        self.ip = target;
                    } else {
                        self.jump_to(target)?;
                    }
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
                let value = self.load_local_value(index)?;
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
                            return self.complete_active_frame();
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

            x if x == OpCode::CallValue as u8 => {
                let call_ip = self.ip.saturating_sub(1);
                let argc = self.read_u8()?;
                return self.execute_call_value(argc, Some(call_ip));
            }
            other => return Err(VmError::InvalidOpcode(other)),
        }
        Ok(ExecOutcome::Continue)
    }

    pub fn resume(&mut self) -> VmResult<VmStatus> {
        let allow_jit = !matches!(
            self.execution_frames
                .last()
                .map(|frame| &frame.continuation),
            Some(FrameContinuation::ReturnToHost)
        );
        self.run_internal(None, allow_jit)
    }

    pub fn stack(&self) -> &[Value] {
        &self.stack
    }

    pub fn locals(&self) -> &[Value] {
        &self.locals
    }

    pub fn set_local(&mut self, index: u8, value: Value) -> VmResult<()> {
        self.store_local_with_drop_contract(index, value)?;
        let config = *self.jit.config();
        self.set_jit_config(config);
        Ok(())
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

    pub(super) fn owns_callable(&self, value: &Value) -> bool {
        let Value::Callable(target) = value else {
            return false;
        };
        self.owned_callables.iter().any(|owned| {
            owned
                .upgrade()
                .is_some_and(|owned| Arc::ptr_eq(&owned, target))
        })
    }

    pub fn resolve_exported_callable(&self, name: &str) -> VmResult<Value> {
        let exported = self
            .program
            .exported_callables
            .iter()
            .find(|exported| exported.name == name)
            .ok_or_else(|| {
                VmError::HostError(format!("unknown exported script function '{name}'"))
            })?;
        let value = self
            .locals
            .get(exported.local_slot as usize)
            .cloned()
            .ok_or(VmError::InvalidLocal(
                u8::try_from(exported.local_slot).unwrap_or(u8::MAX),
            ))?;
        if matches!(value, Value::Callable(_)) {
            Ok(value)
        } else {
            Err(VmError::InvalidFrameState(
                "exported script function is not initialized",
            ))
        }
    }

    pub fn debug_info(&self) -> Option<&crate::debug_info::DebugInfo> {
        self.program.debug.as_ref()
    }

    pub fn call_depth(&self) -> usize {
        self.call_depth
    }

    pub fn queue_callable(&mut self, callable: Value, args: Vec<Value>) -> VmResult<()> {
        self.queue_callable_with_subscription(callable, args, None)
    }

    pub(super) fn queue_callable_with_subscription(
        &mut self,
        callable: Value,
        args: Vec<Value>,
        subscription: Option<Arc<AtomicBool>>,
    ) -> VmResult<()> {
        if self.shutdown {
            return Err(VmError::InvalidFrameState("vm is shut down"));
        }
        if !matches!(&callable, Value::Callable(_)) {
            return Err(VmError::InvalidCallable);
        }
        self.queued_callables.push_back(QueuedCallable {
            callable,
            args,
            subscription,
        });
        Ok(())
    }

    pub fn queued_callable_count(&self) -> usize {
        self.queued_callables.len()
    }

    pub fn drain_callable_queue(&mut self) -> VmResult<Vec<Value>> {
        if self.draining_queued_callables {
            return Err(VmError::InvalidFrameState(
                "callable queue is already being drained",
            ));
        }
        if !self.execution_frames.is_empty() {
            return Err(VmError::InvalidFrameState(
                "queued callables can only run after the root frame halts",
            ));
        }
        self.draining_queued_callables = true;
        let mut results = Vec::with_capacity(self.queued_callables.len());
        while let Some(queued) = self.queued_callables.pop_front() {
            if queued
                .subscription
                .as_ref()
                .is_some_and(|active| !active.load(Ordering::Acquire))
            {
                continue;
            }
            match self.start_callable(queued.callable, &queued.args) {
                Ok(VmStatus::Halted) => {
                    let Some(result) = self.host_return.take() else {
                        self.completed_callable_results.extend(results);
                        self.draining_queued_callables = false;
                        return Err(VmError::InvalidFrameState(
                            "queued invocation completed without a result",
                        ));
                    };
                    results.push(result);
                }
                Ok(VmStatus::Yielded) => {
                    self.completed_callable_results.extend(results);
                    self.draining_queued_callables = false;
                    return Err(VmError::InvalidFrameState(
                        "queued invocation yielded; resume it before draining again",
                    ));
                }
                Ok(VmStatus::Waiting(_)) => {
                    self.completed_callable_results.extend(results);
                    self.draining_queued_callables = false;
                    return Err(VmError::InvalidFrameState(
                        "queued invocation is waiting; resume it before draining again",
                    ));
                }
                Err(err) => {
                    self.completed_callable_results.extend(results);
                    self.draining_queued_callables = false;
                    return Err(err);
                }
            }
        }
        self.draining_queued_callables = false;
        Ok(results)
    }

    pub fn shutdown(&mut self) {
        self.invalidate_callback_registries();
        self.cancel_waiting_host_op();
        self.queued_callables.clear();
        self.completed_callable_results.clear();
        self.owned_callables.clear();
        self.draining_queued_callables = false;
        self.clear_stack_with_drop_contract();
        self.capture_cells.clear();
        self.shared_capture_slots.clear();
        self.clear_locals_with_drop_contract();
        self.execution_frames.clear();
        self.active_local_base_cache = 0;
        self.active_operand_stack_base_cache = 0;
        self.call_depth = 0;
        self.host_return = None;
        self.waiting_host_op = None;
        crate::builtins::runtime::close_all_handles(self);
        self.shutdown = true;
    }

    pub(super) fn register_callback_registry(&mut self, active: &Arc<AtomicBool>) {
        self.callback_registry_flags.push(Arc::downgrade(active));
    }

    fn invalidate_callback_registries(&mut self) {
        for active in self
            .callback_registry_flags
            .drain(..)
            .filter_map(|flag| flag.upgrade())
        {
            active.store(false, Ordering::Release);
        }
    }

    pub fn start_callable(&mut self, callable: Value, args: &[Value]) -> VmResult<VmStatus> {
        if self.shutdown {
            return Err(VmError::InvalidFrameState("vm is shut down"));
        }
        if !matches!(&callable, Value::Callable(_)) {
            return Err(VmError::InvalidCallable);
        }
        if !self.execution_frames.is_empty() {
            return Err(VmError::InvalidFrameState(
                "host invocation requires a halted VM",
            ));
        }
        let argc = u8::try_from(args.len())
            .map_err(|_| VmError::InvalidFrameState("too many arguments"))?;
        let stack_base = self.stack.len();
        let frame_count = self.execution_frames.len();
        self.stack.push(callable);
        self.stack.extend_from_slice(args);
        self.host_return = None;
        let outcome = match self.execute_call_value(argc, None) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.abort_host_invocation(stack_base, frame_count);
                return Err(error);
            }
        };
        if self.execution_frames.len() == frame_count {
            let result = match outcome {
                ExecOutcome::Continue | ExecOutcome::Halted => {
                    self.stack.pop().unwrap_or(Value::Null)
                }
                ExecOutcome::Yielded => {
                    self.abort_host_invocation(stack_base, frame_count);
                    return Err(VmError::InvalidFrameState(
                        "direct host callable invocation yielded",
                    ));
                }
                ExecOutcome::Waiting(_) => {
                    self.abort_host_invocation(stack_base, frame_count);
                    return Err(VmError::InvalidFrameState(
                        "direct host callable invocation is waiting",
                    ));
                }
            };
            self.stack.truncate(stack_base);
            self.host_return = Some(result);
            return Ok(VmStatus::Halted);
        }
        if let Some(frame) = self.execution_frames.last_mut() {
            frame.continuation = FrameContinuation::ReturnToHost;
        }
        match self.run_internal(None, false) {
            Ok(status) => Ok(status),
            Err(error) => {
                self.abort_host_invocation(stack_base, frame_count);
                Err(error)
            }
        }
    }

    pub fn invoke_callable(&mut self, callable: Value, args: &[Value]) -> VmResult<Value> {
        let stack_base = self.stack.len();
        let frame_count = self.execution_frames.len();
        match self.start_callable(callable, args)? {
            VmStatus::Halted => self.host_return.take().ok_or(VmError::InvalidFrameState(
                "host invocation completed without a result",
            )),
            VmStatus::Yielded => {
                self.abort_host_invocation(stack_base, frame_count);
                Err(VmError::InvalidFrameState("host invocation yielded"))
            }
            VmStatus::Waiting(_) => {
                self.abort_host_invocation(stack_base, frame_count);
                Err(VmError::InvalidFrameState("host invocation is waiting"))
            }
        }
    }

    fn abort_host_invocation(&mut self, stack_base: usize, frame_count: usize) {
        while self.execution_frames.len() > frame_count {
            let Some(frame) = self.execution_frames.pop() else {
                break;
            };
            let frame_end = frame.local_base.saturating_add(frame.local_count);
            self.capture_cells
                .retain(|absolute, _| *absolute < frame.local_base || *absolute >= frame_end);
            self.shared_capture_slots
                .retain(|absolute| *absolute < frame.local_base || *absolute >= frame_end);
            if frame.local_base <= self.locals.len() {
                let drained = self.locals.drain(frame.local_base..).collect::<Vec<_>>();
                for value in drained {
                    self.drop_value_with_contract(value);
                }
            }
        }
        self.active_local_base_cache = self
            .execution_frames
            .last()
            .map(|frame| frame.local_base)
            .unwrap_or(0);
        self.active_operand_stack_base_cache = self
            .execution_frames
            .last()
            .map(|frame| frame.operand_stack_base)
            .unwrap_or(0);
        while self.stack.len() > stack_base {
            if let Some(value) = self.stack.pop() {
                self.drop_value_with_contract(value);
            }
        }
        self.call_depth = self.script_frame_depth();
        self.host_return = None;
        self.cancel_waiting_host_op();
        self.last_yield_reason = None;
        self.map_iterators
            .truncate(self.call_depth.saturating_add(1));
    }

    pub fn take_callable_result(&mut self) -> Option<Value> {
        self.completed_callable_results
            .pop_front()
            .or_else(|| self.host_return.take())
    }

    pub fn execution_frames(&self) -> Vec<VmExecutionFrameSnapshot> {
        self.execution_frames
            .iter()
            .map(|frame| VmExecutionFrameSnapshot {
                continuation: match frame.continuation {
                    FrameContinuation::Halt => VmFrameContinuation::Halt,
                    FrameContinuation::ResumeBytecode { return_ip } => {
                        VmFrameContinuation::ResumeBytecode { return_ip }
                    }
                    FrameContinuation::ReturnToHost => VmFrameContinuation::ReturnToHost,
                },
                operand_stack_base: frame.operand_stack_base,
                local_base: frame.local_base,
                local_count: frame.local_count,
                prototype_id: frame.prototype_id,
            })
            .collect()
    }
}
