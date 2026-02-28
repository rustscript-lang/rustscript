use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::builtins::BuiltinFunction;
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use std::sync::{Mutex, OnceLock};

mod builtin_runtime;
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
mod jit_native;

pub use crate::bytecode::{HostImport, OpCode, Program, Value};

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
        }
    }
}

impl std::error::Error for VmError {}

pub type VmResult<T> = Result<T, VmError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmStatus {
    Halted,
    Yielded,
}

#[derive(Debug, PartialEq)]
pub enum CallOutcome {
    Return(Vec<Value>),
    Yield,
}

pub trait HostFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome>;
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
    program: Program,
    program_cache_key: u64,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    host_functions: Vec<VmHostFunction>,
    host_function_symbols: HashMap<String, u16>,
    resolved_calls: Vec<u16>,
    resolved_calls_dirty: bool,
    call_depth: usize,
    jit: crate::jit::TraceJitEngine,
    native_traces: HashMap<usize, NativeTrace>,
    native_trace_exec_count: u64,
    io_state: builtin_runtime::IoState,
}

enum StepExecOutcome {
    Continue,
    Halted,
    Yielded,
}

enum TraceExecOutcome {
    Continue,
    Halted,
    Yielded,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
type NativeTraceEntry = unsafe extern "C" fn(*mut Vm) -> i32;

#[cfg(not(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
)))]
type NativeTraceEntry = fn(*mut Vm) -> i32;

struct NativeTrace {
    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    _memory: Arc<jit_native::ExecutableMemory>,
    entry: NativeTraceEntry,
    code: Arc<[u8]>,
    root_ip: usize,
    terminal: crate::jit::JitTraceTerminal,
    has_yielding_call: bool,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NativeTraceCacheKey {
    root_ip: usize,
    terminal: crate::jit::JitTraceTerminal,
    steps: Vec<crate::jit::TraceStep>,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
struct NativeTraceCacheEntry {
    memory: Arc<jit_native::ExecutableMemory>,
    code: Arc<[u8]>,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
struct NativeTraceCache {
    active_program_key: Option<u64>,
    entries: HashMap<NativeTraceCacheKey, NativeTraceCacheEntry>,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
static NATIVE_TRACE_CACHE: OnceLock<Mutex<NativeTraceCache>> = OnceLock::new();

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache() -> &'static Mutex<NativeTraceCache> {
    NATIVE_TRACE_CACHE.get_or_init(|| {
        Mutex::new(NativeTraceCache {
            active_program_key: None,
            entries: HashMap::new(),
        })
    })
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_key(trace: &crate::jit::JitTrace) -> NativeTraceCacheKey {
    NativeTraceCacheKey {
        root_ip: trace.root_ip,
        terminal: trace.terminal.clone(),
        steps: trace.steps.clone(),
    }
}

fn compute_program_cache_key(program: &Program) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    program.code.hash(&mut hasher);
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
        let program_cache_key = compute_program_cache_key(&program);
        Self {
            program,
            program_cache_key,
            ip: 0,
            stack: Vec::new(),
            locals: Vec::new(),
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            call_depth: 0,
            jit: crate::jit::TraceJitEngine::default(),
            native_traces: HashMap::new(),
            native_trace_exec_count: 0,
            io_state: builtin_runtime::IoState::default(),
        }
    }

    pub fn with_locals(program: Program, local_count: usize) -> Self {
        let program_cache_key = compute_program_cache_key(&program);
        Self {
            program,
            program_cache_key,
            ip: 0,
            stack: Vec::new(),
            locals: vec![Value::Null; local_count],
            host_functions: Vec::new(),
            host_function_symbols: HashMap::new(),
            resolved_calls: Vec::new(),
            resolved_calls_dirty: true,
            call_depth: 0,
            jit: crate::jit::TraceJitEngine::default(),
            native_traces: HashMap::new(),
            native_trace_exec_count: 0,
            io_state: builtin_runtime::IoState::default(),
        }
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

    pub fn run(&mut self) -> VmResult<VmStatus> {
        self.run_internal(None, true)
    }

    pub fn run_with_debugger(
        &mut self,
        debugger: &mut crate::debugger::Debugger,
    ) -> VmResult<VmStatus> {
        self.run_internal(Some(debugger), false)
    }

    pub fn set_jit_config(&mut self, config: crate::jit::JitConfig) {
        self.jit.set_config(config);
    }

    pub fn jit_config(&self) -> &crate::jit::JitConfig {
        self.jit.config()
    }

    pub fn jit_snapshot(&self) -> crate::jit::JitSnapshot {
        self.jit.snapshot()
    }

    pub fn dump_jit_info(&self) -> String {
        let mut out = self.jit.dump_text(self.program.debug.as_ref());
        out.push_str(&format!(
            "  native trace executions: {}\n",
            self.native_trace_exec_count
        ));
        if self.native_traces.is_empty() {
            out.push_str("  native traces: 0\n");
            return out;
        }

        out.push_str(&format!("  native traces: {}\n", self.native_traces.len()));
        let mut ids: Vec<usize> = self.native_traces.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(native) = self.native_traces.get(&id) {
                out.push_str(&format!(
                    "  native trace#{} entry=0x{:X} code_bytes={}\n",
                    id,
                    native.entry as usize,
                    native.code.len()
                ));
                out.push_str("    code:");
                for byte in native.code.iter() {
                    out.push_str(&format!(" {:02X}", byte));
                }
                out.push('\n');
            }
        }
        out
    }

    fn run_internal(
        &mut self,
        mut debugger: Option<&mut crate::debugger::Debugger>,
        allow_jit: bool,
    ) -> VmResult<VmStatus> {
        self.ensure_call_bindings()?;
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
                    match self.execute_jit_entry(trace_id)? {
                        TraceExecOutcome::Continue => continue,
                        TraceExecOutcome::Halted => {
                            if let Some(active_debugger) = debugger.as_deref_mut() {
                                active_debugger.on_vm_status(self, VmStatus::Halted);
                            }
                            return Ok(VmStatus::Halted);
                        }
                        TraceExecOutcome::Yielded => {
                            if let Some(active_debugger) = debugger.as_deref_mut() {
                                active_debugger.on_vm_status(self, VmStatus::Yielded);
                            }
                            return Ok(VmStatus::Yielded);
                        }
                    }
                }
            }

            if self.ip >= self.program.code.len() {
                return Err(VmError::BytecodeBounds);
            }

            let opcode = self.read_u8()?;
            match self.execute_interpreter_instruction(opcode)? {
                StepExecOutcome::Continue => {}
                StepExecOutcome::Halted => {
                    if let Some(active_debugger) = debugger.as_deref_mut() {
                        active_debugger.on_vm_status(self, VmStatus::Halted);
                    }
                    return Ok(VmStatus::Halted);
                }
                StepExecOutcome::Yielded => {
                    if let Some(active_debugger) = debugger.as_deref_mut() {
                        active_debugger.on_vm_status(self, VmStatus::Yielded);
                    }
                    return Ok(VmStatus::Yielded);
                }
            }
        }
    }

    fn execute_interpreter_instruction(&mut self, opcode: u8) -> VmResult<StepExecOutcome> {
        match opcode {
            x if x == OpCode::Nop as u8 => {}
            x if x == OpCode::Ret as u8 => return Ok(StepExecOutcome::Halted),
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
                self.binary_numeric_op(|lhs, rhs| Ok(lhs.wrapping_sub(rhs)), |lhs, rhs| Ok(lhs - rhs))?;
            }
            x if x == OpCode::Mul as u8 => {
                self.binary_numeric_op(|lhs, rhs| Ok(lhs.wrapping_mul(rhs)), |lhs, rhs| Ok(lhs * rhs))?;
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
                if self.execute_host_call(index, argc_u8, call_ip)? {
                    return Ok(StepExecOutcome::Yielded);
                }
            }
            other => return Err(VmError::InvalidOpcode(other)),
        }
        Ok(StepExecOutcome::Continue)
    }

    #[cfg_attr(
        any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ),
        allow(dead_code)
    )]
    fn execute_jit_trace(&mut self, trace_id: usize) -> VmResult<TraceExecOutcome> {
        let Some(trace) = self.jit.trace_clone(trace_id) else {
            return Ok(TraceExecOutcome::Continue);
        };
        for step in &trace.steps {
            match step {
                crate::jit::TraceStep::Nop => {}
                crate::jit::TraceStep::Ldc(index) => {
                    let value = self
                        .program
                        .constants
                        .get(*index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidConstant(*index))?;
                    self.stack.push(value);
                }
                crate::jit::TraceStep::Add => {
                    self.binary_add_op()?;
                }
                crate::jit::TraceStep::Sub => {
                    self.binary_numeric_op(|lhs, rhs| Ok(lhs - rhs), |lhs, rhs| Ok(lhs - rhs))?;
                }
                crate::jit::TraceStep::Mul => {
                    self.binary_numeric_op(|lhs, rhs| Ok(lhs * rhs), |lhs, rhs| Ok(lhs * rhs))?;
                }
                crate::jit::TraceStep::Div => {
                    self.binary_numeric_op(
                        |lhs, rhs| {
                            if rhs == 0 {
                                return Err(VmError::DivisionByZero);
                            }
                            Ok(lhs / rhs)
                        },
                        |lhs, rhs| {
                            if rhs == 0.0 {
                                return Err(VmError::DivisionByZero);
                            }
                            Ok(lhs / rhs)
                        },
                    )?;
                }
                crate::jit::TraceStep::Shl => {
                    let rhs = self.pop_shift_amount()?;
                    let lhs = self.pop_int()?;
                    self.stack.push(Value::Int(lhs << rhs));
                }
                crate::jit::TraceStep::Shr => {
                    let rhs = self.pop_shift_amount()?;
                    let lhs = self.pop_int()?;
                    self.stack.push(Value::Int(lhs >> rhs));
                }
                crate::jit::TraceStep::Neg => {
                    let value = self.pop_numeric()?;
                    match value {
                        NumericValue::Int(value) => self.stack.push(Value::Int(-value)),
                        NumericValue::Float(value) => self.stack.push(Value::Float(-value)),
                    }
                }
                crate::jit::TraceStep::Ceq => {
                    let rhs = self.pop_value()?;
                    let lhs = self.pop_value()?;
                    self.stack.push(Value::Bool(lhs == rhs));
                }
                crate::jit::TraceStep::Clt => {
                    self.compare_numeric_op(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?;
                }
                crate::jit::TraceStep::Cgt => {
                    self.compare_numeric_op(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?;
                }
                crate::jit::TraceStep::Pop => {
                    self.pop_value()?;
                }
                crate::jit::TraceStep::Dup => {
                    let value = self.peek_value()?.clone();
                    self.stack.push(value);
                }
                crate::jit::TraceStep::Ldloc(index) => {
                    let value = self
                        .locals
                        .get(*index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidLocal(*index))?;
                    self.stack.push(value);
                }
                crate::jit::TraceStep::Stloc(index) => {
                    let value = self.pop_value()?;
                    let slot = self
                        .locals
                        .get_mut(*index as usize)
                        .ok_or(VmError::InvalidLocal(*index))?;
                    *slot = value;
                }
                crate::jit::TraceStep::Call {
                    index,
                    argc,
                    call_ip,
                } => {
                    if self.execute_host_call(*index, *argc, *call_ip)? {
                        return Ok(TraceExecOutcome::Yielded);
                    }
                }
                crate::jit::TraceStep::GuardFalse { exit_ip } => {
                    let condition = self.pop_bool()?;
                    if !condition {
                        self.jump_to(*exit_ip)?;
                        self.jit.mark_trace_executed(trace_id);
                        return Ok(TraceExecOutcome::Continue);
                    }
                }
                crate::jit::TraceStep::JumpToIp { target_ip } => {
                    self.jump_to(*target_ip)?;
                    self.jit.mark_trace_executed(trace_id);
                    return Ok(TraceExecOutcome::Continue);
                }
                crate::jit::TraceStep::JumpToRoot => {
                    self.jump_to(trace.root_ip)?;
                    self.jit.mark_trace_executed(trace_id);
                    return Ok(TraceExecOutcome::Continue);
                }
                crate::jit::TraceStep::Ret => {
                    self.jit.mark_trace_executed(trace_id);
                    return Ok(TraceExecOutcome::Halted);
                }
            }
        }
        self.jit.mark_trace_executed(trace_id);
        Ok(TraceExecOutcome::Continue)
    }

    fn execute_jit_entry(&mut self, trace_id: usize) -> VmResult<TraceExecOutcome> {
        #[cfg(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ))]
        {
            self.execute_jit_native(trace_id)
        }
        #[cfg(not(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        )))]
        {
            self.execute_jit_trace(trace_id)
        }
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn execute_jit_native(&mut self, trace_id: usize) -> VmResult<TraceExecOutcome> {
        self.ensure_native_trace(trace_id)?;
        let (entry, root_ip, terminal, has_yielding_call) = {
            let native = self.native_traces.get(&trace_id).ok_or_else(|| {
                VmError::JitNative(format!("native trace entry for id {} missing", trace_id))
            })?;
            (
                native.entry,
                native.root_ip,
                native.terminal.clone(),
                native.has_yielding_call,
            )
        };

        loop {
            jit_native::clear_bridge_error();
            let status = unsafe { entry(self as *mut Vm) };
            self.native_trace_exec_count = self.native_trace_exec_count.saturating_add(1);
            self.jit.mark_trace_executed(trace_id);

            match status {
                jit_native::STATUS_CONTINUE => return Ok(TraceExecOutcome::Continue),
                jit_native::STATUS_TRACE_EXIT => {
                    // Fast path: if this trace looped back to its own root and cannot yield via host
                    // calls, keep executing in native mode without bouncing through the interpreter.
                    if !has_yielding_call
                        && terminal == crate::jit::JitTraceTerminal::LoopBack
                        && self.ip == root_ip
                    {
                        continue;
                    }
                    return Ok(TraceExecOutcome::Continue);
                }
                jit_native::STATUS_HALTED => return Ok(TraceExecOutcome::Halted),
                jit_native::STATUS_YIELDED => return Ok(TraceExecOutcome::Yielded),
                jit_native::STATUS_ERROR => {
                    let err = jit_native::take_bridge_error().unwrap_or_else(|| {
                        VmError::JitNative(
                            "jit bridge reported failure without VmError".to_string(),
                        )
                    });
                    return Err(err);
                }
                other => {
                    return Err(VmError::JitNative(format!(
                        "unexpected native trace return status {}",
                        other
                    )));
                }
            }
        }
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn ensure_native_trace(&mut self, trace_id: usize) -> VmResult<()> {
        if self.native_traces.contains_key(&trace_id) {
            return Ok(());
        }

        let trace = self.jit.trace_clone(trace_id).ok_or_else(|| {
            VmError::JitNative(format!("trace {} missing for native compile", trace_id))
        })?;
        let key = native_trace_cache_key(&trace);
        let cache = native_trace_cache();
        let (memory, code) = {
            let mut guard = cache
                .lock()
                .map_err(|_| VmError::JitNative("native trace cache lock poisoned".to_string()))?;
            if guard.active_program_key != Some(self.program_cache_key) {
                guard.entries.clear();
                guard.active_program_key = Some(self.program_cache_key);
            }

            if let Some(hit) = guard.entries.get(&key) {
                (Arc::clone(&hit.memory), Arc::clone(&hit.code))
            } else {
                let code = Arc::<[u8]>::from(
                    jit_native::emit_native_trace_bytes(&trace)?.into_boxed_slice(),
                );
                let memory = Arc::new(jit_native::ExecutableMemory::from_code(code.as_ref())?);
                guard.entries.insert(
                    key,
                    NativeTraceCacheEntry {
                        memory: Arc::clone(&memory),
                        code: Arc::clone(&code),
                    },
                );
                (memory, code)
            }
        };
        let entry = unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(memory.ptr) };
        self.native_traces.insert(
            trace_id,
            NativeTrace {
                _memory: memory,
                entry,
                code,
                root_ip: trace.root_ip,
                terminal: trace.terminal,
                has_yielding_call: trace.has_yielding_call,
            },
        );
        Ok(())
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
        &self.program
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

    pub fn jit_native_trace_count(&self) -> usize {
        self.native_traces.len()
    }

    pub fn jit_native_exec_count(&self) -> u64 {
        self.native_trace_exec_count
    }

    fn pop_value(&mut self) -> VmResult<Value> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
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

    fn execute_host_call(&mut self, index: u16, argc_u8: u8, call_ip: usize) -> VmResult<bool> {
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
            let values = builtin_runtime::execute_builtin_call(self, builtin, &args)?;
            for value in values {
                self.stack.push(value);
            }
            return Ok(false);
        }

        let resolved_index = self.resolve_call_target(index, argc_u8)?;

        self.call_depth += 1;
        let function_ptr = self
            .host_functions
            .get_mut(resolved_index as usize)
            .ok_or(VmError::InvalidCall(index))? as *mut VmHostFunction;
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
                Ok(false)
            }
            CallOutcome::Yield => {
                for value in args {
                    self.stack.push(value);
                }
                self.ip = call_ip;
                Ok(true)
            }
        }
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
        builtin_runtime::close_all_handles(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn native_trace_cache_resets_when_program_changes() {
        {
            let mut guard = native_trace_cache()
                .lock()
                .expect("native trace cache lock should succeed");
            guard.entries.clear();
            guard.active_program_key = None;
        }

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

        let mut vm_one = Vm::with_locals(compiled_one.program, compiled_one.locals);
        vm_one.set_jit_config(crate::jit::JitConfig {
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

        let (cache_program_after_one, cache_entries_after_one) = {
            let guard = native_trace_cache()
                .lock()
                .expect("native trace cache lock should succeed");
            (guard.active_program_key, guard.entries.len())
        };
        assert_eq!(
            cache_program_after_one,
            Some(vm_one.program_cache_key),
            "cache should be keyed to first program after first run"
        );
        assert_eq!(
            cache_entries_after_one, vm_one_trace_count,
            "cache entry count should match first program traces"
        );

        let mut vm_two = Vm::with_locals(compiled_two.program, compiled_two.locals);
        vm_two.set_jit_config(crate::jit::JitConfig {
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

        let (cache_program_after_two, cache_entries_after_two) = {
            let guard = native_trace_cache()
                .lock()
                .expect("native trace cache lock should succeed");
            (guard.active_program_key, guard.entries.len())
        };
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
}
