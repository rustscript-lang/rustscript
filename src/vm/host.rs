use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::builtins::BuiltinFunction;
use crate::host_api::ResourceTypeKey;

use super::async_host::WaitingHostOp;
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

/// A host function that receives exclusive ownership of its call arguments.
///
/// The dispatch drains the call operands from the guest operand stack into an
/// [`OwnedHostCall`] before the function runs. Arguments the function takes
/// through [`OwnedHostCall::take_arg`] are owned by the host for good;
/// arguments it leaves untaken stay guest-owned — they are restored to the
/// operand stack when the call fails and released when the call succeeds.
///
/// This is the *generic* owned-value seam: the registry teaches the VM to
/// transfer callable / scalar / aggregate values exactly once, without any
/// domain-specific knowledge entering `src/vm`.
///
/// Handlers must not return [`CallOutcome::Yield`]: owned dispatch has already
/// drained the call operands, so a yielded call cannot be re-executed and the
/// outcome is rejected as a host error (with the untaken arguments restored).
pub trait HostOwnedFunction: Send {
    /// Runs one owned host call. `call` carries the drained arguments and the
    /// live VM; the function takes ownership of the arguments it needs.
    fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome>;
}

/// Registry-supplied context for one owned host-function instance.
///
/// The context is handed to the registration factory when a registry binds a
/// VM, so an owned host function can retain the immutable registry/binding
/// configuration it needs to spawn an isolated owned-value execution VM
/// later. It deliberately carries nothing else: no host state, no execution
/// frames, no request-local data.
pub struct OwnedHostContext<'a> {
    registry: &'a HostFunctionRegistry,
}

impl<'a> OwnedHostContext<'a> {
    /// The registry this instance was created from.
    ///
    /// Cloning the registry is cheap: a clone shares the immutable entry
    /// table, capability profile, and composition with its source.
    pub fn registry(&self) -> &'a HostFunctionRegistry {
        self.registry
    }
}

type OwnedHostFactory = dyn Fn(OwnedHostContext<'_>) -> Box<dyn HostOwnedFunction> + Send + Sync;

/// One in-flight owned host call: the live VM plus the drained call arguments.
///
/// `args` holds the operand-stack tail the call consumed, in declaration
/// order. Taking an argument transfers ownership to the host and clears the
/// argument slot, so the untaken remainder is exactly what the dispatch
/// restores when the call fails without double drop.
pub struct OwnedHostCall<'vm> {
    vm: &'vm mut Vm,
    args: Vec<Value>,
    taken: Vec<bool>,
}

impl<'vm> OwnedHostCall<'vm> {
    fn new(vm: &'vm mut Vm, args: Vec<Value>) -> Self {
        let taken = vec![false; args.len()];
        Self { vm, args, taken }
    }

    /// Mutable access to the calling VM.
    ///
    /// An owned host function must not re-enter VM execution on this VM: the
    /// call is dispatched with the source VM's frames and pending operation
    /// still live. Structurally different work belongs on a fresh VM created
    /// through [`Self::spawn_owned_callable_vm`].
    pub fn vm(&mut self) -> &mut Vm {
        self.vm
    }

    /// Read-only access to the calling VM.
    pub fn vm_ref(&self) -> &Vm {
        self.vm
    }

    /// Every call argument, in declaration order. Taken arguments read
    /// [`Value::Null`].
    pub fn args(&self) -> &[Value] {
        &self.args
    }

    /// One call argument by index.
    pub fn arg(&self, index: usize) -> Option<&Value> {
        self.args.get(index)
    }

    /// Whether the host already took ownership of `index`.
    pub fn is_taken(&self, index: usize) -> bool {
        self.taken.get(index).copied().unwrap_or(false)
    }

    /// Takes ownership of argument `index`.
    ///
    /// The value moves out of the call into the host and the guest slot is
    /// cleared. Taking the same index twice, or taking a missing index, is a
    /// host error and leaves ownership unchanged.
    pub fn take_arg(&mut self, index: usize) -> VmResult<Value> {
        let Some(taken) = self.taken.get_mut(index) else {
            return Err(VmError::HostError(format!(
                "owned host call has no argument at index {index}"
            )));
        };
        if *taken {
            return Err(VmError::HostError(format!(
                "owned host call argument {index} was already taken"
            )));
        }
        *taken = true;
        Ok(std::mem::replace(&mut self.args[index], Value::Null))
    }

    /// Restores a previously taken argument to the owned call.
    ///
    /// This is the transactional rollback primitive for a host that has
    /// accepted an argument tentatively but cannot complete its handoff. The
    /// value is placed back in its original slot and the slot becomes untaken,
    /// so the normal dispatch failure path restores it to the guest exactly
    /// once. Only an argument that this call already took may be restored.
    pub fn restore_arg(&mut self, index: usize, value: Value) -> VmResult<()> {
        let Some(taken) = self.taken.get_mut(index) else {
            return Err(VmError::HostError(format!(
                "owned host call has no argument at index {index}"
            )));
        };
        if !*taken {
            return Err(VmError::HostError(format!(
                "owned host call argument {index} was not taken"
            )));
        }
        self.args[index] = value;
        *taken = false;
        Ok(())
    }

    /// The untaken arguments, in declaration order, as the dispatch restores
    /// them when this call fails.
    fn into_untaken_args(self) -> Vec<Value> {
        self.args
            .into_iter()
            .zip(self.taken)
            .filter_map(|(value, taken)| (!taken).then_some(value))
            .collect()
    }

    /// Creates a fresh, isolated VM that owns `callable` and its captured
    /// callable graph.
    ///
    /// The fresh execution instance:
    ///
    /// * owns a brand-new execution instance, stack, resource table, waiting
    ///   slot, and module-state store;
    /// * reuses the *immutable* configuration of the calling VM — the same
    ///   `Arc<Program>` and the same caller-provided standard-surface
    ///   composition — and binds the host functions from `registry`;
    /// * installs only the module state `configure` supplies;
    /// * never copies the calling VM's frames, stack, locals, resources,
    ///   waiting operation, callback queue, or request-local host state.
    ///
    /// A callable (or captured callable) whose prototype does not belong to
    /// the source program is rejected: owned callable graphs are program-local
    /// and are never portable across Program boundaries.
    pub fn spawn_owned_callable_vm(
        &mut self,
        registry: &HostFunctionRegistry,
        callable: &Value,
        configure: impl FnOnce(&mut Vm) -> VmResult<()>,
    ) -> VmResult<Vm> {
        let program = Arc::clone(&self.vm.program);
        let composition = self.vm.host.standard_composition.clone();
        let mut owned = Vec::new();
        let mut visited = Vec::new();
        collect_owned_callable_graph(self.vm, callable, 0, &mut visited, &mut owned)?;
        let mut vm = Vm::try_new_shared(program.clone())?;
        if let Some(composition) = composition {
            vm.set_standard_composition(composition);
        }
        registry.bind_vm_cached(&mut vm)?;
        configure(&mut vm)?;
        vm.instance.owned_callables.extend(owned);
        clear_owned_callable_vm_state(&mut vm);
        Ok(vm)
    }
}

impl Vm {
    /// Reclaims an owned callable VM after a failed execution round.
    ///
    /// The callable graph is collected while the VM still owns it. Waiting
    /// host work is then cancelled, the ordinary reusable-VM reset closes
    /// run-scoped state, and the graph is adopted again only after the reset
    /// reports `Ready`. Module state, host bindings, and immutable program
    /// configuration remain attached to the VM across this operation.
    pub(crate) fn recover_owned_callable(&mut self, callable: &Value) -> VmResult<()> {
        let mut owned = Vec::new();
        let mut visited = Vec::new();
        collect_owned_callable_graph(self, callable, 0, &mut visited, &mut owned)?;

        // Preserve a typed cancellation failure, but still run the reset: the
        // scope close is the generic final cleanup path for any operation that
        // could not be retired by the waiting-slot helper.
        let cancellation = self.try_cancel_waiting_host_op();
        self.reset_for_reuse();
        if !self.is_reusable() {
            return Err(self.reset_error().cloned().map(VmError::Reset).unwrap_or(
                VmError::InvalidFrameState("owned callable vm is not reusable"),
            ));
        }
        self.instance.owned_callables.extend(owned);
        clear_owned_callable_vm_state(self);
        cancellation?;
        Ok(())
    }
}

fn clear_owned_callable_vm_state(vm: &mut Vm) {
    // `try_new_shared` and `reset_for_reuse` leave the normal root frame in
    // place. An owned callable VM must remain halted so it never executes the
    // source program as a second root invocation.
    vm.instance.execution_frames.clear();
    vm.instance.stack.clear();
    vm.instance.host_return = None;
    vm.instance.call_depth = 0;
    vm.instance.ip = vm.program.code.len();
}

/// Maximum traversal depth of an owned callable graph (top-level callable,
/// its capture cells, and every nested callable inside them).
const MAX_OWNED_CALLABLE_GRAPH_DEPTH: u8 = 64;

/// Depth- and cycle-bounded walk over an owned callable graph.
///
/// Cycles are legal (a closure may capture itself); every callable identity is
/// visited once. A callable whose prototype id is absent from `program` is a
/// structured rejection — the graph must belong to the program the fresh VM
/// executes.
fn collect_owned_callable_graph(
    source: &Vm,
    value: &Value,
    depth: u8,
    visited: &mut Vec<usize>,
    owned: &mut Vec<std::sync::Weak<CallableValue>>,
) -> VmResult<()> {
    if depth > MAX_OWNED_CALLABLE_GRAPH_DEPTH {
        return Err(VmError::HostError(
            "owned callable graph exceeds the maximum traversal depth".to_string(),
        ));
    }
    match value {
        Value::Callable(callable) => {
            if !source.owns_callable(value) {
                return Err(VmError::InvalidFrameState(
                    "callable does not belong to the source vm",
                ));
            }
            let identity = Arc::as_ptr(callable) as usize;
            if visited.contains(&identity) {
                return Ok(());
            }
            visited.push(identity);
            let prototype = source
                .program
                .callable_prototypes
                .get(callable.prototype_id as usize)
                .ok_or(VmError::InvalidCallablePrototype(callable.prototype_id))?;
            if prototype.capture_source_slots.len() != prototype.capture_modes.len()
                || prototype.capture_slots.len() != prototype.capture_modes.len()
            {
                return Err(VmError::InvalidFrameState(
                    "callable capture layout mismatch",
                ));
            }
            if let Some(environment) = &callable.env {
                let cells = environment
                    .cells
                    .lock()
                    .map_err(|_| VmError::InvalidFrameState("callable capture lock is poisoned"))?
                    .clone();
                if cells.len() != prototype.capture_modes.len() {
                    return Err(VmError::InvalidFrameState(
                        "callable environment layout mismatch",
                    ));
                }
                for (source_slot, cell) in prototype.capture_source_slots.iter().zip(cells) {
                    let captured = cell
                        .lock()
                        .map_err(|_| VmError::InvalidFrameState("capture cell lock is poisoned"))?
                        .clone();
                    validate_owned_capture(
                        source,
                        *source_slot,
                        &captured,
                        depth + 1,
                        visited,
                        owned,
                    )?;
                }
            } else if !prototype.capture_modes.is_empty() {
                return Err(VmError::InvalidFrameState(
                    "callable capture environment is missing",
                ));
            }
            owned.push(Arc::downgrade(callable));
            Ok(())
        }
        Value::Array(values) => {
            for item in values.iter() {
                collect_owned_callable_graph(source, item, depth + 1, visited, owned)?;
            }
            Ok(())
        }
        Value::Map(map) => {
            for (key, item) in map.iter() {
                collect_owned_callable_graph(source, key, depth + 1, visited, owned)?;
                collect_owned_callable_graph(source, item, depth + 1, visited, owned)?;
            }
            Ok(())
        }
        Value::Null
        | Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::String(_)
        | Value::Bytes(_) => Ok(()),
    }
}

fn validate_owned_capture(
    source: &Vm,
    source_slot: u16,
    captured: &Value,
    depth: u8,
    visited: &mut Vec<usize>,
    owned: &mut Vec<std::sync::Weak<CallableValue>>,
) -> VmResult<()> {
    let schema = source
        .program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.local_schemas.get(usize::from(source_slot)))
        .and_then(|schema| schema.as_ref());
    if schema.is_some_and(|schema| {
        schema_contains_resource_named(schema, &source.program.named_struct_schemas)
    }) {
        return Err(VmError::HostError(
            "owned callable graph contains a resource-bearing capture".to_string(),
        ));
    }
    collect_owned_callable_graph(source, captured, depth, visited, owned)
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
    /// Owned-dispatch entry: the factory receives the registry at bind time and
    /// produces a per-VM owned host function that consumes its arguments.
    OwnedFactory(Arc<OwnedHostFactory>),
}

#[derive(Clone)]
struct RegistryEntry {
    arity: u8,
    kind: RegistryEntryKind,
    legacy_resource_return_key: Option<ResourceTypeKey>,
}

/// Bounding depth for recursive schema walks in the exact host-call contract.
///
/// Schemas deeper than this are rejected at registration time (structured
/// `HostImportBindingError`); the call-time extraction is then guaranteed to
/// stay well within the bound.
const MAX_EXACT_SCHEMA_DEPTH: u8 = 64;

/// One resource-passing argument occurrence in an exact manual host call.
#[derive(Clone, Debug)]
struct ExactResourceSpec {
    /// Parameter / raw-argument index.
    arg_index: usize,
    handle: ResourceHandle,
    key: ResourceTypeKey,
    mode: ResourceAccessMode,
}

/// The single type-erased contract for an exact manual host call with
/// resource-passing parameters (C1/C2/C5).
///
/// Every manual binding kind (dynamic / static / stack / args) funnels
/// through [`run_guarded_host_call`], which:
///
/// 1. **builds** the contract from `(HostImportSchema, args)` — extracting
///    every resource-passing occurrence (bounded recursion, depth ≤
///    [`MAX_EXACT_SCHEMA_DEPTH`]) with its expected key and rejecting illegal
///    same-handle aliases;
/// 2. **validates** every occurrence against the live execution scope before
///    the user function runs (handle structure, arena/generation, slot key,
///    not taken, open, and for `TakeOwned` guest-owned, child-free and
///    operation-free) — read-only, zero mutation, so a bad argument never
///    reaches the user function;
/// 3. **commits** after the function returns: every declared `TakeOwned`
///    must have moved GuestOwned → Taken by *this* invocation; anything still
///    guest-owned is safely reclaimed (close launch failures latched in the
///    scope), a consumed `Borrow`/`BorrowMut` is a structured conflict, and
///    the user error / panic boundary keeps taken values taken.
#[derive(Debug)]
struct ExactHostCallContract {
    specs: Vec<ExactResourceSpec>,
}

/// Maps a catalog passing mode to the resource frame mode; `Value` is not a
/// resource operation and returns `None`.
fn passing_to_access_mode(passing: HostParamPassing) -> Option<ResourceAccessMode> {
    match passing {
        HostParamPassing::Value => None,
        HostParamPassing::Borrow => Some(ResourceAccessMode::Borrow),
        HostParamPassing::BorrowMut => Some(ResourceAccessMode::BorrowMut),
        HostParamPassing::TakeOwned => Some(ResourceAccessMode::TakeOwned),
    }
}

/// The expected key of a *directly addressable* resource-passing schema.
///
/// Only a direct `Resource(key)` or a single `Optional<Resource(key)>` is
/// addressable by the current handle ABI (a `Null` argument legally skips the
/// optional). A resource nested inside an aggregate has no addressable handle
/// and is rejected at registration, so it never reaches the call-time path.
fn addressable_resource_key(
    schema: &crate::compiler::TypeSchema,
) -> Option<(ResourceTypeKey, bool)> {
    match schema {
        crate::compiler::TypeSchema::Resource(key) => Some((key.clone(), false)),
        crate::compiler::TypeSchema::Optional(inner) => match inner.as_ref() {
            crate::compiler::TypeSchema::Resource(key) => Some((key.clone(), true)),
            _ => None,
        },
        _ => None,
    }
}

/// Depth-bounded recursive probe for a `TypeSchema::Resource` occurrence.
///
/// Guards against schema-shaped denial-of-service at registration time: any
/// nesting deeper than [`MAX_EXACT_SCHEMA_DEPTH`] is a structured rejection
/// (the walk exits at depth 65), so the extracted call-time contract is
/// guaranteed to stay within the bound. All resource-bearing params are
/// probed here once at registration; scalars return `false` without
/// recursion.
#[cfg(test)]
fn schema_walk_has_resource(
    schema: &crate::compiler::TypeSchema,
    depth: u8,
) -> Result<bool, HostImportBindingError> {
    schema_walk_has_resource_in(schema, depth, &HashMap::new(), &mut Vec::new())
}

fn schema_contains_resource_named(
    schema: &crate::compiler::TypeSchema,
    named_struct_schemas: &HashMap<String, crate::bytecode::NamedStructSchema>,
) -> bool {
    schema_walk_has_resource_in(schema, 0, named_struct_schemas, &mut Vec::new()).unwrap_or(true)
}

/// Depth-bounded resource probe that resolves [`TypeSchema::Named`] bodies
/// through the catalog/runtime named-struct table.
///
/// Compiler identity stays `TypeSchema::Named`; this walk is the VM resource
/// semantics path. Recursive named edges are instantiated one body at a time
/// with a cycle set so a finite declaration cannot unbounded-recurse.
fn schema_walk_has_resource_in(
    schema: &crate::compiler::TypeSchema,
    depth: u8,
    named_struct_schemas: &HashMap<String, crate::bytecode::NamedStructSchema>,
    active: &mut Vec<String>,
) -> Result<bool, HostImportBindingError> {
    use crate::compiler::TypeSchema;
    if depth > MAX_EXACT_SCHEMA_DEPTH {
        return Err(HostImportBindingError::InvalidSchema {
            import: String::new(),
            reason: format!(
                "resource schema is nested deeper than depth limit {}",
                MAX_EXACT_SCHEMA_DEPTH
            ),
        });
    }
    match schema {
        TypeSchema::Resource(_) => Ok(true),
        TypeSchema::Optional(inner) => {
            schema_walk_has_resource_in(inner, depth + 1, named_struct_schemas, active)
        }
        TypeSchema::Named(name, type_args) => {
            for arg in type_args {
                if schema_walk_has_resource_in(arg, depth + 1, named_struct_schemas, active)? {
                    return Ok(true);
                }
            }
            if active.iter().any(|seen| seen == name) {
                return Ok(false);
            }
            let Some(def) = named_struct_schemas.get(name) else {
                return Err(HostImportBindingError::InvalidSchema {
                    import: name.clone(),
                    reason: format!(
                        "named struct '{name}' has no installed body; catalog named-struct \
                         schemas must be installed before exact registration"
                    ),
                });
            };
            let Some(body) = def.instantiate(type_args) else {
                return Err(HostImportBindingError::InvalidSchema {
                    import: name.clone(),
                    reason: format!(
                        "named struct '{name}' could not be instantiated with {} type argument(s)",
                        type_args.len()
                    ),
                });
            };
            active.push(name.clone());
            let found =
                schema_walk_has_resource_in(&body, depth + 1, named_struct_schemas, active)?;
            active.pop();
            Ok(found)
        }
        TypeSchema::Array(element) => {
            schema_walk_has_resource_in(element, depth + 1, named_struct_schemas, active)
        }
        TypeSchema::ArrayTuple(items) => {
            for item in items {
                if schema_walk_has_resource_in(item, depth + 1, named_struct_schemas, active)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            if schema_walk_has_resource_in(rest, depth + 1, named_struct_schemas, active)? {
                return Ok(true);
            }
            for item in prefix {
                if schema_walk_has_resource_in(item, depth + 1, named_struct_schemas, active)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        TypeSchema::Map(value) => {
            schema_walk_has_resource_in(value, depth + 1, named_struct_schemas, active)
        }
        TypeSchema::Object(fields) => {
            for value in fields.values() {
                if schema_walk_has_resource_in(value, depth + 1, named_struct_schemas, active)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        TypeSchema::Callable { params, result } => {
            if schema_walk_has_resource_in(result, depth + 1, named_struct_schemas, active)? {
                return Ok(true);
            }
            for param in params {
                if schema_walk_has_resource_in(param, depth + 1, named_struct_schemas, active)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        TypeSchema::Unknown
        | TypeSchema::Null
        | TypeSchema::Int
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::Bool
        | TypeSchema::String
        | TypeSchema::Bytes
        | TypeSchema::GenericParam(_) => Ok(false),
    }
}

/// Registration-time exact-schema validation (C1 addressability).
///
/// Every exact binding (dynamic / static / stack / args) funnels through this
/// before any registry mutation. Rejections are structured
/// `HostImportBindingError`s and leave the registry untouched:
///
/// * A `Value`-passing param whose schema *contains* a resource is rejected —
///   value passing cannot address a resource.
/// * A borrow param (`Borrow`/`BorrowMut`) whose schema contains **no**
///   resource is rejected. A resource-free `TakeOwned` is accepted only by
///   the explicit owned-dispatch path, where it transfers the value itself.
/// * A resource-passing param whose schema is not directly addressable by the
///   current `Value::Int` handle ABI (a resource nested inside
///   `Array`/`Map`/deeper `Optional`/...) is rejected at registration — it can
///   never be extracted at call time.
/// * The exact return is checked the same way: a return whose schema
///   *contains* a resource is valid only as a direct `Resource(key)` or a
///   single `Optional<Resource(key)>`; any aggregate-nested resource return is
///   rejected because the current handle-carrier ABI cannot represent it (the
///   call-time `NestedResource` policy stays as defense in depth).
/// * Any resource occurrence nested deeper than [`MAX_EXACT_SCHEMA_DEPTH`] is
///   rejected (the walk exits at depth 65).
/// * For args-only (non-VM-aware) registrations, **any** resource-passing
///   param is rejected: such a function has no `&mut Vm`, so it cannot
///   enforce or observe the resource contract.
#[cfg(test)]
fn validate_exact_registration_schema(
    name: &str,
    schema: &HostImportSchema,
    vm_aware: bool,
) -> Result<(), HostImportBindingError> {
    validate_exact_registration_schema_with_named(name, schema, vm_aware, &HashMap::new())
}

fn validate_exact_registration_schema_with_named(
    name: &str,
    schema: &HostImportSchema,
    vm_aware: bool,
    named_struct_schemas: &HashMap<String, crate::bytecode::NamedStructSchema>,
) -> Result<(), HostImportBindingError> {
    validate_exact_registration_schema_full(name, schema, vm_aware, named_struct_schemas, false)
}

/// [`validate_exact_registration_schema_with_named`] with the owned-dispatch
/// allowance made explicit.
///
/// `owned_value_transfer` is set only by [`HostFunctionRegistry::register_exact_owned`]:
/// a resource-free `TakeOwned` parameter is then a legitimate owned *value*
/// transfer (callable, scalar, or aggregate) instead of the silent-drop
/// hazard every other registration kind would create.
fn validate_exact_registration_schema_full(
    name: &str,
    schema: &HostImportSchema,
    vm_aware: bool,
    named_struct_schemas: &HashMap<String, crate::bytecode::NamedStructSchema>,
    owned_value_transfer: bool,
) -> Result<(), HostImportBindingError> {
    for param in &schema.params {
        let has_resource =
            schema_walk_has_resource_in(&param.schema, 0, named_struct_schemas, &mut Vec::new())?;
        if !has_resource {
            match param.passing {
                crate::host_api::HostParamPassing::Value => {}
                crate::host_api::HostParamPassing::TakeOwned if owned_value_transfer => {}
                crate::host_api::HostParamPassing::Borrow
                | crate::host_api::HostParamPassing::BorrowMut
                | crate::host_api::HostParamPassing::TakeOwned => {
                    return Err(HostImportBindingError::InvalidSchema {
                        import: format!("{name}::{}", param.name),
                        reason: format!(
                            "parameter '{}' declares {:?} passing but its schema {:#?} contains \
                             no resource; non-resource parameters must use Value",
                            param.name, param.passing, param.schema,
                        ),
                    });
                }
            }
            continue;
        }
        if !vm_aware {
            return Err(HostImportBindingError::InvalidSchema {
                import: format!("{name}::{}", param.name),
                reason: format!(
                    "Args-only exact registration cannot enforce resource passing for '{}' \
                     (schema {:#?}); use a VM-aware registration wrapper",
                    param.name, param.schema,
                ),
            });
        }
        if param.passing == crate::host_api::HostParamPassing::Value {
            return Err(HostImportBindingError::InvalidSchema {
                import: format!("{name}::{}", param.name),
                reason: format!(
                    "Value-passing parameter '{}' carries a resource (schema {:#?}); \
                     resource parameters must use Borrow/BorrowMut/TakeOwned",
                    param.name, param.schema,
                ),
            });
        }
        if addressable_resource_key(&param.schema).is_none() {
            return Err(HostImportBindingError::InvalidSchema {
                import: format!("{name}::{}", param.name),
                reason: format!(
                    "resource-passing parameter '{}' schema {:#?} is not directly \
                     addressable by the handle ABI",
                    param.name, param.schema,
                ),
            });
        }
    }
    // Exact return shape (finding: only `Resource(key)` and
    // `Optional<Resource(key)>` may carry a resource across the boundary).
    if schema_walk_has_resource_in(
        &schema.return_type,
        0,
        named_struct_schemas,
        &mut Vec::new(),
    )? && addressable_resource_key(&schema.return_type).is_none()
    {
        return Err(HostImportBindingError::InvalidSchema {
            import: name.to_string(),
            reason: format!(
                "exact return schema {:#?} contains a resource nested inside an \
                     aggregate; only Resource(key) and Optional<Resource(key)> returns are \
                     representable by the handle ABI",
                schema.return_type,
            ),
        });
    }
    Ok(())
}

/// Whether an exact schema contains any resource-passing parameter
/// (`Borrow`/`BorrowMut`/`TakeOwned` on an addressable resource schema). Such
/// registrations must be wrapped in [`ExactHostCallContract`] so the
/// preflight + commit never run unguarded. Registration validation guarantees
/// any resource-bearing non-`Value` param is directly addressable, so
/// `addressable_resource_key` is a complete probe here.
fn schema_requires_guard(schema: &HostImportSchema) -> bool {
    schema.params.iter().any(|param| {
        param.passing != crate::host_api::HostParamPassing::Value
            && addressable_resource_key(&param.schema).is_some()
    })
}

fn resource_access_conflict_error(left: &ExactResourceSpec, right: &ExactResourceSpec) -> VmError {
    VmError::Resource(
        ResourceError::new(
            ResourceErrorCode::ResourceAccessConflict,
            "resource::exact_call",
            format!(
                "resource argument {} and {} alias handle {} with conflicting access \
                 modes {:?}/{:?}",
                left.arg_index,
                right.arg_index,
                left.handle.raw(),
                left.mode,
                right.mode,
            ),
        )
        .with_value(left.handle.raw()),
    )
}

impl ExactHostCallContract {
    /// Builds the contract from the import schema and raw arguments.
    ///
    /// Extracts every resource-passing occurrence (bounded recursion) with its
    /// expected key, decodes the raw handle from each argument, and rejects
    /// illegal same-handle aliases. Any failure is structured and consumes
    /// nothing.
    fn build(schema: &HostImportSchema, args: &[Value]) -> VmResult<Self> {
        let mut specs = Vec::new();
        for (index, param) in schema.params.iter().enumerate() {
            let Some(mode) = passing_to_access_mode(param.passing) else {
                // `Value` params are not resource operations; resource-bearing
                // `Value` params are rejected at registration.
                continue;
            };
            if !param.schema.contains_resource() {
                // A resource-free `TakeOwned` parameter is an owned *value*
                // transfer (accepted only for owned-dispatch registrations);
                // it has no handle to operate on and no resource contract to
                // enforce. Every other resource-passing mode on a resource-free
                // schema is rejected at registration. Reaching call time means
                // the registration funnel was bypassed, so refuse to run rather
                // than silently dropping the declared mode (which would let a
                // callee assume a borrowing/taking contract the caller never
                // granted).
                if param.passing == crate::host_api::HostParamPassing::TakeOwned {
                    continue;
                }
                return Err(VmError::HostImportBinding(
                    HostImportBindingError::InvalidSchema {
                        import: String::new(),
                        reason: format!(
                            "resource-passing parameter '{}' declares {mode:?} but its schema \
                             {:#?} contains no resource",
                            param.name, param.schema,
                        ),
                    },
                ));
            }
            let Some((key, optional)) = addressable_resource_key(&param.schema) else {
                return Err(VmError::HostImportBinding(
                    HostImportBindingError::InvalidSchema {
                        import: String::new(),
                        reason: format!(
                            "resource-passing parameter '{}' schema {:#?} is not directly \
                             addressable by the handle ABI",
                            param.name, param.schema,
                        ),
                    },
                ));
            };
            let value = args.get(index).ok_or_else(|| {
                VmError::Resource(ResourceError::new(
                    ResourceErrorCode::InvalidResourceHandle,
                    "resource::exact_call",
                    format!("exact host call is missing argument at index {index}"),
                ))
            })?;
            if optional && matches!(value, Value::Null) {
                // Legal skip for Optional(Resource).
                continue;
            }
            let handle = ResourceHandle::from_value(value).map_err(VmError::Resource)?;
            specs.push(ExactResourceSpec {
                arg_index: index,
                handle,
                key,
                mode,
            });
        }
        // Alias graph: only shared `Borrow` + shared `Borrow` is legal for one
        // handle. Duplicate TakeOwned, TakeOwned+Borrow/BorrowMut and
        // BorrowMut+Borrow all reject here, before the user function runs.
        for (index, left) in specs.iter().enumerate() {
            for right in specs.iter().skip(index + 1) {
                if left.handle != right.handle {
                    continue;
                }
                if left.mode == ResourceAccessMode::Borrow
                    && right.mode == ResourceAccessMode::Borrow
                {
                    continue;
                }
                return Err(resource_access_conflict_error(left, right));
            }
        }
        Ok(Self { specs })
    }

    /// Read-only pre-call validation of every occurrence against the live
    /// execution scope (C1). Zero mutation: any rejection leaves every
    /// resource untouched and the user function uninvoked.
    fn validate(&self, vm: &mut Vm) -> VmResult<()> {
        for spec in &self.specs {
            vm.host
                .execution_scope_validate_exact_access(spec.handle, &spec.key, spec.mode)
                .map_err(VmError::from)?;
        }
        Ok(())
    }

    /// Post-call commit / cleanup (C2). Returns the first structured error, if
    /// any.
    ///
    /// - A declared `TakeOwned` that is now `Taken` was consumed by this
    ///   invocation (old / previously-taken handles never satisfy it — they
    ///   are rejected up front).
    /// - A `TakeOwned` still guest-owned is safely reclaimed; a close-launch
    ///   failure is latched in the scope's first-error state without losing
    ///   the primary error. Wrong-key / foreign / stale / already-taken /
    ///   closed handles are never closed.
    /// - A `Borrow`/`BorrowMut` argument that ended up `Taken` is a
    ///   structured conflict (the callee consumed a borrowed argument).
    fn commit(&self, vm: &mut Vm) -> Option<VmError> {
        let mut first_error = None;
        for spec in &self.specs {
            let ownership = vm.host.execution_scope().resources().ownership(spec.handle);
            match spec.mode {
                ResourceAccessMode::TakeOwned => {
                    if ownership == Some(ResourceOwnership::Taken) {
                        continue;
                    }
                    first_error.get_or_insert_with(|| {
                        VmError::Resource(
                            ResourceError::new(
                                ResourceErrorCode::ResourceNotConsumed,
                                "resource::exact_call",
                                format!(
                                    "declared TakeOwned argument at index {} (handle {}, key {}) \
                                     was not consumed by the host function",
                                    spec.arg_index,
                                    spec.handle.raw(),
                                    spec.key,
                                ),
                            )
                            .with_value(spec.handle.raw()),
                        )
                    });
                    if ownership == Some(ResourceOwnership::GuestOwned) {
                        let release = vm.host.execution_scope_release_guest_owner(
                            spec.handle,
                            OwnershipRelease::close(),
                        );
                        if let Err(ExecutionScopeError::Resource(error)) = release {
                            vm.host.execution_scope_record_release_error(error);
                        }
                    }
                }
                ResourceAccessMode::Borrow | ResourceAccessMode::BorrowMut => {
                    if ownership == Some(ResourceOwnership::Taken) {
                        first_error.get_or_insert_with(|| {
                            VmError::Resource(
                                ResourceError::new(
                                    ResourceErrorCode::ResourceAccessConflict,
                                    "resource::exact_call",
                                    format!(
                                        "resource argument at index {} declared {:?} was consumed \
                                         by the host function",
                                        spec.arg_index, spec.mode,
                                    ),
                                )
                                .with_value(spec.handle.raw()),
                            )
                        });
                    }
                }
                ResourceAccessMode::Value => unreachable!(),
            }
        }
        first_error
    }
}

fn run_guarded_host_call<F>(
    vm: &mut Vm,
    args: &[Value],
    schema: &HostImportSchema,
    call: F,
) -> VmResult<CallOutcome>
where
    F: FnOnce(&mut Vm, &[Value]) -> VmResult<CallOutcome>,
{
    let contract = ExactHostCallContract::build(schema, args)?;
    contract.validate(vm)?;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(vm, args)));
    match outcome {
        Ok(Ok(outcome)) => match contract.commit(vm) {
            Some(error) => Err(error),
            None => Ok(outcome),
        },
        Ok(Err(error)) => {
            // The user function failed: the primary error is preserved; any
            // unconsumed guest-owned resources are still reclaimed (close
            // failures latched in the scope) and taken values stay Taken.
            let _ = contract.commit(vm);
            Err(error)
        }
        Err(payload) => {
            let _ = contract.commit(vm);
            std::panic::resume_unwind(payload)
        }
    }
}

struct GuardedHostFunction {
    inner: Box<dyn HostFunction>,
    schema: HostImportSchema,
}

impl HostFunction for GuardedHostFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        run_guarded_host_call(vm, args, &self.schema, |vm, args| self.inner.call(vm, args))
    }
}

struct GuardedStaticHostFunction {
    function: StaticHostFunction,
    schema: HostImportSchema,
}

impl HostFunction for GuardedStaticHostFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        run_guarded_host_call(vm, args, &self.schema, |vm, args| (self.function)(vm, args))
    }
}

/// Owned-dispatch sibling of [`GuardedHostFunction`].
///
/// Resource-bearing exact schemas keep their full preflight/commit contract
/// when they are registered through [`HostFunctionRegistry::register_exact_owned`]:
/// the argument list is the owned call's (already drained) argument list, and a
/// resource-free `TakeOwned` value transfer is simply not a resource operation.
struct GuardedOwnedHostFunction {
    inner: Box<dyn HostOwnedFunction>,
    schema: HostImportSchema,
}

impl HostOwnedFunction for GuardedOwnedHostFunction {
    fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
        let contract = ExactHostCallContract::build(&self.schema, call.args())?;
        contract.validate(call.vm())?;
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.inner.call(call)));
        match outcome {
            Ok(Ok(outcome)) => match contract.commit(call.vm()) {
                Some(error) => Err(error),
                None => Ok(outcome),
            },
            Ok(Err(error)) => {
                // The user function failed: the primary error is preserved;
                // any unconsumed guest-owned resources are still reclaimed
                // (close failures latched in the scope) and taken values stay
                // Taken.
                let _ = contract.commit(call.vm());
                Err(error)
            }
            Err(payload) => {
                let _ = contract.commit(call.vm());
                std::panic::resume_unwind(payload)
            }
        }
    }
}

struct GuardedHostStackFunction {
    inner: Box<dyn HostStackFunction>,
    schema: HostImportSchema,
}

impl HostStackFunction for GuardedHostStackFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        run_guarded_host_call(vm, args, &self.schema, |vm, args| self.inner.call(vm, args))
    }
}

struct GuardedStaticHostStackFunction {
    function: StaticHostStackFunction,
    schema: HostImportSchema,
}

impl HostStackFunction for GuardedStaticHostStackFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        run_guarded_host_call(vm, args, &self.schema, |vm, args| (self.function)(vm, args))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBindingPlan {
    import_signature: Vec<HostImport>,
    registry_slots: Vec<u16>,
    legacy_resource_return_keys: Vec<Option<ResourceTypeKey>>,
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

impl HostBindingPlan {
    /// The exact import signature this plan was computed for (includes each import's schema).
    pub fn import_signature(&self) -> &[HostImport] {
        &self.import_signature
    }
}

/// The registry a [`HostFunctionRegistry::bind_vm_cached`] should bind from:
/// either the current registry (all surfaces present) or a freshly staged /
/// memoized snapshot that already carries every required standard surface.
#[cfg(feature = "runtime")]
struct StandardStageResult {
    registry: Arc<HostFunctionRegistry>,
}

pub struct HostFunctionRegistry {
    entries: Arc<Vec<RegistryEntry>>,
    by_name: Arc<HashMap<String, u16>>,
    /// Exact-schema host imports: name -> (exact schema -> registry slot).
    /// A single name can host many exact schemas (overloads); each maps to its
    /// own slot so the plan/dispatch never collapses them onto one `by_name` slot.
    by_exact: Arc<HashMap<String, HashMap<HostImportSchema, u16>>>,
    plan_cache: Arc<RwLock<HashMap<Vec<HostImport>, Arc<HostBindingPlan>>>>,
    allowed_builtin_calls: Arc<Vec<u16>>,
    allow_default_builtin_capabilities: bool,
    allow_default_host_capabilities: bool,
    capability_profile: Arc<CapabilityProfile>,
    registry_state: Arc<()>,
    registry_generation_token: Arc<()>,
    registry_generation: Arc<AtomicU64>,
    /// Isolated for ordinary `Clone` siblings. Transaction staging explicitly
    /// preserves this origin and publishes only through its typed handle.
    transaction_origin: Arc<()>,
    /// Memoized fully-staged standard snapshot (per registry lineage). After a
    /// successful auto-stage of missing standard adapter surfaces, the staged
    /// registry is cached here so subsequent binds reuse it without
    /// re-registering, re-validating, or bumping the generation counter. The
    /// guard records the source registry's generation at publish time; a
    /// later mutation of the source registry invalidates the snapshot.
    standard_staging_snapshot: Arc<RwLock<Option<StandardStagingSnapshot>>>,
    /// Deterministic count of standard-surface registration rounds performed
    /// by this registry lineage through [`bind_vm_cached`].
    standard_staging_registrations: Arc<AtomicU64>,
    /// Caller-provided standard-surface composition for this registry
    /// instance (explicit per-instance state, never a process global).
    /// `Some` enables the standard auto-stage / fallback paths; `None` means
    /// this registry has no standard composition (e.g. `HostFunctionRegistry::empty`).
    composition: Option<Arc<dyn super::standard_composition::StandardSurfaceComposition>>,
    /// Named struct bodies used by VM resource walks. Compiler `HostImportSchema`
    /// identity stays `TypeSchema::Named`; this table supplies the Object body
    /// for nested-resource detection at exact registration.
    named_struct_schemas: Arc<HashMap<String, crate::bytecode::NamedStructSchema>>,
}

/// A memoized fully-staged standard snapshot plus the source-registry
/// generation it was published against. The snapshot is only reused while the
/// source registry's generation is unchanged, so later custom registrations or
/// capability changes invalidate it.
#[cfg(feature = "runtime")]
#[derive(Clone)]
struct StandardStagingSnapshot {
    registry: Arc<HostFunctionRegistry>,
    source_generation: u64,
}

/// A private, single-use publication capability for one registry origin.
///
/// Keeping the staged registry and origin token together prevents a caller
/// from committing an arbitrary clone or committing the same staging twice.
/// The public API exposes only [`HostFunctionRegistry::transactionally`].
struct RegistryTransaction {
    origin: Arc<()>,
    staged: Option<HostFunctionRegistry>,
}

impl RegistryTransaction {
    fn registry_mut(&mut self) -> &mut HostFunctionRegistry {
        self.staged
            .as_mut()
            .expect("registry transaction must be live while staging")
    }
}

impl Clone for HostFunctionRegistry {
    fn clone(&self) -> Self {
        let snapshot = self
            .standard_staging_snapshot
            .read()
            .expect("poisoned lock")
            .clone();
        Self {
            entries: Arc::clone(&self.entries),
            by_name: Arc::clone(&self.by_name),
            by_exact: Arc::clone(&self.by_exact),
            plan_cache: Arc::clone(&self.plan_cache),
            allowed_builtin_calls: Arc::clone(&self.allowed_builtin_calls),
            allow_default_builtin_capabilities: self.allow_default_builtin_capabilities,
            allow_default_host_capabilities: self.allow_default_host_capabilities,
            capability_profile: Arc::clone(&self.capability_profile),
            registry_state: Arc::clone(&self.registry_state),
            registry_generation_token: Arc::clone(&self.registry_generation_token),
            registry_generation: Arc::clone(&self.registry_generation),
            transaction_origin: Arc::new(()),
            standard_staging_snapshot: Arc::new(RwLock::new(snapshot)),
            standard_staging_registrations: Arc::new(AtomicU64::new(
                self.standard_staging_registrations.load(Ordering::Acquire),
            )),
            composition: self.composition.clone(),
            named_struct_schemas: Arc::clone(&self.named_struct_schemas),
        }
    }
}

impl HostFunctionRegistry {
    pub fn empty() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            by_name: Arc::new(HashMap::new()),
            by_exact: Arc::new(HashMap::new()),
            plan_cache: Arc::new(RwLock::new(HashMap::new())),
            allowed_builtin_calls: Arc::new(Vec::new()),
            allow_default_builtin_capabilities: true,
            allow_default_host_capabilities: true,
            capability_profile: Arc::new(CapabilityProfile::allow_all()),
            registry_state: Arc::new(()),
            registry_generation_token: Arc::new(()),
            registry_generation: Arc::new(AtomicU64::new(0)),
            transaction_origin: Arc::new(()),
            standard_staging_snapshot: Arc::new(RwLock::new(None)),
            standard_staging_registrations: Arc::new(AtomicU64::new(0)),
            composition: None,
            named_struct_schemas: Arc::new(HashMap::new()),
        }
    }

    /// Installs named struct bodies used by VM resource walks at exact
    /// registration. Compiler import identity remains `TypeSchema::Named`.
    pub fn install_named_struct_schemas(
        &mut self,
        schemas: HashMap<String, crate::bytecode::NamedStructSchema>,
    ) {
        let map = Arc::make_mut(&mut self.named_struct_schemas);
        map.extend(schemas);
    }

    /// Derives a fresh, isolated registry origin from an immutable registry
    /// template.
    ///
    /// The outer standard-runtime layer memoizes the builtin-composed default
    /// registry template and derives every public `HostFunctionRegistry::new()`
    /// from it through this helper. The result shares the template's immutable
    /// entries/capability state but starts a *new* origin: its private
    /// transaction handle cannot publish into another registry, and its
    /// memoized staging snapshot / plan cache start empty.
    pub(crate) fn fresh_origin_clone(&self) -> Self {
        let mut clone = self.transaction_clone();
        clone.transaction_origin = Arc::new(());
        clone
    }

    /// Replaces the registry's immutable capability profile.
    pub fn set_capability_profile(&mut self, profile: CapabilityProfile) {
        self.allowed_builtin_calls = Arc::new(profile.allowed_builtin_calls().to_vec());
        self.allow_default_builtin_capabilities = profile.allows_all_builtins();
        self.allow_default_host_capabilities = profile.allows_all_host_imports();
        self.capability_profile = Arc::new(profile);
        self.invalidate_plan_cache();
    }

    /// Associate a registered namespaced standard adapter with the matching
    /// builtin capability when the caller has granted that builtin. This keeps
    /// restricted exact imports governed by the same capability profile as the
    /// legacy adapter path without granting unrelated host names.
    pub(crate) fn authorize_registered_builtin_import(&mut self, name: &str) {
        let Some(builtin) = BuiltinFunction::from_namespaced_name(name) else {
            return;
        };
        if !self.capability_profile.allows_host_import(name)
            && self.capability_profile.allows_builtin(builtin)
        {
            self.set_capability_profile(self.capability_profile.with_host_import(name));
        }
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
        let next_generation = self
            .registry_generation
            .load(Ordering::Acquire)
            .saturating_add(1);
        self.registry_state = Arc::new(());
        self.registry_generation_token = Arc::new(());
        self.registry_generation = Arc::new(AtomicU64::new(next_generation));
        self.plan_cache = Arc::new(RwLock::new(HashMap::new()));
    }

    /// Creates an isolated staging registry for a registration transaction.
    ///
    /// This helper is private so callers cannot publish an arbitrary clone.
    /// The public [`transactionally`](Self::transactionally) API owns the
    /// origin token and single-use publication handle.
    fn transaction_clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
            by_name: Arc::clone(&self.by_name),
            by_exact: Arc::clone(&self.by_exact),
            plan_cache: Arc::new(RwLock::new(HashMap::new())),
            allowed_builtin_calls: Arc::clone(&self.allowed_builtin_calls),
            allow_default_builtin_capabilities: self.allow_default_builtin_capabilities,
            allow_default_host_capabilities: self.allow_default_host_capabilities,
            capability_profile: Arc::clone(&self.capability_profile),
            registry_state: Arc::new(()),
            registry_generation_token: Arc::new(()),
            registry_generation: Arc::new(AtomicU64::new(
                self.registry_generation.load(Ordering::Relaxed),
            )),
            transaction_origin: Arc::clone(&self.transaction_origin),
            // A staging clone starts with no memoized standard snapshot: its
            // published state is what `bind_vm_cached` caches after a
            // successful stage. Sharing the origin's snapshot here would leak
            // another registry lineage's staged adapters into this one.
            standard_staging_snapshot: Arc::new(RwLock::new(None)),
            standard_staging_registrations: Arc::new(AtomicU64::new(0)),
            // The staging clone keeps the origin's caller-provided composition
            // (per-instance state), so `bind_vm_cached` on the staged registry
            // can still auto-stage missing surfaces.
            composition: self.composition.clone(),
            named_struct_schemas: Arc::clone(&self.named_struct_schemas),
        }
    }

    fn begin_transaction(&self) -> RegistryTransaction {
        RegistryTransaction {
            origin: Arc::clone(&self.transaction_origin),
            staged: Some(self.transaction_clone()),
        }
    }

    /// Publishes a transaction produced by this registry exactly once.
    ///
    /// The origin check is defensive even though the transaction type is
    /// private: it keeps future internal call sites from publishing staging
    /// from an unrelated registry lineage.
    fn commit_transaction(&mut self, transaction: &mut RegistryTransaction) -> VmResult<()> {
        if !Arc::ptr_eq(&self.transaction_origin, &transaction.origin) {
            return Err(VmError::HostError(
                "registry transaction belongs to a different registry".to_string(),
            ));
        }
        let staged = transaction.staged.take().ok_or_else(|| {
            VmError::HostError("registry transaction was already committed".to_string())
        })?;
        *self = staged;
        self.invalidate_plan_cache();
        Ok(())
    }

    /// Applies a fallible staging closure transactionally.
    ///
    /// The closure receives an isolated staging registry produced by a private
    /// transaction handle. If it returns an error, the caller's registry is
    /// left observationally unchanged (slots, plans, capability profile and
    /// revision). A panic also drops the staging handle before publication.
    /// On success the staged state is published exactly once.
    pub fn transactionally<F>(&mut self, stage: F) -> VmResult<()>
    where
        F: FnOnce(&mut HostFunctionRegistry) -> VmResult<()>,
    {
        let mut transaction = self.begin_transaction();
        stage(transaction.registry_mut())?;
        self.commit_transaction(&mut transaction)
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Factory(Arc::new(factory)),
            legacy_resource_return_key: None,
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::Static(function),
            legacy_resource_return_key: None,
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::StackFactory(Arc::new(factory)),
            legacy_resource_return_key: None,
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::StackStatic(function),
            legacy_resource_return_key: None,
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::ArgsFactory(Arc::new(factory)),
            legacy_resource_return_key: None,
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::ArgsStatic(function),
            legacy_resource_return_key: None,
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
            entry.legacy_resource_return_key = None;
            self.invalidate_plan_cache();
            return;
        }

        let entries = Arc::make_mut(&mut self.entries);
        let slot = entries.len() as u16;
        entries.push(RegistryEntry {
            arity,
            kind: RegistryEntryKind::ArgsStaticNonYielding(function),
            legacy_resource_return_key: None,
        });
        Arc::make_mut(&mut self.by_name).insert(name, slot);
        self.invalidate_plan_cache();
    }

    pub(crate) fn stage_missing_exact_imports_from(
        &mut self,
        source: &HostFunctionRegistry,
        imports: &[HostImport],
    ) -> VmResult<bool> {
        let mut staged = false;
        for import in imports {
            let Some(schema) = import.schema.as_ref() else {
                continue;
            };
            if self
                .by_exact
                .get(&import.name)
                .is_some_and(|schemas| schemas.contains_key(schema))
            {
                continue;
            }
            let source_slot = source
                .by_exact
                .get(&import.name)
                .and_then(|schemas| schemas.get(schema))
                .copied()
                .ok_or_else(|| {
                    VmError::HostImportBinding(HostImportBindingError::MissingExact {
                        import: import.name.clone(),
                    })
                })?;
            let entry = source
                .entries
                .get(usize::from(source_slot))
                .cloned()
                .ok_or_else(|| {
                    VmError::HostError(format!(
                        "exact source slot {source_slot} for '{}' is missing",
                        import.name
                    ))
                })?;
            let slot = u16::try_from(self.entries.len()).map_err(|_| {
                VmError::HostError("host function registry exceeds u16 slot capacity".to_string())
            })?;
            Arc::make_mut(&mut self.entries).push(entry);
            Arc::make_mut(&mut self.by_exact)
                .entry(import.name.clone())
                .or_default()
                .insert(schema.clone(), slot);
            self.authorize_registered_builtin_import(&import.name);
            staged = true;
        }
        if staged {
            self.invalidate_plan_cache();
        }
        Ok(staged)
    }

    /// Registers a dynamic host fn under an exact `HostImportSchema` (name + ordered param
    /// schemas/passing + return type + catalog fingerprint).
    ///
    /// The import is addressable only by programs whose `HostImport` schema equals `schema`
    /// (including the catalog fingerprint). It never occupies the legacy by-name slot.
    /// Registering an identical name+schema twice is an explicit error (no silent replacement).
    pub fn register_exact(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        factory: impl Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            true,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        let guarded = schema_requires_guard(&schema);
        let factory: Arc<HostFactory> = Arc::new(factory);
        let kind = if guarded {
            let schema_for_guard = schema.clone();
            let factory_for_guard = Arc::clone(&factory);
            RegistryEntryKind::Factory(Arc::new(move || {
                Box::new(GuardedHostFunction {
                    inner: factory_for_guard(),
                    schema: schema_for_guard.clone(),
                })
            }))
        } else {
            RegistryEntryKind::Factory(factory)
        };
        self.push_exact(name, arity, schema, kind)
    }

    pub fn register_exact_static(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        function: StaticHostFunction,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            true,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        let guarded = schema_requires_guard(&schema);
        if guarded {
            let schema_for_guard = schema.clone();
            self.push_exact(
                name,
                arity,
                schema,
                RegistryEntryKind::Factory(Arc::new(move || {
                    Box::new(GuardedStaticHostFunction {
                        function,
                        schema: schema_for_guard.clone(),
                    })
                })),
            )
        } else {
            self.push_exact(name, arity, schema, RegistryEntryKind::Static(function))
        }
    }

    pub fn register_exact_stack(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        factory: impl Fn() -> Box<dyn HostStackFunction> + Send + Sync + 'static,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            true,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        let guarded = schema_requires_guard(&schema);
        let factory: Arc<HostStackFactory> = Arc::new(factory);
        let kind = if guarded {
            let schema_for_guard = schema.clone();
            let factory_for_guard = Arc::clone(&factory);
            RegistryEntryKind::StackFactory(Arc::new(move || {
                Box::new(GuardedHostStackFunction {
                    inner: factory_for_guard(),
                    schema: schema_for_guard.clone(),
                })
            }))
        } else {
            RegistryEntryKind::StackFactory(factory)
        };
        self.push_exact(name, arity, schema, kind)
    }

    pub fn register_exact_static_stack(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        function: StaticHostStackFunction,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            true,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        let guarded = schema_requires_guard(&schema);
        if guarded {
            let schema_for_guard = schema.clone();
            self.push_exact(
                name,
                arity,
                schema,
                RegistryEntryKind::StackFactory(Arc::new(move || {
                    Box::new(GuardedStaticHostStackFunction {
                        function,
                        schema: schema_for_guard.clone(),
                    })
                })),
            )
        } else {
            self.push_exact(
                name,
                arity,
                schema,
                RegistryEntryKind::StackStatic(function),
            )
        }
    }

    pub fn register_exact_args(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        factory: impl Fn() -> Box<dyn HostArgsFunction> + Send + Sync + 'static,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            false,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        self.push_exact(
            name,
            arity,
            schema,
            RegistryEntryKind::ArgsFactory(Arc::new(factory)),
        )
    }

    pub fn register_exact_static_args(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        function: StaticHostArgsFunction,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            false,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        self.push_exact(name, arity, schema, RegistryEntryKind::ArgsStatic(function))
    }

    pub fn register_exact_static_non_yielding_args(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        function: StaticHostArgsFunction,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_with_named(
            &name,
            &schema,
            false,
            &self.named_struct_schemas,
        )
        .map_err(VmError::HostImportBinding)?;
        self.push_exact(
            name,
            arity,
            schema,
            RegistryEntryKind::ArgsStaticNonYielding(function),
        )
    }

    /// Registers an exact host fn whose dispatch drains the call operands and
    /// transfers ownership of the arguments the function takes.
    ///
    /// The exact schema follows the same validation as every other exact
    /// registration, with one owned-dispatch allowance: a `TakeOwned`
    /// parameter may carry a *resource-free* schema (a callable, scalar, or
    /// aggregate value) and is then transferred as an owned value rather than
    /// an addressable handle. Resource-bearing `TakeOwned` parameters keep
    /// their full preflight/commit contract: the owned dispatch wraps such a
    /// function in the same guarded contract every other exact binding uses.
    ///
    /// `factory` runs once per bind and receives the registry through
    /// [`OwnedHostContext`], so an owned host function can retain the
    /// immutable registry/binding configuration it needs to spawn an isolated
    /// owned-value execution VM. Registering the same name+schema twice is an
    /// explicit error (no silent replacement).
    pub fn register_exact_owned(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        schema: HostImportSchema,
        factory: impl Fn(OwnedHostContext<'_>) -> Box<dyn HostOwnedFunction> + Send + Sync + 'static,
    ) -> VmResult<u16> {
        let name = name.into();
        validate_exact_registration_schema_full(
            &name,
            &schema,
            true,
            &self.named_struct_schemas,
            true,
        )
        .map_err(VmError::HostImportBinding)?;
        let factory: Arc<OwnedHostFactory> = Arc::new(factory);
        let kind = if schema_requires_guard(&schema) {
            let schema_for_guard = schema.clone();
            let factory_for_guard = Arc::clone(&factory);
            RegistryEntryKind::OwnedFactory(Arc::new(move |context| {
                Box::new(GuardedOwnedHostFunction {
                    inner: factory_for_guard(context),
                    schema: schema_for_guard.clone(),
                })
            }))
        } else {
            RegistryEntryKind::OwnedFactory(factory)
        };
        self.push_exact(name, arity, schema, kind)
    }

    /// Core exact-schema slot pusher: duplicate (name+schema) is an explicit structured error;
    /// legacy name-only bindings live in `by_name`, exact bindings in `by_exact`, so a legacy
    /// binding can never hijack a distinct exact slot.
    ///
    /// All validation (arity vs. schema parameter count, schema return-coarse determinism,
    /// duplicate detection, and the `u16` slot-space capacity check) happens **before** any
    /// mutation, so a rejected registration leaves the registry's entries, `by_exact` map,
    /// slot numbering, plan cache and generation counter untouched.
    fn push_exact(
        &mut self,
        name: String,
        arity: u8,
        schema: HostImportSchema,
        kind: RegistryEntryKind,
    ) -> VmResult<u16> {
        // A schema with more parameters than `u8` can address can never match the `arity` of an
        // `HostImport`, so it is rejected up front (and `u8::try_from` avoids a silent truncation).
        let params_len = u8::try_from(schema.params.len()).map_err(|_| {
            VmError::HostImportBinding(HostImportBindingError::InvalidSchema {
                import: name.clone(),
                reason: format!(
                    "schema declares {} parameters; at most 255 are addressable",
                    schema.params.len()
                ),
            })
        })?;
        if params_len != arity {
            return Err(VmError::HostImportBinding(
                HostImportBindingError::SchemaArityMismatch {
                    import: name,
                    expected: params_len,
                    got: arity,
                },
            ));
        }
        // No registration-time rejection on the return schema's coarse value type.
        // A return whose `coarse_value_type()` is `Unknown` (`Number`,
        // `Optional<Number>`, `Optional<Unknown>`, `GenericParam`, ...) is a
        // legitimate structured schema and can be registered; return consistency
        // is verified later at bind time in `resolve_import` against the matched
        // schema's `coarse_value_type()`.
        if let Some(schemas) = self.by_exact.get(&name)
            && schemas.contains_key(&schema)
        {
            return Err(VmError::HostImportBinding(
                HostImportBindingError::Duplicate { import: name },
            ));
        }
        // `u16` slot space: check capacity before any map allocation or entry push so an
        // exhausted registry reports a structured error with no partial mutation.
        let slot = u16::try_from(self.entries.len()).map_err(|_| {
            VmError::HostImportBinding(HostImportBindingError::CapacityExceeded {
                import: name.clone(),
                limit: u16::MAX as usize + 1,
            })
        })?;
        let entries = Arc::make_mut(&mut self.entries);
        entries.push(RegistryEntry {
            arity,
            kind,
            legacy_resource_return_key: match &schema.return_type {
                crate::compiler::ir::TypeSchema::Resource(key) => Some(key.clone()),
                _ => None,
            },
        });
        let map = Arc::make_mut(&mut self.by_exact);
        map.entry(name).or_default().insert(schema, slot);
        self.invalidate_plan_cache();
        Ok(slot)
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
        #[cfg(feature = "runtime")]
        if let Some(decision) = self.standard_stage_decision(&vm.program.imports)? {
            // `registry` is either the memoized snapshot or the freshly staged
            // clone; both already carry every required standard surface, so
            // the bind resolves without re-registering anything.
            let plan = decision.registry.prepare_shared_plan(&vm.program.imports)?;
            return decision.registry.bind_vm_with_plan(vm, &plan);
        }
        let plan = self.prepare_shared_plan(&vm.program.imports)?;
        self.bind_vm_with_plan(vm, &plan)
    }

    /// A memoized or freshly staged registry that already satisfies every
    /// required standard adapter surface for `imports`, or `None` when no
    /// standard auto-stage is warranted.
    ///
    /// * `None` — don't auto-stage: there are no exact imports, a required
    ///   import is not from the standard catalog, or the registry already
    ///   carries a custom / mixed-fingerprint exact entry that must not be
    ///   silently combined with the standard snapshot. The caller falls back
    ///   to its normal (non-staging) resolution path.
    /// * `Some` — the returned registry already covers every required
    ///   standard surface: either the memoized snapshot (reused with zero
    ///   re-registration), the current registry (all surfaces present), or a
    ///   freshly staged clone (only the missing surfaces were added and the
    ///   snapshot memoized for later binds).
    #[cfg(feature = "runtime")]
    fn standard_stage_decision(
        &self,
        imports: &[HostImport],
    ) -> VmResult<Option<StandardStageResult>> {
        let Some(composition) = self.composition.as_ref() else {
            // No caller-provided composition on this registry instance: the
            // registry has nothing standard to stage, so fall through to the
            // ordinary (non-staging) resolution path.
            return Ok(None);
        };
        let fingerprint = composition.standard_catalog_fingerprint();
        if imports.is_empty()
            || imports
                .iter()
                .any(|import| !composition.import_in_standard(import))
        {
            return Ok(None);
        }

        // Memoized snapshot reuse: when self hasn't changed since publication,
        // ask the opaque composition to ensure this import set against a clone
        // of the cached registry. The first bind may have staged only one
        // standard surface, so direct reuse would make later imports depend on
        // bind order. A fully covering snapshot returns unchanged with zero
        // registration; newly required surfaces extend and replace the cached
        // snapshot. A later source mutation still invalidates it by generation.
        let source_generation = self.registry_generation.load(Ordering::Acquire);
        let mut snapshot_guard = self
            .standard_staging_snapshot
            .write()
            .expect("poisoned lock");
        let cached = snapshot_guard
            .as_ref()
            .filter(|snapshot| snapshot.source_generation == source_generation)
            .map(|snapshot| Arc::clone(&snapshot.registry));
        if let Some(cached) = cached {
            let mut expanded = cached.transaction_clone();
            if !composition.ensure_surfaces(imports, &mut expanded)? {
                return Ok(Some(StandardStageResult { registry: cached }));
            }
            self.standard_staging_registrations
                .fetch_add(1, Ordering::Relaxed);
            let expanded = Arc::new(expanded);
            *snapshot_guard = Some(StandardStagingSnapshot {
                registry: Arc::clone(&expanded),
                source_generation,
            });
            return Ok(Some(StandardStageResult { registry: expanded }));
        }

        // Reject custom / mixed fingerprints in the registry: an existing
        // exact entry that is not standard-fingerprint compatible must never
        // be combined with the standard snapshot. Restricted registries keep
        // their capability policy; we simply don't auto-stage.
        for schemas in self.by_exact.values() {
            if schemas
                .keys()
                .any(|schema| schema.fingerprint != fingerprint)
            {
                return Ok(None);
            }
        }
        let mut staged = self.transaction_clone();
        // One opaque required/present/stage call: the composition
        // implementation computes which surfaces `imports` requires and which
        // `staged` already carries, and registers exactly the missing ones.
        // The core never sees a surface mask or count.
        if !composition.ensure_surfaces(imports, &mut staged)? {
            // Every required exact entry is already present. Release the
            // publication lock before cloning because `Clone` snapshots this
            // registry's cache into a detached lock.
            drop(snapshot_guard);
            return Ok(Some(StandardStageResult {
                registry: Arc::new(self.clone()),
            }));
        }
        self.standard_staging_registrations
            .fetch_add(1, Ordering::Relaxed);
        let staged = Arc::new(staged);
        // Publish the staged snapshot so subsequent binds reuse it without
        // re-registering: the fully-staged registry is the immutable template,
        // guarded by the source registry's current generation.
        *snapshot_guard = Some(StandardStagingSnapshot {
            registry: Arc::clone(&staged),
            source_generation,
        });
        Ok(Some(StandardStageResult { registry: staged }))
    }

    /// Deterministic count of standard auto-stage registration rounds performed
    /// by this registry lineage through [`bind_vm_cached`]. A second bind that
    /// reuses the memoized snapshot does not increment it.
    pub fn standard_staging_registrations(&self) -> u64 {
        self.standard_staging_registrations.load(Ordering::Relaxed)
    }

    /// Installs the caller-provided standard-surface composition on this
    /// registry instance (explicit per-instance state). The outer
    /// standard-runtime constructor calls this so the registry's
    /// `bind_vm_cached` auto-stage path can compose the standard surfaces
    /// without the core knowing them.
    ///
    /// Replacing the composition is a *registry mutation*: it invalidates the
    /// memoized staging snapshot and the plan cache/generation so a bind under
    /// the new composition can never reuse a snapshot staged under a previous
    /// composition.
    pub fn set_standard_composition(
        &mut self,
        composition: Arc<dyn super::standard_composition::StandardSurfaceComposition>,
    ) {
        self.composition = Some(composition);
        self.invalidate_plan_cache();
        *self
            .standard_staging_snapshot
            .write()
            .expect("poisoned lock") = None;
    }

    /// The memoized fully-staged standard snapshot, if one was published.
    pub fn standard_staging_snapshot(&self) -> Option<Arc<HostFunctionRegistry>> {
        self.standard_staging_snapshot
            .read()
            .expect("poisoned lock")
            .as_ref()
            .map(|snapshot| Arc::clone(&snapshot.registry))
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

    /// Resolves a host import to its exact registry slot.
    ///
    /// * `schema: Some(schema)` — the slot is the exact-schema binding whose `HostImportSchema`
    ///   equals `schema` (including the catalog fingerprint). A legacy by-name slot is **never**
    ///   used as a fallback; a mismatch is a structured rejection.
    /// * `schema: None` — legacy by-name (schema-less) resolution, unchanged.
    pub fn resolve_import(&self, import: &HostImport) -> VmResult<u16> {
        match import.schema.as_ref() {
            Some(schema) => {
                let slot = self
                    .by_exact
                    .get(&import.name)
                    .and_then(|schemas| schemas.get(schema))
                    .copied()
                    .ok_or_else(|| {
                        VmError::HostImportBinding(HostImportBindingError::MissingExact {
                            import: import.name.clone(),
                        })
                    })?;
                // arity / coarse return-type consistency against the resolved schema:
                if schema.params.len() as u8 != import.arity {
                    return Err(VmError::InvalidCallArity {
                        import: import.name.clone(),
                        expected: schema.params.len() as u8,
                        got: import.arity,
                    });
                }
                if import.return_type != schema.return_type.coarse_value_type() {
                    return Err(VmError::HostImportBinding(
                        HostImportBindingError::ReturnTypeMismatch {
                            import: import.name.clone(),
                            expected: schema.return_type.coarse_value_type(),
                            got: import.return_type,
                        },
                    ));
                }
                Ok(slot)
            }
            None => {
                let slot = self
                    .by_name
                    .get(&import.name)
                    .copied()
                    .ok_or_else(|| VmError::UnboundImport(import.name.clone()))?;
                if self
                    .entries
                    .get(slot as usize)
                    .ok_or(VmError::InvalidCall(slot))?
                    .arity
                    != import.arity
                {
                    return Err(VmError::InvalidCallArity {
                        import: import.name.clone(),
                        expected: self.entries[slot as usize].arity,
                        got: import.arity,
                    });
                }
                Ok(slot)
            }
        }
    }

    /// Number of distinct plan-cache entries. Each is keyed by the full `Vec<HostImport>`,
    /// which embeds each import's schema, so exact schemas are plan-cache-partitioned.
    pub fn plan_cache_len(&self) -> usize {
        self.plan_cache.read().expect("plan cache read lock").len()
    }

    /// Current registry revision used to invalidate cached binding plans.
    pub fn registry_generation(&self) -> u64 {
        self.registry_generation.load(Ordering::Relaxed)
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
            let registry_slot = self.resolve_import(import)?;
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

        let mut allowed_host_function_slots = imports
            .iter()
            .zip(resolved_calls.iter().copied())
            .filter_map(|(import, vm_slot)| {
                self.capability_profile
                    .allows_host_import(&import.name)
                    .then_some(vm_slot)
            })
            .collect::<Vec<_>>();
        allowed_host_function_slots.sort_unstable();
        allowed_host_function_slots.dedup();
        let legacy_resource_return_keys = registry_slots
            .iter()
            .map(|slot| {
                self.entries
                    .get(*slot as usize)
                    .and_then(|entry| entry.legacy_resource_return_key.clone())
            })
            .collect();
        let import_key = imports.to_vec();
        let computed = Arc::new(HostBindingPlan {
            import_signature: import_key.clone(),
            registry_slots,
            legacy_resource_return_keys,
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
                RegistryEntryKind::OwnedFactory(factory) => {
                    vm.register_owned_function(factory(OwnedHostContext { registry: self }));
                }
            }
        }
        vm.set_default_host_fallback_enabled(false);
        vm.host.legacy_resource_return_keys = plan.legacy_resource_return_keys.clone();
        vm.host.allowed_builtin_calls = plan.allowed_builtin_calls.clone();
        vm.host.allow_default_builtin_capabilities = plan.allow_default_builtin_capabilities;
        vm.host.allowed_host_function_slots = plan.allowed_host_function_slots.clone();
        vm.host.allow_default_host_capabilities = plan.allow_default_host_capabilities;
        vm.install_resolved_calls(plan.resolved_calls.clone())?;
        // Propagate the registry's caller-provided standard-surface composition
        // to the VM (explicit per-instance state): a VM bound by a standard
        // registry carries that composition forward for its default-fallback
        // paths. This is the outer standard-runtime constructor path — never a
        // hidden installation inside `HostRuntime::new()`.
        if let Some(composition) = self.composition.clone() {
            vm.host.standard_composition = Some(composition);
        }
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
    /// Owned-dispatch binding: the call operands are drained from the guest
    /// operand stack and ownership of the taken arguments transfers to the
    /// function. The option is empty only while the function is executing;
    /// this avoids borrowing a registry vector across `HostOwnedFunction::call`.
    OwnedDynamic(Option<Box<dyn HostOwnedFunction>>),
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

/// Exact-return policy for an interpreter host call, derived from the targeted
/// `HostImport.schema` (C1/C4 resource ABI scope).
///
/// * `Legacy` — no exact schema, or a non-resource exact return: old behavior.
/// * `Resource(key)` — the exact return is `TypeSchema::Resource`: the returned
///   value must be an `Int` that decodes as a structurally valid resource
///   handle whose **live slot key** equals `key`; the handle is then marked
///   guest-owned (ownership transfers HostOwned → GuestOwned) before any stack
///   mutation.
/// * `OptionalResource(key)` — the exact return is
///   `TypeSchema::Optional(Resource)`: `Value::Null` is a legal no-resource
///   return; a handle is validated and transferred exactly like `Resource`.
/// * `NestedResource` — the exact return schema *nest-contains* a resource
///   (inside an `Array`, `Map`, deeper `Optional`, ...) that the current
///   `Value::Int` handle-carrier ABI cannot represent: any returned value is an
///   explicit structured rejection; there is no silent coarse pass-through.
///
/// The expected key rides along so every sync from-stack / static / args /
/// async completion transfer verifies the live slot key before the ownership
/// mark (C4 `ResourceKeyMismatch`). The policy is `Clone` (the expected key
/// is owned), which is enough for the async waiting-op snapshot — it is
/// moved (never copied) out of the waiting slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExactHostReturnPolicy {
    Legacy,
    Resource(ResourceTypeKey),
    OptionalResource(ResourceTypeKey),
    NestedResource,
}

impl ExactHostReturnPolicy {
    fn key(&self) -> Option<&ResourceTypeKey> {
        match self {
            Self::Resource(key) | Self::OptionalResource(key) => Some(key),
            Self::Legacy | Self::NestedResource => None,
        }
    }

    fn transfers_ownership(&self) -> bool {
        self.key().is_some()
    }
}

/// Classifies an import's exact-return policy from its resolved exact schema.
#[cfg(test)]
pub(crate) fn exact_host_return_policy(import: Option<&HostImport>) -> ExactHostReturnPolicy {
    exact_host_return_policy_with_named(import, &HashMap::new())
}

pub(crate) fn exact_host_return_policy_with_named(
    import: Option<&HostImport>,
    named_struct_schemas: &HashMap<String, crate::bytecode::NamedStructSchema>,
) -> ExactHostReturnPolicy {
    use crate::compiler::TypeSchema;
    let Some(schema) = import.and_then(|import| import.schema.as_ref()) else {
        return ExactHostReturnPolicy::Legacy;
    };
    match &schema.return_type {
        TypeSchema::Resource(key) => ExactHostReturnPolicy::Resource(key.clone()),
        TypeSchema::Optional(inner) => match inner.as_ref() {
            TypeSchema::Resource(key) => ExactHostReturnPolicy::OptionalResource(key.clone()),
            other if schema_contains_resource_named(other, named_struct_schemas) => {
                ExactHostReturnPolicy::NestedResource
            }
            _ => ExactHostReturnPolicy::Legacy,
        },
        other if schema_contains_resource_named(other, named_struct_schemas) => {
            ExactHostReturnPolicy::NestedResource
        }
        _ => ExactHostReturnPolicy::Legacy,
    }
}

/// Validates a single host-returned value against the exact-return policy.
///
/// `Legacy` keeps the coarse `ValueType` consistency check (or passes the value
/// through unchanged when the caller historically pushed without validation —
/// the coarse check is supplied via `expected` by the non-yielding paths).
pub(crate) fn validate_exact_host_return_value(
    value: Value,
    policy: ExactHostReturnPolicy,
    expected: Option<ValueType>,
) -> VmResult<Value> {
    match policy {
        ExactHostReturnPolicy::Legacy => validate_non_yielding_host_value(value, expected),
        ExactHostReturnPolicy::Resource(_) | ExactHostReturnPolicy::OptionalResource(_) => {
            if ResourceHandle::from_value(&value).is_ok()
                || (matches!(value, Value::Null)
                    && matches!(policy, ExactHostReturnPolicy::OptionalResource(_)))
            {
                Ok(value)
            } else {
                Err(VmError::TypeMismatch("resource handle"))
            }
        }
        ExactHostReturnPolicy::NestedResource => Err(VmError::TypeMismatch(
            "nested resource return cannot be represented by the current ABI",
        )),
    }
}

/// Validates a `CallReturn` before it is pushed to the operand stack.
///
/// `Legacy` pushes the values unchanged (old behavior); `Resource`/`Optional`
/// require a single structurally-valid handle (or `Null` for the optional);
/// `NestedResource` rejects any return.
pub(crate) fn validate_exact_host_return_values(
    values: CallReturn,
    policy: ExactHostReturnPolicy,
) -> VmResult<CallReturn> {
    match policy {
        ExactHostReturnPolicy::Legacy => Ok(values),
        ExactHostReturnPolicy::Resource(_) | ExactHostReturnPolicy::OptionalResource(_) => {
            match values {
                CallReturn::One(value) => {
                    if ResourceHandle::from_value(&value).is_ok()
                        || (matches!(value, Value::Null)
                            && matches!(policy, ExactHostReturnPolicy::OptionalResource(_)))
                    {
                        Ok(CallReturn::One(value))
                    } else {
                        Err(VmError::TypeMismatch("resource handle"))
                    }
                }
                CallReturn::None => Err(VmError::TypeMismatch(
                    "resource-returning host produced no value",
                )),
            }
        }
        ExactHostReturnPolicy::NestedResource => Err(VmError::TypeMismatch(
            "nested resource return cannot be represented by the current ABI",
        )),
    }
}

#[inline]
fn builtin_for_binding_name(name: &str) -> Option<BuiltinFunction> {
    if !name.contains("::") {
        return None;
    }
    BuiltinFunction::from_namespaced_name(name)
}

impl Vm {
    fn mark_host_bindings_dirty_for_registration(&mut self) {
        // Registration only appends a host-function slot; existing per-import
        // resolutions keep pointing at their original slots. So a resolved-call
        // cache that already carries exactly one entry per declared import
        // stays valid and must not be marked dirty. Only an absent or
        // incomplete cache (size != import count, i.e. never installed for
        // this program) needs re-resolution against the registry.
        if self.host.resolved_calls.len() != self.program.imports.len() {
            self.host.resolved_calls_dirty = true;
        }
    }

    pub fn register_function(&mut self, function: Box<dyn HostFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::Dynamic(function));
        self.mark_host_bindings_dirty_for_registration();
        index
    }

    /// Registers an owned-dispatch host function on this VM.
    ///
    /// Owned bindings are normally created by
    /// [`HostFunctionRegistry::register_exact_owned`] and installed through
    /// the registry bind; this entry point exists for the bind path itself and
    /// for embeddings that compose their own slot table.
    pub fn register_owned_function(&mut self, function: Box<dyn HostOwnedFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::OwnedDynamic(Some(function)));
        self.mark_host_bindings_dirty_for_registration();
        index
    }

    pub fn register_static_function(&mut self, function: StaticHostFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::Static(function));
        self.mark_host_bindings_dirty_for_registration();
        index
    }

    pub fn register_stack_function(&mut self, function: Box<dyn HostStackFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::StackDynamic(function));
        self.mark_host_bindings_dirty_for_registration();
        index
    }

    pub fn register_static_stack_function(&mut self, function: StaticHostStackFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::StackStatic(function));
        self.mark_host_bindings_dirty_for_registration();
        index
    }

    pub fn register_args_function(&mut self, function: Box<dyn HostArgsFunction>) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsDynamic(function));
        self.mark_host_bindings_dirty_for_registration();
        index
    }

    pub fn register_static_args_function(&mut self, function: StaticHostArgsFunction) -> u16 {
        let index = self.host.host_functions.len() as u16;
        self.host
            .host_functions
            .push(VmHostFunction::ArgsStatic(function));
        self.mark_host_bindings_dirty_for_registration();
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
        self.mark_host_bindings_dirty_for_registration();
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

    pub fn set_runtime_print_sink<F>(&mut self, sink: F)
    where
        F: FnMut(String) + Send + 'static,
    {
        self.host.runtime_print_sink = Some(Box::new(sink));
    }

    pub fn clear_runtime_print_sink(&mut self) {
        self.host.runtime_print_sink = None;
    }

    /// Enables or disables implicit binding of built-in host functions.
    ///
    /// Disabling this makes the VM use only explicitly registered host functions. The default
    /// remains enabled for backwards compatibility until a registry is bound.
    pub fn set_default_host_fallback_enabled(&mut self, enabled: bool) {
        self.host.allow_default_host_fallback = enabled;
        self.host.resolved_calls_dirty = true;
    }

    pub fn default_host_fallback_enabled(&self) -> bool {
        self.host.allow_default_host_fallback
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

    fn effective_exact_host_return_policy(&self, import_index: u16) -> ExactHostReturnPolicy {
        let policy = exact_host_return_policy_with_named(
            self.program.imports.get(usize::from(import_index)),
            &self.program.named_struct_schemas,
        );
        if !matches!(policy, ExactHostReturnPolicy::Legacy) {
            return policy;
        }
        let Some(vm_slot) = self.host.resolved_calls.get(usize::from(import_index)) else {
            return policy;
        };
        self.host
            .legacy_resource_return_keys
            .get(usize::from(*vm_slot))
            .and_then(|key| key.clone())
            .map(ExactHostReturnPolicy::Resource)
            .unwrap_or(policy)
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
        let exact_policy = self.effective_exact_host_return_policy(index);
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
                exact_policy,
            );
        }
        if self.bound_host_function_uses_args_slice(resolved_index)? {
            self.execute_bound_args_host_function(
                resolved_index,
                argc,
                call_ip,
                expected_return_type,
                exact_policy,
            )
        } else if self.bound_host_function_uses_stack_borrow(resolved_index)? {
            self.execute_bound_stack_host_function(resolved_index, argc, call_ip, exact_policy)
        } else if self.bound_host_function_is_owned(resolved_index)? {
            self.execute_bound_owned_host_function(resolved_index, argc, call_ip, exact_policy)
        } else {
            self.execute_bound_host_function_from_stack(resolved_index, argc, call_ip, exact_policy)
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
            self.execute_bound_args_host_function(
                resolved_index,
                argc,
                call_ip,
                None,
                ExactHostReturnPolicy::Legacy,
            )
        } else if self.bound_host_function_uses_stack_borrow(resolved_index)? {
            self.execute_bound_stack_host_function(
                resolved_index,
                argc,
                call_ip,
                ExactHostReturnPolicy::Legacy,
            )
        } else {
            self.execute_bound_host_function_from_stack(
                resolved_index,
                argc,
                call_ip,
                ExactHostReturnPolicy::Legacy,
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
                self.set_waiting_bound_host_op(op_id, ExactHostReturnPolicy::Legacy)?;
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
        exact_policy: ExactHostReturnPolicy,
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
                | VmHostFunction::ArgsStaticNonYielding(_)
                | VmHostFunction::OwnedDynamic(_) => unreachable!(),
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
                // Validate BEFORE any stack mutation; on failure keep the
                // pre-call snapshot (no half-truncated state).
                let values = match validate_exact_host_return_values(values, exact_policy.clone()) {
                    Ok(values) => values,
                    Err(error) => {
                        self.instance.stack = saved_stack;
                        return Err(error);
                    }
                };
                // Exact `Resource` returns transfer ownership: the returned
                // handle's table entry moves HostOwned -> GuestOwned here,
                // before any stack mutation. A structurally valid handle that
                // is foreign/stale/already-guest/taken/closing is a structured
                // error that leaves the pre-call stack untouched.
                if let Err(error) =
                    self.transfer_exact_host_return_ownership(&values, exact_policy.clone())
                {
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
                self.record_callable_stream_resume_ip(op_id, resume_ip);
                self.set_waiting_bound_host_op(op_id, exact_policy.clone())?;
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

    /// Whether the resolved slot is an owned-dispatch binding.
    fn bound_host_function_is_owned(&self, resolved_index: u16) -> VmResult<bool> {
        let function = self
            .host
            .host_functions
            .get(resolved_index as usize)
            .ok_or(VmError::InvalidCall(resolved_index))?;
        Ok(matches!(function, VmHostFunction::OwnedDynamic(_)))
    }

    /// Executes an owned host function: the call operands are drained from the
    /// operand stack into an [`OwnedHostCall`] and ownership of the arguments
    /// the function takes transfers to it.
    ///
    /// Contract:
    ///
    /// * every failure path — a host error, a panic, a rejected `Yield`, or a
    ///   rejected exact return (`validate` or ownership transfer) — restores
    ///   every argument the host did **not** take to the operand stack exactly
    ///   once, so nothing is lost and nothing is dropped twice;
    /// * arguments the host took stay consumed on those paths, and values the
    ///   handler pushed onto the operand stack remain on it;
    /// * a successful call leaves the guest source consumed (the taken
    ///   arguments moved into the host, the rest released);
    /// * `Yield` is unsupported for owned dispatch: the drained operands
    ///   cannot be re-executed, so the outcome is rejected as a structured
    ///   [`VmError::HostError`] and the instruction pointer is left past the
    ///   call instruction rather than rewound for a retry;
    /// * `Halt`, `Pending`, and exact-return validation follow the ordinary
    ///   bound-host-call contract;
    /// * panic handling unwinds without restoring already-taken arguments
    ///   (they are owned by the host at that point) while still restoring the
    ///   untaken remainder through the same failure path.
    pub(super) fn execute_bound_owned_host_function(
        &mut self,
        resolved_index: u16,
        argc: usize,
        call_ip: usize,
        exact_policy: ExactHostReturnPolicy,
    ) -> VmResult<HostCallExecOutcome> {
        let arg_start = self
            .instance
            .stack
            .len()
            .checked_sub(argc)
            .ok_or(VmError::StackUnderflow)?;
        // Take the owned function out of its option slot before borrowing the
        // VM. The slot remains present as `OwnedDynamic(None)`, which rejects
        // same-slot re-entry without holding a vector borrow across the call.
        let mut function = {
            let slot = self
                .host
                .host_functions
                .get_mut(resolved_index as usize)
                .ok_or(VmError::InvalidCall(resolved_index))?;
            match slot {
                VmHostFunction::OwnedDynamic(function) => function.take().ok_or_else(|| {
                    VmError::HostError("owned host function is already executing".to_string())
                })?,
                _ => unreachable!("owned dispatch requires an owned host function"),
            }
        };
        let mut saved_stack = std::mem::take(&mut self.instance.stack);
        self.instance.call_depth += 1;
        let args = saved_stack.split_off(arg_start);
        let (call_result, untaken) = {
            let mut call = OwnedHostCall::new(self, args);
            let call_result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| function.call(&mut call)));
            let depth = call.vm().instance.call_depth;
            call.vm().instance.call_depth = depth.saturating_sub(1);
            // Values not taken by the host are kept for every failure path —
            // an ordinary error, a panic, a rejected `Yield`, and a rejected
            // exact return all restore them exactly once. A successful
            // `Return`/`Halt`/`Pending` call owns every value it took, and the
            // remaining drained values are released when this vector drops.
            let untaken = call.into_untaken_args();
            (call_result, untaken)
        };

        // Restore the function after the call has released every borrow of the
        // VM. Appending is the defensive fallback for a host handler that
        // replaces its slot while mutating the registration vector; the
        // function remains owned and is never dropped or used through a stale
        // pointer. Normal append-only mutation retains the original slot.
        let mut function = Some(function);
        let restored = match self.host.host_functions.get_mut(resolved_index as usize) {
            Some(VmHostFunction::OwnedDynamic(slot)) if slot.is_none() => {
                *slot = function.take();
                true
            }
            _ => false,
        };
        if !restored {
            self.host
                .host_functions
                .push(VmHostFunction::OwnedDynamic(function));
        }

        let mut host_stack = std::mem::take(&mut self.instance.stack);
        let outcome = match call_result {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(err)) => {
                let mut untaken = untaken;
                saved_stack.append(&mut untaken);
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                return Err(err);
            }
            Err(payload) => {
                let mut untaken = untaken;
                saved_stack.append(&mut untaken);
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                std::panic::resume_unwind(payload);
            }
        };

        match outcome {
            CallOutcome::Return(values) => {
                // Validate BEFORE any stack mutation. The owned call has
                // already accepted (and possibly taken) its operands, so a
                // rejected exact return restores every argument the host did
                // not take exactly once — the same failure contract as an
                // `Ok(Err)` outcome — while arguments the host took stay
                // consumed and the host stack keeps every value the handler
                // pushed.
                let values = match validate_exact_host_return_values(values, exact_policy.clone()) {
                    Ok(values) => values,
                    Err(error) => {
                        let mut untaken = untaken;
                        saved_stack.append(&mut untaken);
                        saved_stack.append(&mut host_stack);
                        self.instance.stack = saved_stack;
                        return Err(error);
                    }
                };
                // A structurally valid handle that is foreign/stale/already
                // guest-owned/taken/closing is a structured error on the same
                // terms: untaken arguments are restored, taken ones stay taken.
                if let Err(error) =
                    self.transfer_exact_host_return_ownership(&values, exact_policy.clone())
                {
                    let mut untaken = untaken;
                    saved_stack.append(&mut untaken);
                    saved_stack.append(&mut host_stack);
                    self.instance.stack = saved_stack;
                    return Err(error);
                }
                saved_stack.append(&mut host_stack);
                values.push_onto_stack(&mut saved_stack);
                self.instance.stack = saved_stack;
                Ok(HostCallExecOutcome::Returned)
            }
            CallOutcome::Halt => {
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                Ok(HostCallExecOutcome::Halted)
            }
            CallOutcome::Yield => {
                // Owned dispatch drains the call operands before the handler
                // runs, so a yielded call cannot be re-executed: its arguments
                // no longer exist on the guest stack. Reject the outcome as a
                // structured error and restore every untaken argument through
                // the ordinary failure path; values the handler already took
                // remain owned by the handler. The instruction pointer stays
                // past the call instruction — it is never rewound for a retry.
                let mut untaken = untaken;
                saved_stack.append(&mut untaken);
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                Err(VmError::HostError(
                    "owned host function returned yield, which owned dispatch does not support"
                        .to_string(),
                ))
            }
            CallOutcome::Pending(op_id) => {
                saved_stack.append(&mut host_stack);
                self.instance.stack = saved_stack;
                let resume_ip = self.call_resume_ip(call_ip)?;
                self.record_callable_stream_resume_ip(op_id, resume_ip);
                self.set_waiting_bound_host_op(op_id, exact_policy.clone())?;
                self.instance.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
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
        exact_policy: ExactHostReturnPolicy,
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
        let value =
            validate_exact_host_return_value(value, exact_policy.clone(), expected_return_type)?;
        self.transfer_exact_host_return_ownership_value(&value, exact_policy.clone())?;
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
        exact_policy: ExactHostReturnPolicy,
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
                | VmHostFunction::StackStatic(_)
                | VmHostFunction::OwnedDynamic(_) => unreachable!(),
            }
        };
        self.instance.call_depth = self.instance.call_depth.saturating_sub(1);
        let (outcome, non_yielding) = outcome;
        let outcome = outcome?;
        if non_yielding {
            let value = require_non_yielding_host_value(outcome)?;
            let value = validate_exact_host_return_value(
                value,
                exact_policy.clone(),
                expected_return_type,
            )?;
            self.transfer_exact_host_return_ownership_value(&value, exact_policy.clone())?;
            self.instance.stack.truncate(arg_start);
            self.instance.stack.push(value);
            return Ok(HostCallExecOutcome::Returned);
        }

        match outcome {
            CallOutcome::Return(values) => {
                // Validate BEFORE truncating the call operands or pushing; a
                // bad return must leave the stack at its pre-call snapshot.
                let values = validate_exact_host_return_values(values, exact_policy.clone())?;
                self.transfer_exact_host_return_ownership(&values, exact_policy.clone())?;
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
                self.record_callable_stream_resume_ip(op_id, resume_ip);
                self.set_waiting_bound_host_op(op_id, exact_policy.clone())?;
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
        exact_policy: ExactHostReturnPolicy,
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
                | VmHostFunction::ArgsStaticNonYielding(_)
                | VmHostFunction::OwnedDynamic(_) => unreachable!(),
            }
        };
        self.instance.call_depth = self.instance.call_depth.saturating_sub(1);
        let outcome = outcome?;

        match outcome {
            CallOutcome::Return(values) => {
                // Validate BEFORE truncating the call operands or pushing; a
                // bad return must leave the stack at its pre-call snapshot.
                let values = validate_exact_host_return_values(values, exact_policy.clone())?;
                self.transfer_exact_host_return_ownership(&values, exact_policy.clone())?;
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
                self.record_callable_stream_resume_ip(op_id, resume_ip);
                self.set_waiting_bound_host_op(op_id, exact_policy.clone())?;
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

    /// Decodes `op_id` and verifies that it names a live operation in this
    /// VM's current execution scope.
    ///
    /// Bound-host admission and external completion share this validation so
    /// malformed, stale, and foreign ids are rejected consistently before
    /// either waiting state or an operation driver can be mutated.
    pub(super) fn validate_current_scope_operation_id(
        &self,
        op_id: HostOpId,
    ) -> VmResult<crate::vm::operation::OperationId> {
        let scope_id = crate::vm::operation::OperationId::from_raw(op_id).map_err(|error| {
            VmError::Operation(
                crate::vm::operation::OperationError::new(
                    error.code(),
                    "vm::host-operation-id",
                    format!("host operation id {op_id} is not a valid packed operation id"),
                )
                .with_value(op_id),
            )
        })?;
        let status = self
            .host
            .execution_scope()
            .operations()
            .status(scope_id)
            .map_err(VmError::Operation)?;
        if status != crate::vm::operation::OperationStatus::Pending {
            return Err(VmError::Operation(
                crate::vm::operation::OperationError::new(
                    crate::vm::operation::OperationErrorCode::OperationNotPending,
                    "vm::host-operation-admission",
                    format!(
                        "operation {op_id} is terminal ({status:?}); only Pending operations may enter Waiting"
                    ),
                )
                .with_value(op_id),
            ));
        }
        Ok(scope_id)
    }

    /// Validates that a bound HostFunction's `CallOutcome::Pending` id is a
    /// live operation in this VM's current `ExecutionScope`, then records the
    /// VM's waiting state with the call-site exact-return policy.
    ///
    /// This is the single path by which *any* bound host slot's pending result
    /// enters Waiting. Every production pending host operation is a real
    /// execution-scope operation: the packed id must decode to a
    /// [`crate::vm::operation::OperationId`] that is currently registered in
    /// this VM's scope. A fabricated arbitrary / stale / foreign / zero id is
    /// rejected with a structured [`VmError::Operation`] **before** the VM
    /// enters Waiting — there is no legacy lifecycle distinction between
    /// runtime-owned builtins and embedder custom operations.
    fn set_waiting_bound_host_op(
        &mut self,
        op_id: HostOpId,
        exact_policy: ExactHostReturnPolicy,
    ) -> VmResult<()> {
        self.validate_current_scope_operation_id(op_id)?;
        self.set_waiting_operation(op_id, exact_policy)
    }

    /// Test-only waiting-state helper.
    ///
    /// Constructs the VM's waiting state for an operation that was already
    /// registered as a live current-scope operation (e.g. via
    /// `submit_host_future`). Routes through the same scope-membership
    /// validation as every production bound-pending path, so it can never
    /// accept a fabricated arbitrary id — it only exists to drive
    /// poll/cancel/completion unit tests without executing a program.
    #[cfg(test)]
    pub(super) fn set_waiting_host_op(&mut self, op_id: HostOpId) -> VmResult<()> {
        self.set_waiting_bound_host_op(op_id, ExactHostReturnPolicy::Legacy)
    }

    fn set_waiting_operation(
        &mut self,
        op_id: HostOpId,
        exact_policy: ExactHostReturnPolicy,
    ) -> VmResult<()> {
        if let Some(active) = self.instance.waiting_host_op.clone()
            && active.op_id != op_id
        {
            return Err(VmError::HostError(format!(
                "vm already waiting on host op {}, cannot wait on {}",
                active.op_id, op_id
            )));
        }
        self.instance.waiting_host_op = Some(WaitingHostOp {
            op_id,
            exact_policy,
        });
        Ok(())
    }

    pub(super) fn complete_waiting_host_op(
        &mut self,
        op_id: HostOpId,
        values: CallReturn,
    ) -> VmResult<()> {
        let waiting = self.instance.waiting_host_op.take().ok_or_else(|| {
            VmError::HostError(format!(
                "host op {op_id} completed but vm is not waiting on any op",
            ))
        })?;
        if waiting.op_id != op_id {
            let active_id = waiting.op_id;
            self.instance.waiting_host_op = Some(waiting);
            return Err(VmError::HostError(format!(
                "host op {op_id} completed while vm waits on {active_id}"
            )));
        }
        self.finish_taken_waiting_host_op(waiting, values)
    }

    pub(super) fn finish_taken_waiting_host_op(
        &mut self,
        waiting: WaitingHostOp,
        values: CallReturn,
    ) -> VmResult<()> {
        let values = validate_exact_host_return_values(values, waiting.exact_policy.clone())?;
        // Exact `Resource` async completions transfer ownership the same way
        // a synchronous return does: the handle's table entry moves
        // HostOwned -> GuestOwned before any stack mutation. A structurally
        // valid handle that is foreign/stale/already-guest/taken/closing is a
        // structured error that terminates the waiting op and leaves the
        // stack untouched.
        self.transfer_exact_host_return_ownership(&values, waiting.exact_policy)?;
        values.push_onto_stack(&mut self.instance.stack);
        Ok(())
    }

    /// Transfers a resource produced at an asynchronous materialization boundary
    /// only when the active return contract is legacy/schema-less. Exact
    /// contracts deliberately remain HostOwned here so their single strict
    /// transfer still occurs in the generic exact-return path.
    pub(crate) fn transfer_legacy_materialized_resource(
        &mut self,
        handle: ResourceHandle,
        expected_key: ResourceTypeKey,
    ) -> VmResult<()> {
        let policy = self
            .instance
            .waiting_host_op
            .as_ref()
            .map(|waiting| waiting.exact_policy.clone())
            .ok_or_else(|| {
                VmError::HostError(
                    "legacy resource materialization requires a waiting host operation".to_string(),
                )
            })?;
        if matches!(policy, ExactHostReturnPolicy::Legacy) {
            self.host
                .execution_scope_mark_guest_owned_with_key(handle, &expected_key)
                .map_err(VmError::from)?;
        }
        Ok(())
    }

    // ---- exact host-return ownership transfer (C1/C4) ----------------------

    /// Transfers ownership of a validated exact resource host return from
    /// HostOwned to GuestOwned in the current execution scope, before any
    /// stack mutation.
    ///
    /// Only the `Resource`/`OptionalResource` policies transfer; `Legacy` and
    /// `NestedResource` keep their prior behavior (NestedResource is already
    /// rejected by validation, so this is a no-op there). The transfer first
    /// verifies the returned handle's **live slot key** matches the schema's
    /// expected key: a mismatch is a structured `ResourceKeyMismatch` that
    /// leaves the resource HostOwned. Any other mark failure (foreign arena,
    /// stale generation, already taken, closing/closed, or already
    /// guest-owned) is also a structured `VmError` — the caller keeps the
    /// pre-call stack snapshot, exactly like a validation failure.
    fn transfer_exact_host_return_ownership(
        &mut self,
        values: &CallReturn,
        policy: ExactHostReturnPolicy,
    ) -> VmResult<()> {
        if !policy.transfers_ownership() {
            return Ok(());
        }
        let Some(value) = values.as_slice().first() else {
            return Ok(());
        };
        self.transfer_exact_host_return_ownership_value(value, policy)
    }

    /// Single-value variant of
    /// [`transfer_exact_host_return_ownership`](Self::transfer_exact_host_return_ownership).
    fn transfer_exact_host_return_ownership_value(
        &mut self,
        value: &Value,
        policy: ExactHostReturnPolicy,
    ) -> VmResult<()> {
        if !policy.transfers_ownership() {
            return Ok(());
        }
        let Some(key) = policy.key() else {
            return Ok(());
        };
        let handle = match ResourceHandle::from_value(value) {
            Ok(handle) => handle,
            // A validated `Null` optional return carries no resource; a value
            // that was already validated as a structurally valid handle is
            // decoded above. Any other decode failure here is a defensive
            // inconsistency, not a runtime condition.
            Err(_) => return Ok(()),
        };
        self.host
            .execution_scope_mark_guest_owned_with_key(handle, key)
            .map_err(VmError::from)
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
            let has_exact_import = self
                .program
                .imports
                .iter()
                .any(|import| import.schema.is_some());
            let has_legacy_import = self
                .program
                .imports
                .iter()
                .any(|import| import.schema.is_none());
            if has_exact_import && !has_legacy_import {
                // The default fallback for a bare exact-import program stages
                // every *enabled* standard surface from the caller-provided
                // composition layer (IO under `runtime`, HTTP under
                // `http-client`, SQLite under `sqlite`). The VM core never
                // names a concrete builtin module or feature.
                let Some(composition) = self.host.standard_composition.clone() else {
                    return Err(VmError::UnboundImport(
                        "exact import requires a standard composition".to_string(),
                    ));
                };
                let registry = composition.build_default_registry()?;
                registry.bind_vm_cached(self)?;
                return Ok(());
            }

            // Only schema-less legacy imports may take the name-only default
            // fallback; an import carrying an exact schema must resolve
            // exclusively through the exact registry (see below).
            let import_names = self
                .program
                .imports
                .iter()
                .filter(|import| import.schema.is_none())
                .map(|import| import.name.clone())
                .collect::<Vec<_>>();
            let composition = self.host.standard_composition.clone();
            for name in import_names {
                if let Some(composition) = composition.as_ref() {
                    let _ = composition.bind_default_name(self, &name);
                }
            }
        }

        let use_legacy_order = self.host.host_function_symbols.is_empty();
        let mut resolved = Vec::with_capacity(self.program.imports.len());
        let imports = self.program.imports.clone();
        for (index, import) in imports.iter().enumerate() {
            // An exact-schema import can never be satisfied by a name-only or
            // positionally-bound legacy host function: name/position binding
            // would bypass the exact registry where wrong-key, alias, and
            // TakeOwned enforcement live. Reject with a structured error
            // directing the embedder to exact/registry binding
            // (`HostFunctionRegistry::register_exact{,_static,_stack,...}`
            // plus `bind_vm_cached` / `bind_vm_with_plan`).
            if import.schema.is_some() {
                return Err(VmError::HostImportBinding(
                    HostImportBindingError::MissingExact {
                        import: import.name.clone(),
                    },
                ));
            }
            if use_legacy_order {
                if index >= self.host.host_functions.len() {
                    return Err(VmError::InvalidCall(index as u16));
                }
                resolved.push(index as u16);
                continue;
            }

            let bound = if let Some(bound) =
                self.host.host_function_symbols.get(&import.name).copied()
            {
                bound
            } else if self.host.allow_default_host_fallback
                && self
                    .host
                    .standard_composition
                    .clone()
                    .is_some_and(|composition| composition.bind_default_name(self, &import.name))
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

    /// Whether one host import may be lowered to the native non-yielding
    /// inline shim on the JIT path.
    ///
    /// A host import whose exact schema carries a resource anywhere (params or
    /// return) must never be marked native/non-yielding inline eligible: the
    /// native non-yielding scalar/i64 shim has no resource-handle ABI, so such
    /// calls must keep exiting to the interpreter for structure validation (C1)
    /// and the exact ownership contract (C2/C4) that lives in the interpreter's
    /// guarded call machinery. Only `ArgsStaticNonYielding` bindings are
    /// eligible in the first place.
    #[cfg(test)]
    pub(super) fn jit_import_is_inline_eligible(
        schema: Option<&HostImportSchema>,
        host_fn: Option<&VmHostFunction>,
    ) -> bool {
        Self::jit_import_is_inline_eligible_with_named(schema, host_fn, &HashMap::new())
    }

    fn jit_import_is_inline_eligible_with_named(
        schema: Option<&HostImportSchema>,
        host_fn: Option<&VmHostFunction>,
        named_struct_schemas: &HashMap<String, crate::bytecode::NamedStructSchema>,
    ) -> bool {
        let schema_has_resource =
            schema.is_some_and(|schema| {
                schema.params.iter().any(|param| {
                    schema_contains_resource_named(&param.schema, named_struct_schemas)
                }) || schema_contains_resource_named(&schema.return_type, named_struct_schemas)
            });
        if schema_has_resource {
            return false;
        }
        matches!(host_fn, Some(VmHostFunction::ArgsStaticNonYielding(_)))
    }

    pub(super) fn sync_jit_non_yielding_host_imports(&mut self) {
        let imports = self
            .host
            .resolved_calls
            .iter()
            .enumerate()
            .map(|(index, &slot)| {
                // A host import whose exact schema carries a resource anywhere
                // (params or return) must never be marked native/non-yielding
                // inline eligible: the native non-yielding scalar/i64 shim has
                // no resource-handle ABI, so such calls must keep exiting to
                // the interpreter for return-structure validation (C1). Only
                // `ArgsStaticNonYielding` bindings are eligible in the first
                // place.
                let schema = self
                    .program
                    .imports
                    .get(index)
                    .and_then(|import| import.schema.as_ref());
                let host_fn = self.host.host_functions.get(usize::from(slot));
                Self::jit_import_is_inline_eligible_with_named(
                    schema,
                    host_fn,
                    &self.program.named_struct_schemas,
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

#[cfg(test)]
mod exact_binding_registration_tests {
    use super::*;
    use crate::compiler::TypeSchema;
    use crate::host_api::HostApiFingerprint;
    use crate::resource::ResourceResult;

    fn dummy_static(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::None))
    }

    fn registry_entry() -> RegistryEntry {
        RegistryEntry {
            arity: 0,
            kind: RegistryEntryKind::Static(dummy_static),
            legacy_resource_return_key: None,
        }
    }

    fn empty_int_schema() -> HostImportSchema {
        HostImportSchema {
            params: Vec::new(),
            return_type: TypeSchema::Int,
            fingerprint: HostApiFingerprint::from_wire(0),
        }
    }

    /// When the exact registry's `u16` slot space is full (65536 entries), the next exact
    /// registration fails with a structured `CapacityExceeded` error and mutates nothing:
    /// entries, `by_exact` map, plan cache and registry generation all stay untouched.
    #[test]
    fn capacity_fails_structurally_at_full_without_mutation() {
        let mut registry = HostFunctionRegistry::new();
        let generation_before = registry.registry_generation.load(Ordering::Relaxed);
        {
            let entries = Arc::make_mut(&mut registry.entries);
            // 65536 is exactly one past the largest representable slot index (65535).
            entries.resize(65536, registry_entry());
        }
        // Force an observable plan-cache state so we can assert it survives the rejection.
        registry.prepare_plan(&[]).unwrap();
        let cache_before = registry.plan_cache_len();

        let err = registry
            .push_exact(
                "overflow::f".to_string(),
                0,
                empty_int_schema(),
                RegistryEntryKind::Static(dummy_static),
            )
            .expect_err("exact registration past the u16 boundary must fail");
        assert!(
            matches!(
                err,
                VmError::HostImportBinding(HostImportBindingError::CapacityExceeded {
                    ref import,
                    limit,
                }) if import == "overflow::f" && limit == 65536
            ),
            "expected structured CapacityExceeded, got: {err}"
        );

        assert_eq!(
            registry.entries.len(),
            65536,
            "no entry may be pushed when capacity is exceeded"
        );
        assert!(
            !registry.by_exact.contains_key("overflow::f"),
            "no exact slot may be created when capacity is exceeded"
        );
        assert_eq!(
            registry.registry_generation.load(Ordering::Relaxed),
            generation_before,
            "registry generation must not change on a rejected registration"
        );
        assert_eq!(
            registry.plan_cache_len(),
            cache_before,
            "plan cache must survive a rejected registration"
        );
    }

    /// The largest representable exact slot (65535) is still insertable: `u16` conversion uses
    /// `try_from`, so the boundary itself succeeds without truncation.
    #[test]
    fn successful_push_at_last_u16_slot_succeeds_without_truncation() {
        let mut registry = HostFunctionRegistry::new();
        {
            let entries = Arc::make_mut(&mut registry.entries);
            entries.resize(65535, registry_entry()); // last valid slot index == 65535
        }
        let slot = registry
            .push_exact(
                "boundary::last".to_string(),
                0,
                empty_int_schema(),
                RegistryEntryKind::Static(dummy_static),
            )
            .expect("push into slot 65535 is within u16 capacity");
        assert_eq!(slot, 65535, "slot must not truncate at the u16 boundary");
        assert_eq!(registry.entries.len(), 65536);
    }

    use crate::host_api::HostParamPassing;

    struct TestResource;

    impl HostResource for TestResource {
        fn resource_type_key() -> Option<crate::host_api::ResourceTypeKey> {
            Some(crate::host_api::ResourceTypeKey::new("test.guard").unwrap())
        }

        fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }

        fn poll_close(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<ResourceResult<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    struct NoTake;

    impl HostFunction for NoTake {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::none()))
        }
    }

    fn take_schema(key: crate::host_api::ResourceTypeKey) -> HostImportSchema {
        HostImportSchema {
            params: vec![HostImportParam {
                name: "resource".to_string(),
                schema: TypeSchema::Resource(key),
                passing: HostParamPassing::TakeOwned,
            }],
            return_type: TypeSchema::Null,
            fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
        }
    }

    #[test]
    fn manual_take_owned_registration_reclaims_unconsumed_guest_resource() {
        let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
            .expect("test VM construction must not fail");
        let handle = vm
            .host_context()
            .push_resource(TestResource)
            .unwrap()
            .handle();
        vm.host_context().mark_resource_guest_owned(handle).unwrap();

        let mut registry = HostFunctionRegistry::new();
        let schema = take_schema(crate::host_api::ResourceTypeKey::new("test.guard").unwrap());
        let slot = registry
            .register_exact("test::guard", 1, schema, || Box::new(NoTake))
            .unwrap();
        let mut guarded = match &registry.entries[slot as usize].kind {
            RegistryEntryKind::Factory(factory) => factory(),
            _ => panic!("expected guarded factory"),
        };
        let error = guarded.call(&mut vm, &[handle.as_value()]).unwrap_err();
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceNotConsumed),
            "unconsumed take must be a structured resource_not_consumed, got: {error}"
        );
        assert_eq!(
            vm.host_context().resource_ownership(handle),
            Some(ResourceOwnership::HostOwned),
            "unconsumed take reclaims the guest slot; the closed slot remains resolvable as host-owned",
        );
    }

    // ---- review finding 2: legacy name-only binding cannot satisfy an
    // exact-schema import ------------------------------------------------

    /// A base program importing `test::guard` (an exact TakeOwned resource
    /// schema) and calling it once.
    fn legacy_schema_import_program(import: &HostImport) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 1);
        code.ret();
        Program::with_imports_and_debug(
            vec![Value::Int(0)],
            code.finish(),
            vec![import.clone()],
            None,
        )
    }

    fn guard_take_import() -> HostImport {
        HostImport {
            name: "test::guard".into(),
            arity: 1,
            return_type: crate::bytecode::ValueType::Null,
            schema: Some(take_schema(
                crate::host_api::ResourceTypeKey::new("test.guard").unwrap(),
            )),
        }
    }

    fn assert_missing_exact(error: VmError, label: &str) {
        assert!(
            matches!(
                &error,
                VmError::HostImportBinding(HostImportBindingError::MissingExact { import })
                    if import == "test::guard"
            ),
            "{label}: expected structured MissingExact, got: {error}"
        );
    }

    /// An exact-schema import can never bind through the legacy name-only
    /// `bind_*` / positional registration APIs: that path bypasses the exact
    /// registry where wrong-key, alias, and TakeOwned enforcement live. Each
    /// attempt is a structured `MissingExact` directing the embedder to
    /// exact/registry binding (`register_exact*` + `bind_vm_cached`).
    #[test]
    fn legacy_name_only_binding_rejects_exact_schema_imports() {
        let import = guard_take_import();

        // (a) name-only: bind_static_function puts a function in the symbol
        // table under the import's name; the import still must not resolve to
        // it.
        let mut named = Vm::try_new(legacy_schema_import_program(&import))
            .expect("test VM construction must not fail");
        named.bind_static_function("test::guard", dummy_static);
        assert_missing_exact(
            named
                .ensure_call_bindings()
                .expect_err("name-only bind_* must not satisfy an exact-schema import"),
            "name-only",
        );

        // (b) positional: register_static_function binds by slot order with no
        // symbol at all; exact-schema imports are still never positionally
        // bound.
        let mut positional = Vm::try_new(legacy_schema_import_program(&import))
            .expect("test VM construction must not fail");
        positional.register_static_function(dummy_static);
        assert_missing_exact(
            positional
                .ensure_call_bindings()
                .expect_err("positional binding must not satisfy an exact-schema import"),
            "positional",
        );

        // (c) the default host fallback is gated off exact-schema imports: a
        // fresh VM that would otherwise self-bind every import leaves the
        // schema import unbound and rejects it.
        let mut fresh = Vm::try_new(legacy_schema_import_program(&import))
            .expect("test VM construction must not fail");
        fresh.set_standard_composition(crate::builtins::runtime::standard_composition());
        assert_missing_exact(
            fresh
                .ensure_call_bindings()
                .expect_err("default fallback must not satisfy an exact-schema import"),
            "default-fallback",
        );
    }

    /// The same guard holds across every legacy `bind_*` variant (dynamic,
    /// stack, args) — none can satisfy an exact-schema import.
    #[test]
    fn every_legacy_bind_variant_rejects_exact_schema_imports() {
        fn stack_fn(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::none()))
        }
        fn args_fn(_args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::none()))
        }
        struct DynFn;
        impl HostFunction for DynFn {
            fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
                Ok(CallOutcome::Return(CallReturn::none()))
            }
        }
        let import = guard_take_import();

        let mut dynamic = Vm::try_new(legacy_schema_import_program(&import))
            .expect("test VM construction must not fail");
        dynamic.bind_function("test::guard", Box::new(DynFn));
        assert_missing_exact(
            dynamic
                .ensure_call_bindings()
                .expect_err("bind_function must not satisfy an exact-schema import"),
            "bind_function",
        );

        let mut stack = Vm::try_new(legacy_schema_import_program(&import))
            .expect("test VM construction must not fail");
        stack.bind_static_stack_function("test::guard", stack_fn);
        assert_missing_exact(
            stack
                .ensure_call_bindings()
                .expect_err("bind_static_stack_function must not satisfy an exact-schema import"),
            "bind_static_stack_function",
        );

        let mut args = Vm::try_new(legacy_schema_import_program(&import))
            .expect("test VM construction must not fail");
        args.bind_static_args_function("test::guard", args_fn);
        assert_missing_exact(
            args.ensure_call_bindings()
                .expect_err("bind_static_args_function must not satisfy an exact-schema import"),
            "bind_static_args_function",
        );
    }
}

/// Internal unit tests for the private exact-contract entry points that the
/// integration suite cannot reach: `ExactHostCallContract::build`'s alias
/// graph / Optional-Null skip, the depth-bounded `schema_walk_has_resource`,
/// registration-time `validate_exact_registration_schema`, and
/// `exact_host_return_policy` classification.
#[cfg(test)]
mod exact_contract_unit_tests {
    use super::*;
    use crate::compiler::TypeSchema;
    use crate::host_api::HostParamPassing;

    fn guard_key() -> crate::host_api::ResourceTypeKey {
        crate::host_api::ResourceTypeKey::new("test.guard").unwrap()
    }

    fn schema_with_params(
        params: Vec<HostImportParam>,
        return_type: TypeSchema,
    ) -> HostImportSchema {
        HostImportSchema {
            params,
            return_type,
            fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
        }
    }

    fn take_param(name: &str, schema: TypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.into(),
            schema,
            passing: HostParamPassing::TakeOwned,
        }
    }

    fn borrow_param(name: &str, schema: TypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.into(),
            schema,
            passing: HostParamPassing::Borrow,
        }
    }

    fn no_specs(error: VmError, label: &str) {
        assert!(
            matches!(
                error,
                VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
            ),
            "{label}: expected structured InvalidSchema, got: {error}"
        );
    }

    #[test]
    fn build_skips_optional_resource_null() {
        let schema = schema_with_params(
            vec![take_param(
                "f",
                TypeSchema::Optional(Box::new(TypeSchema::Resource(guard_key()))),
            )],
            TypeSchema::Null,
        );
        let contract = ExactHostCallContract::build(&schema, &[Value::Null])
            .expect("Null legally skips an Optional(Resource) argument");
        assert!(
            contract.specs.is_empty(),
            "no resource occurrence may be extracted from a skipped Optional"
        );
    }

    #[test]
    fn build_missing_argument_is_structured() {
        let schema = schema_with_params(
            vec![take_param("f", TypeSchema::Resource(guard_key()))],
            TypeSchema::Null,
        );
        let error = ExactHostCallContract::build(&schema, &[])
            .expect_err("missing argument must be rejected");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::InvalidResourceHandle),
            "got: {error}"
        );
    }

    #[test]
    fn build_rejects_duplicate_take_owned_alias() {
        let schema = schema_with_params(
            vec![
                take_param("a", TypeSchema::Resource(guard_key())),
                take_param("b", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Null,
        );
        let resource = ResourceHandle::encode(1, 0, 1).expect("valid encoding");
        let value = resource.as_value();
        let error = ExactHostCallContract::build(&schema, &[value.clone(), value])
            .expect_err("duplicate TakeOwned alias must be rejected");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceAccessConflict),
            "got: {error}"
        );
    }

    #[test]
    fn build_rejects_take_plus_borrow_alias_but_allows_borrow_borrow() {
        // TakeOwned + Borrow on the same handle: rejected.
        let mixed = schema_with_params(
            vec![
                take_param("t", TypeSchema::Resource(guard_key())),
                borrow_param("b", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Null,
        );
        let resource = ResourceHandle::encode(1, 0, 1).expect("valid encoding");
        let value = resource.as_value();
        let error = ExactHostCallContract::build(&mixed, &[value.clone(), value.clone()])
            .expect_err("take+borrow alias");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceAccessConflict),
            "got: {error}"
        );

        // Two shared Borrows on the same handle: legal.
        let borrows = schema_with_params(
            vec![
                borrow_param("a", TypeSchema::Resource(guard_key())),
                borrow_param("b", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Null,
        );
        let contract = ExactHostCallContract::build(&borrows, &[value.clone(), value])
            .expect("two shared borrows of one handle are legal");
        assert_eq!(contract.specs.len(), 2);
    }

    #[test]
    fn build_rejects_aggregate_nested_resource_at_call_time() {
        // Defensive: a non-addressable aggregate resource schema reaches
        // call-time only if registration validation was bypassed.
        let schema = schema_with_params(
            vec![take_param(
                "f",
                TypeSchema::Array(Box::new(TypeSchema::Resource(guard_key()))),
            )],
            TypeSchema::Null,
        );
        let resource = ResourceHandle::encode(1, 0, 1).expect("valid encoding");
        no_specs(
            ExactHostCallContract::build(&schema, &[resource.as_value()])
                .expect_err("aggregate resource is not addressable"),
            "build",
        );
    }

    #[test]
    fn schema_walk_bounds_depth_at_64() {
        fn nested(depth: u8, leaf: TypeSchema) -> TypeSchema {
            let mut schema = leaf;
            for _ in 0..depth {
                schema = TypeSchema::Optional(Box::new(schema));
            }
            schema
        }
        let file = guard_key();
        // Resource at depth 63 -> present, depth within bound.
        assert_eq!(
            schema_walk_has_resource(&nested(63, TypeSchema::Resource(file.clone())), 0),
            Ok(true)
        );
        // No-resource scalar nested exactly 64 deep -> no error, absent.
        assert_eq!(
            schema_walk_has_resource(&nested(64, TypeSchema::Int), 0),
            Ok(false)
        );
        // Depth 65 -> structured rejection.
        assert!(
            matches!(
                schema_walk_has_resource(&nested(65, TypeSchema::Int), 0),
                Err(HostImportBindingError::InvalidSchema { .. })
            ),
            "depth 65 walk must reject"
        );
    }

    #[test]
    fn validate_registration_rejects_args_resource_and_value_resource_and_aggregate() {
        // Args-only + resource-passing param -> rejected.
        let args_schema = schema_with_params(
            vec![take_param("f", TypeSchema::Resource(guard_key()))],
            TypeSchema::Null,
        );
        assert!(
            matches!(
                validate_exact_registration_schema("x", &args_schema, false),
                Err(HostImportBindingError::InvalidSchema { .. })
            ),
            "Args-only registration must reject resource passing"
        );

        // VM-aware + resource-bearing Value param -> rejected.
        let value_schema = schema_with_params(
            vec![HostImportParam {
                name: "v".into(),
                schema: TypeSchema::Resource(guard_key()),
                passing: HostParamPassing::Value,
            }],
            TypeSchema::Null,
        );
        assert!(
            matches!(
                validate_exact_registration_schema("x", &value_schema, true),
                Err(HostImportBindingError::InvalidSchema { .. })
            ),
            "Value-passing resource param must be rejected"
        );

        // VM-aware + aggregate-nested resource (not addressable) -> rejected.
        let aggregate_schema = schema_with_params(
            vec![take_param(
                "f",
                TypeSchema::Array(Box::new(TypeSchema::Resource(guard_key()))),
            )],
            TypeSchema::Null,
        );
        assert!(
            matches!(
                validate_exact_registration_schema("x", &aggregate_schema, true),
                Err(HostImportBindingError::InvalidSchema { .. })
            ),
            "aggregate-nested resource must be rejected"
        );

        // VM-aware + directly-addressable TakeOwned -> accepted.
        let ok_schema = schema_with_params(
            vec![take_param("f", TypeSchema::Resource(guard_key()))],
            TypeSchema::Null,
        );
        validate_exact_registration_schema("x", &ok_schema, true)
            .expect("direct TakeOwned resource is addressable");
    }

    #[test]
    fn return_policy_classifies_exact_returns() {
        let file = guard_key();
        // Direct Resource return -> Resource(key).
        let import = HostImport {
            name: "x::r".into(),
            arity: 0,
            return_type: crate::bytecode::ValueType::Int,
            schema: Some(HostImportSchema {
                params: vec![],
                return_type: TypeSchema::Resource(file.clone()),
                fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
            }),
        };
        assert!(matches!(
            exact_host_return_policy(Some(&import)),
            ExactHostReturnPolicy::Resource(ref key) if *key == file
        ));

        // Optional<Resource> return -> OptionalResource(key) (addressable, C4).
        let import = HostImport {
            name: "x::opt".into(),
            arity: 0,
            return_type: crate::bytecode::ValueType::Null,
            schema: Some(HostImportSchema {
                params: vec![],
                return_type: TypeSchema::Optional(Box::new(TypeSchema::Resource(file.clone()))),
                fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
            }),
        };
        assert!(matches!(
            exact_host_return_policy(Some(&import)),
            ExactHostReturnPolicy::OptionalResource(ref key) if *key == file
        ));

        // Resource nested inside an aggregate -> NestedResource (rejected).
        let import = HostImport {
            name: "x::deep".into(),
            arity: 0,
            return_type: crate::bytecode::ValueType::Array,
            schema: Some(HostImportSchema {
                params: vec![],
                return_type: TypeSchema::Array(Box::new(TypeSchema::Resource(file))),
                fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
            }),
        };
        assert!(matches!(
            exact_host_return_policy(Some(&import)),
            ExactHostReturnPolicy::NestedResource
        ));

        // Non-resource exact return -> Legacy.
        let import = HostImport {
            name: "x::plain".into(),
            arity: 0,
            return_type: crate::bytecode::ValueType::Int,
            schema: Some(HostImportSchema {
                params: vec![],
                return_type: TypeSchema::Int,
                fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
            }),
        };
        assert!(matches!(
            exact_host_return_policy(Some(&import)),
            ExactHostReturnPolicy::Legacy
        ));

        // schema:None -> Legacy.
        let import = HostImport {
            name: "x::none".into(),
            arity: 0,
            return_type: crate::bytecode::ValueType::Int,
            schema: None,
        };
        assert!(matches!(
            exact_host_return_policy(Some(&import)),
            ExactHostReturnPolicy::Legacy
        ));
    }

    // ---- review findings 1 & 3: registration/bind schema validation --------

    #[test]
    fn validate_rejects_resource_passing_mode_on_resource_free_schema() {
        // A Borrow/TakeOwned mode on a schema that contains no resource has no
        // handle to operate on; registration must reject it instead of
        // silently dropping the declared mode.
        for passing in [
            HostParamPassing::Borrow,
            HostParamPassing::BorrowMut,
            HostParamPassing::TakeOwned,
        ] {
            let schema = schema_with_params(
                vec![HostImportParam {
                    name: "n".into(),
                    schema: TypeSchema::Int,
                    passing,
                }],
                TypeSchema::Null,
            );
            let error = validate_exact_registration_schema("x", &schema, true)
                .expect_err("resource-passing mode on a resource-free schema must be rejected");
            assert!(
                matches!(error, HostImportBindingError::InvalidSchema { .. }),
                "got: {error}"
            );
        }

        // Value on a plain schema stays legal.
        let ok = schema_with_params(
            vec![HostImportParam {
                name: "n".into(),
                schema: TypeSchema::Int,
                passing: HostParamPassing::Value,
            }],
            TypeSchema::Null,
        );
        validate_exact_registration_schema("x", &ok, true)
            .expect("Value on a plain schema is legal");

        // The args-only funnel rejects it too (still structured, still early).
        let take = schema_with_params(
            vec![HostImportParam {
                name: "n".into(),
                schema: TypeSchema::Int,
                passing: HostParamPassing::TakeOwned,
            }],
            TypeSchema::Null,
        );
        assert!(validate_exact_registration_schema("x", &take, false).is_err());
    }

    #[test]
    fn build_rejects_resource_passing_mode_on_resource_free_schema() {
        // Defense in depth: if a guard-schema ever reached call time with a
        // resource-passing mode on a resource-free param, it must be a
        // structured rejection — never a silent drop that lets the callee
        // assume a contract the caller never granted. A resource-free
        // `TakeOwned` parameter is the one exception: it is an owned *value*
        // transfer (accepted only by owned-dispatch registration), so it
        // carries no resource contract and extracts no spec.
        let schema = schema_with_params(
            vec![HostImportParam {
                name: "n".into(),
                schema: TypeSchema::Int,
                passing: HostParamPassing::Borrow,
            }],
            TypeSchema::Null,
        );
        no_specs(
            ExactHostCallContract::build(&schema, &[Value::Int(1)])
                .expect_err("resource-free Borrow must not be silently dropped"),
            "build resource-free borrow",
        );
        let schema = schema_with_params(vec![take_param("n", TypeSchema::Int)], TypeSchema::Null);
        let contract = ExactHostCallContract::build(&schema, &[Value::Int(1)])
            .expect("resource-free TakeOwned is an owned value transfer, not a resource op");
        assert!(
            contract.specs.is_empty(),
            "an owned value transfer extracts no resource spec"
        );
        // Passing a handle where a plain Int is declared still faults on the
        // schema (the mode was dropped before the handle decode). No spec is
        // extracted from a resource-free param under any path.
        let contract = ExactHostCallContract::build(
            &schema_with_params(
                vec![HostImportParam {
                    name: "v".into(),
                    schema: TypeSchema::Int,
                    passing: HostParamPassing::Value,
                }],
                TypeSchema::Null,
            ),
            &[Value::Int(7)],
        )
        .expect("Value param extracts no resource spec");
        assert!(contract.specs.is_empty());
    }

    #[test]
    fn validate_rejects_aggregate_nested_resource_return() {
        // Only a direct Resource(key) or single Optional<Resource(key)> may
        // carry a resource across the boundary; any other resource-bearing
        // return shape is rejected at registration.
        let file = guard_key();

        let array = schema_with_params(
            vec![],
            TypeSchema::Array(Box::new(TypeSchema::Resource(file.clone()))),
        );
        assert!(
            matches!(
                validate_exact_registration_schema("x", &array, true),
                Err(HostImportBindingError::InvalidSchema { .. })
            ),
            "Array<Resource> return must be rejected"
        );

        let nested_optional = schema_with_params(
            vec![],
            TypeSchema::Optional(Box::new(TypeSchema::Optional(Box::new(
                TypeSchema::Resource(file.clone()),
            )))),
        );
        assert!(
            matches!(
                validate_exact_registration_schema("x", &nested_optional, true),
                Err(HostImportBindingError::InvalidSchema { .. })
            ),
            "Optional<Optional<Resource>> return must be rejected"
        );

        // The two legal resource-bearing returns register fine.
        let direct = schema_with_params(vec![], TypeSchema::Resource(file.clone()));
        validate_exact_registration_schema("x", &direct, true)
            .expect("Resource(key) return is representable");

        let optional = schema_with_params(
            vec![],
            TypeSchema::Optional(Box::new(TypeSchema::Resource(file))),
        );
        validate_exact_registration_schema("x", &optional, true)
            .expect("Optional<Resource(key)> return is representable");
    }

    // ---- review finding 4: JIT inline-shing gate determinism ----------------

    fn no_yield_args(_args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::None))
    }

    fn args_static(_args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::None))
    }

    #[test]
    fn jit_inline_eligibility_excludes_every_resource_carrying_import() {
        let file = guard_key();
        let non_yielding = VmHostFunction::ArgsStaticNonYielding(no_yield_args);
        let plain_args = VmHostFunction::ArgsStatic(args_static);

        // Resource param -> never inline-eligible, even on a non-yielding binding.
        let resource_param = schema_with_params(
            vec![borrow_param("r", TypeSchema::Resource(file.clone()))],
            TypeSchema::Null,
        );
        assert!(!Vm::jit_import_is_inline_eligible(
            Some(&resource_param),
            Some(&non_yielding)
        ));

        // Resource return -> never inline-eligible.
        let resource_return = schema_with_params(vec![], TypeSchema::Resource(file.clone()));
        assert!(!Vm::jit_import_is_inline_eligible(
            Some(&resource_return),
            Some(&non_yielding)
        ));

        // Optional<Resource> return -> never inline-eligible either.
        let optional_return = schema_with_params(
            vec![],
            TypeSchema::Optional(Box::new(TypeSchema::Resource(file))),
        );
        assert!(!Vm::jit_import_is_inline_eligible(
            Some(&optional_return),
            Some(&non_yielding)
        ));

        // A scalar schema on a non-yielding binding is eligible...
        let scalar = schema_with_params(vec![], TypeSchema::Int);
        assert!(Vm::jit_import_is_inline_eligible(
            Some(&scalar),
            Some(&non_yielding)
        ));
        // ...but only `ArgsStaticNonYielding` bindings qualify at all.
        assert!(!Vm::jit_import_is_inline_eligible(
            Some(&scalar),
            Some(&plain_args)
        ));
        assert!(!Vm::jit_import_is_inline_eligible(None, Some(&plain_args)));
        // A schema-less legacy binding on a non-yielding slot stays eligible.
        assert!(Vm::jit_import_is_inline_eligible(None, Some(&non_yielding)));
    }

    #[test]
    fn jit_sync_flags_mark_resource_return_import_non_inline() {
        // The real sync path over a bound VM: import 0 is a scalar
        // non-yielding args function (may inline natively), import 1 is the
        // same non-yielding args kind but returns a Resource (must stay on the
        // interpreter boundary — its slot is flagged not inline-eligible).
        let file = guard_key();

        macro_rules! make_import {
            ($name:expr, $return_type:expr, $schema:expr) => {{
                HostImport {
                    name: $name.into(),
                    arity: 0,
                    return_type: $return_type,
                    schema: Some(HostImportSchema {
                        params: vec![],
                        return_type: $schema,
                        fingerprint: crate::host_api::HostApiCatalog::default().fingerprint(),
                    }),
                }
            }};
        }
        let scalar = make_import!(
            "acme::scalar",
            crate::bytecode::ValueType::Int,
            TypeSchema::Int
        );
        let open = make_import!(
            "acme::open",
            crate::bytecode::ValueType::Unknown,
            TypeSchema::Resource(file)
        );

        let mut registry = HostFunctionRegistry::new();
        registry
            .register_exact_static_non_yielding_args(
                &scalar.name,
                0,
                scalar.schema.clone().expect("schema"),
                no_yield_args,
            )
            .expect("register scalar");
        registry
            .register_exact_static_non_yielding_args(
                &open.name,
                0,
                open.schema.clone().expect("schema"),
                no_yield_args,
            )
            .expect("register open");

        let mut code = crate::BytecodeBuilder::new();
        code.ret();
        let program = Program::with_imports_and_debug(
            Vec::new(),
            code.finish(),
            vec![scalar.clone(), open.clone()],
            None,
        );
        let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
        registry.bind_vm_cached(&mut vm).expect("bind");
        vm.sync_jit_non_yielding_host_imports();
        assert_eq!(
            vm.engine.jit.non_yielding_host_imports(),
            &[true, false],
            "scalar import may inline; the resource-return import must not — dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[cfg(test)]
mod registry_transaction_tests {
    use super::*;

    fn dummy(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::none()))
    }

    fn import(name: &str) -> HostImport {
        HostImport {
            name: name.to_string(),
            arity: 0,
            return_type: crate::bytecode::ValueType::Unknown,
            schema: None,
        }
    }

    #[test]
    fn closure_error_rolls_back_slots_generation_and_plan_cache() {
        let mut registry = HostFunctionRegistry::empty();
        registry.register_static("baseline", 0, dummy);
        registry.prepare_plan(&[]).expect("baseline plan");
        let generation = registry.registry_generation();
        let cache_len = registry.plan_cache_len();

        let error = registry
            .transactionally(|staged| {
                staged.register_static("staged", 0, dummy);
                Err(VmError::HostError("abort".to_string()))
            })
            .expect_err("closure error must abort publication");
        assert!(matches!(error, VmError::HostError(message) if message == "abort"));
        assert!(matches!(
            registry.resolve_import(&import("baseline")),
            Ok(0)
        ));
        assert!(matches!(
            registry.resolve_import(&import("staged")),
            Err(VmError::UnboundImport(name)) if name == "staged"
        ));
        assert_eq!(registry.registry_generation(), generation);
        assert_eq!(registry.plan_cache_len(), cache_len);
    }

    #[test]
    fn panic_unwind_drops_staging_without_mutating_the_original() {
        let mut registry = HostFunctionRegistry::empty();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = registry.transactionally(|staged| {
                staged.register_static("panic_only", 0, dummy);
                panic!("abort staging");
            });
        }));
        assert!(result.is_err());
        assert!(matches!(
            registry.resolve_import(&import("panic_only")),
            Err(VmError::UnboundImport(name)) if name == "panic_only"
        ));
    }

    #[test]
    fn staging_is_deeply_isolated_until_one_successful_publication() {
        let mut registry = HostFunctionRegistry::empty();
        let original = registry.clone();
        let generation = registry.registry_generation();
        let cache_len = registry.plan_cache_len();
        registry
            .transactionally(|staged| {
                staged.register_static("published", 0, dummy);
                assert!(staged.resolve_import(&import("published")).is_ok());
                assert!(original.resolve_import(&import("published")).is_err());
                assert_eq!(original.registry_generation(), generation);
                assert_eq!(original.plan_cache_len(), cache_len);
                Ok(())
            })
            .expect("successful transaction should publish once");
        assert!(registry.resolve_import(&import("published")).is_ok());
        assert!(registry.registry_generation() > generation);
        assert_eq!(registry.plan_cache_len(), 0);
    }

    #[test]
    fn ordinary_clones_share_until_mutation_while_staging_is_always_detached() {
        let registry = HostFunctionRegistry::empty();
        let sibling = registry.clone();
        assert!(Arc::ptr_eq(
            &registry.registry_state,
            &sibling.registry_state
        ));
        assert!(Arc::ptr_eq(
            &registry.registry_generation_token,
            &sibling.registry_generation_token
        ));
        assert!(Arc::ptr_eq(
            &registry.registry_generation,
            &sibling.registry_generation
        ));

        let transaction = registry.begin_transaction();
        let staged = transaction.staged.as_ref().expect("live staging");
        assert!(!Arc::ptr_eq(
            &registry.registry_state,
            &staged.registry_state
        ));
        assert!(!Arc::ptr_eq(
            &registry.registry_generation_token,
            &staged.registry_generation_token
        ));
        assert!(!Arc::ptr_eq(
            &registry.registry_generation,
            &staged.registry_generation
        ));
    }

    #[test]
    fn unrelated_and_double_publications_are_rejected_by_the_private_handle() {
        let mut first = HostFunctionRegistry::empty();
        let mut second = HostFunctionRegistry::empty();
        let fresh_a = HostFunctionRegistry::new();
        let fresh_b = HostFunctionRegistry::new();
        assert!(!Arc::ptr_eq(
            &fresh_a.transaction_origin,
            &fresh_b.transaction_origin
        ));
        let mut transaction = second.begin_transaction();

        let unrelated = first
            .commit_transaction(&mut transaction)
            .expect_err("unrelated registry must not publish staging");
        assert!(matches!(unrelated, VmError::HostError(message) if message.contains("different")));
        first
            .resolve_import(&import("anything"))
            .expect_err("unrelated commit must leave first unchanged");

        second
            .commit_transaction(&mut transaction)
            .expect("origin registry may publish its own transaction");
        let double = second
            .commit_transaction(&mut transaction)
            .expect_err("the same transaction cannot publish twice");
        assert!(matches!(double, VmError::HostError(message) if message.contains("already")));
    }
}

#[cfg(test)]
mod owned_dispatch_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::bytecode::{
        CallableEnvironment, CallableKind, CallablePrototype, CallableTarget, HostImport,
        HostImportParam, Program, TypeMap,
    };
    use crate::compiler::TypeSchema;
    use crate::host_api::{HostApiFingerprint, HostParamSchema, HostTypeSchema};

    fn owned_schema(params: Vec<HostImportParam>, return_type: TypeSchema) -> HostImportSchema {
        HostImportSchema {
            params,
            return_type,
            fingerprint: HostApiFingerprint::from_wire(0),
        }
    }

    fn int_param(name: &str) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema: TypeSchema::Int,
            passing: crate::host_api::HostParamPassing::Value,
        }
    }

    /// A resource-free callable parameter passed with `TakeOwned`: the owned
    /// value transfer this facility exists for.
    fn callable_take_param(name: &str) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema: TypeSchema::Callable {
                params: vec![TypeSchema::Bool],
                result: Box::new(TypeSchema::Unknown),
            },
            passing: crate::host_api::HostParamPassing::TakeOwned,
        }
    }

    fn owned_import(schema: HostImportSchema) -> HostImport {
        HostImport {
            name: "test::register".to_string(),
            arity: schema.params.len() as u8,
            return_type: schema.return_type.coarse_value_type(),
            schema: Some(schema),
        }
    }

    /// One script callable prototype (zero captures) so the guest can build a
    /// real `Value::Callable` for the owned transfer.
    fn one_callable_prototype() -> CallablePrototype {
        CallablePrototype {
            kind: CallableKind::Closure,
            target: CallableTarget::ScriptFunction(0),
            arity: 1,
            frame_local_count: 2,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }
    }

    fn capture_program(
        local_schemas: Vec<TypeSchema>,
        prototypes: Vec<CallablePrototype>,
    ) -> Program {
        let local_count = local_schemas.len();
        let mut program = Program::new(vec![], vec![]).with_local_count(local_count);
        program.type_map = Some(TypeMap {
            strict_types: true,
            local_types: vec![ValueType::Unknown; local_count],
            local_schemas: local_schemas.into_iter().map(Some).collect(),
            callable_slots: vec![true; local_count],
            optional_slots: vec![false; local_count],
            operand_types: HashMap::new(),
        });
        program.callable_prototypes = prototypes;
        program
    }

    fn closure_value(prototype_id: u32, captures: Vec<Value>) -> (Value, Arc<CallableValue>) {
        let callable = Arc::new(CallableValue {
            prototype_id,
            kind: CallableKind::Closure,
            env: (!captures.is_empty()).then(|| {
                Arc::new(CallableEnvironment {
                    cells: Mutex::new(
                        captures
                            .into_iter()
                            .map(|value| Arc::new(Mutex::new(value)))
                            .collect(),
                    ),
                })
            }),
        });
        (Value::Callable(Arc::clone(&callable)), callable)
    }

    fn capture_prototype(
        source_slots: Vec<u16>,
        modes: Vec<crate::CaptureBindingMode>,
    ) -> CallablePrototype {
        CallablePrototype {
            kind: CallableKind::Closure,
            target: CallableTarget::ScriptFunction(0),
            arity: 0,
            frame_local_count: 1,
            parameter_slots: Vec::new(),
            capture_slots: (0..source_slots.len()).map(|slot| slot as u16).collect(),
            capture_source_slots: source_slots,
            capture_modes: modes,
            self_slot: None,
            schema: None,
        }
    }

    /// Resource-bearing capture rejection is schema-only. The host path never
    /// reads or moves the source VM's resource table.
    #[test]
    fn owned_callable_rejects_resource_bearing_aggregate_capture() {
        let key = crate::host_api::ResourceTypeKey::new("test.capture").unwrap();
        let program = capture_program(
            vec![TypeSchema::Array(Box::new(TypeSchema::Resource(key)))],
            vec![capture_prototype(
                vec![0],
                vec![crate::CaptureBindingMode::Move],
            )],
        );
        let mut source = Vm::try_new(program).expect("source VM");
        let (callable, owned) = closure_value(0, vec![Value::array(vec![Value::Int(7)])]);
        source.instance.owned_callables.push(Arc::downgrade(&owned));
        source.instance.locals[0] = callable.clone();
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut source, Vec::new());
        let result = call.spawn_owned_callable_vm(&registry, &callable, |_vm| Ok(()));
        let error = match result {
            Ok(_) => panic!("resource-bearing aggregate capture must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("resource-bearing capture")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn owned_callable_rejects_resource_in_nested_non_move_callable_capture() {
        let key = crate::host_api::ResourceTypeKey::new("test.nested_capture").unwrap();
        let callable_schema = TypeSchema::Callable {
            params: Vec::new(),
            result: Box::new(TypeSchema::Unknown),
        };
        let program = capture_program(
            vec![callable_schema, TypeSchema::Resource(key)],
            vec![
                capture_prototype(vec![0], vec![crate::CaptureBindingMode::Borrow]),
                capture_prototype(vec![1], vec![crate::CaptureBindingMode::Move]),
            ],
        );
        let mut source = Vm::try_new(program).expect("source VM");
        let (nested, nested_owned) = closure_value(1, vec![Value::Int(11)]);
        let (outer, outer_owned) = closure_value(0, vec![nested]);
        source
            .instance
            .owned_callables
            .extend([Arc::downgrade(&nested_owned), Arc::downgrade(&outer_owned)]);
        source.instance.locals[0] = outer.clone();
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut source, Vec::new());
        let result = call.spawn_owned_callable_vm(&registry, &outer, |_vm| Ok(()));
        let error = match result {
            Ok(_) => panic!("nested resource capture must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("resource-bearing capture")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn owned_callable_preserves_resource_free_capture_graphs() {
        let program = capture_program(
            vec![TypeSchema::ArrayTuple(vec![
                TypeSchema::String,
                TypeSchema::Callable {
                    params: Vec::new(),
                    result: Box::new(TypeSchema::Unknown),
                },
            ])],
            vec![
                capture_prototype(vec![0], vec![crate::CaptureBindingMode::Move]),
                capture_prototype(Vec::new(), Vec::new()),
            ],
        );
        let mut source = Vm::try_new(program).expect("source VM");
        let (nested, nested_owned) = closure_value(1, Vec::new());
        let (outer, outer_owned) = closure_value(
            0,
            vec![Value::array(vec![Value::string("capture"), nested])],
        );
        source
            .instance
            .owned_callables
            .extend([Arc::downgrade(&nested_owned), Arc::downgrade(&outer_owned)]);
        source.instance.locals[0] = outer.clone();
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut source, Vec::new());
        let child = call
            .spawn_owned_callable_vm(&registry, &outer, |_vm| Ok(()))
            .expect("resource-free capture graph must transfer");
        assert!(child.owns_callable(&outer));
        assert!(child.owns_callable(&nested_owned_value(&outer)));
    }

    #[test]
    fn owned_callable_rejects_nested_foreign_callable() {
        let callable_schema = TypeSchema::Callable {
            params: Vec::new(),
            result: Box::new(TypeSchema::Unknown),
        };
        let program = capture_program(
            vec![callable_schema],
            vec![capture_prototype(
                vec![0],
                vec![crate::CaptureBindingMode::Borrow],
            )],
        );
        let mut source = Vm::try_new(program).expect("source VM");
        let foreign = closure_value(0, Vec::new()).0;
        let (outer, outer_owned) = closure_value(0, vec![foreign]);
        source
            .instance
            .owned_callables
            .push(Arc::downgrade(&outer_owned));
        source.instance.locals[0] = outer.clone();
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut source, Vec::new());
        let error = match call.spawn_owned_callable_vm(&registry, &outer, |_vm| Ok(())) {
            Ok(_) => panic!("nested foreign callable must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, VmError::InvalidFrameState(message) if message.contains("source vm")),
            "unexpected nested provenance error: {error}"
        );
    }

    #[test]
    fn owned_callable_graph_cycle_is_visited_once() {
        let callable_schema = TypeSchema::Callable {
            params: Vec::new(),
            result: Box::new(TypeSchema::Unknown),
        };
        let program = capture_program(
            vec![callable_schema],
            vec![capture_prototype(
                vec![0],
                vec![crate::CaptureBindingMode::Borrow],
            )],
        );
        let mut source = Vm::try_new(program).expect("source VM");
        let environment = Arc::new(CallableEnvironment {
            cells: Mutex::new(Vec::new()),
        });
        let callable = Arc::new(CallableValue {
            prototype_id: 0,
            kind: CallableKind::Closure,
            env: Some(Arc::clone(&environment)),
        });
        *environment.cells.lock().expect("cycle cells") =
            vec![Arc::new(Mutex::new(Value::Callable(Arc::clone(&callable))))];
        let value = Value::Callable(Arc::clone(&callable));
        source
            .instance
            .owned_callables
            .push(Arc::downgrade(&callable));
        source.instance.locals[0] = value.clone();
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut source, Vec::new());
        let child = call
            .spawn_owned_callable_vm(&registry, &value, |_vm| Ok(()))
            .expect("cyclic resource-free graph must transfer");
        assert!(child.owns_callable(&value));
    }

    #[test]
    fn owned_host_call_can_restore_a_taken_argument() {
        let mut source = Vm::try_new(Program::new(Vec::new(), Vec::new())).expect("source VM");
        let mut call =
            OwnedHostCall::new(&mut source, vec![Value::Int(7), Value::string("callback")]);
        let value = call.take_arg(1).expect("take argument");
        assert!(call.is_taken(1));
        call.restore_arg(1, value).expect("restore argument");
        assert!(!call.is_taken(1));
        assert_eq!(
            call.into_untaken_args(),
            vec![Value::Int(7), Value::string("callback")]
        );
    }

    fn nested_owned_value(outer: &Value) -> Value {
        let Value::Callable(callable) = outer else {
            panic!("expected outer callable");
        };
        let environment = callable.env.as_ref().expect("outer environment");
        let cells = environment.cells.lock().expect("capture cells");
        let Value::Array(values) = &*cells[0].lock().expect("capture cell") else {
            panic!("expected aggregate capture");
        };
        values[1].clone()
    }

    /// Program that pushes `int_arg`, builds prototype 0's callable, and calls
    /// import slot 0 with `(int_arg, callable)`.
    fn owned_call_program(import: HostImport, int_arg: i64) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.ldc(1);
        code.call(BuiltinFunction::ArrayNew.call_index(), 0);
        code.call(BuiltinFunction::BindCallable.call_index(), 2);
        code.call(0, 2);
        code.ret();
        let mut program = Program::with_imports_and_debug(
            vec![Value::Int(int_arg), Value::Int(0)],
            code.finish(),
            vec![import],
            None,
        );
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    /// What one owned host call observed.
    #[derive(Default)]
    struct Seen {
        callable: Option<Value>,
        second_take_failed: bool,
    }

    /// An owned host function that takes the callable argument (index 1) and
    /// records what it saw. `fail` makes it fail after taking ownership.
    struct OwnedSpy {
        seen: Arc<Mutex<Seen>>,
        fail: bool,
    }

    impl HostOwnedFunction for OwnedSpy {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let value = call.take_arg(1)?;
            let mut seen = self.seen.lock().expect("seen lock");
            seen.second_take_failed = call.take_arg(1).is_err();
            seen.callable = Some(value);
            drop(seen);
            if self.fail {
                return Err(VmError::HostError(
                    "owned spy rejected the call".to_string(),
                ));
            }
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }

    /// Exercises a host-side handoff that must roll back after provenance
    /// validation fails.
    struct RollbackOwned {
        registry: HostFunctionRegistry,
    }

    impl HostOwnedFunction for RollbackOwned {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let callback = call.take_arg(1)?;
            let Value::Callable(source_callable) = &callback else {
                return Err(VmError::TypeMismatch("callable"));
            };
            let foreign = Value::Callable(Arc::new(CallableValue {
                prototype_id: source_callable.prototype_id,
                kind: source_callable.kind,
                env: None,
            }));
            let error = match call.spawn_owned_callable_vm(&self.registry, &foreign, |_vm| Ok(())) {
                Ok(_) => panic!("foreign callable must fail provenance validation"),
                Err(error) => error,
            };
            call.restore_arg(1, callback)?;
            Err(error)
        }
    }

    /// Records the isolated VM the owned call spawned.
    struct SpawningSpy {
        registry: HostFunctionRegistry,
        fresh_owned_callable: Arc<Mutex<Option<bool>>>,
        fresh_frames: Arc<Mutex<Option<usize>>>,
        fresh_stack_empty: Arc<Mutex<Option<bool>>>,
        source_unchanged: Arc<Mutex<Option<bool>>>,
    }

    impl HostOwnedFunction for SpawningSpy {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let callable = call.take_arg(1)?;
            let before = {
                let vm = call.vm_ref();
                (
                    vm.instance.execution_frames.len(),
                    vm.instance.stack.len(),
                    vm.instance.locals.len(),
                )
            };
            let fresh = call.spawn_owned_callable_vm(&self.registry, &callable, |_vm| Ok(()))?;
            let after = {
                let vm = call.vm_ref();
                (
                    vm.instance.execution_frames.len(),
                    vm.instance.stack.len(),
                    vm.instance.locals.len(),
                )
            };
            *self.fresh_owned_callable.lock().expect("lock") = Some(fresh.owns_callable(&callable));
            *self.fresh_frames.lock().expect("lock") = Some(fresh.instance.execution_frames.len());
            *self.fresh_stack_empty.lock().expect("lock") = Some(fresh.instance.stack.is_empty());
            *self.source_unchanged.lock().expect("lock") = Some(before == after);
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }

    fn bound_vm(program: Program, registry: &HostFunctionRegistry) -> Vm {
        let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
        registry.bind_vm_cached(&mut vm).expect("bind registry");
        vm
    }

    #[test]
    fn register_exact_owned_accepts_resource_free_take_owned_callable_parameter() {
        let mut registry = HostFunctionRegistry::empty();
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let slot = registry
            .register_exact_owned("test::register", 2, schema, |_context| {
                Box::new(OwnedSpy {
                    seen: Arc::new(Mutex::new(Seen::default())),
                    fail: false,
                })
            })
            .expect("an owned registration may take a resource-free callable");
        assert_eq!(slot as usize, 0);
    }

    #[test]
    fn plain_exact_registration_still_rejects_resource_free_take_owned() {
        let mut registry = HostFunctionRegistry::empty();
        let schema = owned_schema(vec![callable_take_param("callback")], TypeSchema::Bool);
        let error = registry
            .register_exact("test::register", 1, schema, || Box::new(NoopHost))
            .expect_err("a plain exact binding must not declare an owned value transfer");
        assert!(
            matches!(
                error,
                VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
            ),
            "expected a structured invalid-schema rejection, got: {error}"
        );
    }

    struct NoopHost;

    impl HostFunction for NoopHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::none()))
        }
    }

    struct MutatingOwned {
        calls: Arc<AtomicUsize>,
    }

    impl HostOwnedFunction for MutatingOwned {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = call.take_arg(1)?;
            call.vm().register_function(Box::new(NoopHost));
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }

    fn owned_call_twice_program(import: HostImport, int_arg: i64) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        for _ in 0..2 {
            code.ldc(0);
            code.ldc(1);
            code.call(BuiltinFunction::ArrayNew.call_index(), 0);
            code.call(BuiltinFunction::BindCallable.call_index(), 2);
            code.call(0, 2);
        }
        code.ret();
        let mut program = Program::with_imports_and_debug(
            vec![Value::Int(int_arg), Value::Int(0)],
            code.finish(),
            vec![import],
            None,
        );
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    #[test]
    fn owned_dispatch_restores_function_after_handler_mutates_host_vector() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(MutatingOwned {
                    calls: Arc::clone(&factory_calls),
                })
            })
            .expect("register owned");
        let mut vm = bound_vm(
            owned_call_twice_program(owned_import(schema), 41),
            &registry,
        );
        assert_eq!(
            vm.run().expect("mutating calls must succeed"),
            VmStatus::Halted
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            vm.host.host_functions.len() >= 3,
            "the handler must have been able to append registrations without invalidating dispatch"
        );
    }

    struct PanickingOwned {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for PanickingOwned {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl HostOwnedFunction for PanickingOwned {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let _ = call.take_arg(1)?;
            panic!("owned host panic");
        }
    }

    #[test]
    fn owned_dispatch_restores_function_and_untaken_values_after_panic() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let drops = Arc::new(AtomicUsize::new(0));
        let factory_drops = Arc::clone(&drops);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(PanickingOwned {
                    drops: Arc::clone(&factory_drops),
                })
            })
            .expect("register owned");
        let mut vm = bound_vm(owned_call_program(owned_import(schema), 41), &registry);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run()));
        assert!(
            result.is_err(),
            "the existing owned-call panic contract must propagate"
        );
        assert_eq!(vm.instance.stack, vec![Value::Int(41)]);
        assert!(matches!(
            vm.host.host_functions.first(),
            Some(VmHostFunction::OwnedDynamic(Some(_)))
        ));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(vm);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// An owned host function that takes the callable argument and then
    /// returns `CallOutcome::Yield`.
    ///
    /// Owned dispatch drains the call operands before the handler runs, so
    /// there is no operand stack left to re-execute the call from. This
    /// outcome must therefore be rejected as a structured error rather than
    /// silently yielding with the operands already consumed.
    struct YieldingOwned {
        calls: Arc<AtomicUsize>,
        entry_ip: Arc<AtomicUsize>,
        held: Arc<Mutex<Option<Value>>>,
    }

    impl HostOwnedFunction for YieldingOwned {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entry_ip.store(call.vm_ref().ip(), Ordering::SeqCst);
            let callable = call.take_arg(1)?;
            *self.held.lock().expect("held callback") = Some(callable);
            Ok(CallOutcome::Yield)
        }
    }

    #[test]
    fn owned_dispatch_rejects_yield_and_restores_untaken_arguments() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let entry_ip = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(Mutex::new(None));
        let factory_calls = Arc::clone(&calls);
        let factory_entry_ip = Arc::clone(&entry_ip);
        let factory_held = Arc::clone(&held);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(YieldingOwned {
                    calls: Arc::clone(&factory_calls),
                    entry_ip: Arc::clone(&factory_entry_ip),
                    held: Arc::clone(&factory_held),
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(owned_import(schema), 41), &registry);
        let error = vm
            .run()
            .expect_err("an owned handler yield must be rejected, not dispatched again");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("yield")),
            "expected a structured owned-yield rejection, got: {error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(
                held.lock().expect("held callback").as_ref(),
                Some(Value::Callable(_))
            ),
            "the callable the handler took stays owned by the handler"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "the untaken delay argument must be restored exactly once"
        );
        assert_eq!(
            vm.ip(),
            entry_ip.load(Ordering::SeqCst),
            "the rejected dispatch must not rewind the instruction pointer for \
             an unsafe retry"
        );
        // Whatever a repeated resume reports, it must not re-dispatch the call
        // with drained/missing operands.
        let _ = vm.resume();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a repeated resume must not silently re-dispatch the owned handler"
        );
    }

    #[test]
    fn owned_dispatch_transfers_the_callable_exactly_once() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let spy_seen = Arc::clone(&seen);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(OwnedSpy {
                    seen: Arc::clone(&spy_seen),
                    fail: false,
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(owned_import(schema), 41), &registry);
        let status = vm.run().expect("the owned call must succeed");
        assert_eq!(status, VmStatus::Halted);

        let seen = seen.lock().expect("seen lock");
        assert!(
            matches!(seen.callable, Some(Value::Callable(_))),
            "the host must receive the callable value itself"
        );
        assert!(
            seen.second_take_failed,
            "a second take of the same argument must be rejected"
        );
        let stack = &vm.instance.stack;
        assert_eq!(
            stack,
            &vec![Value::Bool(true)],
            "a successful owned call leaves the guest operands consumed and the \
             return value on the stack"
        );
    }

    #[test]
    fn failed_owned_call_restores_untaken_arguments_without_double_drop() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(OwnedSpy {
                    seen: Arc::new(Mutex::new(Seen::default())),
                    fail: true,
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(owned_import(schema), 41), &registry);
        let error = vm.run().expect_err("the owned call must fail");
        assert!(matches!(error, VmError::HostError(ref message) if message.contains("rejected")));
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "an error before host acceptance restores every argument the host did \
             not take; the taken callable stays with the host"
        );
    }

    #[test]
    fn failed_owned_call_rolls_back_after_provenance_validation() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |context| {
                Box::new(RollbackOwned {
                    registry: context.registry().clone(),
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(owned_import(schema), 41), &registry);
        let error = vm.run().expect_err("the provenance failure must fail");
        assert!(
            matches!(error, VmError::InvalidFrameState(message) if message.contains("source vm"))
        );
        assert_eq!(vm.instance.stack.len(), 2);
        assert!(matches!(vm.instance.stack[1], Value::Callable(_)));
    }

    #[test]
    fn owned_call_spawns_an_isolated_callable_vm() {
        let schema = owned_schema(
            vec![int_param("delay"), callable_take_param("callback")],
            TypeSchema::Bool,
        );
        let owned_callable = Arc::new(Mutex::new(None));
        let fresh_frames = Arc::new(Mutex::new(None));
        let fresh_stack_empty = Arc::new(Mutex::new(None));
        let source_unchanged = Arc::new(Mutex::new(None));
        let spy_owned = Arc::clone(&owned_callable);
        let spy_frames = Arc::clone(&fresh_frames);
        let spy_fresh_stack = Arc::clone(&fresh_stack_empty);
        let spy_source_unchanged = Arc::clone(&source_unchanged);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |context| {
                Box::new(SpawningSpy {
                    registry: context.registry().clone(),
                    fresh_owned_callable: Arc::clone(&spy_owned),
                    fresh_frames: Arc::clone(&spy_frames),
                    fresh_stack_empty: Arc::clone(&spy_fresh_stack),
                    source_unchanged: Arc::clone(&spy_source_unchanged),
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(owned_import(schema), 41), &registry);
        let status = vm.run().expect("the spawning owned call must succeed");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(
            *owned_callable.lock().expect("lock"),
            Some(true),
            "the fresh VM must own the transferred callable graph"
        );
        assert_eq!(
            *fresh_frames.lock().expect("lock"),
            Some(0),
            "the fresh VM starts halted with no execution frames; no source \
             frames were copied"
        );
        assert_eq!(
            *fresh_stack_empty.lock().expect("lock"),
            Some(true),
            "the fresh VM starts with an empty operand stack"
        );
        assert_eq!(
            *source_unchanged.lock().expect("lock"),
            Some(true),
            "spawning the owned VM must leave the source VM's frames, stack, and \
             locals untouched"
        );
    }

    #[test]
    fn spawned_vm_rejects_a_foreign_callable_with_a_colliding_prototype_id() {
        let mut program = Program::new(vec![], vec![]).with_local_count(1);
        program.callable_prototypes = vec![one_callable_prototype()];
        let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
        let source_callable = Arc::new(CallableValue {
            prototype_id: 0,
            kind: CallableKind::Closure,
            env: None,
        });
        vm.instance
            .owned_callables
            .push(Arc::downgrade(&source_callable));
        vm.instance.locals[0] = Value::Callable(Arc::clone(&source_callable));
        let foreign = Value::Callable(Arc::new(CallableValue {
            prototype_id: 0,
            kind: CallableKind::Closure,
            env: None,
        }));
        let mut call = OwnedHostCall::new(&mut vm, vec![]);
        let registry = HostFunctionRegistry::empty();
        let result = call.spawn_owned_callable_vm(&registry, &foreign, |_vm| Ok(()));
        match result {
            Ok(_) => panic!("a foreign callable with a colliding prototype must be rejected"),
            Err(error) => assert!(
                matches!(error, VmError::InvalidFrameState(message) if message.contains("source vm")),
                "expected a source-provenance rejection, got: {error}"
            ),
        }
    }

    #[test]
    fn owned_dispatch_keeps_legacy_host_kinds_untouched() {
        // A `Value`-only exact registration continues to dispatch through the
        // borrowed-slice path and ignores no declared ownership.
        let schema = owned_schema(vec![int_param("value")], TypeSchema::Int);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_static("test::echo", 1, schema, |_vm, args| {
                Ok(CallOutcome::Return(CallReturn::one(
                    args.first().cloned().unwrap_or(Value::Null),
                )))
            })
            .expect("register exact static");

        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 1);
        code.ret();
        let echo_schema = owned_schema(
            vec![HostImportParam {
                name: "value".to_string(),
                schema: TypeSchema::Int,
                passing: crate::host_api::HostParamPassing::Value,
            }],
            TypeSchema::Int,
        );
        let program = Program::with_imports_and_debug(
            vec![Value::Int(7)],
            code.finish(),
            vec![HostImport {
                name: "test::echo".to_string(),
                arity: 1,
                return_type: crate::bytecode::ValueType::Int,
                schema: Some(echo_schema),
            }],
            None,
        );
        let mut vm = bound_vm(program, &registry);
        assert_eq!(vm.run().expect("run"), VmStatus::Halted);
        assert_eq!(vm.instance.stack, vec![Value::Int(7)]);
    }

    /// The `HostApiBuilder` catalog gate accepts a resource-free `TakeOwned`
    /// parameter through the same host-schema vocabulary.
    #[test]
    fn host_catalog_schema_vocabulary_models_owned_callable_transfer() {
        let param = HostParamSchema::with_passing(
            "callback",
            HostTypeSchema::Callable {
                params: vec![HostTypeSchema::Bool],
                result: Box::new(HostTypeSchema::Unknown),
            },
            crate::host_api::HostParamPassing::TakeOwned,
        );
        assert_eq!(param.passing, crate::host_api::HostParamPassing::TakeOwned);
        assert!(!param.ty.contains_resource());
    }
}

/// Guarded owned registrations that carry a resource-bearing parameter.
///
/// The owned dispatch drains the call operands and transfers ownership of the
/// arguments the handler takes, while a resource-bearing parameter keeps its
/// exact-schema preflight/commit contract ([`ExactHostCallContract`]). The
/// tests below drive *real* `register_exact_owned` registrations through
/// `Vm::run`, so wrong-key / invalid-handle preflights (the host must not run
/// and nothing may be consumed), successful transfers (declaration order, each
/// declared argument transferred exactly once), and the commit guarantees
/// (unconsumed resources are reclaimed, borrow conflicts are reported) are
/// exercised end to end. The last group pins the owned-dispatch failure
/// contract for a rejected exact resource return: arguments the host took stay
/// consumed, every untaken argument is restored exactly once, and the host
/// stack is preserved.
#[cfg(test)]
mod owned_resource_dispatch_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;

    use super::*;
    use crate::bytecode::{
        CallableKind, CallablePrototype, CallableTarget, HostImport, HostImportParam, Program,
    };
    use crate::compiler::TypeSchema;
    use crate::host_api::{HostApiFingerprint, HostParamPassing};
    use crate::resource::ResourceResult;

    fn bound_vm(program: Program, registry: &HostFunctionRegistry) -> Vm {
        let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
        registry.bind_vm_cached(&mut vm).expect("bind registry");
        vm
    }

    fn schema(params: Vec<HostImportParam>, return_type: TypeSchema) -> HostImportSchema {
        HostImportSchema {
            params,
            return_type,
            fingerprint: HostApiFingerprint::from_wire(0),
        }
    }

    fn guard_key() -> ResourceTypeKey {
        ResourceTypeKey::new("test.guard").expect("resource key")
    }

    fn other_key() -> ResourceTypeKey {
        ResourceTypeKey::new("test.other").expect("resource key")
    }

    fn value_param(name: &str, schema: TypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema,
            passing: HostParamPassing::Value,
        }
    }

    fn take_param(name: &str, schema: TypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema,
            passing: HostParamPassing::TakeOwned,
        }
    }

    fn borrow_mut_param(name: &str, schema: TypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema,
            passing: HostParamPassing::BorrowMut,
        }
    }

    fn callable_schema() -> TypeSchema {
        TypeSchema::Callable {
            params: vec![TypeSchema::Bool],
            result: Box::new(TypeSchema::Unknown),
        }
    }

    fn import(name: &str, schema: HostImportSchema) -> HostImport {
        HostImport {
            name: name.to_string(),
            arity: schema.params.len() as u8,
            return_type: schema.return_type.coarse_value_type(),
            schema: Some(schema),
        }
    }

    /// A resource whose live key is `test.guard`; closing it is observable.
    struct GuardResource {
        closes: Arc<AtomicUsize>,
    }

    impl HostResource for GuardResource {
        fn resource_type_key() -> Option<ResourceTypeKey> {
            Some(guard_key())
        }

        fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(CloseProgress::Ready)
        }

        fn poll_close(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<ResourceResult<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A resource whose live key is `test.other`; closing it is observable.
    struct OtherResource {
        closes: Arc<AtomicUsize>,
    }

    impl HostResource for OtherResource {
        fn resource_type_key() -> Option<ResourceTypeKey> {
            Some(other_key())
        }

        fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(CloseProgress::Ready)
        }

        fn poll_close(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<ResourceResult<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// What one owned resource call observed.
    #[derive(Default)]
    struct Seen {
        calls: usize,
        first_arg: Option<Value>,
        second_arg: Option<Value>,
        second_take_failed: bool,
        handle: Option<u64>,
        callable: Option<Value>,
        fresh_owns_callable: Option<bool>,
        consumed: bool,
        host_marker: bool,
    }

    /// `test::ping`: pushes a guard-keyed resource and returns its raw handle
    /// as an `Int`. The exact `Resource(test.guard)` return transfer marks the
    /// handle guest-owned before the guest uses it.
    struct PingGuard {
        closes: Arc<AtomicUsize>,
        recorded: Arc<Mutex<Option<u64>>>,
    }

    impl HostFunction for PingGuard {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            let handle = vm
                .host_context()
                .push_resource(GuardResource {
                    closes: Arc::clone(&self.closes),
                })
                .expect("push guard resource")
                .handle();
            *self.recorded.lock().expect("ping record") = Some(handle.raw());
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(
                handle.raw() as i64,
            ))))
        }
    }

    /// `test::ping` variant that returns a resource of the *other* key.
    struct PingOther {
        closes: Arc<AtomicUsize>,
        recorded: Arc<Mutex<Option<u64>>>,
    }

    impl HostFunction for PingOther {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            let handle = vm
                .host_context()
                .push_resource(OtherResource {
                    closes: Arc::clone(&self.closes),
                })
                .expect("push other resource")
                .handle();
            *self.recorded.lock().expect("ping record") = Some(handle.raw());
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(
                handle.raw() as i64,
            ))))
        }
    }

    fn register_guard_ping(
        registry: &mut HostFunctionRegistry,
        closes: Arc<AtomicUsize>,
        recorded: Arc<Mutex<Option<u64>>>,
    ) {
        registry
            .register_exact(
                "test::ping",
                0,
                schema(vec![], TypeSchema::Resource(guard_key())),
                move || {
                    Box::new(PingGuard {
                        closes: Arc::clone(&closes),
                        recorded: Arc::clone(&recorded),
                    })
                },
            )
            .expect("register guard ping");
    }

    fn register_other_ping(
        registry: &mut HostFunctionRegistry,
        closes: Arc<AtomicUsize>,
        recorded: Arc<Mutex<Option<u64>>>,
    ) {
        registry
            .register_exact(
                "test::ping",
                0,
                schema(vec![], TypeSchema::Resource(other_key())),
                move || {
                    Box::new(PingOther {
                        closes: Arc::clone(&closes),
                        recorded: Arc::clone(&recorded),
                    })
                },
            )
            .expect("register other ping");
    }

    fn guard_ping_import() -> HostImport {
        import(
            "test::ping",
            schema(vec![], TypeSchema::Resource(guard_key())),
        )
    }

    fn other_ping_import() -> HostImport {
        import(
            "test::ping",
            schema(vec![], TypeSchema::Resource(other_key())),
        )
    }

    /// One script callable prototype (zero captures) so the guest can build a
    /// real `Value::Callable` for the owned transfer.
    fn one_callable_prototype() -> CallablePrototype {
        CallablePrototype {
            kind: CallableKind::Closure,
            target: CallableTarget::ScriptFunction(0),
            arity: 1,
            frame_local_count: 2,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }
    }

    /// `test::ping()` first, then the arity-2 owned import with the int value
    /// in slot 0, so the drained arguments are `[Int, handle]`.
    fn ping_then_int_program(ping: HostImport, take: HostImport, int_arg: i64) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 0);
        code.call(1, 2);
        code.ret();
        Program::with_imports_and_debug(
            vec![Value::Int(int_arg)],
            code.finish(),
            vec![ping, take],
            None,
        )
    }

    /// `test::ping()` first, then the arity-2 owned import with the callable in
    /// slot 1, so the drained arguments are `[handle, callable]`.
    fn ping_then_callable_program(ping: HostImport, take: HostImport) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        code.call(0, 0);
        code.ldc(0);
        code.call(BuiltinFunction::ArrayNew.call_index(), 0);
        code.call(BuiltinFunction::BindCallable.call_index(), 2);
        code.call(1, 2);
        code.ret();
        let mut program = Program::with_imports_and_debug(
            vec![Value::Int(0)],
            code.finish(),
            vec![ping, take],
            None,
        );
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    /// The arity-2 owned import called with raw constant arguments, so an
    /// invalid handle can be passed without a resource-producing host.
    fn plain_take_program(take: HostImport, args: Vec<Value>) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        for index in 0..args.len() {
            code.ldc(index as u32);
        }
        code.call(0, args.len() as u8);
        code.ret();
        Program::with_imports_and_debug(args, code.finish(), vec![take], None)
    }

    /// `[Int, callable]` arguments built entirely by the guest.
    fn int_then_callable_program(take: HostImport, int_arg: i64) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.ldc(1);
        code.call(BuiltinFunction::ArrayNew.call_index(), 0);
        code.call(BuiltinFunction::BindCallable.call_index(), 2);
        code.call(0, 2);
        code.ret();
        let mut program = Program::with_imports_and_debug(
            vec![Value::Int(int_arg), Value::Int(0)],
            code.finish(),
            vec![take],
            None,
        );
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    fn ownership(vm: &mut Vm, raw: u64) -> Option<ResourceOwnership> {
        let handle = ResourceHandle::from_raw(raw).expect("valid raw handle");
        vm.host_context().resource_ownership(handle)
    }

    /// `(delay: Int, resource: Resource(test.guard) TakeOwned) -> bool`.
    ///
    /// The handler reads both arguments in declaration order, takes the
    /// resource argument (never the int), and optionally consumes the table
    /// entry through a real `begin_resource_access` take.
    struct TakeIntThenResource {
        seen: Arc<Mutex<Seen>>,
        consume: bool,
    }

    impl HostOwnedFunction for TakeIntThenResource {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let first = call.arg(0).cloned().unwrap_or(Value::Null);
            let second = call.arg(1).cloned().unwrap_or(Value::Null);
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = Some(first);
                seen.second_arg = Some(second);
            }
            let handle_value = call.take_arg(1)?;
            let handle = ResourceHandle::from_value(&handle_value).map_err(VmError::from)?;
            let repeat_take_failed = call.take_arg(1).is_err();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = repeat_take_failed;
                seen.handle = Some(handle.raw());
            }
            if self.consume {
                let frame =
                    call.vm()
                        .begin_resource_access(vec![ResourceAccessRequest::take_owned::<
                            GuardResource,
                        >(handle)])?;
                let _owned = frame.take_owned::<GuardResource>(0)?;
                drop(frame);
                self.seen.lock().expect("seen").consumed = true;
            }
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }

    /// `(resource: Resource TakeOwned, callback: Callable TakeOwned) -> bool`.
    ///
    /// Takes both arguments in declaration order; optionally consumes the
    /// resource and spawns the isolated owned-callable VM for the callable.
    struct TakeResourceThenCallable {
        seen: Arc<Mutex<Seen>>,
        registry: HostFunctionRegistry,
        consume: bool,
        spawn: bool,
    }

    impl HostOwnedFunction for TakeResourceThenCallable {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let first = call.arg(0).cloned().unwrap_or(Value::Null);
            let second = call.arg(1).cloned().unwrap_or(Value::Null);
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = Some(first);
                seen.second_arg = Some(second);
            }
            let handle_value = call.take_arg(0)?;
            let handle = ResourceHandle::from_value(&handle_value).map_err(VmError::from)?;
            let callable = call.take_arg(1)?;
            let repeat_take_failed = call.take_arg(1).is_err();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = repeat_take_failed;
                seen.handle = Some(handle.raw());
                seen.callable = Some(callable.clone());
            }
            if self.spawn {
                let fresh =
                    call.spawn_owned_callable_vm(&self.registry, &callable, |_vm| Ok(()))?;
                self.seen.lock().expect("seen").fresh_owns_callable =
                    Some(fresh.owns_callable(&callable));
            }
            if self.consume {
                let frame =
                    call.vm()
                        .begin_resource_access(vec![ResourceAccessRequest::take_owned::<
                            GuardResource,
                        >(handle)])?;
                let _owned = frame.take_owned::<GuardResource>(0)?;
                drop(frame);
                self.seen.lock().expect("seen").consumed = true;
            }
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }

    /// `(resource: Resource(test.guard) BorrowMut, callback: Callable TakeOwned)
    /// -> bool`: the handler illegally consumes the borrowed resource.
    struct ConsumeBorrowedResource {
        seen: Arc<Mutex<Seen>>,
    }

    impl HostOwnedFunction for ConsumeBorrowedResource {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = call.arg(0).cloned();
                seen.second_arg = call.arg(1).cloned();
            }
            let handle_value = call.arg(0).cloned().unwrap_or(Value::Null);
            let handle = ResourceHandle::from_value(&handle_value).map_err(VmError::from)?;
            let callable = call.take_arg(1)?;
            self.seen.lock().expect("seen").callable = Some(callable);
            let frame =
                call.vm()
                    .begin_resource_access(vec![ResourceAccessRequest::take_owned::<
                        GuardResource,
                    >(handle)])?;
            let _owned = frame.take_owned::<GuardResource>(0)?;
            drop(frame);
            self.seen.lock().expect("seen").consumed = true;
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
    }

    #[test]
    fn owned_int_resource_preflight_rejects_wrong_key_without_calling_host() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = schema(
            vec![
                value_param("delay", TypeSchema::Int),
                take_param("resource", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        register_other_ping(&mut registry, Arc::clone(&closes), Arc::clone(&ping_raw));
        let seen_for_factory = Arc::clone(&seen);
        registry
            .register_exact_owned("test::take", 2, take_schema.clone(), move |_context| {
                Box::new(TakeIntThenResource {
                    seen: Arc::clone(&seen_for_factory),
                    consume: true,
                })
            })
            .expect("register owned resource take");

        let mut vm = bound_vm(
            ping_then_int_program(other_ping_import(), import("test::take", take_schema), 41),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("a wrong-key resource must not reach the host");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceKeyMismatch),
            "expected a structured key mismatch, got: {error}"
        );
        assert_eq!(
            seen.lock().expect("seen").calls,
            0,
            "the pre-call contract must reject the handle before the host runs"
        );
        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        assert_eq!(
            ownership(&mut vm, raw),
            Some(ResourceOwnership::GuestOwned),
            "a rejected preflight must leave a guest-owned resource untouched"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "a rejected preflight must not consume or close the resource"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41), Value::Int(raw as i64)],
            "the host took nothing, so every drained argument must be restored exactly once"
        );
    }

    #[test]
    fn owned_int_resource_preflight_rejects_invalid_handle_without_calling_host() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = schema(
            vec![
                value_param("delay", TypeSchema::Int),
                take_param("resource", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let seen_for_factory = Arc::clone(&seen);
        registry
            .register_exact_owned("test::take", 2, take_schema.clone(), move |_context| {
                Box::new(TakeIntThenResource {
                    seen: Arc::clone(&seen_for_factory),
                    consume: true,
                })
            })
            .expect("register owned resource take");

        let invalid = Value::string("not a resource handle");
        let mut vm = bound_vm(
            plain_take_program(
                import("test::take", take_schema),
                vec![Value::Int(41), invalid.clone()],
            ),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("an invalid handle must not reach the host");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::InvalidResourceHandle),
            "expected a structured invalid-handle rejection, got: {error}"
        );
        assert_eq!(
            seen.lock().expect("seen").calls,
            0,
            "an invalid handle must be rejected before the host runs"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41), invalid],
            "both untaken arguments must be restored exactly once"
        );
    }

    #[test]
    fn owned_int_resource_transfers_and_consumes_exactly_once() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = schema(
            vec![
                value_param("delay", TypeSchema::Int),
                take_param("resource", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        register_guard_ping(&mut registry, Arc::clone(&closes), Arc::clone(&ping_raw));
        let seen_for_factory = Arc::clone(&seen);
        registry
            .register_exact_owned("test::take", 2, take_schema.clone(), move |_context| {
                Box::new(TakeIntThenResource {
                    seen: Arc::clone(&seen_for_factory),
                    consume: true,
                })
            })
            .expect("register owned resource take");

        let mut vm = bound_vm(
            ping_then_int_program(guard_ping_import(), import("test::take", take_schema), 41),
            &registry,
        );
        assert_eq!(
            vm.run().expect("the owned resource call must succeed"),
            VmStatus::Halted
        );

        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.calls, 1);
        assert_eq!(
            seen.first_arg,
            Some(Value::Int(41)),
            "argument slot 0 must carry the delay value"
        );
        assert_eq!(
            seen.second_arg,
            Some(Value::Int(raw as i64)),
            "argument slot 1 must carry the resource handle"
        );
        assert!(seen.second_take_failed, "a repeat take must be rejected");
        assert_eq!(seen.handle, Some(raw));
        assert!(seen.consumed, "the handler consumed the resource");
        drop(seen);
        assert_eq!(
            ownership(&mut vm, raw),
            Some(ResourceOwnership::Taken),
            "the consumed resource must be marked taken exactly once"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "a consumed resource must not be closed"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Bool(true)],
            "untaken arguments are released on success and the return value lands on the stack"
        );
    }

    #[test]
    fn owned_resource_callable_transfers_both_arguments_exactly_once() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = schema(
            vec![
                take_param("resource", TypeSchema::Resource(guard_key())),
                take_param("callback", callable_schema()),
            ],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        register_guard_ping(&mut registry, Arc::clone(&closes), Arc::clone(&ping_raw));
        let seen_for_factory = Arc::clone(&seen);
        registry
            .register_exact_owned("test::take", 2, take_schema.clone(), move |context| {
                Box::new(TakeResourceThenCallable {
                    seen: Arc::clone(&seen_for_factory),
                    registry: context.registry().clone(),
                    consume: true,
                    spawn: true,
                })
            })
            .expect("register owned resource+callable take");

        let mut vm = bound_vm(
            ping_then_callable_program(guard_ping_import(), import("test::take", take_schema)),
            &registry,
        );
        assert_eq!(
            vm.run()
                .expect("the owned resource+callable call must succeed"),
            VmStatus::Halted
        );

        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.calls, 1);
        assert_eq!(seen.first_arg, Some(Value::Int(raw as i64)));
        assert!(
            matches!(seen.second_arg, Some(Value::Callable(_))),
            "argument slot 1 must carry the callable value"
        );
        assert!(matches!(seen.callable, Some(Value::Callable(_))));
        assert!(seen.second_take_failed, "a repeat take must be rejected");
        assert_eq!(
            seen.fresh_owns_callable,
            Some(true),
            "the spawned VM must own the transferred callable graph"
        );
        assert!(seen.consumed);
        drop(seen);
        assert_eq!(ownership(&mut vm, raw), Some(ResourceOwnership::Taken));
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert_eq!(vm.instance.stack, vec![Value::Bool(true)]);
    }

    #[test]
    fn owned_resource_commit_reclaims_an_unconsumed_resource() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = schema(
            vec![
                value_param("delay", TypeSchema::Int),
                take_param("resource", TypeSchema::Resource(guard_key())),
            ],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        register_guard_ping(&mut registry, Arc::clone(&closes), Arc::clone(&ping_raw));
        let seen_for_factory = Arc::clone(&seen);
        registry
            .register_exact_owned("test::take", 2, take_schema.clone(), move |_context| {
                Box::new(TakeIntThenResource {
                    seen: Arc::clone(&seen_for_factory),
                    consume: false,
                })
            })
            .expect("register owned resource take");

        let mut vm = bound_vm(
            ping_then_int_program(guard_ping_import(), import("test::take", take_schema), 41),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("a declared take that was not consumed must fail the call");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceNotConsumed),
            "expected a structured not-consumed rejection, got: {error}"
        );
        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        assert_eq!(
            ownership(&mut vm, raw),
            Some(ResourceOwnership::HostOwned),
            "the unconsumed guest-owned resource must be reclaimed by the commit"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            1,
            "the reclaimed resource must be closed exactly once"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "the untaken delay argument must be restored exactly once"
        );
    }

    #[test]
    fn owned_resource_commit_reports_a_consumed_borrowed_resource() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = schema(
            vec![
                borrow_mut_param("resource", TypeSchema::Resource(guard_key())),
                take_param("callback", callable_schema()),
            ],
            TypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        register_guard_ping(&mut registry, Arc::clone(&closes), Arc::clone(&ping_raw));
        let seen_for_factory = Arc::clone(&seen);
        registry
            .register_exact_owned("test::take", 2, take_schema.clone(), move |_context| {
                Box::new(ConsumeBorrowedResource {
                    seen: Arc::clone(&seen_for_factory),
                })
            })
            .expect("register owned borrowed-resource take");

        let mut vm = bound_vm(
            ping_then_callable_program(guard_ping_import(), import("test::take", take_schema)),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("consuming a borrowed argument must fail the commit");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceAccessConflict),
            "expected a structured borrow conflict, got: {error}"
        );
        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        assert_eq!(
            ownership(&mut vm, raw),
            Some(ResourceOwnership::Taken),
            "the illegal consumption itself is not undone by the commit"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(raw as i64)],
            "the untaken (borrowed) argument must be restored exactly once"
        );
    }

    /// Owned handler that takes the callable and returns an exact `Resource`
    /// return that is not a handle at all.
    struct BadReturnValue {
        seen: Arc<Mutex<Seen>>,
    }

    impl HostOwnedFunction for BadReturnValue {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = call.arg(0).cloned();
            }
            let callable = call.take_arg(1)?;
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = call.take_arg(1).is_err();
                seen.callable = Some(callable);
            }
            Ok(CallOutcome::Return(CallReturn::one(Value::string(
                "not a resource handle",
            ))))
        }
    }

    /// Owned handler that takes the callable and returns a live handle of the
    /// wrong key, so the exact return *validates* and the ownership transfer
    /// fails.
    struct BadReturnKey {
        seen: Arc<Mutex<Seen>>,
    }

    impl HostOwnedFunction for BadReturnKey {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = call.arg(0).cloned();
            }
            let callable = call.take_arg(1)?;
            let handle = call
                .vm()
                .host_context()
                .push_resource(OtherResource {
                    closes: Arc::new(AtomicUsize::new(0)),
                })
                .expect("push other resource")
                .handle();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = call.take_arg(1).is_err();
                seen.callable = Some(callable);
                seen.handle = Some(handle.raw());
            }
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(
                handle.raw() as i64,
            ))))
        }
    }

    /// Owned handler that takes the callable, leaves a value on the operand
    /// stack, and then returns an invalid exact resource return.
    struct BadReturnWithHostStack {
        seen: Arc<Mutex<Seen>>,
    }

    impl HostOwnedFunction for BadReturnWithHostStack {
        fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            let callable = call.take_arg(1)?;
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = call.arg(0).cloned();
                seen.second_take_failed = call.take_arg(1).is_err();
                seen.callable = Some(callable);
                seen.host_marker = true;
            }
            call.vm().instance.stack.push(Value::Int(7));
            Ok(CallOutcome::Return(CallReturn::one(Value::string(
                "not a resource handle",
            ))))
        }
    }

    /// `(delay: Int, callback: Callable TakeOwned) -> Resource(test.guard)`.
    fn int_callable_resource_schema() -> HostImportSchema {
        schema(
            vec![
                value_param("delay", TypeSchema::Int),
                take_param("callback", callable_schema()),
            ],
            TypeSchema::Resource(guard_key()),
        )
    }

    fn register_bad_return(
        registry: &mut HostFunctionRegistry,
        schema: HostImportSchema,
        seen: &Arc<Mutex<Seen>>,
        bad: fn(Arc<Mutex<Seen>>) -> Box<dyn HostOwnedFunction>,
    ) {
        let seen_for_factory = Arc::clone(seen);
        registry
            .register_exact_owned("test::take", 2, schema, move |_context| {
                bad(Arc::clone(&seen_for_factory))
            })
            .expect("register owned resource-return take");
    }

    /// The exact return is rejected by *validation* (not a handle): the host
    /// already accepted and took its argument, so the taken callable stays
    /// consumed while every untaken argument is restored exactly once.
    #[test]
    fn owned_dispatch_restores_untaken_arguments_on_a_rejected_resource_return() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = int_callable_resource_schema();
        let mut registry = HostFunctionRegistry::empty();
        register_bad_return(&mut registry, take_schema.clone(), &seen, |seen| {
            Box::new(BadReturnValue { seen })
        });

        let mut vm = bound_vm(
            int_then_callable_program(import("test::take", take_schema), 41),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("a non-handle exact resource return must be rejected");
        let message = match &error {
            VmError::TypeMismatch(message) => *message,
            other => panic!("expected a structured resource-handle rejection, got: {other}"),
        };
        assert_eq!(message, "resource handle");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.calls, 1);
        assert!(seen.second_take_failed);
        assert!(
            matches!(seen.callable, Some(Value::Callable(_))),
            "the argument the host took stays consumed by the host"
        );
        drop(seen);
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "the untaken delay argument must be restored exactly once after a \
             rejected exact return; nothing is dropped twice"
        );
    }

    /// The exact return *validates* but its ownership transfer fails (wrong
    /// key): the taken callable stays consumed, the untaken delay is restored,
    /// and the wrongly-keyed resource is never transferred to the guest.
    #[test]
    fn owned_dispatch_restores_untaken_arguments_on_a_failed_ownership_transfer() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = int_callable_resource_schema();
        let mut registry = HostFunctionRegistry::empty();
        register_bad_return(&mut registry, take_schema.clone(), &seen, |seen| {
            Box::new(BadReturnKey { seen })
        });

        let mut vm = bound_vm(
            int_then_callable_program(import("test::take", take_schema), 41),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("a key-mismatched exact resource return must be rejected");
        assert_eq!(
            error.resource_error_code(),
            Some(ResourceErrorCode::ResourceKeyMismatch),
            "expected a structured key mismatch, got: {error}"
        );
        let raw = seen
            .lock()
            .expect("seen")
            .handle
            .expect("the handler recorded its returned handle");
        assert_eq!(
            ownership(&mut vm, raw),
            Some(ResourceOwnership::HostOwned),
            "a failed ownership transfer must leave the resource host-owned"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "the untaken delay argument must be restored exactly once"
        );
        assert!(
            matches!(
                seen.lock().expect("seen").callable,
                Some(Value::Callable(_))
            ),
            "the argument the host took stays consumed by the host"
        );
    }

    /// A rejected exact return keeps the values the host pushed onto the
    /// operand stack and restores the untaken arguments exactly once.
    #[test]
    fn owned_dispatch_preserves_the_host_stack_on_a_rejected_resource_return() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = int_callable_resource_schema();
        let mut registry = HostFunctionRegistry::empty();
        register_bad_return(&mut registry, take_schema.clone(), &seen, |seen| {
            Box::new(BadReturnWithHostStack { seen })
        });

        let mut vm = bound_vm(
            int_then_callable_program(import("test::take", take_schema), 41),
            &registry,
        );
        let _ = vm
            .run()
            .expect_err("a non-handle exact resource return must be rejected");
        assert!(
            seen.lock().expect("seen").host_marker,
            "the handler must have pushed its stack value"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41), Value::Int(7)],
            "the restored untaken argument precedes the preserved host-stack value"
        );
    }
}
