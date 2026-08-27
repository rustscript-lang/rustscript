use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, Wake, Waker};

use crate::builtins::BuiltinFunction;
use crate::host_api::{HostImportSchema, HostTypeSchema};
use crate::vm::operation::{OperationCancelReason, OperationId, OperationOutcome};
use crate::vm::resource::handle::ResourceHandle;
use crate::vm::resource::table::ResourceTable;

use super::async_host::{HostFuture, HostFutureOutput};
use super::capability::CapabilityProfile;
use super::*;

pub type HostOpId = u64;

/// Adapter-owned completion for an operation registered in the execution
/// scope. The generic VM owns only this opaque hook: adapters retain ownership
/// of their result mailbox and any resource-table side effects.
pub(crate) type ScopedOperationCompletion =
    Box<dyn FnOnce(&mut Vm, OperationOutcome) -> VmResult<CallReturn> + Send + 'static>;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum CallReturn {
    #[default]
    None,
    One(Value),
    Many(Vec<Value>),
}

impl CallReturn {
    pub fn none() -> Self {
        Self::None
    }

    pub fn one(value: Value) -> Self {
        Self::One(value)
    }

    pub fn many(values: Vec<Value>) -> Self {
        match values.len() {
            0 => Self::None,
            1 => Self::One(
                values
                    .into_iter()
                    .next()
                    .expect("single-value return should contain one value"),
            ),
            _ => Self::Many(values),
        }
    }

    pub fn from_values(values: Vec<Value>) -> Self {
        Self::many(values)
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::One(_) => false,
            Self::Many(values) => values.is_empty(),
        }
    }

    pub fn as_slice(&self) -> &[Value] {
        match self {
            Self::None => &[],
            Self::One(value) => std::slice::from_ref(value),
            Self::Many(values) => values,
        }
    }

    pub(crate) fn push_onto_stack(self, stack: &mut Vec<Value>) {
        match self {
            Self::None => {}
            Self::One(value) => stack.push(value),
            Self::Many(values) => stack.extend(values),
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

    /// Legacy cancellation hook kept for bridge implementations that do not
    /// need a lifecycle reason. VM cleanup calls `cancel_op_with_reason`.
    fn cancel_op(&mut self, _op_id: HostOpId) {}

    /// Removes/cancels a bridge-owned operation. The VM supplies the lifecycle
    /// reason for manual completion, bridge replacement, reset, and drop; the
    /// default preserves compatibility with bridges implementing only
    /// `cancel_op`.
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
    schema: Option<HostImportSchema>,
    kind: RegistryEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistrySchemaError {
    InvalidArity {
        name: String,
        arity: usize,
    },
    Duplicate {
        schema: Box<HostImportSchema>,
    },
    DispatchConflict {
        existing: Box<HostImportSchema>,
        requested: Box<HostImportSchema>,
    },
}

type HostPlanCache =
    HashMap<(Vec<HostImport>, Vec<Option<HostImportSchema>>), Arc<HostBindingPlan>>;

impl std::fmt::Display for RegistrySchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArity { name, arity } => {
                write!(f, "catalog function '{name}' has unsupported arity {arity}")
            }
            Self::Duplicate { schema } => {
                write!(
                    f,
                    "catalog schema for '{}' is already registered",
                    schema.name
                )
            }
            Self::DispatchConflict {
                existing,
                requested,
            } => write!(
                f,
                "catalog schemas for '{}' have the same dispatch shape but differ in identity: existing {existing:?}, requested {requested:?}",
                requested.name
            ),
        }
    }
}

impl std::error::Error for RegistrySchemaError {}

fn normalize_import_schemas(
    imports: &[HostImport],
    schemas: &[Option<HostImportSchema>],
) -> VmResult<Vec<Option<HostImportSchema>>> {
    if schemas.is_empty() {
        return Ok(vec![None; imports.len()]);
    }
    if schemas.len() != imports.len() {
        return Err(VmError::HostError(format!(
            "host import schema count mismatch: expected {}, got {}",
            imports.len(),
            schemas.len()
        )));
    }
    Ok(schemas.to_vec())
}

fn same_dispatch_shape(lhs: &HostImportSchema, rhs: &HostImportSchema) -> bool {
    lhs.name == rhs.name
        && lhs.params.len() == rhs.params.len()
        && lhs
            .params
            .iter()
            .zip(rhs.params.iter())
            .all(|(left, right)| left.schema == right.schema && left.passing == right.passing)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBindingPlan {
    import_signature: Vec<HostImport>,
    import_schemas: Vec<Option<HostImportSchema>>,
    registry_slots: Vec<u16>,
    registry_schemas: Vec<Option<HostImportSchema>>,
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
    catalog_by_schema: Arc<HashMap<HostImportSchema, u16>>,
    plan_cache: Arc<RwLock<HostPlanCache>>,
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
            catalog_by_schema: Arc::new(HashMap::new()),
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
        let mut registry = Self::empty();
        crate::builtins::runtime::register_default_host_functions(&mut registry);
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
        self.invalidate_plan_cache();
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
            || self
                .catalog_by_schema
                .keys()
                .any(|schema| schema.name == name)
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
            schema: None,
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
            schema: None,
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
            schema: None,
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
            schema: None,
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
            schema: None,
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
            schema: None,
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
            schema: None,
            kind: RegistryEntryKind::ArgsStaticNonYielding(function),
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    fn register_catalog_entry(
        &mut self,
        schema: HostImportSchema,
        kind: RegistryEntryKind,
    ) -> Result<u16, RegistrySchemaError> {
        let arity =
            u8::try_from(schema.arity()).map_err(|_| RegistrySchemaError::InvalidArity {
                name: schema.name.clone(),
                arity: schema.arity(),
            })?;
        if self.catalog_by_schema.contains_key(&schema) {
            return Err(RegistrySchemaError::Duplicate {
                schema: Box::new(schema),
            });
        }
        if let Some(existing) = self
            .catalog_by_schema
            .keys()
            .find(|existing| same_dispatch_shape(existing, &schema))
        {
            return Err(RegistrySchemaError::DispatchConflict {
                existing: Box::new(existing.clone()),
                requested: Box::new(schema),
            });
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = u16::try_from(entries.len()).map_err(|_| RegistrySchemaError::InvalidArity {
            name: schema.name.clone(),
            arity: schema.arity(),
        })?;
        entries.push(RegistryEntry {
            arity,
            schema: Some(schema.clone()),
            kind,
        });
        Arc::make_mut(&mut self.catalog_by_schema).insert(schema, slot);
        self.invalidate_plan_cache();
        Ok(slot)
    }

    pub fn register_catalog<F>(
        &mut self,
        schema: HostImportSchema,
        factory: F,
    ) -> Result<u16, RegistrySchemaError>
    where
        F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
    {
        self.register_catalog_entry(schema, RegistryEntryKind::Factory(Arc::new(factory)))
    }

    pub fn register_catalog_static(
        &mut self,
        schema: HostImportSchema,
        function: StaticHostFunction,
    ) -> Result<u16, RegistrySchemaError> {
        self.register_catalog_entry(schema, RegistryEntryKind::Static(function))
    }

    pub fn register_catalog_stack<F>(
        &mut self,
        schema: HostImportSchema,
        factory: F,
    ) -> Result<u16, RegistrySchemaError>
    where
        F: Fn() -> Box<dyn HostStackFunction> + Send + Sync + 'static,
    {
        self.register_catalog_entry(schema, RegistryEntryKind::StackFactory(Arc::new(factory)))
    }

    pub fn register_catalog_static_stack(
        &mut self,
        schema: HostImportSchema,
        function: StaticHostStackFunction,
    ) -> Result<u16, RegistrySchemaError> {
        self.register_catalog_entry(schema, RegistryEntryKind::StackStatic(function))
    }

    pub fn register_catalog_args<F>(
        &mut self,
        schema: HostImportSchema,
        factory: F,
    ) -> Result<u16, RegistrySchemaError>
    where
        F: Fn() -> Box<dyn HostArgsFunction> + Send + Sync + 'static,
    {
        self.register_catalog_entry(schema, RegistryEntryKind::ArgsFactory(Arc::new(factory)))
    }

    pub fn register_catalog_static_args(
        &mut self,
        schema: HostImportSchema,
        function: StaticHostArgsFunction,
    ) -> Result<u16, RegistrySchemaError> {
        self.register_catalog_entry(schema, RegistryEntryKind::ArgsStatic(function))
    }

    pub fn register_catalog_static_non_yielding_args(
        &mut self,
        schema: HostImportSchema,
        function: StaticHostArgsFunction,
    ) -> Result<u16, RegistrySchemaError> {
        self.register_catalog_entry(schema, RegistryEntryKind::ArgsStaticNonYielding(function))
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
        if let Some(composition) = self.standard_composition.as_ref() {
            let mut composed = self.clone();
            composition.ensure_surfaces(&vm.program.imports, &mut composed)?;
            composed.standard_composition = Some(Arc::clone(composition));
            return composed.bind_vm_cached_inner(vm);
        }
        self.bind_vm_cached_inner(vm)
    }

    fn bind_vm_cached_inner(&self, vm: &mut Vm) -> VmResult<()> {
        self.validate_program_capabilities(&vm.program)?;
        let plan = self.prepare_shared_plan_with_schemas(
            &vm.program.imports,
            &vm.program.host_import_schemas,
        )?;
        self.bind_vm_with_plan(vm, &plan)?;
        if let Some(composition) = self.standard_composition.as_ref() {
            vm.host.standard_composition = Some(Arc::clone(composition));
        }
        Ok(())
    }

    pub fn prepare_plan(&self, imports: &[HostImport]) -> VmResult<HostBindingPlan> {
        Ok(self.prepare_shared_plan(imports)?.as_ref().clone())
    }

    pub fn prepare_shared_plan(&self, imports: &[HostImport]) -> VmResult<Arc<HostBindingPlan>> {
        self.prepare_shared_plan_with_schemas(imports, &[])
    }

    pub fn prepare_plan_with_schemas(
        &self,
        imports: &[HostImport],
        schemas: &[Option<HostImportSchema>],
    ) -> VmResult<HostBindingPlan> {
        Ok(self
            .prepare_shared_plan_with_schemas(imports, schemas)?
            .as_ref()
            .clone())
    }

    pub fn prepare_shared_plan_with_schemas(
        &self,
        imports: &[HostImport],
        schemas: &[Option<HostImportSchema>],
    ) -> VmResult<Arc<HostBindingPlan>> {
        let schemas = normalize_import_schemas(imports, schemas)?;
        self.plan_for_imports(imports, &schemas)
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

    fn plan_for_imports(
        &self,
        imports: &[HostImport],
        import_schemas: &[Option<HostImportSchema>],
    ) -> VmResult<Arc<HostBindingPlan>> {
        let cache_key = (imports.to_vec(), import_schemas.to_vec());
        if let Some(plan) = self
            .plan_cache
            .read()
            .expect("host binding plan cache read lock should not be poisoned")
            .get(&cache_key)
            .cloned()
            && self.plan_matches_current(&plan)
        {
            return Ok(plan);
        }

        let mut registry_slot_to_vm_slot: HashMap<u16, u16> = HashMap::new();
        let mut registry_slots = Vec::new();
        let mut resolved_calls = Vec::with_capacity(imports.len());

        for (import, import_schema) in imports.iter().zip(import_schemas.iter()) {
            let registry_slot = if let Some(schema) = import_schema {
                if schema.name != import.name || schema.arity() != usize::from(import.arity) {
                    return Err(VmError::HostError(format!(
                        "host import '{}' does not match its full catalog schema",
                        import.name
                    )));
                }
                self.catalog_by_schema
                    .get(schema)
                    .copied()
                    .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?
            } else {
                let catalog_candidates = self
                    .catalog_by_schema
                    .keys()
                    .filter(|schema| {
                        schema.name == import.name && schema.arity() == usize::from(import.arity)
                    })
                    .count();
                if catalog_candidates > 0 {
                    return Err(VmError::HostError(format!(
                        "host import '{}' has {} catalog overloads; full schema and fingerprint are required",
                        import.name, catalog_candidates
                    )));
                }
                self.by_name
                    .get(&import.name)
                    .copied()
                    .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?
            };
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
            if let Some(schema) = import_schema
                && entry.schema.as_ref() != Some(schema)
            {
                return Err(VmError::HostError(format!(
                    "host registry schema for '{}' does not match the full call-site identity",
                    import.name
                )));
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
        let registry_schemas = registry_slots
            .iter()
            .map(|slot| {
                self.entries
                    .get(usize::from(*slot))
                    .and_then(|entry| entry.schema.clone())
            })
            .collect();
        let computed = Arc::new(HostBindingPlan {
            import_signature: import_key,
            import_schemas: import_schemas.to_vec(),
            registry_slots,
            registry_schemas,
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
        cache.insert(cache_key, Arc::clone(&computed));
        Ok(computed)
    }

    pub fn bind_vm_with_plan(&self, vm: &mut Vm, plan: &HostBindingPlan) -> VmResult<()> {
        self.validate_program_capabilities(&vm.program)?;
        if vm.program.imports != plan.import_signature {
            return Err(VmError::HostError(
                "host binding plan does not match vm import signature".to_string(),
            ));
        }
        if normalize_import_schemas(&vm.program.imports, &vm.program.host_import_schemas)?
            != plan.import_schemas
        {
            return Err(VmError::HostError(
                "host binding plan does not match vm catalog schema identity".to_string(),
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
            let host_slot = vm.host.host_function_schemas.len() - 1;
            if let Some(schema) = vm.host.host_function_schemas.get_mut(host_slot) {
                *schema = plan.registry_schemas.get(host_slot).cloned().flatten();
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
        CallOutcome::Return(CallReturn::Many(_)) => Err(VmError::HostError(
            "non-yielding host function returned multiple values".to_string(),
        )),
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

fn validate_coarse_host_value(value: &Value, expected: ValueType) -> VmResult<()> {
    let valid = matches!(
        (expected, value),
        (ValueType::Unknown, _)
            | (ValueType::Null, Value::Null)
            | (ValueType::Int, Value::Int(_))
            | (ValueType::Float, Value::Float(_))
            | (ValueType::Bool, Value::Bool(_))
            | (ValueType::String, Value::String(_))
            | (ValueType::Bytes, Value::Bytes(_))
            | (ValueType::Array, Value::Array(_))
            | (ValueType::Map, Value::Map(_))
            | (ValueType::Callable, Value::Callable(_))
    );
    if valid {
        return Ok(());
    }
    let expected = match expected {
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

pub(crate) fn validate_host_call_return(
    values: &CallReturn,
    expected: Option<ValueType>,
    schema: Option<&HostImportSchema>,
    program: &Program,
    resources: &ResourceTable,
) -> VmResult<()> {
    let value_slice: &[Value] = match values {
        CallReturn::None => &[],
        CallReturn::One(value) => std::slice::from_ref(value),
        CallReturn::Many(values) => values,
    };

    if let Some(schema) = schema {
        if matches!(schema.return_type, HostTypeSchema::Unknown) {
            return Ok(());
        }
        if value_slice.is_empty() && matches!(schema.return_type, HostTypeSchema::Null) {
            return Ok(());
        }
        if value_slice.len() != 1 {
            return Err(VmError::HostError(format!(
                "host return cardinality mismatch for '{}': expected one value, got {}",
                schema.name,
                value_slice.len()
            )));
        }
        return validate_host_value(&value_slice[0], &schema.return_type, program, resources);
    }

    match expected {
        None | Some(ValueType::Unknown) => Ok(()),
        Some(ValueType::Null) if value_slice.is_empty() => Ok(()),
        Some(expected) => {
            if value_slice.len() != 1 {
                return Err(VmError::HostError(format!(
                    "host return cardinality mismatch: expected one value, got {}",
                    value_slice.len()
                )));
            }
            validate_coarse_host_value(&value_slice[0], expected)
        }
    }
}

fn callable_schema_matches(
    expected: &HostTypeSchema,
    actual: &crate::compiler::TypeSchema,
) -> bool {
    use crate::compiler::TypeSchema;

    match (expected, actual) {
        (HostTypeSchema::Unknown, _) => true,
        (HostTypeSchema::Null, TypeSchema::Null)
        | (HostTypeSchema::Int, TypeSchema::Int)
        | (HostTypeSchema::Float, TypeSchema::Float)
        | (HostTypeSchema::Bool, TypeSchema::Bool)
        | (HostTypeSchema::String, TypeSchema::String)
        | (HostTypeSchema::Bytes, TypeSchema::Bytes) => true,
        (HostTypeSchema::Number, TypeSchema::Int | TypeSchema::Float | TypeSchema::Number) => true,
        (HostTypeSchema::Array(expected), TypeSchema::Array(actual)) => {
            callable_schema_matches(expected, actual)
        }
        (HostTypeSchema::Array(expected), TypeSchema::ArrayTuple(items)) => items
            .iter()
            .all(|item| callable_schema_matches(expected, item)),
        (HostTypeSchema::Array(expected), TypeSchema::ArrayTupleRest { prefix, rest }) => {
            prefix
                .iter()
                .all(|item| callable_schema_matches(expected, item))
                && callable_schema_matches(expected, rest)
        }
        (HostTypeSchema::Map(expected), TypeSchema::Map(actual)) => {
            callable_schema_matches(expected, actual)
        }
        (HostTypeSchema::Map(expected), TypeSchema::Object(fields)) => fields
            .values()
            .all(|item| callable_schema_matches(expected, item)),
        (HostTypeSchema::Optional(expected), TypeSchema::Optional(actual)) => {
            callable_schema_matches(expected, actual)
        }
        (
            HostTypeSchema::Callable {
                params: expected_params,
                result: expected_result,
            },
            TypeSchema::Callable {
                params: actual_params,
                result: actual_result,
            },
        ) => {
            if expected_params.is_empty()
                && matches!(expected_result.as_ref(), HostTypeSchema::Unknown)
            {
                return true;
            }
            expected_params.len() == actual_params.len()
                && expected_params
                    .iter()
                    .zip(actual_params)
                    .all(|(expected, actual)| callable_schema_matches(expected, actual))
                && callable_schema_matches(expected_result, actual_result)
        }
        (HostTypeSchema::Resource(_), _) => false,
        _ => false,
    }
}

fn host_callable_schema_matches(expected: &HostTypeSchema, actual: &HostTypeSchema) -> bool {
    match (expected, actual) {
        (HostTypeSchema::Unknown, _) => true,
        (HostTypeSchema::Null, HostTypeSchema::Null)
        | (HostTypeSchema::Int, HostTypeSchema::Int)
        | (HostTypeSchema::Float, HostTypeSchema::Float)
        | (HostTypeSchema::Bool, HostTypeSchema::Bool)
        | (HostTypeSchema::String, HostTypeSchema::String)
        | (HostTypeSchema::Bytes, HostTypeSchema::Bytes)
        | (HostTypeSchema::Number, HostTypeSchema::Number) => true,
        (HostTypeSchema::Array(expected), HostTypeSchema::Array(actual))
        | (HostTypeSchema::Map(expected), HostTypeSchema::Map(actual))
        | (HostTypeSchema::Optional(expected), HostTypeSchema::Optional(actual)) => {
            host_callable_schema_matches(expected, actual)
        }
        (
            HostTypeSchema::Callable {
                params: expected_params,
                result: expected_result,
            },
            HostTypeSchema::Callable {
                params: actual_params,
                result: actual_result,
            },
        ) => {
            (expected_params.is_empty()
                && matches!(expected_result.as_ref(), HostTypeSchema::Unknown))
                || (expected_params.len() == actual_params.len()
                    && expected_params
                        .iter()
                        .zip(actual_params)
                        .all(|(expected, actual)| host_callable_schema_matches(expected, actual))
                    && host_callable_schema_matches(expected_result, actual_result))
        }
        (HostTypeSchema::Resource(expected), HostTypeSchema::Resource(actual)) => {
            expected == actual
        }
        _ => false,
    }
}

fn validate_callable_value(
    value: &Value,
    expected_params: &[HostTypeSchema],
    expected_result: &HostTypeSchema,
    program: &Program,
) -> VmResult<()> {
    let Value::Callable(callable) = value else {
        return Err(VmError::TypeMismatch("callable"));
    };
    let prototype = program
        .callable_prototypes
        .get(callable.prototype_id as usize)
        .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
    if prototype.kind != callable.kind {
        return Err(VmError::TypeMismatch("callable"));
    }
    let matches = match prototype.schema.as_ref() {
        Some(crate::compiler::TypeSchema::Callable { params, result }) => {
            prototype.arity as usize == params.len()
                && ((expected_params.is_empty()
                    && matches!(expected_result, HostTypeSchema::Unknown))
                    || (expected_params.len() == params.len()
                        && expected_params
                            .iter()
                            .zip(params)
                            .all(|(expected, actual)| callable_schema_matches(expected, actual))
                        && callable_schema_matches(expected_result, result)))
        }
        Some(_) => false,
        None => match prototype.target {
            crate::CallableTarget::HostImport(import) => {
                let Some(Some(import_schema)) = program.host_import_schemas.get(import as usize)
                else {
                    return Err(VmError::TypeMismatch("callable"));
                };
                prototype.arity as usize == import_schema.params.len()
                    && ((expected_params.is_empty()
                        && matches!(expected_result, HostTypeSchema::Unknown))
                        || (expected_params.len() == import_schema.params.len()
                            && expected_params.iter().zip(&import_schema.params).all(
                                |(expected, actual)| {
                                    host_callable_schema_matches(expected, &actual.schema)
                                },
                            )
                            && host_callable_schema_matches(
                                expected_result,
                                &import_schema.return_type,
                            )))
            }
            crate::CallableTarget::ScriptFunction(_) => false,
        },
    };
    if !matches {
        return Err(VmError::TypeMismatch("callable"));
    }
    Ok(())
}

fn validate_host_value(
    value: &Value,
    schema: &HostTypeSchema,
    program: &Program,
    resources: &ResourceTable,
) -> VmResult<()> {
    match schema {
        HostTypeSchema::Unknown => Ok(()),
        HostTypeSchema::Null => {
            if matches!(value, Value::Null) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("null"))
            }
        }
        HostTypeSchema::Int => {
            if matches!(value, Value::Int(_)) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("int"))
            }
        }
        HostTypeSchema::Float => {
            if matches!(value, Value::Float(_)) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("float"))
            }
        }
        HostTypeSchema::Number => {
            if matches!(value, Value::Int(_) | Value::Float(_)) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("number"))
            }
        }
        HostTypeSchema::Bool => {
            if matches!(value, Value::Bool(_)) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("bool"))
            }
        }
        HostTypeSchema::String => {
            if matches!(value, Value::String(_)) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("string"))
            }
        }
        HostTypeSchema::Bytes => {
            if matches!(value, Value::Bytes(_)) {
                Ok(())
            } else {
                Err(VmError::TypeMismatch("bytes"))
            }
        }
        HostTypeSchema::Array(inner) => {
            let Value::Array(values) = value else {
                return Err(VmError::TypeMismatch("array"));
            };
            for value in values.iter() {
                validate_host_value(value, inner, program, resources)?;
            }
            Ok(())
        }
        HostTypeSchema::Map(inner) => {
            let Value::Map(values) = value else {
                return Err(VmError::TypeMismatch("map"));
            };
            for (_, value) in values.iter() {
                validate_host_value(value, inner, program, resources)?;
            }
            Ok(())
        }
        HostTypeSchema::Optional(inner) => {
            if matches!(value, Value::Null) {
                Ok(())
            } else {
                validate_host_value(value, inner, program, resources)
            }
        }
        HostTypeSchema::Callable { params, result } => {
            validate_callable_value(value, params, result, program)
        }
        HostTypeSchema::Resource(key) => {
            let Value::Int(raw) = value else {
                return Err(VmError::TypeMismatch("resource"));
            };
            let handle = ResourceHandle::from_raw(*raw as u64)
                .map_err(|error| VmError::HostError(error.to_string()))?;
            resources
                .validate_resource_type_key(handle, key)
                .map_err(|error| VmError::HostError(error.to_string()))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WaitingHostOp {
    pub(super) op_id: HostOpId,
    pub(super) source: WaitingHostOpSource,
    pub(super) expected_return_type: Option<ValueType>,
    pub(super) expected_return_schema: Option<HostImportSchema>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaitingHostOpSource {
    HostBridge,
    ScopedOperation,
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

impl Vm {
    pub fn register_function(&mut self, function: Box<dyn HostFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::Dynamic(function));
        self.host.host_function_schemas.push(None);
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_function(&mut self, function: StaticHostFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::Static(function));
        self.host.host_function_schemas.push(None);
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_stack_function(&mut self, function: Box<dyn HostStackFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::StackDynamic(function));
        self.host.host_function_schemas.push(None);
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_stack_function(&mut self, function: StaticHostStackFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::StackStatic(function));
        self.host.host_function_schemas.push(None);
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_args_function(&mut self, function: Box<dyn HostArgsFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsDynamic(function));
        self.host.host_function_schemas.push(None);
        self.host.resolved_calls_dirty = true;
        index
    }

    pub fn register_static_args_function(&mut self, function: StaticHostArgsFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsStatic(function));
        self.host.host_function_schemas.push(None);
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
        self.host.host_function_schemas.push(None);
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
        self.host.host_function_schemas.push(None);
        self.host
            .builtin_overrides
            .insert(builtin_call_index, host_slot);
    }

    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) {
        self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested);
        self.host
            .cancel_submitted_host_ops(OperationCancelReason::Requested);
        self.host.async_bridge = Some(bridge);
    }

    pub fn clear_async_bridge(&mut self) {
        self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested);
        self.host
            .cancel_submitted_host_ops(OperationCancelReason::Requested);
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

    /// Replaces this VM's standard host-surface composition.
    ///
    /// The composition is stored on the VM's host runtime and is consulted
    /// when implicit host bindings are constructed. Each VM therefore builds
    /// and binds its own registry; changing one VM cannot affect another.
    pub fn set_standard_composition(
        &mut self,
        composition: Arc<dyn super::standard_composition::StandardSurfaceComposition>,
    ) {
        self.host.standard_composition = Some(composition);
        self.host.resolved_calls_dirty = true;
    }

    /// Returns this VM's standard host-surface composition, if configured.
    pub fn standard_composition(
        &self,
    ) -> Option<&Arc<dyn super::standard_composition::StandardSurfaceComposition>> {
        self.host.standard_composition.as_ref()
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

    /// Registers the adapter-owned completion for one scoped operation.
    #[allow(dead_code)]
    pub(crate) fn register_scoped_operation_completion(
        &mut self,
        op_id: OperationId,
        completion: impl FnOnce(&mut Vm, OperationOutcome) -> VmResult<CallReturn> + Send + 'static,
    ) -> VmResult<()> {
        if self.host.scoped_operation_completions.contains_key(&op_id) {
            return Err(VmError::HostError(format!(
                "scoped operation {} already has a completion",
                op_id.raw()
            )));
        }
        self.host
            .scoped_operation_completions
            .insert(op_id, Box::new(completion));
        Ok(())
    }

    /// Discards an adapter-owned completion when operation startup fails
    /// before the VM can enter the waiting state.
    #[allow(dead_code)]
    pub(crate) fn discard_scoped_operation_completion(&mut self, op_id: OperationId) {
        self.host.scoped_operation_completions.remove(&op_id);
    }

    pub fn waiting_host_op_id(&self) -> Option<HostOpId> {
        self.instance.waiting_host_op.as_ref().map(|op| op.op_id)
    }

    fn cleanup_submitted_host_op_with_reason(
        &mut self,
        op_id: HostOpId,
        reason: OperationCancelReason,
    ) {
        if self.host.submitted_host_ops.remove(&op_id)
            && let Some(bridge) = self.host.async_bridge.as_mut()
        {
            bridge.cancel_op_with_reason(op_id, reason);
        }
    }

    fn cleanup_waiting_host_bridge_op_with_reason(
        &mut self,
        op_id: HostOpId,
        reason: OperationCancelReason,
    ) {
        self.host.submitted_host_ops.remove(&op_id);
        if let Some(bridge) = self.host.async_bridge.as_mut() {
            bridge.cancel_op_with_reason(op_id, reason);
        }
    }

    fn cleanup_waiting_host_op(
        &mut self,
        waiting: WaitingHostOp,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        match waiting.source {
            WaitingHostOpSource::HostBridge => {
                self.cleanup_waiting_host_bridge_op_with_reason(waiting.op_id, reason);
                Ok(())
            }
            WaitingHostOpSource::ScopedOperation => {
                let op_id = OperationId::from_raw(waiting.op_id).map_err(|error| {
                    VmError::ExecutionScope(ExecutionScopeError::Operation(error))
                })?;
                self.host.scoped_operation_completions.remove(&op_id);
                self.execution_scope()
                    .abort_operation(op_id, reason)
                    .map(|_| ())
                    .map_err(VmError::ExecutionScope)
            }
        }
    }

    pub(super) fn cancel_waiting_host_op_with_reason(&mut self, reason: OperationCancelReason) {
        let Some(waiting) = self.instance.waiting_host_op.take() else {
            return;
        };
        let _ = self.cleanup_waiting_host_op(waiting, reason);
    }

    pub(super) fn cancel_waiting_host_op(&mut self) {
        self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested);
    }

    pub fn complete_host_op(
        &mut self,
        op_id: HostOpId,
        values: impl Into<CallReturn>,
    ) -> VmResult<()> {
        let waiting = self.instance.waiting_host_op.clone().ok_or_else(|| {
            VmError::HostError(format!(
                "host op {op_id} completed but vm is not waiting on any op"
            ))
        })?;
        if waiting.op_id != op_id {
            return Err(VmError::HostError(format!(
                "host op {op_id} completed while vm waits on {}",
                waiting.op_id
            )));
        }

        let values = values.into();
        if let Err(error) = validate_host_call_return(
            &values,
            waiting.expected_return_type,
            waiting.expected_return_schema.as_ref(),
            &self.program,
            self.host.execution_scope.resources(),
        ) {
            let cleanup_result =
                self.cleanup_waiting_host_op(waiting, OperationCancelReason::Requested);
            self.instance.waiting_host_op = None;
            cleanup_result?;
            return Err(error);
        }
        let cleanup_result =
            self.cleanup_waiting_host_op(waiting, OperationCancelReason::Requested);
        self.instance.waiting_host_op = None;
        cleanup_result?;
        values.push_onto_stack(&mut self.instance.stack);
        Ok(())
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.instance.waiting_host_op.clone() else {
            return Poll::Ready(Ok(()));
        };

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
            WaitingHostOpSource::ScopedOperation => self.poll_scoped_operation(waiting.op_id, cx),
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(output)) => {
                let bridge_owned = matches!(waiting.source, WaitingHostOpSource::HostBridge);
                let values = match output.finish(self) {
                    Ok(values) => values,
                    Err(err) => {
                        if bridge_owned {
                            self.cleanup_submitted_host_op_with_reason(
                                waiting.op_id,
                                OperationCancelReason::Requested,
                            );
                        }
                        self.instance.waiting_host_op = None;
                        return Poll::Ready(Err(err));
                    }
                };
                if bridge_owned {
                    self.cleanup_submitted_host_op_with_reason(
                        waiting.op_id,
                        OperationCancelReason::Requested,
                    );
                }
                if let Err(error) = self.complete_waiting_host_op(waiting.op_id, values) {
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                if matches!(waiting.source, WaitingHostOpSource::HostBridge) {
                    self.cleanup_submitted_host_op_with_reason(
                        waiting.op_id,
                        OperationCancelReason::Requested,
                    );
                }
                self.instance.waiting_host_op = None;
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_scoped_operation(
        &mut self,
        raw_op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let op_id = match OperationId::from_raw(raw_op_id) {
            Ok(op_id) => op_id,
            Err(error) => {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "invalid scoped host operation {raw_op_id}: {error}"
                ))));
            }
        };
        match self.execution_scope().poll_operation(op_id, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.host.scoped_operation_completions.remove(&op_id);
                Poll::Ready(Err(VmError::HostError(format!(
                    "scoped host operation {raw_op_id} failed: {error}"
                ))))
            }
            Poll::Ready(Ok(outcome)) => {
                let Some(completion) = self.host.scoped_operation_completions.remove(&op_id) else {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "scoped host operation {raw_op_id} has no completion"
                    ))));
                };
                Poll::Ready(completion(self, outcome).map(HostFutureOutput::Return))
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
        let expected_return_schema = self
            .program
            .host_import_schemas
            .get(usize::from(index))
            .and_then(Clone::clone);
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
                expected_return_schema.as_ref(),
            );
        }
        if self.bound_host_function_uses_args_slice(resolved_index)? {
            self.execute_bound_args_host_function(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                expected_return_schema.as_ref(),
            )
        } else if self.bound_host_function_uses_stack_borrow(resolved_index)? {
            self.execute_bound_stack_host_function(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                expected_return_schema.as_ref(),
            )
        } else {
            self.execute_bound_host_function_from_stack(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                expected_return_schema.as_ref(),
            )
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
        let expected_return_type = BuiltinFunction::from_call_index(builtin_call_index)
            .map(|builtin| builtin.static_return_type());
        if self.bound_host_function_uses_args_slice(resolved_index)? {
            self.execute_bound_args_host_function(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                None,
            )
        } else if self.bound_host_function_uses_stack_borrow(resolved_index)? {
            self.execute_bound_stack_host_function(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                None,
            )
        } else {
            self.execute_bound_host_function_from_stack(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                None,
            )
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
                let expected_return_type = Some(builtin.static_return_type());
                if self.host.submitted_host_ops.contains(&op_id) {
                    if let Err(error) = self.set_waiting_host_op_with_return(
                        op_id,
                        WaitingHostOpSource::HostBridge,
                        expected_return_type,
                        None,
                    ) {
                        self.cleanup_submitted_host_op_with_reason(
                            op_id,
                            OperationCancelReason::Requested,
                        );
                        return Err(error);
                    }
                } else {
                    self.set_waiting_host_op_with_return(
                        op_id,
                        WaitingHostOpSource::ScopedOperation,
                        expected_return_type,
                        None,
                    )?;
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
        expected_return_type: Option<ValueType>,
        expected_return_schema: Option<&HostImportSchema>,
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
                if let Err(error) = validate_host_call_return(
                    &values,
                    expected_return_type,
                    expected_return_schema,
                    &self.program,
                    self.host.execution_scope.resources(),
                ) {
                    saved_stack.truncate(arg_start);
                    saved_stack.append(&mut host_stack);
                    self.instance.stack = saved_stack;
                    return Err(error);
                }
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
                self.set_waiting_host_op_with_return(
                    op_id,
                    WaitingHostOpSource::HostBridge,
                    expected_return_type,
                    expected_return_schema,
                )?;
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
        expected_return_schema: Option<&HostImportSchema>,
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
        let returned = CallReturn::one(value);
        validate_host_call_return(
            &returned,
            expected_return_type,
            expected_return_schema,
            &self.program,
            self.host.execution_scope.resources(),
        )?;
        let value = require_non_yielding_host_value(CallOutcome::Return(returned))?;
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
        expected_return_schema: Option<&HostImportSchema>,
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
            let returned = CallReturn::one(value);
            validate_host_call_return(
                &returned,
                expected_return_type,
                expected_return_schema,
                &self.program,
                self.host.execution_scope.resources(),
            )?;
            let value = require_non_yielding_host_value(CallOutcome::Return(returned))?;
            self.instance.stack.truncate(arg_start);
            self.instance.stack.push(value);
            return Ok(HostCallExecOutcome::Returned);
        }

        match outcome {
            CallOutcome::Return(values) => {
                validate_host_call_return(
                    &values,
                    expected_return_type,
                    expected_return_schema,
                    &self.program,
                    self.host.execution_scope.resources(),
                )?;
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
                self.set_waiting_host_op_with_return(
                    op_id,
                    WaitingHostOpSource::HostBridge,
                    expected_return_type,
                    expected_return_schema,
                )?;
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
        expected_return_type: Option<ValueType>,
        expected_return_schema: Option<&HostImportSchema>,
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
                validate_host_call_return(
                    &values,
                    expected_return_type,
                    expected_return_schema,
                    &self.program,
                    self.host.execution_scope.resources(),
                )?;
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
                self.set_waiting_host_op_with_return(
                    op_id,
                    WaitingHostOpSource::HostBridge,
                    expected_return_type,
                    expected_return_schema,
                )?;
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

    pub(super) fn set_waiting_host_op_with_return(
        &mut self,
        op_id: HostOpId,
        source: WaitingHostOpSource,
        expected_return_type: Option<ValueType>,
        expected_return_schema: Option<&HostImportSchema>,
    ) -> VmResult<()> {
        if let Some(active) = self.instance.waiting_host_op.as_ref()
            && active.op_id != op_id
        {
            return Err(VmError::HostError(format!(
                "vm already waiting on host op {}, cannot wait on {}",
                active.op_id, op_id
            )));
        }
        let expected_return_schema = expected_return_schema.cloned();
        self.instance.waiting_host_op = Some(WaitingHostOp {
            op_id,
            source,
            expected_return_type,
            expected_return_schema,
        });
        Ok(())
    }

    pub(super) fn complete_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        values: CallReturn,
    ) -> VmResult<()> {
        let waiting = self.instance.waiting_host_op.clone().ok_or_else(|| {
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
        if let Err(error) = validate_host_call_return(
            &values,
            waiting.expected_return_type,
            waiting.expected_return_schema.as_ref(),
            &self.program,
            self.host.execution_scope.resources(),
        ) {
            self.instance.waiting_host_op = None;
            return Err(error);
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
            let imports = self.program.imports.clone();
            let Some(composition) = self.host.standard_composition.clone() else {
                return HostFunctionRegistry::new().bind_vm_cached(self);
            };
            let mut registry = composition.build_default_registry()?;
            composition.ensure_surfaces(&imports, &mut registry)?;
            if imports
                .iter()
                .all(|import| registry.contains_name(&import.name))
            {
                return registry.bind_vm_cached(self);
            }
            for import in &imports {
                let _ = composition.bind_default_name(self, &import.name);
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
                    && let Some(composition) = self.host.standard_composition.clone()
                    && composition.bind_default_name(self, &import.name)
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
