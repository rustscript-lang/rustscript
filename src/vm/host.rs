use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll, Wake, Waker};

use crate::builtins::BuiltinFunction;
use crate::vm::operation::OperationCancelReason;

use super::async_host::{HostFuture, HostFutureOutput};
use super::capability::CapabilityProfile;
use super::*;

pub type HostOpId = u64;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum CallReturn {
    #[default]
    None,
    One(Value),
}

impl CallReturn {
    pub fn none() -> Self {
        Self::None
    }

    pub fn one(value: Value) -> Self {
        Self::One(value)
    }

    pub fn from_values(values: Vec<Value>) -> Self {
        match values.len() {
            0 => Self::None,
            1 => Self::One(
                values
                    .into_iter()
                    .next()
                    .expect("single-value return should contain one value"),
            ),
            _ => Self::One(Value::array(values)),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn as_slice(&self) -> &[Value] {
        match self {
            Self::None => &[],
            Self::One(value) => std::slice::from_ref(value),
        }
    }

    pub(crate) fn push_onto_stack(self, stack: &mut Vec<Value>) {
        match self {
            Self::None => {}
            Self::One(value) => stack.push(value),
        }
    }
}

impl From<Vec<Value>> for CallReturn {
    fn from(values: Vec<Value>) -> Self {
        Self::from_values(values)
    }
}

#[derive(Debug, PartialEq)]
pub enum CallOutcome {
    Return(CallReturn),
    Halt,
    Yield,
    Pending(HostOpId),
}

pub trait HostFunction: Send {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome>;
}

/// VM-aware host functions that opt into borrowed stack-tail dispatch.
///
/// Implementations must not re-enter the VM or otherwise structurally mutate
/// the value stack while `args` is borrowed for the duration of `call`.
pub trait HostStackFunction: Send {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome>;
}

pub trait HostArgsFunction: Send {
    fn call(&mut self, args: &[Value]) -> VmResult<CallOutcome>;
}

pub trait HostAsyncBridge: Send {
    fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
        Err(VmError::HostError(
            "async host bridge does not accept submitted futures".to_string(),
        ))
    }

    fn poll_op(&mut self, op_id: HostOpId, cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>>;

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        self.poll_op(op_id, cx)
            .map(|result| result.map(HostFutureOutput::Return))
    }

    fn cancel_op(&mut self, _op_id: HostOpId) {}

    fn cancel_op_with_reason(&mut self, op_id: HostOpId, _reason: OperationCancelReason) {
        self.cancel_op(op_id);
    }
}

pub type StaticHostFunction = fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>;
pub type StaticHostStackFunction = fn(&mut Vm, &[Value]) -> VmResult<CallOutcome>;
pub type StaticHostArgsFunction = fn(&[Value]) -> VmResult<CallOutcome>;

type HostFactory = dyn Fn() -> Box<dyn HostFunction> + Send + Sync;
type HostStackFactory = dyn Fn() -> Box<dyn HostStackFunction> + Send + Sync;
type HostArgsFactory = dyn Fn() -> Box<dyn HostArgsFunction> + Send + Sync;

#[derive(Clone)]
enum RegistryEntryKind {
    Factory(Arc<HostFactory>),
    Static(StaticHostFunction),
    StackFactory(Arc<HostStackFactory>),
    StackStatic(StaticHostStackFunction),
    ArgsFactory(Arc<HostArgsFactory>),
    ArgsStatic(StaticHostArgsFunction),
    ArgsStaticNonYielding(StaticHostArgsFunction),
}

#[derive(Clone)]
struct RegistryEntry {
    arity: u8,
    kind: RegistryEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBindingPlan {
    import_signature: Vec<HostImport>,
    registry_slots: Vec<u16>,
    resolved_calls: Vec<u16>,
    allowed_builtin_calls: Vec<u16>,
    allow_default_builtin_capabilities: bool,
    allowed_host_function_slots: Vec<u16>,
    allow_default_host_capabilities: bool,
    capability_profile: Arc<CapabilityProfile>,
    capability_fingerprint: u64,
    registry_state: Arc<()>,
    registry_generation_token: Arc<()>,
    registry_generation: u64,
}

#[derive(Clone)]
pub struct HostFunctionRegistry {
    entries: Arc<Vec<RegistryEntry>>,
    by_name: Arc<HashMap<String, u16>>,
    plan_cache: Arc<RwLock<HashMap<Vec<HostImport>, Arc<HostBindingPlan>>>>,
    allowed_builtin_calls: Arc<Vec<u16>>,
    allow_default_builtin_capabilities: bool,
    allow_default_host_capabilities: bool,
    capability_profile: Arc<CapabilityProfile>,
    registry_state: Arc<()>,
    registry_generation_token: Arc<()>,
    registry_generation: Arc<AtomicU64>,
    /// Caller-provided standard-surface composition strategy, if installed.
    ///
    /// This is explicit per-instance state: the outer standard-runtime
    /// constructor installs it; `src/vm` never names a concrete domain.
    standard_composition: Option<Arc<dyn super::standard_composition::StandardSurfaceComposition>>,
}

impl Default for HostFunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostFunctionRegistry {
    pub fn empty() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            by_name: Arc::new(HashMap::new()),
            plan_cache: Arc::new(RwLock::new(HashMap::new())),
            allowed_builtin_calls: Arc::new(Vec::new()),
            allow_default_builtin_capabilities: true,
            allow_default_host_capabilities: true,
            capability_profile: Arc::new(CapabilityProfile::allow_all()),
            registry_state: Arc::new(()),
            registry_generation_token: Arc::new(()),
            registry_generation: Arc::new(AtomicU64::new(0)),
            standard_composition: None,
        }
    }

    pub fn new() -> Self {
        static DEFAULT_REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();

        let mut registry = DEFAULT_REGISTRY
            .get_or_init(|| {
                let mut registry = Self::empty();
                crate::builtins::runtime::register_default_host_functions(&mut registry);
                registry.allow_default_builtin_capabilities = true;
                registry.allow_default_host_capabilities = true;
                registry
            })
            .clone();
        registry.plan_cache = Arc::new(RwLock::new(HashMap::new()));
        registry.capability_profile = Arc::new(CapabilityProfile::allow_all());
        registry.registry_state = Arc::new(());
        registry.registry_generation_token = Arc::new(());
        registry.registry_generation = Arc::new(AtomicU64::new(0));
        registry
    }

    /// Returns the standard host registry with every registered host function present but
    /// requiring an explicit capability grant before execution.
    pub fn restricted() -> Self {
        let mut registry = Self::new();
        registry.allow_default_builtin_capabilities = false;
        registry.allow_default_host_capabilities = false;
        registry.capability_profile = Arc::new(CapabilityProfile::deny_all());
        registry.registry_state = Arc::new(());
        registry.registry_generation_token = Arc::new(());
        registry.registry_generation = Arc::new(AtomicU64::new(0));
        registry.invalidate_plan_cache();
        registry
    }

    /// Replaces the registry's immutable capability profile.
    pub fn set_capability_profile(&mut self, profile: CapabilityProfile) {
        self.allowed_builtin_calls = Arc::new(profile.allowed_builtin_calls().to_vec());
        self.allow_default_builtin_capabilities = profile.allows_all_builtins();
        self.allow_default_host_capabilities = profile.allows_all_host_imports();
        self.capability_profile = Arc::new(profile);
        self.invalidate_plan_cache();
    }

    /// Installs the caller-provided standard-surface composition strategy.
    ///
    /// Explicit per-instance state: the outer standard-runtime constructor
    /// installs it; `src/vm` never names a concrete domain module or feature.
    pub fn set_standard_composition(
        &mut self,
        composition: Arc<dyn super::standard_composition::StandardSurfaceComposition>,
    ) {
        self.standard_composition = Some(composition);
    }

    /// The installed standard-surface composition strategy, if any.
    pub fn standard_composition(
        &self,
    ) -> Option<&Arc<dyn super::standard_composition::StandardSurfaceComposition>> {
        self.standard_composition.as_ref()
    }

    /// Whether a host function with the given name is currently registered.
    pub fn contains_name(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Explicitly permits a namespaced builtin when this registry is used as a capability plan.
    pub fn allow_builtin(&mut self, name: impl AsRef<str>) -> VmResult<()> {
        let name = name.as_ref();
        if self.by_name.contains_key(name) {
            self.capability_profile = Arc::new(self.capability_profile.with_host_import(name));
            self.invalidate_plan_cache();
            return Ok(());
        }
        let builtin = BuiltinFunction::from_namespaced_name(name)
            .ok_or_else(|| VmError::HostError(format!("unknown namespaced builtin '{name}'")))?;
        let calls = Arc::make_mut(&mut self.allowed_builtin_calls);
        if !calls.contains(&builtin.call_index()) {
            calls.push(builtin.call_index());
            calls.sort_unstable();
        }
        self.capability_profile = Arc::new(self.capability_profile.with_builtin(builtin));
        self.invalidate_plan_cache();
        Ok(())
    }

    fn invalidate_plan_cache(&mut self) {
        self.registry_state = Arc::new(());
        self.registry_generation.fetch_add(1, Ordering::Relaxed);
        self.plan_cache = Arc::new(RwLock::new(HashMap::new()));
    }

    pub fn register<F>(&mut self, name: impl Into<String>, arity: u8, factory: F)
    where
        F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
    {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::Factory(Arc::new(factory));
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Factory(Arc::new(factory)),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    pub fn register_static(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        function: StaticHostFunction,
    ) {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::Static(function);
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Static(function),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    pub fn register_stack<F>(&mut self, name: impl Into<String>, arity: u8, factory: F)
    where
        F: Fn() -> Box<dyn HostStackFunction> + Send + Sync + 'static,
    {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::StackFactory(Arc::new(factory));
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::StackFactory(Arc::new(factory)),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    pub fn register_static_stack(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        function: StaticHostStackFunction,
    ) {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::StackStatic(function);
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::StackStatic(function),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    pub fn register_args<F>(&mut self, name: impl Into<String>, arity: u8, factory: F)
    where
        F: Fn() -> Box<dyn HostArgsFunction> + Send + Sync + 'static,
    {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::ArgsFactory(Arc::new(factory));
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::ArgsFactory(Arc::new(factory)),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    pub fn register_static_args(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        function: StaticHostArgsFunction,
    ) {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::ArgsStatic(function);
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::ArgsStatic(function),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    /// Registers a static args-only host function that always returns one value synchronously.
    ///
    /// The returned [`Value`] must match the return type declared by the corresponding host
    /// import. Returning a different type is reported as [`VmError::TypeMismatch`]. Returning no
    /// value, `Halt`, `Yield`, or `Pending` violates the contract and is reported as a host error.
    /// When appropriate, the native JIT may keep traces active across the call boundary.
    pub fn register_static_non_yielding_args(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        function: StaticHostArgsFunction,
    ) {
        let name = name.into();
        if let Some(&slot) = self.by_name.get(&name)
            && let Some(entry) = Arc::make_mut(&mut self.entries).get_mut(slot as usize)
        {
            entry.arity = arity;
            entry.kind = RegistryEntryKind::ArgsStaticNonYielding(function);
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::ArgsStaticNonYielding(function),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    fn validate_builtin_capability(&self, call_index: u16) -> VmResult<()> {
        if let Some(builtin) = BuiltinFunction::from_call_index(call_index)
            && builtin.requires_explicit_host_capability()
            && !self.allowed_builtin_calls.contains(&call_index)
        {
            return Err(VmError::HostError(format!(
                "capability profile does not allow builtin '{}'",
                builtin.name()
            )));
        }
        Ok(())
    }

    fn validate_program_capabilities(&self, program: &Program) -> VmResult<()> {
        if self.allow_default_builtin_capabilities {
            return Ok(());
        }
        let mut ip = 0usize;
        while let Some(&raw_opcode) = program.code.get(ip) {
            let opcode =
                OpCode::try_from(raw_opcode).map_err(|_| VmError::InvalidOpcode(raw_opcode))?;
            let operand_end = ip
                .checked_add(1 + opcode.operand_len())
                .ok_or(VmError::BytecodeBounds)?;
            if operand_end > program.code.len() {
                return Err(VmError::BytecodeBounds);
            }
            if opcode == OpCode::Call {
                let bytes: [u8; 2] = program.code[ip + 1..ip + 3]
                    .try_into()
                    .map_err(|_| VmError::BytecodeBounds)?;
                self.validate_builtin_capability(u16::from_le_bytes(bytes))?;
            }
            ip = operand_end;
        }
        for prototype in &program.callable_prototypes {
            if let CallableTarget::HostImport(call_index) = prototype.target {
                self.validate_builtin_capability(call_index)?;
            }
        }
        Ok(())
    }

    pub fn bind_vm_cached(&self, vm: &mut Vm) -> VmResult<()> {
        self.validate_program_capabilities(&vm.program)?;
        let plan = self.prepare_shared_plan(&vm.program.imports)?;
        self.bind_vm_with_plan(vm, &plan)
    }

    pub fn prepare_plan(&self, imports: &[HostImport]) -> VmResult<HostBindingPlan> {
        Ok(self.prepare_shared_plan(imports)?.as_ref().clone())
    }

    pub fn prepare_shared_plan(&self, imports: &[HostImport]) -> VmResult<Arc<HostBindingPlan>> {
        self.plan_for_imports(imports)
    }

    fn plan_matches_current(&self, plan: &HostBindingPlan) -> bool {
        self.capability_profile.fingerprint() == plan.capability_fingerprint
            && self.capability_profile.as_ref() == plan.capability_profile.as_ref()
            && Arc::ptr_eq(&self.registry_state, &plan.registry_state)
            && Arc::ptr_eq(
                &self.registry_generation_token,
                &plan.registry_generation_token,
            )
            && self.registry_generation.load(Ordering::Relaxed) == plan.registry_generation
    }

    fn plan_for_imports(&self, imports: &[HostImport]) -> VmResult<Arc<HostBindingPlan>> {
        if let Some(plan) = self
            .plan_cache
            .read()
            .expect("host binding plan cache read lock should not be poisoned")
            .get(imports)
            .cloned()
            && self.plan_matches_current(&plan)
        {
            return Ok(plan);
        }

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
            if !self.allow_default_host_capabilities
                && !self.capability_profile.allows_host_import(&import.name)
            {
                return Err(VmError::HostError(format!(
                    "capability profile does not allow host import '{}'",
                    import.name
                )));
            }
            if entry.arity != import.arity {
                return Err(VmError::InvalidCallArity {
                    import: import.name.clone(),
                    expected: entry.arity,
                    got: import.arity,
                });
            }

            let vm_slot = if let Some(&existing) = registry_slot_to_vm_slot.get(&registry_slot) {
                existing
            } else {
                let slot = registry_slots.len() as u16;
                registry_slots.push(registry_slot);
                registry_slot_to_vm_slot.insert(registry_slot, slot);
                slot
            };
            resolved_calls.push(vm_slot);
        }

        let allowed_host_function_slots = imports
            .iter()
            .zip(resolved_calls.iter().copied())
            .filter_map(|(import, vm_slot)| {
                self.capability_profile
                    .allows_host_import(&import.name)
                    .then_some(vm_slot)
            })
            .collect::<Vec<_>>();
        let import_key = imports.to_vec();
        let computed = Arc::new(HostBindingPlan {
            import_signature: import_key.clone(),
            registry_slots,
            resolved_calls,
            allowed_builtin_calls: self.allowed_builtin_calls.as_ref().clone(),
            allow_default_builtin_capabilities: self.allow_default_builtin_capabilities,
            allowed_host_function_slots,
            allow_default_host_capabilities: self.allow_default_host_capabilities,
            capability_profile: Arc::clone(&self.capability_profile),
            capability_fingerprint: self.capability_profile.fingerprint(),
            registry_state: Arc::clone(&self.registry_state),
            registry_generation_token: Arc::clone(&self.registry_generation_token),
            registry_generation: self.registry_generation.load(Ordering::Relaxed),
        });
        let mut cache = self
            .plan_cache
            .write()
            .expect("host binding plan cache write lock should not be poisoned");
        cache.insert(import_key, Arc::clone(&computed));
        Ok(computed)
    }

    pub fn bind_vm_with_plan(&self, vm: &mut Vm, plan: &HostBindingPlan) -> VmResult<()> {
        self.validate_program_capabilities(&vm.program)?;
        if vm.program.imports != plan.import_signature {
            return Err(VmError::HostError(
                "host binding plan does not match vm import signature".to_string(),
            ));
        }
        if self.capability_profile.fingerprint() != plan.capability_fingerprint
            || self.capability_profile.as_ref() != plan.capability_profile.as_ref()
        {
            return Err(VmError::HostError(
                "host binding plan belongs to a different capability profile".to_string(),
            ));
        }
        if !Arc::ptr_eq(&self.registry_state, &plan.registry_state) {
            return Err(VmError::HostError(
                "host binding plan belongs to a different registry state".to_string(),
            ));
        }
        if !Arc::ptr_eq(
            &self.registry_generation_token,
            &plan.registry_generation_token,
        ) || self.registry_generation.load(Ordering::Relaxed) != plan.registry_generation
        {
            return Err(VmError::HostError(
                "host binding plan is stale for this registry".to_string(),
            ));
        }
        if !vm.host.host_functions.is_empty() || !vm.host.host_function_symbols.is_empty() {
            return Err(VmError::HostError(
                "host binding cache requires an unbound vm".to_string(),
            ));
        }

        vm.host.host_functions.reserve(plan.registry_slots.len());
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
                RegistryEntryKind::StackFactory(factory) => {
                    vm.register_stack_function(factory());
                }
                RegistryEntryKind::StackStatic(function) => {
                    vm.register_static_stack_function(*function);
                }
                RegistryEntryKind::ArgsFactory(factory) => {
                    vm.register_args_function(factory());
                }
                RegistryEntryKind::ArgsStatic(function) => {
                    vm.register_static_args_function(*function);
                }
                RegistryEntryKind::ArgsStaticNonYielding(function) => {
                    vm.register_static_non_yielding_args_function(*function);
                }
            }
        }
        vm.set_default_host_fallback_enabled(false);
        vm.host.allowed_builtin_calls = plan.allowed_builtin_calls.clone();
        vm.host.allow_default_builtin_capabilities = plan.allow_default_builtin_capabilities;
        vm.host.allowed_host_function_slots = plan.allowed_host_function_slots.clone();
        vm.host.allow_default_host_capabilities = plan.allow_default_host_capabilities;
        vm.install_resolved_calls(plan.resolved_calls.clone())?;
        Ok(())
    }
}

pub(super) enum VmHostFunction {
    Dynamic(Box<dyn HostFunction>),
    Static(StaticHostFunction),
    StackDynamic(Box<dyn HostStackFunction>),
    StackStatic(StaticHostStackFunction),
    ArgsDynamic(Box<dyn HostArgsFunction>),
    ArgsStatic(StaticHostArgsFunction),
    ArgsStaticNonYielding(StaticHostArgsFunction),
}

pub(super) enum HostCallExecOutcome {
    Returned,
    Halted,
    Yielded,
    Pending(HostOpId),
}

pub(crate) fn require_non_yielding_host_value(outcome: CallOutcome) -> VmResult<Value> {
    match outcome {
        CallOutcome::Return(CallReturn::One(value)) => Ok(value),
        CallOutcome::Return(CallReturn::None) => Err(VmError::HostError(
            "non-yielding host function returned no value".to_string(),
        )),
        CallOutcome::Halt => Err(VmError::HostError(
            "non-yielding host function returned halt".to_string(),
        )),
        CallOutcome::Yield => Err(VmError::HostError(
            "non-yielding host function returned yield".to_string(),
        )),
        CallOutcome::Pending(_) => Err(VmError::HostError(
            "non-yielding host function returned pending".to_string(),
        )),
    }
}

pub(crate) fn validate_non_yielding_host_value(
    value: Value,
    expected: Option<ValueType>,
) -> VmResult<Value> {
    let valid = matches!(
        (expected, &value),
        (None | Some(ValueType::Unknown), _)
            | (Some(ValueType::Null), Value::Null)
            | (Some(ValueType::Int), Value::Int(_))
            | (Some(ValueType::Float), Value::Float(_))
            | (Some(ValueType::Bool), Value::Bool(_))
            | (Some(ValueType::String), Value::String(_))
            | (Some(ValueType::Bytes), Value::Bytes(_))
            | (Some(ValueType::Array), Value::Array(_))
            | (Some(ValueType::Map), Value::Map(_))
            | (Some(ValueType::Callable), Value::Callable(_))
    );
    if valid {
        return Ok(value);
    }
    let expected = match expected.expect("known expected host return type") {
        ValueType::Unknown => unreachable!(),
        ValueType::Null => "null",
        ValueType::Int => "int",
        ValueType::Float => "float",
        ValueType::Bool => "bool",
        ValueType::String => "string",
        ValueType::Bytes => "bytes",
        ValueType::Array => "array",
        ValueType::Map => "map",
        ValueType::Callable => "callable",
    };
    Err(VmError::TypeMismatch(expected))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WaitingHostOp {
    pub(super) op_id: HostOpId,
    pub(super) source: WaitingHostOpSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaitingHostOpSource {
    HostBridge,
    BuiltinIo,
    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    BuiltinSqlite,
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

#[inline]
fn builtin_for_binding_name(name: &str) -> Option<BuiltinFunction> {
    if !name.contains("::") {
        return None;
    }
    BuiltinFunction::from_namespaced_name(name)
}

/// Maps a pending builtin to the waiting-op source that polls its concrete
/// driver. IO builtins are driven through the builtin IO mailbox; SQLite
/// builtins through their own completion mailbox. Both poll the shared
/// execution-scope operation registry; only the result-mailbox lookup
/// differs.
#[cfg_attr(
    not(all(feature = "sqlite", not(target_arch = "wasm32"))),
    allow(unused_variables)
)]
fn builtin_waiting_source(builtin: BuiltinFunction) -> WaitingHostOpSource {
    #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
    {
        // The generated `BuiltinFunction::name()` renders the source name with
        // `::` collapsed to `_` (e.g. `sqlite_execute`), matching the internal
        // catalog name rather than the guest-facing `sqlite::execute`.
        if builtin.name().starts_with("sqlite_") {
            return WaitingHostOpSource::BuiltinSqlite;
        }
    }
    WaitingHostOpSource::BuiltinIo
}

impl Vm {
    pub fn register_function(&mut self, function: Box<dyn HostFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::Dynamic(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_function(&mut self, function: StaticHostFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::Static(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_stack_function(&mut self, function: Box<dyn HostStackFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::StackDynamic(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_stack_function(&mut self, function: StaticHostStackFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::StackStatic(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_args_function(&mut self, function: Box<dyn HostArgsFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsDynamic(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_args_function(&mut self, function: StaticHostArgsFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsStatic(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    /// Registers a static args-only host function that always returns one value synchronously.
    ///
    /// When used to resolve a declared host import, the returned [`Value`] must match that
    /// import's return type. Returning a different type is reported as
    /// [`VmError::TypeMismatch`]. Returning no value, `Halt`, `Yield`, or `Pending` violates the
    /// contract and is a host error.
    pub fn register_static_non_yielding_args_function(
        &mut self,
        function: StaticHostArgsFunction,
    ) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsStaticNonYielding(function));
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn bind_function(&mut self, name: impl Into<String>, function: Box<dyn HostFunction>) {
        let name = name.into();
        if let Some(builtin) = builtin_for_binding_name(&name) {
            self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Dynamic(function));
            return;
        }
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::Dynamic(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
    }

    pub fn bind_static_function(&mut self, name: impl Into<String>, function: StaticHostFunction) {
        let name = name.into();
        if let Some(builtin) = builtin_for_binding_name(&name) {
            self.bind_builtin_overrideslot(builtin.call_index(), VmHostFunction::Static(function));
            return;
        }
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::Static(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_static_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
    }

    pub fn bind_stack_function(
        &mut self,
        name: impl Into<String>,
        function: Box<dyn HostStackFunction>,
    ) {
        let name = name.into();
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::StackDynamic(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_stack_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
    }

    pub fn bind_static_stack_function(
        &mut self,
        name: impl Into<String>,
        function: StaticHostStackFunction,
    ) {
        let name = name.into();
        if let Some(builtin) = builtin_for_binding_name(&name) {
            self.bind_builtin_overrideslot(
                builtin.call_index(),
                VmHostFunction::StackStatic(function),
            );
            return;
        }
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::StackStatic(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_static_stack_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
    }

    pub fn bind_args_function(
        &mut self,
        name: impl Into<String>,
        function: Box<dyn HostArgsFunction>,
    ) {
        let name = name.into();
        if let Some(builtin) = builtin_for_binding_name(&name) {
            self.bind_builtin_overrideslot(
                builtin.call_index(),
                VmHostFunction::ArgsDynamic(function),
            );
            return;
        }
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::ArgsDynamic(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_args_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
    }

    pub fn bind_static_args_function(
        &mut self,
        name: impl Into<String>,
        function: StaticHostArgsFunction,
    ) {
        let name = name.into();
        if let Some(builtin) = builtin_for_binding_name(&name) {
            self.bind_builtin_overrideslot(
                builtin.call_index(),
                VmHostFunction::ArgsStatic(function),
            );
            return;
        }
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::ArgsStatic(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_static_args_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
    }

    /// Binds a static args-only host function that always returns one value synchronously.
    ///
    /// This is equivalent to [`Vm::bind_static_args_function`] except that the VM may keep
    /// native JIT traces active across the call boundary. The returned [`Value`] must match the
    /// return type declared by the corresponding host import. Returning a different type, no
    /// value, `Halt`, `Yield`, or `Pending` violates the contract and is reported as a host error.
    pub fn bind_static_non_yielding_args_function(
        &mut self,
        name: impl Into<String>,
        function: StaticHostArgsFunction,
    ) {
        let name = name.into();
        if let Some(builtin) = builtin_for_binding_name(&name) {
            self.bind_builtin_overrideslot(
                builtin.call_index(),
                VmHostFunction::ArgsStaticNonYielding(function),
            );
            return;
        }
        if let Some(&index) = self.host.host_function_symbols.get(&name)
            && let Some(slot) = self.host.host_functions.get_mut(index as usize)
        {
            *slot = VmHostFunction::ArgsStaticNonYielding(function);
            self.host.resolved_calls_dirty = true;
            return;
        }

        let index = self.register_static_non_yielding_args_function(function);
        self.host.host_function_symbols.insert(name, index);
        self.host.resolved_calls_dirty = true;
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
        if let Some(&host_slot) = self.host.builtin_overrides.get(&builtin_call_index)
            && let Some(slot) = self.host.host_functions.get_mut(host_slot as usize)
        {
            *slot = function;
            return;
        }

        let host_slot = self.host.host_functions.len() as u16;
        self.host.host_functions.push(function);
        self.host
            .builtin_overrides
            .insert(builtin_call_index, host_slot);
    }

    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) {
        self.cancel_waiting_host_op();
        self.host.async_bridge = Some(bridge);
    }

    pub fn clear_async_bridge(&mut self) {
        self.cancel_waiting_host_op();
        self.host.async_bridge = None;
    }

    pub fn set_runtime_print_sink<F>(&mut self, sink: F)
    where
        F: FnMut(String) + Send + 'static,
    {
        self.host.runtime_print_sink = Some(Box::new(sink));
    }

    pub fn clear_runtime_print_sink(&mut self) {
        self.host.runtime_print_sink = None;
    }

    pub(crate) fn write_runtime_print(&mut self, rendered: String) -> VmResult<()> {
        let Some(sink) = self.host.runtime_print_sink.as_mut() else {
            return Err(VmError::HostError(
                "runtime print sink is not configured".to_string(),
            ));
        };
        sink(rendered);
        Ok(())
    }

    /// Enables or disables implicit binding of built-in host functions.
    ///
    /// Disabling this makes the VM use only explicitly registered host
    /// functions. The default remains enabled for backwards compatibility
    /// until a registry is bound.
    pub fn set_default_host_fallback_enabled(&mut self, enabled: bool) {
        self.host.allow_default_host_fallback = enabled;
        self.host.resolved_calls_dirty = true;
    }

    /// Whether unbound host imports fall back to the default host functions.
    pub fn default_host_fallback_enabled(&self) -> bool {
        self.host.allow_default_host_fallback
    }

    pub fn allocate_host_op_id(&mut self) -> HostOpId {
        let op_id = self.host.next_host_op_id;
        self.host.next_host_op_id = self.host.next_host_op_id.wrapping_add(1).max(1);
        op_id
    }

    pub fn waiting_host_op_id(&self) -> Option<HostOpId> {
        self.instance.waiting_host_op.map(|op| op.op_id)
    }

    pub(super) fn cancel_waiting_host_op(&mut self) {
        let Some(waiting) = self.instance.waiting_host_op.take() else {
            return;
        };
        match waiting.source {
            WaitingHostOpSource::HostBridge => {
                self.host.submitted_host_ops.remove(&waiting.op_id);
                if let Some(bridge) = self.host.async_bridge.as_mut() {
                    bridge.cancel_op(waiting.op_id);
                }
            }
            WaitingHostOpSource::BuiltinIo => {
                crate::builtins::runtime::cancel_builtin_io_op(self, waiting.op_id);
            }
            #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
            WaitingHostOpSource::BuiltinSqlite => {
                crate::builtins::runtime::cancel_builtin_sqlite_op(self, waiting.op_id);
            }
        }
    }

    pub fn complete_host_op(
        &mut self,
        op_id: HostOpId,
        values: impl Into<CallReturn>,
    ) -> VmResult<()> {
        self.complete_waiting_host_op(op_id, values.into())
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.instance.waiting_host_op else {
            return Poll::Ready(Ok(()));
        };

        // The HostBridge arm produces a `HostFutureOutput` (so a submitted
        // future's completion closure can run against the VM); the runtime
        // builtin arms produce an already-finished `CallReturn`.
        let poll_result: Poll<VmResult<HostFutureOutput>> = match waiting.source {
            WaitingHostOpSource::HostBridge => {
                let bridge_ptr = match self.host.async_bridge.as_mut() {
                    Some(bridge) => bridge.as_mut() as *mut dyn HostAsyncBridge,
                    None => {
                        return Poll::Ready(Err(VmError::HostError(format!(
                            "vm waiting on host op {} without an async bridge",
                            waiting.op_id
                        ))));
                    }
                };
                let submitted = self.host.submitted_host_ops.contains(&waiting.op_id);
                // SAFETY: `bridge_ptr` was derived from the unique mutable borrow of
                // `self.host.async_bridge` above. The bridge methods receive only the
                // pointer's `&mut` reborrow, not `self`, so they cannot move or replace
                // the owning `Box`; the pointer is used only for this synchronous call.
                unsafe {
                    if submitted {
                        (&mut *bridge_ptr).poll_submitted_op(waiting.op_id, cx)
                    } else {
                        (&mut *bridge_ptr)
                            .poll_op(waiting.op_id, cx)
                            .map(|result| result.map(HostFutureOutput::Return))
                    }
                }
            }
            WaitingHostOpSource::BuiltinIo => {
                crate::builtins::runtime::poll_builtin_io_op(self, waiting.op_id, cx)
                    .map(|result| result.map(HostFutureOutput::Return))
            }
            #[cfg(all(feature = "sqlite", not(target_arch = "wasm32")))]
            WaitingHostOpSource::BuiltinSqlite => {
                crate::builtins::runtime::poll_builtin_sqlite_op(self, waiting.op_id, cx)
                    .map(|result| result.map(HostFutureOutput::Return))
            }
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(output)) => {
                let host_bridge_owned = self.host.submitted_host_ops.contains(&waiting.op_id);
                let values = match output.finish(self) {
                    Ok(values) => values,
                    Err(err) => {
                        if host_bridge_owned {
                            self.host.submitted_host_ops.remove(&waiting.op_id);
                        }
                        self.instance.waiting_host_op = None;
                        return Poll::Ready(Err(err));
                    }
                };
                if host_bridge_owned {
                    self.host.submitted_host_ops.remove(&waiting.op_id);
                    if let Some(bridge) = self.host.async_bridge.as_mut() {
                        bridge.cancel_op(waiting.op_id);
                    }
                }
                self.complete_waiting_host_op(waiting.op_id, values)?;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                if self.host.submitted_host_ops.remove(&waiting.op_id)
                    && let Some(bridge) = self.host.async_bridge.as_mut()
                {
                    bridge.cancel_op(waiting.op_id);
                }
                self.instance.waiting_host_op = None;
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

    pub(super) fn execute_host_call(
        &mut self,
        index: u16,
        argc_u8: u8,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let argc = argc_u8 as usize;
        if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            if builtin.requires_explicit_host_capability()
                && !self.host.allow_default_builtin_capabilities
                && !self.host.allowed_builtin_calls.contains(&index)
            {
                return Err(VmError::UnboundImport(builtin.name().to_string()));
            }
            if !builtin.accepts_arity(argc_u8) {
                return Err(VmError::InvalidCallArity {
                    import: builtin.name().to_string(),
                    expected: builtin.arity(),
                    got: argc_u8,
                });
            }
            if self.host.builtin_overrides.contains_key(&index) {
                return self.execute_builtin_override_call(index, argc_u8, call_ip);
            }
            if let Some(outcome) =
                self.try_execute_typed_builtin_fast_path(builtin, argc, call_ip)?
            {
                return Ok(outcome);
            }
            if let Some(outcome) = self.try_execute_builtin_projection_fast_path(builtin, argc)? {
                return Ok(outcome);
            }
            self.record_generic_builtin_call();
            return self.execute_builtin_call_from_stack(builtin, argc, call_ip);
        }

        let expected_return_type = self
            .program
            .imports
            .get(usize::from(index))
            .map(|import| import.return_type);
        let resolved_index = self.resolve_call_target(index, argc_u8)?;
        if !self.host.allow_default_host_capabilities
            && !self
                .host
                .allowed_host_function_slots
                .contains(&resolved_index)
        {
            let import_name = self
                .program
                .imports
                .get(usize::from(index))
                .map(|import| import.name.clone())
                .unwrap_or_else(|| format!("host slot {resolved_index}"));
            return Err(VmError::UnboundImport(import_name));
        }
        if let Some(function) = self
            .host
            .host_functions
            .get(resolved_index as usize)
            .and_then(|function| match function {
                VmHostFunction::ArgsStaticNonYielding(function) => Some(*function),
                _ => None,
            })
        {
            return self.execute_static_non_yielding_args_host_function(
                function,
                argc,
                expected_return_type,
            );
        }
        if self.bound_host_function_uses_args_slice(resolved_index)? {
            self.execute_bound_args_host_function(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
            )
        } else if self.bound_host_function_uses_stack_borrow(resolved_index)? {
            self.execute_bound_stack_host_function(resolved_index, argc, call_ip)
        } else {
            self.execute_bound_host_function_from_stack(resolved_index, argc, call_ip)
        }
    }

    pub(super) fn execute_builtin_override_call(
        &mut self,
        builtin_call_index: u16,
        argc_u8: u8,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let resolved_index = self
            .host
            .builtin_overrides
            .get(&builtin_call_index)
            .copied()
            .ok_or_else(|| {
                VmError::HostError(format!(
                    "missing builtin override slot for call index {builtin_call_index}"
                ))
            })?;
        let argc = argc_u8 as usize;
        if self.bound_host_function_uses_args_slice(resolved_index)? {
            self.execute_bound_args_host_function(resolved_index, argc, call_ip, None)
        } else if self.bound_host_function_uses_stack_borrow(resolved_index)? {
            self.execute_bound_stack_host_function(resolved_index, argc, call_ip)
        } else {
            self.execute_bound_host_function_from_stack(resolved_index, argc, call_ip)
        }
    }

    fn execute_builtin_call_from_stack(
        &mut self,
        builtin: BuiltinFunction,
        argc: usize,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        // Builtin dispatch reads arguments from the current stack tail while mutating the VM.
        // The builtin runtime must not mutate `self.instance.stack` until this borrowed slice is consumed.
        let outcome = unsafe {
            let args = std::slice::from_raw_parts_mut(
                self.instance.stack.as_mut_ptr().add(arg_start),
                argc,
            );
            crate::builtins::runtime::execute_builtin_call(self, builtin, args)
        }?;

        match outcome {
            crate::builtins::runtime::BuiltinCallOutcome::Return(values) => {
                self.instance.stack.truncate(arg_start);
                values.push_onto_stack(&mut self.instance.stack);
                Ok(HostCallExecOutcome::Returned)
            }
            crate::builtins::runtime::BuiltinCallOutcome::Halt => {
                self.instance.stack.truncate(arg_start);
                Ok(HostCallExecOutcome::Halted)
            }
            crate::builtins::runtime::BuiltinCallOutcome::Pending(op_id) => {
                self.instance.stack.truncate(arg_start);
                let resume_ip = self.call_resume_ip(call_ip)?;
                if self.host.submitted_host_ops.contains(&op_id) {
                    if let Err(error) =
                        self.set_waiting_host_op(op_id, WaitingHostOpSource::HostBridge)
                    {
                        self.host.submitted_host_ops.remove(&op_id);
                        if let Some(bridge) = self.host.async_bridge.as_mut() {
                            bridge.cancel_op(op_id);
                        }
                        return Err(error);
                    }
                } else {
                    let source = builtin_waiting_source(builtin);
                    self.set_waiting_host_op(op_id, source)?;
                }
                self.instance.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
    }

    fn try_execute_typed_builtin_fast_path(
        &mut self,
        builtin: BuiltinFunction,
        argc: usize,
        call_ip: usize,
    ) -> VmResult<Option<HostCallExecOutcome>> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        let (lhs, rhs) = self.operand_value_types(call_ip);
        let result = {
            let args = &self.instance.stack[arg_start..];
            match builtin {
                BuiltinFunction::Len => match (lhs, args) {
                    (
                        ValueType::String | ValueType::Bytes | ValueType::Array | ValueType::Map,
                        [value],
                    ) => Self::fast_path_len_result(value),
                    _ => None,
                },
                BuiltinFunction::Slice => match (lhs, rhs, args) {
                    (ValueType::String, ValueType::Int, [source, start, length]) => {
                        Some(Self::fast_path_slice_string_result(source, start, length)?)
                    }
                    (ValueType::Array, ValueType::Int, [source, start, length]) => {
                        Some(Self::fast_path_slice_array_result(source, start, length)?)
                    }
                    (ValueType::Bytes, ValueType::Int, [source, start, length]) => {
                        Some(Self::fast_path_slice_bytes_result(source, start, length)?)
                    }
                    _ => None,
                },
                BuiltinFunction::Get => match (lhs, args) {
                    (
                        ValueType::String | ValueType::Bytes | ValueType::Array | ValueType::Map,
                        [container, key],
                    ) => Self::fast_path_get_result(container, key)?,
                    _ => None,
                },
                BuiltinFunction::Has => match (lhs, args) {
                    (ValueType::Bytes | ValueType::Array | ValueType::Map, [container, key]) => {
                        Self::fast_path_has_result(container, key)?
                    }
                    _ => None,
                },
                BuiltinFunction::StringContains => match (lhs, rhs, args) {
                    (ValueType::String, ValueType::String, [text, needle]) => {
                        Self::fast_path_string_contains_result(text, needle)
                    }
                    _ => None,
                },
                BuiltinFunction::StringReplaceLiteral => match (lhs, rhs, args) {
                    (ValueType::String, ValueType::String, [text, needle, replacement]) => {
                        Self::fast_path_string_replace_literal_result(text, needle, replacement)
                    }
                    _ => None,
                },
                BuiltinFunction::StringLowerAscii => match (lhs, args) {
                    (ValueType::String, [text]) => Self::fast_path_string_lower_ascii_result(text),
                    _ => None,
                },
                BuiltinFunction::BytesFromArrayU8 => match (lhs, args) {
                    (ValueType::Array, [value]) => {
                        Some(Self::fast_path_bytes_from_array_u8_result(value)?)
                    }
                    _ => None,
                },
                BuiltinFunction::BytesToArrayU8 => match (lhs, args) {
                    (ValueType::Bytes, [value]) => {
                        Some(Self::fast_path_bytes_to_array_u8_result(value)?)
                    }
                    _ => None,
                },
                _ => None,
            }
        };
        let Some(value) = result else {
            return Ok(None);
        };
        self.instance.stack.truncate(arg_start);
        self.instance.stack.push(value);
        self.record_typed_builtin_fast_path();
        Ok(Some(HostCallExecOutcome::Returned))
    }

    fn try_execute_builtin_projection_fast_path(
        &mut self,
        builtin: BuiltinFunction,
        argc: usize,
    ) -> VmResult<Option<HostCallExecOutcome>> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        let result = {
            let args = &self.instance.stack[arg_start..];
            match (builtin, args) {
                (BuiltinFunction::Len, [value]) => Self::fast_path_len_result(value),
                (BuiltinFunction::Get, [container, key]) => {
                    Self::fast_path_get_result(container, key)?
                }
                (BuiltinFunction::Has, [container, key]) => {
                    Self::fast_path_has_result(container, key)?
                }
                _ => None,
            }
        };
        let Some(value) = result else {
            return Ok(None);
        };
        self.instance.stack.truncate(arg_start);
        self.instance.stack.push(value);
        self.record_projection_fast_path();
        Ok(Some(HostCallExecOutcome::Returned))
    }

    fn fast_path_len_result(value: &Value) -> Option<Value> {
        match value {
            Value::String(text) => Some(Value::Int(text.chars().count() as i64)),
            Value::Bytes(values) => Some(Value::Int(values.len() as i64)),
            Value::Array(values) => Some(Value::Int(values.len() as i64)),
            Value::Map(entries) => Some(Value::Int(entries.len() as i64)),
            _ => None,
        }
    }

    fn fast_path_string_contains_result(text: &Value, needle: &Value) -> Option<Value> {
        let (Value::String(text), Value::String(needle)) = (text, needle) else {
            return None;
        };
        Some(Value::Bool(
            crate::builtins::runtime::core::builtin_string_contains_impl(
                text.as_str(),
                needle.as_str(),
            ),
        ))
    }

    fn fast_path_string_replace_literal_result(
        text: &Value,
        needle: &Value,
        replacement: &Value,
    ) -> Option<Value> {
        let (Value::String(text), Value::String(needle), Value::String(replacement)) =
            (text, needle, replacement)
        else {
            return None;
        };
        Some(Value::string(
            crate::builtins::runtime::core::builtin_string_replace_literal_impl(
                text.as_str(),
                needle.as_str(),
                replacement.as_str(),
            ),
        ))
    }

    fn fast_path_string_lower_ascii_result(text: &Value) -> Option<Value> {
        let Value::String(text) = text else {
            return None;
        };
        Some(Value::string(
            crate::builtins::runtime::core::builtin_string_lower_ascii_impl(text.as_str()),
        ))
    }

    fn fast_path_get_result(container: &Value, key: &Value) -> VmResult<Option<Value>> {
        match container {
            Value::Array(values) => {
                let index = key.as_int()?;
                if index < 0 {
                    return Err(VmError::HostError(
                        "array index must be non-negative".to_string(),
                    ));
                }
                let index = usize::try_from(index)
                    .map_err(|_| VmError::HostError("array index overflow".to_string()))?;
                let value = values.get(index).cloned().ok_or_else(|| {
                    VmError::HostError(format!("array index {index} out of bounds"))
                })?;
                Ok(Some(value))
            }
            Value::Map(entries) => {
                let value = entries
                    .get(key)
                    .cloned()
                    .ok_or_else(|| VmError::HostError("map key not found".to_string()))?;
                Ok(Some(value))
            }
            Value::Bytes(values) => {
                let index = key.as_int()?;
                if index < 0 {
                    return Err(VmError::HostError(
                        "bytes index must be non-negative".to_string(),
                    ));
                }
                let index = usize::try_from(index)
                    .map_err(|_| VmError::HostError("bytes index overflow".to_string()))?;
                let value = values.get(index).copied().ok_or_else(|| {
                    VmError::HostError(format!("bytes index {index} out of bounds"))
                })?;
                Ok(Some(Value::Int(i64::from(value))))
            }
            Value::String(text) => {
                let index = key.as_int()?;
                if index < 0 {
                    return Err(VmError::HostError(
                        "string index must be non-negative".to_string(),
                    ));
                }
                let index = usize::try_from(index)
                    .map_err(|_| VmError::HostError("string index overflow".to_string()))?;
                let value = text
                    .chars()
                    .nth(index)
                    .map(|ch| Value::string(ch.to_string()))
                    .ok_or_else(|| {
                        VmError::HostError(format!("string index {index} out of bounds"))
                    })?;
                Ok(Some(value))
            }
            _ => Ok(None),
        }
    }

    fn fast_path_has_result(container: &Value, key: &Value) -> VmResult<Option<Value>> {
        match container {
            Value::Array(values) => {
                let index = key.as_int()?;
                let present = if index < 0 {
                    false
                } else {
                    usize::try_from(index)
                        .ok()
                        .is_some_and(|index| index < values.len())
                };
                Ok(Some(Value::Bool(present)))
            }
            Value::Bytes(values) => {
                let index = key.as_int()?;
                let present = if index < 0 {
                    false
                } else {
                    usize::try_from(index)
                        .ok()
                        .is_some_and(|index| index < values.len())
                };
                Ok(Some(Value::Bool(present)))
            }
            Value::Map(entries) => Ok(Some(Value::Bool(entries.get(key).is_some()))),
            _ => Ok(None),
        }
    }

    fn fast_path_slice_bounds(start: i64, length: i64) -> VmResult<Option<(usize, usize)>> {
        if start < 0 || length <= 0 {
            return Ok(None);
        }
        let start = usize::try_from(start).map_err(|_| {
            VmError::HostError("slice start overflow while converting to usize".to_string())
        })?;
        let length = usize::try_from(length).map_err(|_| {
            VmError::HostError("slice length overflow while converting to usize".to_string())
        })?;
        Ok(Some((start, length)))
    }

    fn fast_path_slice_string_result(
        source: &Value,
        start: &Value,
        length: &Value,
    ) -> VmResult<Value> {
        let Value::String(text) = source else {
            return Err(VmError::TypeMismatch("string"));
        };
        let start = start.as_int()?;
        let length = length.as_int()?;
        let Some((start, length)) = Self::fast_path_slice_bounds(start, length)? else {
            return Ok(Value::string(String::new()));
        };
        Ok(Value::string(
            text.chars().skip(start).take(length).collect::<String>(),
        ))
    }

    fn fast_path_slice_array_result(
        source: &Value,
        start: &Value,
        length: &Value,
    ) -> VmResult<Value> {
        let Value::Array(values) = source else {
            return Err(VmError::TypeMismatch("array"));
        };
        let start = start.as_int()?;
        let length = length.as_int()?;
        let Some((start, length)) = Self::fast_path_slice_bounds(start, length)? else {
            return Ok(Value::array(Vec::new()));
        };
        Ok(Value::array(
            values
                .iter()
                .skip(start)
                .take(length)
                .cloned()
                .collect::<Vec<_>>(),
        ))
    }

    fn fast_path_slice_bytes_result(
        source: &Value,
        start: &Value,
        length: &Value,
    ) -> VmResult<Value> {
        let Value::Bytes(values) = source else {
            return Err(VmError::TypeMismatch("bytes"));
        };
        let start = start.as_int()?;
        let length = length.as_int()?;
        let Some((start, length)) = Self::fast_path_slice_bounds(start, length)? else {
            return Ok(Value::bytes(Vec::new()));
        };
        Ok(Value::bytes(
            values
                .iter()
                .skip(start)
                .take(length)
                .copied()
                .collect::<Vec<_>>(),
        ))
    }

    fn fast_path_bytes_from_array_u8_result(value: &Value) -> VmResult<Value> {
        let Value::Array(values) = value else {
            return Err(VmError::TypeMismatch("array"));
        };
        let mut out = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            let Value::Int(value) = value else {
                return Err(VmError::HostError(format!(
                    "bytes::from_array_u8 entry {index} must be an int in 0..=255"
                )));
            };
            let value = u8::try_from(*value).map_err(|_| {
                VmError::HostError(format!(
                    "bytes::from_array_u8 entry {index} must be an int in 0..=255"
                ))
            })?;
            out.push(value);
        }
        Ok(Value::bytes(out))
    }

    fn fast_path_bytes_to_array_u8_result(value: &Value) -> VmResult<Value> {
        let Value::Bytes(payload) = value else {
            return Err(VmError::TypeMismatch("bytes"));
        };
        Ok(Value::array(
            payload
                .iter()
                .copied()
                .map(|byte| Value::Int(i64::from(byte)))
                .collect(),
        ))
    }

    pub(super) fn execute_bound_host_function_from_stack(
        &mut self,
        resolved_index: u16,
        argc: usize,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        let mut saved_stack = std::mem::take(&mut self.instance.stack);
        self.instance.call_depth += 1;
        let function_ptr =
            self.host
                .host_functions
                .get_mut(resolved_index as usize)
                .ok_or(VmError::InvalidCall(resolved_index))? as *mut VmHostFunction;
        let outcome = unsafe {
            let args = &saved_stack[arg_start..];
            match &mut *function_ptr {
                VmHostFunction::Dynamic(function) => function.call(self, args),
                VmHostFunction::Static(function) => function(self, args),
                VmHostFunction::StackDynamic(_)
                | VmHostFunction::StackStatic(_)
                | VmHostFunction::ArgsDynamic(_)
                | VmHostFunction::ArgsStatic(_)
                | VmHostFunction::ArgsStaticNonYielding(_) => unreachable!(),
            }
        };
        self.instance.call_depth = self.instance.call_depth.saturating_sub(1);

        let mut host_stack = std::mem::take(&mut self.instance.stack);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                saved_stack.truncate(arg_start);
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                return Err(err);
            }
        };

        match outcome {
            CallOutcome::Return(values) => {
                saved_stack.truncate(arg_start);
                saved_stack.append(&mut host_stack);
                values.push_onto_stack(&mut saved_stack);
                self.instance.stack = saved_stack;
                Ok(HostCallExecOutcome::Returned)
            }
            CallOutcome::Halt => {
                saved_stack.truncate(arg_start);
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                Ok(HostCallExecOutcome::Halted)
            }
            CallOutcome::Yield => {
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                self.instance.ip = call_ip;
                Ok(HostCallExecOutcome::Yielded)
            }
            CallOutcome::Pending(op_id) => {
                saved_stack.truncate(arg_start);
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                let resume_ip = self.call_resume_ip(call_ip)?;
                self.set_waiting_host_op(op_id, WaitingHostOpSource::HostBridge)?;
                self.instance.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
    }

    fn bound_host_function_uses_args_slice(&self, resolved_index: u16) -> VmResult<bool> {
        let function = self
            .host
            .host_functions
            .get(resolved_index as usize)
            .ok_or(VmError::InvalidCall(resolved_index))?;
        Ok(matches!(
            function,
            VmHostFunction::ArgsDynamic(_)
                | VmHostFunction::ArgsStatic(_)
                | VmHostFunction::ArgsStaticNonYielding(_)
        ))
    }

    fn bound_host_function_uses_stack_borrow(&self, resolved_index: u16) -> VmResult<bool> {
        let function = self
            .host
            .host_functions
            .get(resolved_index as usize)
            .ok_or(VmError::InvalidCall(resolved_index))?;
        Ok(matches!(
            function,
            VmHostFunction::StackDynamic(_) | VmHostFunction::StackStatic(_)
        ))
    }

    #[inline(always)]
    fn execute_static_non_yielding_args_host_function(
        &mut self,
        function: StaticHostArgsFunction,
        argc: usize,
        expected_return_type: Option<ValueType>,
    ) -> VmResult<HostCallExecOutcome> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        self.instance.call_depth += 1;
        let outcome = function(&self.instance.stack[arg_start..]);
        self.instance.call_depth = self.instance.call_depth.saturating_sub(1);
        let value = require_non_yielding_host_value(outcome?)?;
        let value = validate_non_yielding_host_value(value, expected_return_type)?;
        self.instance.stack.truncate(arg_start);
        self.instance.stack.push(value);
        Ok(HostCallExecOutcome::Returned)
    }

    pub(super) fn execute_bound_args_host_function(
        &mut self,
        resolved_index: u16,
        argc: usize,
        call_ip: usize,
        expected_return_type: Option<ValueType>,
    ) -> VmResult<HostCallExecOutcome> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        self.instance.call_depth += 1;
        let outcome = {
            let args = &self.instance.stack[arg_start..];
            let function = self
                .host
                .host_functions
                .get_mut(resolved_index as usize)
                .ok_or(VmError::InvalidCall(resolved_index))?;
            match function {
                VmHostFunction::ArgsDynamic(function) => (function.call(args), false),
                VmHostFunction::ArgsStatic(function) => (function(args), false),
                VmHostFunction::ArgsStaticNonYielding(function) => (function(args), true),
                VmHostFunction::Dynamic(_)
                | VmHostFunction::Static(_)
                | VmHostFunction::StackDynamic(_)
                | VmHostFunction::StackStatic(_) => unreachable!(),
            }
        };
        self.instance.call_depth = self.instance.call_depth.saturating_sub(1);
        let (outcome, non_yielding) = outcome;
        let outcome = outcome?;
        if non_yielding {
            let value = require_non_yielding_host_value(outcome)?;
            let value = validate_non_yielding_host_value(value, expected_return_type)?;
            self.instance.stack.truncate(arg_start);
            self.instance.stack.push(value);
            return Ok(HostCallExecOutcome::Returned);
        }

        match outcome {
            CallOutcome::Return(values) => {
                self.instance.stack.truncate(arg_start);
                values.push_onto_stack(&mut self.instance.stack);
                Ok(HostCallExecOutcome::Returned)
            }
            CallOutcome::Halt => {
                self.instance.stack.truncate(arg_start);
                Ok(HostCallExecOutcome::Halted)
            }
            CallOutcome::Yield => {
                self.instance.ip = call_ip;
                Ok(HostCallExecOutcome::Yielded)
            }
            CallOutcome::Pending(op_id) => {
                self.instance.stack.truncate(arg_start);
                let resume_ip = self.call_resume_ip(call_ip)?;
                self.set_waiting_host_op(op_id, WaitingHostOpSource::HostBridge)?;
                self.instance.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
    }

    pub(super) fn execute_bound_stack_host_function(
        &mut self,
        resolved_index: u16,
        argc: usize,
        call_ip: usize,
    ) -> VmResult<HostCallExecOutcome> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        self.instance.call_depth += 1;
        let function_ptr =
            self.host
                .host_functions
                .get_mut(resolved_index as usize)
                .ok_or(VmError::InvalidCall(resolved_index))? as *mut VmHostFunction;
        // Stack-borrowed host functions opt into the same raw stack-tail borrowing model used
        // by builtin dispatch. They must not re-enter the VM or otherwise mutate `self.instance.stack`
        // while the borrowed slice is alive.
        let outcome = unsafe {
            let args =
                std::slice::from_raw_parts(self.instance.stack.as_ptr().add(arg_start), argc);
            match &mut *function_ptr {
                VmHostFunction::StackDynamic(function) => function.call(self, args),
                VmHostFunction::StackStatic(function) => function(self, args),
                VmHostFunction::Dynamic(_)
                | VmHostFunction::Static(_)
                | VmHostFunction::ArgsDynamic(_)
                | VmHostFunction::ArgsStatic(_)
                | VmHostFunction::ArgsStaticNonYielding(_) => unreachable!(),
            }
        };
        self.instance.call_depth = self.instance.call_depth.saturating_sub(1);
        let outcome = outcome?;

        match outcome {
            CallOutcome::Return(values) => {
                self.instance.stack.truncate(arg_start);
                values.push_onto_stack(&mut self.instance.stack);
                Ok(HostCallExecOutcome::Returned)
            }
            CallOutcome::Halt => {
                self.instance.stack.truncate(arg_start);
                Ok(HostCallExecOutcome::Halted)
            }
            CallOutcome::Yield => {
                self.instance.ip = call_ip;
                Ok(HostCallExecOutcome::Yielded)
            }
            CallOutcome::Pending(op_id) => {
                self.instance.stack.truncate(arg_start);
                let resume_ip = self.call_resume_ip(call_ip)?;
                self.set_waiting_host_op(op_id, WaitingHostOpSource::HostBridge)?;
                self.instance.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
    }

    pub(super) fn call_resume_ip(&self, call_ip: usize) -> VmResult<usize> {
        let opcode = self
            .program
            .code
            .get(call_ip)
            .copied()
            .ok_or(VmError::BytecodeBounds)
            .and_then(|raw| OpCode::try_from(raw).map_err(|_| VmError::InvalidOpcode(raw)))?;
        if !matches!(opcode, OpCode::Call | OpCode::CallValue) {
            return Err(VmError::InvalidOpcode(opcode as u8));
        }
        let resume_ip = call_ip
            .checked_add(1 + opcode.operand_len())
            .ok_or(VmError::BytecodeBounds)?;
        if resume_ip > self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        Ok(resume_ip)
    }

    pub(super) fn set_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        source: WaitingHostOpSource,
    ) -> VmResult<()> {
        if let Some(active) = self.instance.waiting_host_op
            && active.op_id != op_id
        {
            return Err(VmError::HostError(format!(
                "vm already waiting on host op {}, cannot wait on {}",
                active.op_id, op_id
            )));
        }
        self.instance.waiting_host_op = Some(WaitingHostOp { op_id, source });
        Ok(())
    }

    pub(super) fn complete_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        values: CallReturn,
    ) -> VmResult<()> {
        let waiting = self.instance.waiting_host_op.ok_or_else(|| {
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
        self.instance.waiting_host_op = None;
        values.push_onto_stack(&mut self.instance.stack);
        Ok(())
    }

    pub(super) fn install_resolved_calls(&mut self, resolved_calls: Vec<u16>) -> VmResult<()> {
        if self.program.imports.len() != resolved_calls.len() {
            return Err(VmError::HostError(format!(
                "resolved call cache size mismatch: expected {}, got {}",
                self.program.imports.len(),
                resolved_calls.len()
            )));
        }
        for &index in &resolved_calls {
            if index as usize >= self.host.host_functions.len() {
                return Err(VmError::InvalidCall(index));
            }
        }
        self.host.resolved_calls = resolved_calls;
        self.host.resolved_calls_dirty = false;
        Ok(())
    }

    pub(super) fn ensure_call_bindings(&mut self) -> VmResult<()> {
        if self.program.imports.is_empty() || !self.host.resolved_calls_dirty {
            return Ok(());
        }

        if self.host.allow_default_host_fallback
            && self.host.host_function_symbols.is_empty()
            && self.host.host_functions.is_empty()
        {
            let import_names = self
                .program
                .imports
                .iter()
                .map(|import| import.name.clone())
                .collect::<Vec<_>>();
            for name in import_names {
                let _ = crate::builtins::runtime::bind_default_host_function(self, &name);
            }
        }

        let use_legacy_order = self.host.host_function_symbols.is_empty();
        let mut resolved = Vec::with_capacity(self.program.imports.len());
        let imports = self.program.imports.clone();
        for (index, import) in imports.iter().enumerate() {
            if use_legacy_order {
                if index >= self.host.host_functions.len() {
                    return Err(VmError::InvalidCall(index as u16));
                }
                resolved.push(index as u16);
                continue;
            }

            let bound =
                if let Some(bound) = self.host.host_function_symbols.get(&import.name).copied() {
                    bound
                } else if self.host.allow_default_host_fallback
                    && crate::builtins::runtime::bind_default_host_function(self, &import.name)
                {
                    self.host
                        .host_function_symbols
                        .get(&import.name)
                        .copied()
                        .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?
                } else {
                    return Err(VmError::UnboundImport(import.name.clone()));
                };
            resolved.push(bound);
        }

        self.host.resolved_calls = resolved;
        self.host.resolved_calls_dirty = false;
        Ok(())
    }

    pub(super) fn sync_jit_non_yielding_host_imports(&mut self) {
        let imports = self
            .host
            .resolved_calls
            .iter()
            .map(|&slot| {
                matches!(
                    self.host.host_functions.get(usize::from(slot)),
                    Some(VmHostFunction::ArgsStaticNonYielding(_))
                )
            })
            .collect();
        if self.engine.jit.set_non_yielding_host_imports(imports) {
            self.engine.native_traces.clear();
        }
    }

    pub(super) fn resolve_call_target(&mut self, index: u16, argc: u8) -> VmResult<u16> {
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

        self.host
            .resolved_calls
            .get(index as usize)
            .copied()
            .ok_or(VmError::InvalidCall(index))
    }
}
