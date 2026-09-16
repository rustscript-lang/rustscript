use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, Wake, Waker};

use crate::BuiltinFunction;
use crate::host_api::{HostImportSchema, HostParamPassing, HostTypeSchema, ResourceTypeKey};
use crate::vm::execution_scope::ExecutionScopeError;
use crate::vm::operation::{OperationCancelReason, OperationId, OperationOutcome};
use crate::vm::resource::error::{ResourceError, ResourceErrorCode};
use crate::vm::resource::handle::ResourceHandle;
use crate::vm::resource::table::ResourceTable;

use super::async_host::{HostFuture, HostFutureOutput, preserve_stream_cleanup};
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

// The public seam of this contract (the trait, the owned-call view, and the
// factory context) is defined in `crate::host_api` so the crate root can
// re-export it for embedding adapters; the dispatch below, the registry entry
// kind, and every VM-coupled operation stay here, next to the VM invariants
// they rely on.
pub use crate::host_api::{HostOwnedFunction, OwnedHostCall, OwnedHostContext};

type OwnedHostFactory = dyn Fn(OwnedHostContext<'_>) -> Box<dyn HostOwnedFunction> + Send + Sync;

impl<'vm> OwnedHostCall<'vm> {
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
        let mut vm = Vm::new_shared(Arc::clone(&program));
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
    /// reports reusable. Module state, host bindings, and immutable program
    /// configuration remain attached to the VM across this operation.
    pub(crate) fn recover_owned_callable(&mut self, callable: &Value) -> VmResult<()> {
        let mut owned = Vec::new();
        let mut visited = Vec::new();
        collect_owned_callable_graph(self, callable, 0, &mut visited, &mut owned)?;

        // Preserve a typed cancellation failure, but still run the reset: the
        // scope close is the generic final cleanup path for any operation that
        // could not be retired by the waiting-slot helper.
        let cancellation =
            self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested);
        let reset = self.reset_for_reuse();
        if !self.is_reusable() {
            return Err(reset.err().unwrap_or(VmError::InvalidFrameState(
                "owned callable vm is not reusable",
            )));
        }
        self.instance.owned_callables.extend(owned);
        clear_owned_callable_vm_state(self);
        cancellation?;
        reset?;
        Ok(())
    }
}

fn clear_owned_callable_vm_state(vm: &mut Vm) {
    // `new_shared` and `reset_for_reuse` leave the normal root frame in
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
        schema.contains_resource_with_named_types(&source.program.named_struct_decls)
    }) {
        return Err(VmError::HostError(
            "owned callable graph contains a resource-bearing capture".to_string(),
        ));
    }
    collect_owned_callable_graph(source, captured, depth, visited, owned)
}

/// Maximum schema depth of the registration-time resource classification walk.
///
/// A schema that nests deeper than this is rejected at registration instead of
/// being classified as resource-free: classification is fail-closed, so an
/// unobservable schema can never slip past the passing-mode contract.
const MAX_REGISTRATION_SCHEMA_DEPTH: u8 = 64;

/// Resource-access mode of one resource-passing argument occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceAccessMode {
    Borrow,
    BorrowMut,
    TakeOwned,
}

/// Maps a catalog passing mode onto its resource-access mode; `Value` is not a
/// resource operation and returns `None`.
fn passing_to_access_mode(passing: HostParamPassing) -> Option<ResourceAccessMode> {
    match passing {
        HostParamPassing::Value => None,
        HostParamPassing::Borrow => Some(ResourceAccessMode::Borrow),
        HostParamPassing::BorrowMut => Some(ResourceAccessMode::BorrowMut),
        HostParamPassing::TakeOwned => Some(ResourceAccessMode::TakeOwned),
    }
}

/// Depth-bounded, named-aware probe for a resource occurrence in a host schema.
///
/// Named structs are walked through their inline field bodies and callable
/// signatures through their parameters and result, so a resource nested
/// anywhere inside a parameter schema is classified exactly like a directly
/// nested one. The walk is fail-closed: a schema it cannot fully observe
/// (depth overflow) is an error, never a "contains no resource" answer.
fn host_schema_contains_resource(schema: &HostTypeSchema) -> Result<bool, String> {
    let mut pending: Vec<(&HostTypeSchema, u8)> = vec![(schema, 0)];
    while let Some((current, depth)) = pending.pop() {
        if depth > MAX_REGISTRATION_SCHEMA_DEPTH {
            return Err(format!(
                "schema is nested deeper than the supported limit of \
                 {MAX_REGISTRATION_SCHEMA_DEPTH} levels"
            ));
        }
        match current {
            HostTypeSchema::Resource(_) => return Ok(true),
            HostTypeSchema::Array(inner)
            | HostTypeSchema::Map(inner)
            | HostTypeSchema::Optional(inner) => pending.push((inner, depth + 1)),
            HostTypeSchema::Callable { params, result } => {
                pending.push((result, depth + 1));
                for param in params {
                    pending.push((param, depth + 1));
                }
            }
            HostTypeSchema::Named { fields, .. } => {
                for field in fields {
                    pending.push((&field.ty, depth + 1));
                }
            }
            HostTypeSchema::Unknown
            | HostTypeSchema::Null
            | HostTypeSchema::Int
            | HostTypeSchema::Float
            | HostTypeSchema::Number
            | HostTypeSchema::Bool
            | HostTypeSchema::String
            | HostTypeSchema::Bytes => {}
        }
    }
    Ok(false)
}

/// The expected key of a *directly addressable* resource-passing schema.
///
/// Only a direct `Resource(key)` or a single `Optional<Resource(key)>` is
/// addressable by the handle ABI (a `Null` argument legally skips the optional
/// layer). A resource nested inside an aggregate, named struct, or callable
/// signature has no addressable handle and is rejected at registration, so it
/// can never reach the call-time contract.
fn addressable_host_resource_key(schema: &HostTypeSchema) -> Option<(ResourceTypeKey, bool)> {
    match schema {
        HostTypeSchema::Resource(key) => Some((key.clone(), false)),
        HostTypeSchema::Optional(inner) => match inner.as_ref() {
            HostTypeSchema::Resource(key) => Some((key.clone(), true)),
            _ => None,
        },
        _ => None,
    }
}

/// Registration-time passing-mode contract of one catalog schema.
///
/// `owned_value_transfer` is set only by
/// [`HostFunctionRegistry::register_exact_owned`]: a resource-free `TakeOwned`
/// parameter is then a legitimate owned *value* transfer (callable, scalar, or
/// aggregate) instead of the silent-drop hazard every other registration kind
/// would create.
///
/// Every rejection is a structured message; the caller wraps it in the
/// registry's own structured schema error, so a refused registration never
/// mutates the registry.
fn validate_registration_passing(
    schema: &HostImportSchema,
    owned_value_transfer: bool,
) -> Result<(), String> {
    for param in &schema.params {
        let has_resource = host_schema_contains_resource(&param.schema).map_err(|detail| {
            format!(
                "parameter '{}' schema could not be classified as a resource or value: {detail}",
                param.name
            )
        })?;
        if !has_resource {
            match param.passing {
                HostParamPassing::Value => {}
                HostParamPassing::TakeOwned if owned_value_transfer => {}
                _ => {
                    return Err(format!(
                        "parameter '{}' declares {:?} passing but its schema {:#?} contains no \
                         resource; non-resource parameters must use Value, and a resource-free \
                         TakeOwned transfer is only legal on the owned-dispatch registration \
                         path",
                        param.name, param.passing, param.schema,
                    ));
                }
            }
            continue;
        }
        if param.passing == HostParamPassing::Value {
            return Err(format!(
                "parameter '{}' carries a resource (schema {:#?}); resource parameters must use \
                 Borrow/BorrowMut/TakeOwned",
                param.name, param.schema,
            ));
        }
        if owned_value_transfer && addressable_host_resource_key(&param.schema).is_none() {
            return Err(format!(
                "resource-passing parameter '{}' schema {:#?} is not directly addressable by the \
                 handle ABI",
                param.name, param.schema,
            ));
        }
    }
    let return_has_resource = host_schema_contains_resource(&schema.return_type)
        .map_err(|detail| format!("return schema could not be classified: {detail}"))?;
    if return_has_resource
        && owned_value_transfer
        && addressable_host_resource_key(&schema.return_type).is_none()
    {
        return Err(format!(
            "exact return schema {:#?} contains a resource nested inside an aggregate; only \
             Resource(key) and Optional<Resource(key>) returns are representable by the handle ABI",
            schema.return_type,
        ));
    }
    Ok(())
}

/// Whether a registered schema needs the guarded call wrapper.
///
/// A schema needs the guard exactly when it declares an addressable
/// resource-passing parameter: those are the only arguments whose preflight,
/// alias graph, and post-call commit the dispatcher must enforce. A
/// resource-free `TakeOwned` parameter is an owned value transfer and needs no
/// resource contract.
fn schema_requires_guard(schema: &HostImportSchema) -> bool {
    schema.params.iter().any(|param| {
        param.passing != HostParamPassing::Value
            && addressable_host_resource_key(&param.schema).is_some()
    })
}

/// One resource-passing argument occurrence in a host call.
#[derive(Clone, Debug)]
struct ExactResourceSpec {
    /// Argument index in the (drained) call argument list.
    arg_index: usize,
    handle: ResourceHandle,
    key: ResourceTypeKey,
    mode: ResourceAccessMode,
}

fn invalid_resource_handle_error(message: String) -> VmError {
    VmError::ExecutionScope(ExecutionScopeError::Resource(ResourceError::new(
        ResourceErrorCode::InvalidResourceHandle,
        "resource::exact_call",
        message,
    )))
}

fn resource_error(error: ResourceError) -> VmError {
    VmError::ExecutionScope(ExecutionScopeError::Resource(error))
}

/// Decodes one raw handle argument into a validated [`ResourceHandle`].
///
/// Only the handle ABI's `int` encoding is accepted; anything else is a
/// structured invalid-handle rejection raised before the host function runs.
fn resource_handle_argument(value: &Value) -> VmResult<ResourceHandle> {
    let Value::Int(raw) = value else {
        return Err(invalid_resource_handle_error(format!(
            "resource argument must be a handle token, got {value:?}"
        )));
    };
    let raw = u64::try_from(*raw).map_err(|_| {
        invalid_resource_handle_error(
            "resource handle token must be a positive signed integer".to_string(),
        )
    })?;
    ResourceHandle::from_raw(raw).map_err(resource_error)
}

fn resource_access_conflict_error(left: &ExactResourceSpec, right: &ExactResourceSpec) -> VmError {
    resource_error(
        ResourceError::new(
            ResourceErrorCode::ResourceAccessConflict,
            "resource::exact_call",
            format!(
                "resource argument {} and {} alias handle {} with conflicting access modes \
                 {:?}/{:?}",
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

fn consumed_borrowed_resource_error(spec: &ExactResourceSpec) -> VmError {
    resource_error(
        ResourceError::new(
            ResourceErrorCode::ResourceAccessConflict,
            "resource::exact_call",
            format!(
                "resource argument at index {} declared {:?} was consumed by the host function",
                spec.arg_index, spec.mode,
            ),
        )
        .with_value(spec.handle.raw()),
    )
}

/// The single type-erased resource contract of one host call with
/// resource-passing parameters.
///
/// 1. **build** extracts every resource-passing occurrence from the schema and
///    the raw arguments (addressability is guaranteed at registration), decodes
///    each handle, and rejects illegal same-handle aliases;
/// 2. **validate** checks every occurrence against the live execution scope
///    before the user function runs — read-only, zero mutation, so a bad
///    argument never reaches the host function;
/// 3. **commit** runs after the function returns: a declared `TakeOwned` that
///    was consumed passes, a declared `TakeOwned` still live in the table is
///    reported and reclaimed exactly once (never closed twice), and a
///    `Borrow`/`BorrowMut` argument the function consumed is a structured
///    [`ResourceErrorCode::ResourceAccessConflict`].
#[derive(Debug)]
struct ExactHostCallContract {
    specs: Vec<ExactResourceSpec>,
}

impl ExactHostCallContract {
    /// Builds the contract from the import schema and raw arguments.
    ///
    /// Any failure is structured and consumes nothing: the whole argument list
    /// is restored by the owned dispatch because nothing was taken.
    fn build(schema: &HostImportSchema, args: &[Value]) -> VmResult<Self> {
        let mut specs = Vec::new();
        for (index, param) in schema.params.iter().enumerate() {
            let Some(mode) = passing_to_access_mode(param.passing) else {
                // `Value` parameters are not resource operations; a
                // resource-bearing `Value` parameter is rejected at
                // registration.
                continue;
            };
            let has_resource = host_schema_contains_resource(&param.schema).map_err(|detail| {
                VmError::HostError(format!(
                    "host call parameter '{}' schema cannot be classified: {detail}",
                    param.name
                ))
            })?;
            if !has_resource {
                // A resource-free `TakeOwned` parameter is an owned *value*
                // transfer (legal only on the owned-dispatch path). Reaching
                // call time with any other resource-passing mode means the
                // registration funnel was bypassed, so refuse to run rather
                // than silently dropping the declared mode.
                if mode == ResourceAccessMode::TakeOwned {
                    continue;
                }
                return Err(VmError::HostError(format!(
                    "resource-passing parameter '{}' declares {mode:?} but its schema {:#?} \
                     contains no resource",
                    param.name, param.schema,
                )));
            }
            let Some((key, optional)) = addressable_host_resource_key(&param.schema) else {
                return Err(VmError::HostError(format!(
                    "resource-passing parameter '{}' schema {:#?} is not directly addressable by \
                     the handle ABI",
                    param.name, param.schema,
                )));
            };
            let value = args.get(index).ok_or_else(|| {
                invalid_resource_handle_error(format!(
                    "exact host call is missing argument at index {index}"
                ))
            })?;
            if optional && matches!(value, Value::Null) {
                // Legal skip for Optional(Resource).
                continue;
            }
            let handle = resource_handle_argument(value)?;
            specs.push(ExactResourceSpec {
                arg_index: index,
                handle,
                key,
                mode,
            });
        }
        // Alias graph: only shared `Borrow` + shared `Borrow` is legal for one
        // handle. Duplicate `TakeOwned`, `TakeOwned` + `Borrow`/`BorrowMut`, and
        // `BorrowMut` + `Borrow` all reject here, before the host function runs.
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
    /// execution scope.
    ///
    /// The table's declared-key probe observes the handle encoding, arena,
    /// slot generation, concrete/declared key, and open state without
    /// borrowing, taking, closing, or otherwise mutating the resource.
    fn validate(&self, vm: &Vm) -> VmResult<()> {
        for spec in &self.specs {
            vm.host
                .execution_scope
                .resources()
                .validate_resource_type_key(spec.handle, &spec.key)
                .map_err(resource_error)?;
        }
        Ok(())
    }

    /// Whether one occurrence is still live (open) in the execution scope.
    ///
    /// After a successful preflight this is the consumption probe: a consumed
    /// (taken) or closed resource is no longer live.
    fn is_live(vm: &Vm, spec: &ExactResourceSpec) -> bool {
        vm.host
            .execution_scope
            .resources()
            .validate_resource_type_key(spec.handle, &spec.key)
            .is_ok()
    }

    /// Post-call commit / cleanup. Returns the first structured error, if any.
    ///
    /// - A declared `TakeOwned` that is no longer live was consumed by this
    ///   invocation.
    /// - A declared `TakeOwned` still live is reported and reclaimed exactly
    ///   once: the guard takes the drained argument so the declared move is
    ///   honored and the guest never gets a stale handle back. The resource
    ///   itself stays owned by this execution scope and is released exactly
    ///   once by the ordinary scope close, which retires each live resource
    ///   with a single `begin_close` — the reclaim never closes it twice.
    /// - A `Borrow`/`BorrowMut` argument that is no longer live was consumed
    ///   by the host function and is a structured access conflict.
    fn commit(&self, call: &mut OwnedHostCall<'_>) -> Option<VmError> {
        let mut first_error = None;
        for spec in &self.specs {
            if Self::is_live(call.vm_ref(), spec) {
                match spec.mode {
                    ResourceAccessMode::TakeOwned => {
                        first_error.get_or_insert_with(|| {
                            // A4's resource table has no type-erased
                            // "not consumed" code and no type-erased close:
                            // the failure is reported as a structured host
                            // error naming the argument, handle, and key.
                            VmError::HostError(format!(
                                "declared TakeOwned argument at index {} (handle {}, key {}) was \
                                 not consumed by the host function; the argument is reclaimed \
                                 and its resource is released by the scope close",
                                spec.arg_index,
                                spec.handle.raw(),
                                spec.key,
                            ))
                        });
                        // Reclaim exactly once: honour the declared move so the
                        // unconsumed handle is never handed back to the guest.
                        if !call.is_taken(spec.arg_index) {
                            let _ = call.take_arg(spec.arg_index);
                        }
                    }
                    ResourceAccessMode::Borrow | ResourceAccessMode::BorrowMut => {}
                }
                continue;
            }
            if matches!(
                spec.mode,
                ResourceAccessMode::Borrow | ResourceAccessMode::BorrowMut
            ) {
                first_error.get_or_insert_with(|| consumed_borrowed_resource_error(spec));
            }
        }
        first_error
    }
}

/// Owned-dispatch sibling of the ordinary guarded host call wrapper.
///
/// Resource-bearing exact schemas keep their full preflight/commit contract
/// when they are registered through
/// [`HostFunctionRegistry::register_exact_owned`]: the argument list is the
/// owned call's already drained argument list, and a resource-free `TakeOwned`
/// value transfer is simply not a resource operation.
struct GuardedOwnedHostFunction {
    inner: Box<dyn HostOwnedFunction>,
    schema: HostImportSchema,
}

impl HostOwnedFunction for GuardedOwnedHostFunction {
    fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
        let contract = ExactHostCallContract::build(&self.schema, call.args())?;
        contract.validate(call.vm_ref())?;
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.inner.call(call)));
        match outcome {
            Ok(Ok(outcome)) => match contract.commit(call) {
                Some(error) => Err(error),
                None => Ok(outcome),
            },
            Ok(Err(error)) => {
                // The host function failed: the primary error is preserved,
                // while an unconsumed owned resource is still reclaimed (and a
                // consumed borrowed resource still reported is secondary).
                let _ = contract.commit(call);
                Err(error)
            }
            Err(payload) => {
                let _ = contract.commit(call);
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// Terminal state supplied to [`HostAsyncBridge::cleanup_op`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostAsyncOpTerminal {
    /// The submitted future produced a normal result.
    Completed,
    /// The bridge acknowledged cancellation and quiescence.
    Cancelled,
    /// The submitted future failed and no longer owns host work.
    Failed,
}

impl HostAsyncOpTerminal {
    /// Returns the reason used when default cleanup finalizes this terminal
    /// operation. Cleanup is a terminal resource-release action rather than a
    /// new cancellation request, so every terminal state uses the stable
    /// `Requested` compatibility reason; an actual cancellation reason is
    /// delivered earlier through `request_cancel_op`.
    pub const fn cleanup_reason(self) -> OperationCancelReason {
        match self {
            Self::Completed => OperationCancelReason::Requested,
            Self::Cancelled => OperationCancelReason::Requested,
            Self::Failed => OperationCancelReason::Requested,
        }
    }
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
    /// need a lifecycle reason. It is used as a best-effort fallback by the
    /// default [`request_cancel_op`](Self::request_cancel_op) implementation.
    fn cancel_op(&mut self, _op_id: HostOpId) {}

    /// Legacy cancellation hook kept for bridge implementations that do not
    /// need a lifecycle reason. New bridges should implement
    /// [`request_cancel_op`](Self::request_cancel_op) and
    /// [`poll_cancel_op`](Self::poll_cancel_op) instead.
    fn cancel_op_with_reason(&mut self, op_id: HostOpId, _reason: OperationCancelReason) {
        self.cancel_op(op_id);
    }

    /// Requests cancellation of one bridge-owned operation.
    ///
    /// Returning `Ok(())` only records that the request was accepted. It does
    /// not mean that the operation has stopped; callers must poll
    /// [`poll_cancel_op`](Self::poll_cancel_op) until it returns `Ready(Ok(()))`.
    /// The default invokes the legacy best-effort hook, then fails explicitly so
    /// an adapter that has not opted into acknowledgement can never claim
    /// quiescence.
    fn request_cancel_op(
        &mut self,
        op_id: HostOpId,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        self.cancel_op_with_reason(op_id, reason);
        Err(VmError::HostError(format!(
            "async host bridge does not provide cancellation acknowledgement for op {op_id}"
        )))
    }

    /// Polls completion of a previously accepted cancellation request.
    /// `Ready(Ok(()))` is the bridge's acknowledgement that the operation is
    /// terminal and quiescent. The default fails closed rather than treating a
    /// no-op implementation as an acknowledgement.
    fn poll_cancel_op(&mut self, op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        Poll::Ready(Err(VmError::HostError(format!(
            "async host bridge does not provide cancellation acknowledgement for op {op_id}"
        ))))
    }

    /// Runs bridge-side cleanup after a terminal/quiescent outcome has been
    /// reported. The VM invokes this at most once for each tracked operation.
    /// The default preserves compatibility with bridges whose legacy
    /// `cancel_op` method also removes completed operation state while routing
    /// through the reason-aware hook for newer bridges.
    fn cleanup_op(&mut self, op_id: HostOpId, terminal: HostAsyncOpTerminal) -> VmResult<()> {
        self.cancel_op_with_reason(op_id, terminal.cleanup_reason());
        Ok(())
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
    /// Owned-dispatch entry: the factory receives the registry at bind time and
    /// produces a per-VM owned host function that consumes its arguments.
    OwnedFactory(Arc<OwnedHostFactory>),
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
    InvalidSchema {
        name: String,
        detail: String,
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
            Self::InvalidSchema { name, detail } => {
                write!(
                    f,
                    "catalog schema for '{name}' exceeds host schema limits: {detail}"
                )
            }
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
    crate::host_api::validate_optional_host_import_schemas(schemas).map_err(|error| {
        VmError::HostError(format!("invalid host import schema collection: {error}"))
    })?;
    for schema in schemas.iter().flatten() {
        schema.validate().map_err(|error| {
            VmError::HostError(format!(
                "invalid host import schema '{}': {error}",
                schema.name
            ))
        })?;
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
    /// Catalog named-struct bodies. Compiler identity stays `TypeSchema::Named`;
    /// this table supplies Object bodies for nested-resource classification.
    named_struct_schemas: Arc<HashMap<String, crate::compiler::TypeSchema>>,
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
            named_struct_schemas: Arc::new(HashMap::new()),
        }
    }

    pub fn new() -> Self {
        let mut registry = Self::empty();
        crate::install_default_host_functions(&mut registry);
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

    /// Merges catalog named-struct bodies used to classify nested resources
    /// inside `TypeSchema::Named` values. Compiler identity stays named;
    /// runtime values remain maps.
    ///
    /// Identical duplicate bodies are accepted. Conflicting bodies for the
    /// same name are rejected without mutating the registry, so callers that
    /// compose HTTP/SQLite/JIT after defaults cannot silently discard earlier
    /// schemas.
    pub fn install_named_struct_schemas(
        &mut self,
        schemas: HashMap<String, crate::compiler::TypeSchema>,
    ) -> VmResult<()> {
        if schemas.is_empty() {
            return Ok(());
        }
        for (name, schema) in &schemas {
            if let Some(existing) = self.named_struct_schemas.get(name)
                && existing != schema
            {
                return Err(VmError::HostError(format!(
                    "conflicting named struct schema '{name}'"
                )));
            }
        }
        let map = Arc::make_mut(&mut self.named_struct_schemas);
        for (name, schema) in schemas {
            map.entry(name).or_insert(schema);
        }
        self.invalidate_plan_cache();
        Ok(())
    }

    /// Catalog named-struct object bodies installed for VM resource walks.
    pub fn named_struct_schemas(&self) -> &HashMap<String, crate::compiler::TypeSchema> {
        &self.named_struct_schemas
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
        if let Err(error) = schema.validate() {
            return Err(RegistrySchemaError::InvalidSchema {
                name: schema.name.clone(),
                detail: error.to_string(),
            });
        }
        // Passing-mode / resource-shape contract. Only the owned-dispatch entry
        // kind permits a resource-free `TakeOwned` parameter (an owned value
        // transfer); every other path keeps the structural rejection, and a
        // guarded owned registration additionally requires every
        // resource-bearing parameter and return to be directly addressable by
        // the handle ABI.
        if let Err(detail) = validate_registration_passing(
            &schema,
            matches!(kind, RegistryEntryKind::OwnedFactory(_)),
        ) {
            return Err(RegistrySchemaError::InvalidSchema {
                name: schema.name.clone(),
                detail,
            });
        }
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

    /// Applies a registry extension to a private snapshot and publishes it
    /// only after every registration succeeds. Extensions use this to keep
    /// catalog and dispatch state atomic when a later schema is invalid.
    pub fn transactionally<R, F>(&mut self, register: F) -> VmResult<R>
    where
        F: FnOnce(&mut Self) -> VmResult<R>,
    {
        let mut staged = self.clone();
        // Clone shares the generation AtomicU64. Staging must not bump the live
        // counter if this call later rolls back.
        staged.registry_generation = Arc::new(AtomicU64::new(
            self.registry_generation.load(Ordering::Relaxed),
        ));
        let result = register(&mut staged)?;
        *self = staged;
        Ok(result)
    }

    /// Registers one exact catalog entry while checking the source-level
    /// function identity supplied by an extension.
    pub fn register_exact_static(
        &mut self,
        name: &str,
        arity: u8,
        schema: HostImportSchema,
        function: StaticHostFunction,
    ) -> VmResult<u16> {
        if schema.name != name || schema.arity() != usize::from(arity) {
            return Err(VmError::HostError(format!(
                "host schema for '{name}' does not match its exact adapter identity"
            )));
        }
        self.register_catalog_static(schema, function)
            .map_err(|error| VmError::HostError(error.to_string()))
    }

    /// Registers an exact host function whose dispatch drains the call operands
    /// and transfers ownership of the arguments the function takes.
    ///
    /// The exact schema follows the same registration identity check as every
    /// other exact entry (`name`/`arity` must match the schema), plus one
    /// owned-dispatch allowance: a `TakeOwned` parameter may carry a
    /// *resource-free* schema (a callable, scalar, or aggregate value) and is
    /// then transferred as an owned value rather than an addressable handle.
    ///
    /// Resource-bearing parameters are fully supported: every `Borrow`,
    /// `BorrowMut`, and `TakeOwned` parameter that carries a resource is
    /// wrapped by the guarded owned contract — read-only pre-call preflight
    /// against the live execution scope, alias/conflict validation of the
    /// handle graph, and a post-call commit that reports an unconsumed
    /// `TakeOwned` argument and reclaims it exactly once while a consumed
    /// `Borrow`/`BorrowMut` argument is a structured access conflict. Such a
    /// parameter and any resource-bearing return must be directly addressable
    /// by the handle ABI (`Resource(key)` or `Optional<Resource(key)>`); a
    /// nested resource is rejected at registration.
    ///
    /// `factory` runs once per bind and receives the registry through
    /// [`OwnedHostContext`], so an owned host function can retain the
    /// immutable registry/binding configuration it needs to spawn an isolated
    /// owned-value execution VM. Registering the same name+schema twice is an
    /// explicit error (no silent replacement).
    pub fn register_exact_owned(
        &mut self,
        name: &str,
        arity: u8,
        schema: HostImportSchema,
        factory: impl Fn(OwnedHostContext<'_>) -> Box<dyn HostOwnedFunction> + Send + Sync + 'static,
    ) -> VmResult<u16> {
        if schema.name != name || schema.arity() != usize::from(arity) {
            return Err(VmError::HostError(format!(
                "host schema for '{name}' does not match its exact adapter identity"
            )));
        }
        self.register_catalog_entry(schema, RegistryEntryKind::OwnedFactory(Arc::new(factory)))
            .map_err(|error| VmError::HostError(error.to_string()))
    }

    /// Registers an owned-dispatch catalog entry whose dispatch drains the
    /// call operands and transfers ownership of the arguments the function
    /// takes.
    ///
    /// This is the descriptor path for the exact owned registration described
    /// on [`HostFunctionRegistry::register_exact_owned`]: the import identity
    /// comes from the supplied catalog schema instead of a separate
    /// name/arity pair, and every ownership, alias, and resource-bearing
    /// guarantee of the exact owned path is unchanged. `factory` runs once per
    /// bind and receives the registry through [`OwnedHostContext`].
    pub fn register_catalog_owned(
        &mut self,
        schema: HostImportSchema,
        factory: &'static dyn super::host_extension::HostOwnedAdapterFactory,
    ) -> Result<u16, RegistrySchemaError> {
        self.register_catalog_entry(
            schema,
            RegistryEntryKind::OwnedFactory(Arc::new(move |context: OwnedHostContext<'_>| {
                factory.create(context)
            })),
        )
    }

    /// Grants a registered extension import its host capability without
    /// coupling the VM to the extension's concrete domain.
    pub fn authorize_registered_builtin_import(&mut self, name: &str) {
        self.capability_profile = Arc::new(self.capability_profile.with_host_import(name));
        self.invalidate_plan_cache();
    }

    /// Marks an exact import as owning its pending operation. Pending
    /// dispatch is resolved from the generic VM operation/stream registries;
    /// the marker is intentionally a registration hook with no domain state.
    pub fn mark_exact_runtime_owned_pending(&mut self, name: &str) -> VmResult<()> {
        if !self.contains_name(name) {
            return Err(VmError::HostError(format!(
                "cannot mark unregistered host import '{name}' as runtime-owned"
            )));
        }
        Ok(())
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
                RegistryEntryKind::OwnedFactory(factory) => {
                    let function = factory(OwnedHostContext { registry: self });
                    let function: Box<dyn HostOwnedFunction> = match entry.schema.as_ref() {
                        Some(schema) if schema_requires_guard(schema) => {
                            Box::new(GuardedOwnedHostFunction {
                                inner: function,
                                schema: schema.clone(),
                            })
                        }
                        _ => function,
                    };
                    vm.register_owned_function(function);
                }
            }
            let host_slot = vm.host.host_function_schemas.len() - 1;
            if let Some(schema) = vm.host.host_function_schemas.get_mut(host_slot) {
                *schema = plan.registry_schemas.get(host_slot).cloned().flatten();
            }
        }
        vm.set_default_host_fallback_enabled(false);
        vm.host.named_struct_schemas = Arc::clone(&self.named_struct_schemas);
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
        (HostTypeSchema::Resource(expected), TypeSchema::Resource(actual)) => expected == actual,
        (
            HostTypeSchema::Named {
                name: expected_name,
                ..
            },
            TypeSchema::Named(actual_name, args),
        ) => expected_name == actual_name && args.is_empty(),
        (HostTypeSchema::Named { fields, .. }, TypeSchema::Object(actual_fields)) => {
            named_fields_match_object(fields, actual_fields)
        }
        _ => false,
    }
}

fn named_fields_match_object(
    fields: &[crate::host_api::HostStructField],
    actual_fields: &HashMap<String, crate::compiler::TypeSchema>,
) -> bool {
    fields
        .iter()
        .all(|field| match actual_fields.get(&field.name) {
            Some(actual) => callable_schema_matches(&field.ty, actual),
            None => matches!(field.ty, HostTypeSchema::Optional(_)),
        })
        && actual_fields
            .keys()
            .all(|name| fields.iter().any(|field| field.name == *name))
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
        (
            HostTypeSchema::Named {
                name: expected_name,
                fields: expected_fields,
            },
            HostTypeSchema::Named {
                name: actual_name,
                fields: actual_fields,
            },
        ) => {
            expected_name == actual_name
                && expected_fields.iter().all(|expected| {
                    actual_fields.iter().any(|actual| {
                        actual.name == expected.name
                            && host_callable_schema_matches(&expected.ty, &actual.ty)
                    })
                })
                && actual_fields.iter().all(|actual| {
                    expected_fields
                        .iter()
                        .any(|expected| expected.name == actual.name)
                        || matches!(actual.ty, HostTypeSchema::Optional(_))
                })
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
        HostTypeSchema::Named { fields, .. } => {
            let Value::Map(values) = value else {
                return Err(VmError::TypeMismatch("map"));
            };
            for field in fields {
                match values.get(&Value::string(&field.name)) {
                    Some(field_value) => {
                        validate_host_value(field_value, &field.ty, program, resources)?;
                    }
                    None if matches!(field.ty, HostTypeSchema::Optional(_)) => {}
                    None => return Err(VmError::TypeMismatch("map")),
                }
            }
            Ok(())
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
    Manual,
    ScopedOperation,
    CallableStream,
    CallableStreamTermination,
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

    pub fn set_async_bridge(&mut self, bridge: Box<dyn HostAsyncBridge>) -> VmResult<()> {
        if self.host.has_active_bridge_operations()
            || self
                .instance
                .waiting_host_op
                .as_ref()
                .is_some_and(|waiting| {
                    matches!(
                        waiting.source,
                        crate::vm::host::WaitingHostOpSource::HostBridge
                    )
                })
        {
            return Err(VmError::HostError(
                "cannot replace async bridge while an active host operation is present".to_string(),
            ));
        }
        self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested)?;
        self.host.async_bridge = Some(bridge);
        Ok(())
    }

    pub fn clear_async_bridge(&mut self) -> VmResult<()> {
        if self.host.has_active_bridge_operations()
            || self
                .instance
                .waiting_host_op
                .as_ref()
                .is_some_and(|waiting| {
                    matches!(
                        waiting.source,
                        crate::vm::host::WaitingHostOpSource::HostBridge
                    )
                })
        {
            return Err(VmError::HostError(
                "cannot clear async bridge while an active host operation is present".to_string(),
            ));
        }
        self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested)?;
        self.host.async_bridge = None;
        Ok(())
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

    /// Configures the per-item event bound applied by `stream::emit` on the
    /// invocation stream.
    pub fn set_event_limits(&mut self, max_payload_bytes: usize, max_depth: usize) -> VmResult<()> {
        let limits = super::runtime::EventLimits::new(max_payload_bytes, max_depth)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        self.run_ctx.runtime_context = super::runtime::RuntimeContext::with_config(
            super::runtime::RuntimeContextConfig::new(limits),
        )
        .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(())
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

    pub(crate) fn standard_regex_match(&mut self, pattern: &str, text: &str) -> VmResult<bool> {
        let composition = self.host.standard_composition.clone().ok_or_else(|| {
            VmError::HostError("standard surface composition is not installed".to_string())
        })?;
        composition.regex_match(self, pattern, text)
    }

    pub(crate) fn standard_regex_replace(
        &mut self,
        pattern: &str,
        text: &str,
        replacement: &str,
    ) -> VmResult<String> {
        let composition = self.host.standard_composition.clone().ok_or_else(|| {
            VmError::HostError("standard surface composition is not installed".to_string())
        })?;
        composition.regex_replace(self, pattern, text, replacement)
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

    fn cleanup_waiting_host_op(
        &mut self,
        waiting: WaitingHostOp,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        match waiting.source {
            WaitingHostOpSource::HostBridge => {
                self.host.request_cancel_host_op(waiting.op_id, reason)
            }
            WaitingHostOpSource::Manual => Ok(()),
            WaitingHostOpSource::ScopedOperation => {
                let op_id = OperationId::from_raw(waiting.op_id).map_err(|error| {
                    VmError::ExecutionScope(ExecutionScopeError::Operation(error))
                })?;
                self.host.scoped_operation_completions.remove(&op_id);
                let scope = self.execution_scope();
                scope
                    .cancel_operation(op_id, reason)
                    .map_err(VmError::ExecutionScope)?;
                let waker = Waker::noop();
                let mut cx = Context::from_waker(waker);
                match scope.poll_operation_quiescence(op_id, &mut cx) {
                    Poll::Pending | Poll::Ready(Ok(_)) => Ok(()),
                    Poll::Ready(Err(error)) => Err(VmError::ExecutionScope(error)),
                }
            }
            WaitingHostOpSource::CallableStream => self.cancel_callable_stream_with_reason(reason),
            WaitingHostOpSource::CallableStreamTermination => Ok(()),
        }
    }

    pub(super) fn cancel_waiting_host_op_with_reason(
        &mut self,
        reason: OperationCancelReason,
    ) -> VmResult<()> {
        let Some(waiting) = self.instance.waiting_host_op.clone() else {
            return Ok(());
        };
        match waiting.source {
            WaitingHostOpSource::HostBridge => {
                let bridge_cleanup = self.host.request_cancel_host_op(waiting.op_id, reason);
                let stream_cleanup = self.cancel_callable_stream_with_reason(reason);
                match bridge_cleanup {
                    Ok(()) => stream_cleanup,
                    Err(error) => Err(preserve_stream_cleanup(error, stream_cleanup)),
                }
            }
            WaitingHostOpSource::Manual => {
                self.instance.waiting_host_op = None;
                Ok(())
            }
            WaitingHostOpSource::ScopedOperation => {
                self.instance.waiting_host_op = None;
                self.cleanup_waiting_host_op(waiting, reason)
            }
            WaitingHostOpSource::CallableStream => {
                self.instance.waiting_host_op = None;
                self.cancel_callable_stream_with_reason(reason)
            }
            WaitingHostOpSource::CallableStreamTermination => {
                self.instance.waiting_host_op = None;
                Ok(())
            }
        }
    }

    pub(super) fn cancel_waiting_host_op(&mut self) -> VmResult<()> {
        self.cancel_waiting_host_op_with_reason(OperationCancelReason::Requested)
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
        let validation_error = validate_host_call_return(
            &values,
            waiting.expected_return_type,
            waiting.expected_return_schema.as_ref(),
            &self.program,
            self.host.execution_scope.resources(),
        )
        .err();
        let terminal = if validation_error.is_some() {
            HostAsyncOpTerminal::Failed
        } else {
            HostAsyncOpTerminal::Completed
        };
        let cleanup_result = match waiting.source {
            WaitingHostOpSource::HostBridge if self.host.is_bridge_operation_tracked(op_id) => {
                self.host.complete_bridge_operation(op_id, terminal)
            }
            WaitingHostOpSource::HostBridge => Ok(()),
            WaitingHostOpSource::Manual => Ok(()),
            WaitingHostOpSource::ScopedOperation => {
                self.cleanup_waiting_host_op(waiting, OperationCancelReason::Requested)
            }
            WaitingHostOpSource::CallableStream => Ok(()),
            WaitingHostOpSource::CallableStreamTermination => Err(VmError::HostError(
                "callable stream termination cannot be completed as a host operation".to_string(),
            )),
        };
        cleanup_result?;
        self.instance.waiting_host_op = None;
        if let Some(error) = validation_error {
            return Err(error);
        }
        values.push_onto_stack(&mut self.instance.stack);
        Ok(())
    }

    pub fn poll_waiting_host_op(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<()>> {
        let Some(waiting) = self.instance.waiting_host_op.clone() else {
            return match self.host.poll_stream_terminations(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => Poll::Ready(result),
            };
        };

        if matches!(waiting.source, WaitingHostOpSource::HostBridge)
            && self.host.bridge_cancellation_requested(waiting.op_id)
        {
            return match self
                .host
                .poll_bridge_operation_cancellation(waiting.op_id, cx)
            {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(())) => {
                    self.instance.waiting_host_op = None;
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            };
        }

        let bridge_owned = matches!(waiting.source, WaitingHostOpSource::HostBridge)
            && self.host.is_bridge_operation_tracked(waiting.op_id);
        if matches!(waiting.source, WaitingHostOpSource::CallableStream) {
            return self.poll_callable_stream(waiting.op_id, cx);
        }
        if matches!(
            waiting.source,
            WaitingHostOpSource::CallableStreamTermination
        ) {
            return match self.host.poll_stream_terminations(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(())) => {
                    self.instance.waiting_host_op = None;
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(error)) => {
                    self.instance.waiting_host_op = None;
                    Poll::Ready(Err(error))
                }
            };
        }
        let submitted = self.host.submitted_host_ops.contains(&waiting.op_id);
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
            WaitingHostOpSource::Manual => {
                return Poll::Ready(Err(VmError::HostError(format!(
                    "vm waiting on host op {} without an async bridge",
                    waiting.op_id
                ))));
            }
            WaitingHostOpSource::ScopedOperation => self.poll_scoped_operation(waiting.op_id, cx),
            WaitingHostOpSource::CallableStream => unreachable!("callable stream handled above"),
            WaitingHostOpSource::CallableStreamTermination => {
                unreachable!("callable stream termination handled above")
            }
        };

        match poll_result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(output)) => {
                let values = match output.finish(self) {
                    Ok(values) => values,
                    Err(err) => {
                        if bridge_owned {
                            let cleanup = self.host.complete_bridge_operation(
                                waiting.op_id,
                                HostAsyncOpTerminal::Failed,
                            );
                            if let Err(cleanup_error) = cleanup {
                                self.instance.waiting_host_op = None;
                                return Poll::Ready(Err(cleanup_error));
                            }
                        }
                        self.instance.waiting_host_op = None;
                        return Poll::Ready(Err(err));
                    }
                };
                if bridge_owned {
                    let validation = validate_host_call_return(
                        &values,
                        waiting.expected_return_type,
                        waiting.expected_return_schema.as_ref(),
                        &self.program,
                        self.host.execution_scope.resources(),
                    );
                    if let Err(error) = validation {
                        let cleanup = self
                            .host
                            .complete_bridge_operation(waiting.op_id, HostAsyncOpTerminal::Failed);
                        self.instance.waiting_host_op = None;
                        return Poll::Ready(Err(cleanup.err().unwrap_or(error)));
                    }
                    if let Err(error) = self
                        .host
                        .complete_bridge_operation(waiting.op_id, HostAsyncOpTerminal::Completed)
                    {
                        self.instance.waiting_host_op = None;
                        return Poll::Ready(Err(error));
                    }
                    self.instance.waiting_host_op = None;
                    values.push_onto_stack(&mut self.instance.stack);
                    return Poll::Ready(Ok(()));
                }
                if let Err(error) = self.complete_waiting_host_op(waiting.op_id, values) {
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => {
                if matches!(waiting.source, WaitingHostOpSource::HostBridge) {
                    let cleanup = self
                        .host
                        .complete_bridge_operation(waiting.op_id, HostAsyncOpTerminal::Failed);
                    if let Err(cleanup_error) = cleanup {
                        self.instance.waiting_host_op = None;
                        return Poll::Ready(Err(cleanup_error));
                    }
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
        } else if self.bound_host_function_is_owned(resolved_index)? {
            self.execute_bound_owned_host_function(
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
        let composition = self.host.standard_composition.clone().ok_or_else(|| {
            VmError::HostError("standard surface composition is not installed".to_string())
        })?;
        // Standard dispatch reads arguments from the current stack tail while mutating the VM.
        // The composition must not mutate `self.instance.stack` until this borrowed slice is consumed.
        let outcome = unsafe {
            let args = std::slice::from_raw_parts_mut(
                self.instance.stack.as_mut_ptr().add(arg_start),
                argc,
            );
            composition.execute_builtin_call(self, builtin, args)
        }?;

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
                self.instance.stack.truncate(arg_start);
                Ok(HostCallExecOutcome::Yielded)
            }
            CallOutcome::Pending(op_id) => {
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
                        let _ = self
                            .host
                            .request_cancel_host_op(op_id, OperationCancelReason::Requested);
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
                        self.fast_path_string_contains_result(text, needle)
                    }
                    _ => None,
                },
                BuiltinFunction::StringReplaceLiteral => match (lhs, rhs, args) {
                    (ValueType::String, ValueType::String, [text, needle, replacement]) => {
                        self.fast_path_string_replace_literal_result(text, needle, replacement)
                    }
                    _ => None,
                },
                BuiltinFunction::StringLowerAscii => match (lhs, args) {
                    (ValueType::String, [text]) => self.fast_path_string_lower_ascii_result(text),
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

    fn fast_path_string_contains_result(&self, text: &Value, needle: &Value) -> Option<Value> {
        let (Value::String(text), Value::String(needle)) = (text, needle) else {
            return None;
        };
        self.host
            .standard_composition
            .as_ref()?
            .string_contains(text.as_str(), needle.as_str())
            .map(Value::Bool)
    }

    fn fast_path_string_replace_literal_result(
        &self,
        text: &Value,
        needle: &Value,
        replacement: &Value,
    ) -> Option<Value> {
        let (Value::String(text), Value::String(needle), Value::String(replacement)) =
            (text, needle, replacement)
        else {
            return None;
        };
        self.host
            .standard_composition
            .as_ref()?
            .string_replace_literal(text.as_str(), needle.as_str(), replacement.as_str())
            .map(Value::string)
    }

    fn fast_path_string_lower_ascii_result(&self, text: &Value) -> Option<Value> {
        let Value::String(text) = text else {
            return None;
        };
        self.host
            .standard_composition
            .as_ref()?
            .string_lower_ascii(text.as_str())
            .map(Value::string)
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
                    self.host_call_pending_source(op_id),
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
    ///   rejected exact return — restores every argument the host did **not**
    ///   take to the operand stack exactly once, so nothing is lost and
    ///   nothing is dropped twice;
    /// * arguments the host took stay consumed on those paths, and values the
    ///   handler pushed onto the operand stack remain on it;
    /// * a successful call leaves the guest source consumed (the taken
    ///   arguments moved into the host, the rest released);
    /// * `Yield` is unsupported for owned dispatch: the drained operands
    ///   cannot be re-executed, so the outcome is rejected as a structured
    ///   [`VmError::HostError`] and the instruction pointer is left past the
    ///   call instruction rather than rewound for a retry;
    /// * `Halt`, `Pending`, and return validation follow the ordinary
    ///   bound-host-call contract;
    /// * panic handling unwinds without restoring already-taken arguments
    ///   (they are owned by the host at that point) while still restoring the
    ///   untaken remainder through the same failure path.
    pub(super) fn execute_bound_owned_host_function(
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
            // return all restore them exactly once. A successful
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
            self.host.host_function_schemas.push(None);
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
                // rejected return restores every argument the host did not
                // take exactly once — the same failure contract as an `Err`
                // outcome — while arguments the host took stay consumed and
                // the host stack keeps every value the handler pushed.
                if let Err(error) = validate_host_call_return(
                    &values,
                    expected_return_type,
                    expected_return_schema,
                    &self.program,
                    self.host.execution_scope.resources(),
                ) {
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
                self.set_waiting_host_op_with_return(
                    op_id,
                    self.host_call_pending_source(op_id),
                    expected_return_type,
                    expected_return_schema,
                )?;
                self.instance.ip = resume_ip;
                Ok(HostCallExecOutcome::Pending(op_id))
            }
        }
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
                | VmHostFunction::StackStatic(_)
                | VmHostFunction::OwnedDynamic(_) => unreachable!(),
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
                    self.host_call_pending_source(op_id),
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
                | VmHostFunction::ArgsStaticNonYielding(_)
                | VmHostFunction::OwnedDynamic(_) => unreachable!(),
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
                    self.host_call_pending_source(op_id),
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

    fn host_call_pending_source(&self, op_id: HostOpId) -> WaitingHostOpSource {
        if self.host.stream_drivers.contains_key(&op_id) {
            return WaitingHostOpSource::CallableStream;
        }
        if let Ok(operation_id) = OperationId::from_raw(op_id)
            && self
                .host
                .execution_scope
                .operations()
                .status(operation_id)
                .is_ok()
        {
            return WaitingHostOpSource::ScopedOperation;
        }
        if self.host.async_bridge.is_some() {
            WaitingHostOpSource::HostBridge
        } else {
            WaitingHostOpSource::Manual
        }
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
        if matches!(source, WaitingHostOpSource::HostBridge) && self.host.async_bridge.is_some() {
            self.host.track_bridge_host_op(op_id)?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        VmError, callable_schema_matches, host_callable_schema_matches, validate_host_value,
    };
    use crate::ResourceTypeKey;
    use crate::compiler::TypeSchema;
    use crate::host_api::{
        HostApiCatalog, HostFunctionSchema, HostImportSchema, HostStructField, HostTypeSchema,
        MAX_HOST_SCHEMA_DEPTH,
    };
    use crate::{OpCode, Program, Value};

    fn key(name: &str) -> ResourceTypeKey {
        ResourceTypeKey::new(name).expect("test resource key")
    }

    #[test]
    fn callable_schema_matches_direct_resources_by_key() {
        let expected_key = key("test.resource");
        let other_key = key("other.resource");

        assert!(callable_schema_matches(
            &HostTypeSchema::Resource(expected_key.clone()),
            &TypeSchema::Resource(expected_key),
        ));
        assert!(!callable_schema_matches(
            &HostTypeSchema::Resource(key("test.resource")),
            &TypeSchema::Resource(other_key),
        ));
    }

    #[test]
    fn callable_schema_matches_resource_callable_parameters_by_key() {
        let expected_key = key("test.parameter");
        let other_key = key("other.parameter");
        let expected = HostTypeSchema::Callable {
            params: vec![HostTypeSchema::Resource(expected_key.clone())],
            result: Box::new(HostTypeSchema::Null),
        };

        assert!(callable_schema_matches(
            &expected,
            &TypeSchema::Callable {
                params: vec![TypeSchema::Resource(expected_key)],
                result: Box::new(TypeSchema::Null),
            },
        ));
        assert!(!callable_schema_matches(
            &expected,
            &TypeSchema::Callable {
                params: vec![TypeSchema::Resource(other_key)],
                result: Box::new(TypeSchema::Null),
            },
        ));
    }

    #[test]
    fn callable_schema_matches_resource_callable_returns_by_key() {
        let expected_key = key("test.return");
        let other_key = key("other.return");
        let expected = HostTypeSchema::Callable {
            params: Vec::new(),
            result: Box::new(HostTypeSchema::Resource(expected_key.clone())),
        };

        assert!(callable_schema_matches(
            &expected,
            &TypeSchema::Callable {
                params: Vec::new(),
                result: Box::new(TypeSchema::Resource(expected_key)),
            },
        ));
        assert!(!callable_schema_matches(
            &expected,
            &TypeSchema::Callable {
                params: Vec::new(),
                result: Box::new(TypeSchema::Resource(other_key)),
            },
        ));
    }

    fn nested_expected(key: ResourceTypeKey) -> HostTypeSchema {
        HostTypeSchema::Callable {
            params: vec![HostTypeSchema::Optional(Box::new(HostTypeSchema::Array(
                Box::new(HostTypeSchema::Resource(key.clone())),
            )))],
            result: Box::new(HostTypeSchema::Map(Box::new(HostTypeSchema::Resource(key)))),
        }
    }

    fn nested_actual(key: ResourceTypeKey) -> TypeSchema {
        let mut object_fields = HashMap::new();
        object_fields.insert("resource".to_string(), TypeSchema::Resource(key.clone()));
        TypeSchema::Callable {
            params: vec![TypeSchema::Optional(Box::new(TypeSchema::ArrayTupleRest {
                prefix: vec![TypeSchema::Resource(key.clone())],
                rest: Box::new(TypeSchema::Resource(key)),
            }))],
            result: Box::new(TypeSchema::Object(object_fields)),
        }
    }

    #[test]
    fn callable_schema_matches_nested_resource_parameters_and_returns_by_key() {
        let expected_key = key("test.nested");
        let other_key = key("other.nested");

        assert!(callable_schema_matches(
            &nested_expected(expected_key.clone()),
            &nested_actual(expected_key),
        ));
        assert!(!callable_schema_matches(
            &nested_expected(key("test.nested")),
            &nested_actual(other_key),
        ));
    }

    fn named_event_fields() -> Vec<HostStructField> {
        vec![
            HostStructField::new("id", HostTypeSchema::Int),
            HostStructField::new(
                "note",
                HostTypeSchema::Optional(Box::new(HostTypeSchema::String)),
            ),
        ]
    }

    #[test]
    fn callable_schema_matches_named_structs_by_name_and_rejects_maps() {
        let expected = HostTypeSchema::named_struct("SseEvent", named_event_fields());
        assert!(callable_schema_matches(
            &expected,
            &TypeSchema::Named("SseEvent".to_string(), Vec::new()),
        ));
        assert!(!callable_schema_matches(
            &expected,
            &TypeSchema::Named("OtherEvent".to_string(), Vec::new()),
        ));
        assert!(!callable_schema_matches(
            &expected,
            &TypeSchema::Map(Box::new(TypeSchema::Unknown)),
        ));

        let mut object_fields = HashMap::new();
        object_fields.insert("id".to_string(), TypeSchema::Int);
        object_fields.insert(
            "note".to_string(),
            TypeSchema::Optional(Box::new(TypeSchema::String)),
        );
        assert!(callable_schema_matches(
            &expected,
            &TypeSchema::Object(object_fields),
        ));
        assert!(!callable_schema_matches(
            &expected,
            &TypeSchema::Named("SseEvent".to_string(), vec![TypeSchema::Int]),
        ));
    }

    #[test]
    fn host_callable_schema_matches_named_structs_recursively() {
        let expected = HostTypeSchema::named_struct("SseEvent", named_event_fields());
        let matching = HostTypeSchema::named_struct("SseEvent", named_event_fields());
        let mismatched = HostTypeSchema::named_struct(
            "SseEvent",
            vec![HostStructField::new("id", HostTypeSchema::String)],
        );
        assert!(host_callable_schema_matches(&expected, &matching));
        assert!(!host_callable_schema_matches(&expected, &mismatched));
        assert!(!host_callable_schema_matches(
            &expected,
            &HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown)),
        ));
    }

    #[test]
    fn named_host_return_allows_omitted_optional_fields() {
        let program = Program::new(Vec::new(), vec![OpCode::Ret as u8]);
        let resources = crate::vm::resource::ResourceTable::new().expect("resource table");
        let schema = HostTypeSchema::named_struct("OptBox", named_event_fields());
        let mut values = crate::bytecode::VmMap::new();
        values.insert(Value::string("id"), Value::Int(1));
        validate_host_value(&Value::Map(values.into()), &schema, &program, &resources)
            .expect("optional named field may be omitted on host return");
    }

    #[test]
    fn named_host_return_still_requires_non_optional_fields() {
        let program = Program::new(Vec::new(), vec![OpCode::Ret as u8]);
        let resources = crate::vm::resource::ResourceTable::new().expect("resource table");
        let schema = HostTypeSchema::named_struct("OptBox", named_event_fields());
        let values = crate::bytecode::VmMap::new();
        let error = validate_host_value(&Value::Map(values.into()), &schema, &program, &resources)
            .expect_err("required named field must stay present");
        assert!(matches!(error, VmError::TypeMismatch("map")));
    }

    #[test]
    fn install_named_struct_schemas_merges_identical_and_rejects_conflicts() {
        let mut first = HashMap::new();
        first.insert("HandleBox".to_string(), TypeSchema::Int);
        let mut second = HashMap::new();
        second.insert("OtherBox".to_string(), TypeSchema::String);
        let mut conflict = HashMap::new();
        conflict.insert("HandleBox".to_string(), TypeSchema::String);

        let mut registry = super::HostFunctionRegistry::empty();
        registry
            .install_named_struct_schemas(first.clone())
            .expect("first install");
        registry
            .install_named_struct_schemas(first)
            .expect("identical duplicate must be accepted");
        registry
            .install_named_struct_schemas(second)
            .expect("disjoint merge must keep both schemas");
        assert!(registry.named_struct_schemas().contains_key("HandleBox"));
        assert!(registry.named_struct_schemas().contains_key("OtherBox"));
        let error = registry
            .install_named_struct_schemas(conflict)
            .expect_err("conflicting schema must be rejected");
        assert!(
            matches!(error, VmError::HostError(ref message) if message.contains("conflicting named struct schema")),
            "unexpected error: {error:?}"
        );
        assert!(
            matches!(
                registry.named_struct_schemas().get("HandleBox"),
                Some(TypeSchema::Int)
            ),
            "conflict must not mutate the installed table"
        );
    }

    #[test]
    fn bind_vm_copies_named_struct_schemas_onto_host_runtime() {
        let mut schemas = HashMap::new();
        schemas.insert("HandleBox".to_string(), TypeSchema::Int);
        let mut registry = super::HostFunctionRegistry::empty();
        registry
            .install_named_struct_schemas(schemas)
            .expect("install");
        let mut vm = crate::Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]));
        registry.bind_vm_cached(&mut vm).expect("bind");
        assert!(
            matches!(
                vm.host.named_struct_schemas.get("HandleBox"),
                Some(TypeSchema::Int)
            ),
            "bind must copy named-struct bodies onto the VM host runtime"
        );
    }

    #[test]
    fn catalog_registration_rejects_overdepth_schema_before_mutation() {
        let valid_function =
            HostFunctionSchema::with_return("limits::registry", Vec::new(), HostTypeSchema::Int);
        let mut builder = HostApiCatalog::builder();
        builder.function(valid_function.clone());
        let catalog = builder.build().expect("valid catalog");
        let mut invalid = HostImportSchema::from_function(&catalog, &valid_function);
        let mut nested = HostTypeSchema::Int;
        for _ in 0..MAX_HOST_SCHEMA_DEPTH {
            nested = HostTypeSchema::Array(Box::new(nested));
        }
        invalid.return_type = nested;

        let mut registry = super::HostFunctionRegistry::empty();
        let error = registry
            .register_catalog_static(invalid, |_, _| Ok(super::CallOutcome::Halt))
            .expect_err("invalid schema must be rejected");
        assert!(matches!(
            error,
            super::RegistrySchemaError::InvalidSchema { .. }
        ));
        assert!(registry.catalog_by_schema.is_empty());
    }
}

/// Generic owned-value dispatch: the owned-call view, callable-graph transfer,
/// and registration admission. Domain-facing behaviour (a real embedding that
/// registers a callback and drives a private VM) is covered by the integration
/// tests of the modules that build on this seam.
#[cfg(test)]
mod owned_dispatch_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::bytecode::{
        CallableEnvironment, CallableKind, CallablePrototype, CallableTarget, HostImport, Program,
        TypeMap, ValueType,
    };
    use crate::compiler::TypeSchema;
    use crate::host_api::{HostApiFingerprint, HostParamPassing, HostTypeSchema};

    struct NoopOwned;

    impl HostOwnedFunction for NoopOwned {
        fn call(&mut self, _call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::none()))
        }
    }

    fn owned_schema(params: Vec<HostImportParam>, return_type: HostTypeSchema) -> HostImportSchema {
        HostImportSchema {
            name: "test::register".to_string(),
            params,
            return_type,
            fingerprint: HostApiFingerprint::from_wire(0),
        }
    }

    fn named_schema(
        name: &str,
        params: Vec<HostImportParam>,
        return_type: HostTypeSchema,
    ) -> HostImportSchema {
        HostImportSchema {
            name: name.to_string(),
            params,
            return_type,
            fingerprint: HostApiFingerprint::from_wire(0),
        }
    }

    fn take_callable_param(name: &str) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema: HostTypeSchema::Callable {
                params: vec![HostTypeSchema::Bool],
                result: Box::new(HostTypeSchema::Unknown),
            },
            passing: HostParamPassing::TakeOwned,
        }
    }

    fn register_owned(
        registry: &mut HostFunctionRegistry,
        schema: HostImportSchema,
    ) -> VmResult<u16> {
        let name = schema.name.clone();
        let arity = schema.arity() as u8;
        registry.register_exact_owned(&name, arity, schema, |_context| Box::new(NoopOwned))
    }

    #[test]
    fn owned_host_call_can_restore_a_taken_argument() {
        let mut source = Vm::try_new(Program::new(Vec::new(), vec![])).expect("source VM");
        let mut call =
            OwnedHostCall::new(&mut source, vec![Value::Int(7), Value::string("callback")]);
        assert_eq!(call.args().len(), 2);
        assert_eq!(call.arg(0), Some(&Value::Int(7)));
        assert!(!call.is_taken(0));
        let value = call.take_arg(1).expect("take argument");
        assert!(call.is_taken(1));
        assert_eq!(call.arg(1), Some(&Value::Null));
        call.restore_arg(1, value).expect("restore argument");
        assert!(!call.is_taken(1));
        assert_eq!(
            call.into_untaken_args(),
            vec![Value::Int(7), Value::string("callback")]
        );
    }

    #[test]
    fn owned_host_call_rejects_missing_and_double_taking() {
        let mut source = Vm::try_new(Program::new(Vec::new(), vec![])).expect("source VM");
        let mut call = OwnedHostCall::new(&mut source, vec![Value::Int(1)]);
        assert!(call.take_arg(3).is_err(), "missing index must fault");
        call.take_arg(0).expect("first take");
        assert!(call.take_arg(0).is_err(), "double take must fault");
        assert!(
            call.restore_arg(1, Value::Int(0)).is_err(),
            "restoring an untaken index must fault"
        );
        assert_eq!(call.into_untaken_args(), Vec::<Value>::new());
    }

    #[test]
    fn owned_registration_accepts_a_resource_bearing_take_owned_schema() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let schema = owned_schema(
            vec![HostImportParam {
                name: "handle".to_string(),
                schema: HostTypeSchema::Resource(key.clone()),
                passing: HostParamPassing::TakeOwned,
            }],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let slot = register_owned(&mut registry, schema).expect("resource-bearing owned schema");
        assert_eq!(slot, 0);
        assert!(
            schema_requires_guard(
                registry
                    .entries
                    .first()
                    .and_then(|entry| entry.schema.as_ref())
                    .expect("registered schema")
            ),
            "a resource-bearing parameter must select the guarded owned wrapper"
        );
    }

    #[test]
    fn owned_registration_accepts_a_mixed_value_resource_callable_schema() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let schema = owned_schema(
            vec![
                HostImportParam {
                    name: "delay".to_string(),
                    schema: HostTypeSchema::Int,
                    passing: HostParamPassing::Value,
                },
                HostImportParam {
                    name: "handle".to_string(),
                    schema: HostTypeSchema::Optional(Box::new(HostTypeSchema::Resource(
                        key.clone(),
                    ))),
                    passing: HostParamPassing::Borrow,
                },
                HostImportParam {
                    name: "sink".to_string(),
                    schema: HostTypeSchema::Resource(key),
                    passing: HostParamPassing::BorrowMut,
                },
                take_callable_param("callback"),
            ],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        register_owned(&mut registry, schema).expect("mixed owned schema");
        assert!(schema_requires_guard(
            registry
                .entries
                .first()
                .and_then(|entry| entry.schema.as_ref())
                .expect("registered schema")
        ));
    }

    #[test]
    fn owned_registration_rejects_a_nested_resource_parameter() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let schema = owned_schema(
            vec![HostImportParam {
                name: "handle".to_string(),
                schema: HostTypeSchema::Array(Box::new(HostTypeSchema::Resource(key))),
                passing: HostParamPassing::TakeOwned,
            }],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let error = register_owned(&mut registry, schema).expect_err("nested resource parameter");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("directly addressable")),
            "unexpected error: {error}"
        );
        assert!(registry.entries.is_empty(), "rejection must not mutate");
    }

    #[test]
    fn owned_registration_rejects_a_named_nested_resource_parameter() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let schema = owned_schema(
            vec![HostImportParam {
                name: "handle".to_string(),
                schema: crate::host_api::HostTypeSchema::named_struct(
                    "Envelope",
                    vec![crate::host_api::HostStructField::new(
                        "inner",
                        HostTypeSchema::Resource(key),
                    )],
                ),
                passing: HostParamPassing::TakeOwned,
            }],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let error =
            register_owned(&mut registry, schema).expect_err("named nested resource parameter");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("directly addressable")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn owned_registration_rejects_a_nested_resource_return() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let schema = owned_schema(
            vec![take_callable_param("callback")],
            HostTypeSchema::Array(Box::new(HostTypeSchema::Resource(key))),
        );
        let mut registry = HostFunctionRegistry::empty();
        let error = register_owned(&mut registry, schema).expect_err("nested resource return");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("return schema")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn owned_registration_rejects_a_resource_value_parameter() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let schema = owned_schema(
            vec![HostImportParam {
                name: "handle".to_string(),
                schema: HostTypeSchema::Resource(key),
                passing: HostParamPassing::Value,
            }],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let error = register_owned(&mut registry, schema).expect_err("resource passed by value");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("Borrow/BorrowMut/TakeOwned")),
            "unexpected error: {error}"
        );
    }

    fn assert_rejected_ordinary_registration(
        registry: &mut HostFunctionRegistry,
        schema: HostImportSchema,
        expected: &str,
    ) {
        let error = registry
            .register_catalog_static(schema, |_vm, _args| {
                Ok(CallOutcome::Return(CallReturn::none()))
            })
            .expect_err("an ordinary registration must not declare an owned value transfer");
        let RegistrySchemaError::InvalidSchema { detail, .. } = &error else {
            panic!("expected a structured schema rejection, got: {error:?}");
        };
        assert!(
            detail.contains(expected),
            "unexpected rejection detail: {detail}"
        );
        assert!(registry.entries.is_empty(), "rejection must not mutate");
    }

    #[test]
    fn plain_registration_still_rejects_resource_free_take_owned() {
        let mut registry = HostFunctionRegistry::empty();
        let schema = owned_schema(vec![take_callable_param("callback")], HostTypeSchema::Bool);
        assert_rejected_ordinary_registration(&mut registry, schema, "contains no resource");
    }

    #[test]
    fn plain_registration_still_rejects_resource_free_borrow_and_borrow_mut() {
        for passing in [HostParamPassing::Borrow, HostParamPassing::BorrowMut] {
            let mut registry = HostFunctionRegistry::empty();
            let schema = owned_schema(
                vec![HostImportParam {
                    name: "value".to_string(),
                    schema: HostTypeSchema::String,
                    passing,
                }],
                HostTypeSchema::Bool,
            );
            assert_rejected_ordinary_registration(&mut registry, schema, "contains no resource");
        }
    }

    #[test]
    fn plain_registration_rejects_a_resource_passed_by_value() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let mut registry = HostFunctionRegistry::empty();
        let schema = owned_schema(
            vec![HostImportParam {
                name: "handle".to_string(),
                schema: HostTypeSchema::Resource(key),
                passing: HostParamPassing::Value,
            }],
            HostTypeSchema::Bool,
        );
        assert_rejected_ordinary_registration(&mut registry, schema, "Borrow/BorrowMut/TakeOwned");
    }

    #[test]
    fn plain_registration_still_accepts_a_resource_bearing_borrow() {
        let key = crate::host_api::ResourceTypeKey::new("test.handle").expect("key");
        let mut registry = HostFunctionRegistry::empty();
        let schema = owned_schema(
            vec![HostImportParam {
                name: "handle".to_string(),
                schema: HostTypeSchema::Resource(key),
                passing: HostParamPassing::Borrow,
            }],
            HostTypeSchema::Bool,
        );
        registry
            .register_catalog_static(schema, |_vm, _args| {
                Ok(CallOutcome::Return(CallReturn::none()))
            })
            .expect("an ordinary resource-passing registration keeps its own contract");
    }

    #[test]
    fn owned_registration_requires_the_exact_name_and_arity_identity() {
        let schema = owned_schema(vec![take_callable_param("callback")], HostTypeSchema::Bool);
        let mut registry = HostFunctionRegistry::empty();
        let error = registry
            .register_exact_owned("test::other", 1, schema.clone(), |_context| {
                Box::new(NoopOwned)
            })
            .expect_err("name mismatch");
        assert!(matches!(error, VmError::HostError(_)));
        let error = registry
            .register_exact_owned("test::register", 2, schema, |_context| Box::new(NoopOwned))
            .expect_err("arity mismatch");
        assert!(matches!(error, VmError::HostError(_)));
        assert!(registry.entries.is_empty(), "rejection must not mutate");
    }

    #[test]
    fn owned_registration_accepts_a_resource_free_take_owned_schema_once() {
        let schema = owned_schema(vec![take_callable_param("callback")], HostTypeSchema::Bool);
        let mut registry = HostFunctionRegistry::empty();
        let slot = register_owned(&mut registry, schema.clone()).expect("owned registration");
        assert_eq!(slot, 0);
        let error = register_owned(&mut registry, schema).expect_err("duplicate");
        assert!(
            matches!(error, VmError::HostError(_)),
            "unexpected: {error}"
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
    fn owned_callable_rejects_foreign_and_resource_bearing_captures() {
        let key = crate::host_api::ResourceTypeKey::new("test.capture").expect("key");
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
        let error = match call.spawn_owned_callable_vm(&registry, &callable, |_vm| Ok(())) {
            Ok(_) => panic!("resource-bearing capture must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("resource-bearing capture")),
            "unexpected error: {error}"
        );

        // A callable the source VM does not own is foreign: owned graphs are
        // program-local and are never portable across VM boundaries.
        let foreign = closure_value(0, Vec::new()).0;
        let error = match call.spawn_owned_callable_vm(&registry, &foreign, |_vm| Ok(())) {
            Ok(_) => panic!("foreign callable must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, VmError::InvalidFrameState(message) if message.contains("source vm")),
            "unexpected error: {error}"
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
    fn owned_callable_rejects_a_prototype_outside_the_source_program() {
        let program = capture_program(
            vec![TypeSchema::Callable {
                params: Vec::new(),
                result: Box::new(TypeSchema::Unknown),
            }],
            vec![capture_prototype(Vec::new(), Vec::new())],
        );
        let mut source = Vm::try_new(program).expect("source VM");
        let (callable, owned) = closure_value(9, Vec::new());
        source.instance.owned_callables.push(Arc::downgrade(&owned));
        source.instance.locals[0] = callable.clone();
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut source, Vec::new());
        let error = match call.spawn_owned_callable_vm(&registry, &callable, |_vm| Ok(())) {
            Ok(_) => panic!("unknown prototype must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, VmError::InvalidCallablePrototype(9)),
            "unexpected error: {error}"
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

    fn owned_import(schema: &HostImportSchema) -> HostImport {
        HostImport {
            name: schema.name.clone(),
            arity: schema.arity() as u8,
            return_type: ValueType::Unknown,
        }
    }

    fn bound_vm(program: Program, registry: &HostFunctionRegistry) -> Vm {
        let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
        registry.bind_vm_cached(&mut vm).expect("bind registry");
        vm
    }

    /// Program that pushes `int_arg`, builds prototype 0's callable, and calls
    /// import slot 0 with `(int_arg, callable)`.
    fn owned_call_program(schema: &HostImportSchema, int_arg: i64) -> Program {
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
            vec![owned_import(schema)],
            None,
        )
        .with_host_import_schemas(vec![schema.clone()])
        .expect("host import schemas");
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    fn owned_call_twice_program(schema: &HostImportSchema, int_arg: i64) -> Program {
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
            vec![owned_import(schema)],
            None,
        )
        .with_host_import_schemas(vec![schema.clone()])
        .expect("host import schemas");
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
    #[derive(Default)]
    struct SpawnProbe {
        owns_callable: Option<bool>,
        frames: Option<usize>,
        stack_empty: Option<bool>,
        source_unchanged: Option<bool>,
    }

    struct SpawningSpy {
        registry: HostFunctionRegistry,
        probe: Arc<Mutex<SpawnProbe>>,
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
            let mut probe = self.probe.lock().expect("probe lock");
            probe.owns_callable = Some(fresh.owns_callable(&callable));
            probe.frames = Some(fresh.instance.execution_frames.len());
            probe.stack_empty = Some(fresh.instance.stack.is_empty());
            probe.source_unchanged = Some(before == after);
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
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

    /// An owned host function that takes the callable argument and then
    /// returns `CallOutcome::Yield`.
    ///
    /// Owned dispatch drains the call operands before the handler runs, so
    /// there is no operand stack left to re-execute the call from: the outcome
    /// must be rejected as a structured error and never dispatched again.
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

    fn callable_take_schema() -> HostImportSchema {
        owned_schema(
            vec![
                HostImportParam {
                    name: "delay".to_string(),
                    schema: HostTypeSchema::Int,
                    passing: HostParamPassing::Value,
                },
                take_callable_param("callback"),
            ],
            HostTypeSchema::Bool,
        )
    }

    #[test]
    fn owned_dispatch_transfers_the_callable_exactly_once() {
        let schema = callable_take_schema();
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

        let mut vm = bound_vm(owned_call_program(&schema, 41), &registry);
        assert_eq!(
            vm.run().expect("the owned call must succeed"),
            VmStatus::Halted
        );

        let seen = seen.lock().expect("seen lock");
        assert!(
            matches!(seen.callable, Some(Value::Callable(_))),
            "the host must receive the callable value itself"
        );
        assert!(
            seen.second_take_failed,
            "a second take of the same argument must be rejected"
        );
        drop(seen);
        assert_eq!(
            vm.instance.stack,
            vec![Value::Bool(true)],
            "a successful owned call leaves the guest operands consumed and the return value on \
             the stack"
        );
    }

    #[test]
    fn owned_dispatch_rejects_yield_and_restores_untaken_arguments() {
        let schema = callable_take_schema();
        let calls = Arc::new(AtomicUsize::new(0));
        let entry_ip = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(Mutex::new(None));
        let (factory_calls, factory_ip, factory_held) =
            (Arc::clone(&calls), Arc::clone(&entry_ip), Arc::clone(&held));
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(YieldingOwned {
                    calls: Arc::clone(&factory_calls),
                    entry_ip: Arc::clone(&factory_ip),
                    held: Arc::clone(&factory_held),
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(&schema, 41), &registry);
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
            "the rejected dispatch must not rewind the instruction pointer for an unsafe retry"
        );
        let _ = vm.resume();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a repeated resume must not silently re-dispatch the owned handler"
        );
    }

    #[test]
    fn failed_owned_call_restores_untaken_arguments_without_double_drop() {
        let schema = callable_take_schema();
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |_context| {
                Box::new(OwnedSpy {
                    seen: Arc::new(Mutex::new(Seen::default())),
                    fail: true,
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(&schema, 41), &registry);
        let error = vm.run().expect_err("the owned call must fail");
        assert!(matches!(error, VmError::HostError(ref message) if message.contains("rejected")));
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "an error before host acceptance restores every argument the host did not take; the \
             taken callable stays with the host"
        );
    }

    #[test]
    fn failed_owned_call_rolls_back_after_provenance_validation() {
        let schema = callable_take_schema();
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |context| {
                Box::new(RollbackOwned {
                    registry: context.registry().clone(),
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(&schema, 41), &registry);
        let error = vm.run().expect_err("the provenance failure must fail");
        assert!(
            matches!(error, VmError::InvalidFrameState(message) if message.contains("source vm"))
        );
        assert_eq!(vm.instance.stack.len(), 2);
        assert!(matches!(vm.instance.stack[1], Value::Callable(_)));
    }

    #[test]
    fn owned_call_spawns_an_isolated_callable_vm() {
        let schema = callable_take_schema();
        let probe = Arc::new(Mutex::new(SpawnProbe::default()));
        let spy_probe = Arc::clone(&probe);
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_owned("test::register", 2, schema.clone(), move |context| {
                Box::new(SpawningSpy {
                    registry: context.registry().clone(),
                    probe: Arc::clone(&spy_probe),
                })
            })
            .expect("register owned");

        let mut vm = bound_vm(owned_call_program(&schema, 41), &registry);
        assert_eq!(
            vm.run().expect("the spawning owned call must succeed"),
            VmStatus::Halted
        );
        let probe = probe.lock().expect("probe lock");
        assert_eq!(
            probe.owns_callable,
            Some(true),
            "the fresh VM must own the transferred callable graph"
        );
        assert_eq!(
            probe.frames,
            Some(0),
            "the fresh VM starts halted with no execution frames; no source frames were copied"
        );
        assert_eq!(
            probe.stack_empty,
            Some(true),
            "the fresh VM starts with an empty operand stack"
        );
        assert_eq!(
            probe.source_unchanged,
            Some(true),
            "spawning the owned VM must leave the source VM's frames, stack, and locals untouched"
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
    fn owned_dispatch_restores_function_after_handler_mutates_host_vector() {
        let schema = callable_take_schema();
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
        let mut vm = bound_vm(owned_call_twice_program(&schema, 41), &registry);
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

    #[test]
    fn owned_dispatch_restores_function_and_untaken_values_after_panic() {
        let schema = callable_take_schema();
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
        let mut vm = bound_vm(owned_call_program(&schema, 41), &registry);
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

    #[test]
    fn owned_dispatch_keeps_legacy_host_kinds_untouched() {
        // A `Value`-only exact registration continues to dispatch through the
        // borrowed-slice path and declares no ownership.
        let schema = named_schema(
            "test::echo",
            vec![HostImportParam {
                name: "value".to_string(),
                schema: HostTypeSchema::Int,
                passing: HostParamPassing::Value,
            }],
            HostTypeSchema::Int,
        );
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_exact_static("test::echo", 1, schema.clone(), |_vm, args| {
                Ok(CallOutcome::Return(CallReturn::one(
                    args.first().cloned().unwrap_or(Value::Null),
                )))
            })
            .expect("register exact static");

        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 1);
        code.ret();
        let program = Program::with_imports_and_debug(
            vec![Value::Int(7)],
            code.finish(),
            vec![HostImport {
                name: "test::echo".to_string(),
                arity: 1,
                return_type: ValueType::Int,
            }],
            None,
        )
        .with_host_import_schemas(vec![schema])
        .expect("host import schemas");
        let mut vm = bound_vm(program, &registry);
        assert_eq!(vm.run().expect("run"), VmStatus::Halted);
        assert_eq!(vm.instance.stack, vec![Value::Int(7)]);
    }

    /// The `HostApiBuilder` catalog gate accepts a resource-free `TakeOwned`
    /// parameter through the same host-schema vocabulary.
    #[test]
    fn host_catalog_schema_vocabulary_models_owned_callable_transfer() {
        let param = crate::host_api::HostParamSchema::with_passing(
            "callback",
            HostTypeSchema::Callable {
                params: vec![HostTypeSchema::Bool],
                result: Box::new(HostTypeSchema::Unknown),
            },
            HostParamPassing::TakeOwned,
        );
        assert_eq!(param.passing, HostParamPassing::TakeOwned);
        assert!(!param.ty.contains_resource());
    }
}
/// Owned-dispatch behaviour tests for resource-bearing `TakeOwned` /
/// `Borrow` / `BorrowMut` parameters and exact resource returns.
///
/// Ported from the masterline `owned_resource_dispatch_tests` module: the
/// guarded preflight/commit contract is exercised through real `Vm::run`
/// programs, with the resource table's own typed API used for consumption.
#[cfg(test)]
mod owned_resource_dispatch_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;

    use super::*;
    use crate::bytecode::{
        CallableEnvironment, CallableKind, CallablePrototype, CallableTarget, CallableValue,
        HostImport, Program, TypeMap,
    };
    use crate::compiler::TypeSchema;
    use crate::host_api::{HostApiFingerprint, HostParamPassing, HostTypeSchema, ResourceTypeKey};
    use crate::vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};

    fn bound_vm(program: Program, registry: &HostFunctionRegistry) -> Vm {
        let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
        registry.bind_vm_cached(&mut vm).expect("bind registry");
        vm
    }

    fn resource_schema(
        name: &str,
        params: Vec<HostImportParam>,
        return_type: HostTypeSchema,
    ) -> HostImportSchema {
        HostImportSchema {
            name: name.to_string(),
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

    fn value_param(name: &str, schema: HostTypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema,
            passing: HostParamPassing::Value,
        }
    }

    fn take_param(name: &str, schema: HostTypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema,
            passing: HostParamPassing::TakeOwned,
        }
    }

    fn borrow_mut_param(name: &str, schema: HostTypeSchema) -> HostImportParam {
        HostImportParam {
            name: name.to_string(),
            schema,
            passing: HostParamPassing::BorrowMut,
        }
    }

    fn callable_schema() -> HostTypeSchema {
        HostTypeSchema::Callable {
            params: vec![HostTypeSchema::Bool],
            result: Box::new(HostTypeSchema::Unknown),
        }
    }

    fn import(schema: &HostImportSchema) -> HostImport {
        HostImport {
            name: schema.name.clone(),
            arity: schema.arity() as u8,
            return_type: ValueType::Unknown,
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
    ) -> HostImportSchema {
        let schema = resource_schema("test::ping", vec![], HostTypeSchema::Resource(guard_key()));
        registry
            .register_catalog(schema.clone(), move || {
                Box::new(PingGuard {
                    closes: Arc::clone(&closes),
                    recorded: Arc::clone(&recorded),
                })
            })
            .expect("register guard ping");
        schema
    }

    fn register_other_ping(
        registry: &mut HostFunctionRegistry,
        closes: Arc<AtomicUsize>,
        recorded: Arc<Mutex<Option<u64>>>,
    ) -> HostImportSchema {
        let schema = resource_schema("test::ping", vec![], HostTypeSchema::Resource(other_key()));
        registry
            .register_catalog(schema.clone(), move || {
                Box::new(PingOther {
                    closes: Arc::clone(&closes),
                    recorded: Arc::clone(&recorded),
                })
            })
            .expect("register other ping");
        schema
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
    fn ping_then_int_program(
        ping: HostImportSchema,
        take: HostImportSchema,
        int_arg: i64,
    ) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 0);
        code.call(1, 2);
        code.ret();
        Program::with_imports_and_debug(
            vec![Value::Int(int_arg)],
            code.finish(),
            vec![import(&ping), import(&take)],
            None,
        )
        .with_host_import_schemas(vec![ping, take])
        .expect("host import schemas")
    }

    /// `test::ping()` first, then the arity-2 owned import with the callable in
    /// slot 1, so the drained arguments are `[handle, callable]`.
    fn ping_then_callable_program(ping: HostImportSchema, take: HostImportSchema) -> Program {
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
            vec![import(&ping), import(&take)],
            None,
        )
        .with_host_import_schemas(vec![ping, take])
        .expect("host import schemas");
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    /// The arity-2 owned import called with raw constant arguments, so an
    /// invalid handle can be passed without a resource-producing host.
    fn plain_take_program(take: HostImportSchema, args: Vec<Value>) -> Program {
        let mut code = crate::BytecodeBuilder::new();
        for index in 0..args.len() {
            code.ldc(index as u32);
        }
        code.call(0, args.len() as u8);
        code.ret();
        Program::with_imports_and_debug(args, code.finish(), vec![import(&take)], None)
            .with_host_import_schemas(vec![take])
            .expect("host import schemas")
    }

    /// `[Int, callable]` arguments built entirely by the guest.
    fn int_then_callable_program(take: HostImportSchema, int_arg: i64) -> Program {
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
            vec![import(&take)],
            None,
        )
        .with_host_import_schemas(vec![take])
        .expect("host import schemas");
        program.callable_prototypes = vec![one_callable_prototype()];
        program
    }

    fn resource_error_code(error: &VmError) -> Option<ResourceErrorCode> {
        match error {
            VmError::ExecutionScope(ExecutionScopeError::Resource(error)) => Some(error.code()),
            _ => None,
        }
    }

    /// Whether `raw` still names a live (open) resource of `key` in the scope.
    fn is_live(vm: &Vm, raw: u64, key: &ResourceTypeKey) -> bool {
        let handle = ResourceHandle::from_raw(raw).expect("valid raw handle");
        vm.host
            .execution_scope
            .resources()
            .validate_resource_type_key(handle, key)
            .is_ok()
    }

    /// `(delay: Int, resource: Resource(test.guard) TakeOwned) -> bool`.
    ///
    /// The handler reads both arguments in declaration order, takes the
    /// resource argument (never the int), and optionally consumes the table
    /// entry through the scope's own typed take.
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
            let handle = resource_handle_argument(&handle_value)?;
            let repeat_take_failed = call.take_arg(1).is_err();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = repeat_take_failed;
                seen.handle = Some(handle.raw());
            }
            if self.consume {
                let taken = call
                    .vm()
                    .host_context()
                    .take_resource::<GuardResource>(handle)
                    .map_err(|error| VmError::HostError(error.to_string()))?;
                drop(taken);
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
            let handle = resource_handle_argument(&handle_value)?;
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
                let taken = call
                    .vm()
                    .host_context()
                    .take_resource::<GuardResource>(handle)
                    .map_err(|error| VmError::HostError(error.to_string()))?;
                drop(taken);
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
            let handle = resource_handle_argument(&handle_value)?;
            let callable = call.take_arg(1)?;
            self.seen.lock().expect("seen").callable = Some(callable);
            let taken = call
                .vm()
                .host_context()
                .take_resource::<GuardResource>(handle)
                .map_err(|error| VmError::HostError(error.to_string()))?;
            drop(taken);
            self.seen.lock().expect("seen").consumed = true;
            Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true))))
        }
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
            let repeat_take_failed = call.take_arg(1).is_err();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = repeat_take_failed;
                seen.callable = Some(callable);
            }
            Ok(CallOutcome::Return(CallReturn::one(Value::string(
                "not a resource handle",
            ))))
        }
    }

    /// Owned handler that takes the callable and returns a live handle of the
    /// wrong key, so the exact return *validates* as a handle but fails the
    /// resource-key transfer.
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
            let repeat_take_failed = call.take_arg(1).is_err();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.second_take_failed = repeat_take_failed;
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
            let repeat_take_failed = call.take_arg(1).is_err();
            {
                let mut seen = self.seen.lock().expect("seen");
                seen.calls += 1;
                seen.first_arg = call.arg(0).cloned();
                seen.second_take_failed = repeat_take_failed;
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
        resource_schema(
            "test::take",
            vec![
                value_param("delay", HostTypeSchema::Int),
                take_param("callback", callable_schema()),
            ],
            HostTypeSchema::Resource(guard_key()),
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

    /// A wrong-key handle must be rejected by the pre-call contract before the
    /// host function runs; every drained argument returns to the guest and the
    /// resource is neither consumed nor closed.
    #[test]
    fn owned_int_resource_preflight_rejects_wrong_key_without_calling_host() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = resource_schema(
            "test::take",
            vec![
                value_param("delay", HostTypeSchema::Int),
                take_param("resource", HostTypeSchema::Resource(guard_key())),
            ],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let ping_schema =
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
            ping_then_int_program(ping_schema, take_schema, 41),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("a wrong-key resource must not reach the host");
        assert_eq!(
            resource_error_code(&error),
            Some(ResourceErrorCode::ResourceTypeKeyMismatch),
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
        assert!(
            is_live(&vm, raw, &other_key()),
            "a rejected preflight must leave the guest-owned resource untouched"
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

    /// A non-handle argument must be rejected by the pre-call contract before
    /// the host function runs, restoring both drained arguments.
    #[test]
    fn owned_int_resource_preflight_rejects_invalid_handle_without_calling_host() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = resource_schema(
            "test::take",
            vec![
                value_param("delay", HostTypeSchema::Int),
                take_param("resource", HostTypeSchema::Resource(guard_key())),
            ],
            HostTypeSchema::Bool,
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
            plain_take_program(take_schema, vec![Value::Int(41), invalid.clone()]),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("an invalid handle must not reach the host");
        assert_eq!(
            resource_error_code(&error),
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

    /// A declared `TakeOwned` resource is transferred exactly once: both
    /// argument slots reach the host in declaration order, a repeat take is
    /// rejected, the consumed resource is no longer live, and it is never
    /// closed by the call itself.
    #[test]
    fn owned_int_resource_transfers_and_consumes_exactly_once() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = resource_schema(
            "test::take",
            vec![
                value_param("delay", HostTypeSchema::Int),
                take_param("resource", HostTypeSchema::Resource(guard_key())),
            ],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let ping_schema =
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
            ping_then_int_program(ping_schema, take_schema, 41),
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
        assert!(
            !is_live(&vm, raw, &guard_key()),
            "the consumed resource must no longer be live"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "a consumed resource must not be closed by the call"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Bool(true)],
            "untaken arguments are released on success and the return value lands on the stack"
        );
    }

    /// A resource and a callable declared `TakeOwned` transfer both arguments
    /// exactly once, and the spawned isolated VM owns the callable graph.
    #[test]
    fn owned_resource_callable_transfers_both_arguments_exactly_once() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = resource_schema(
            "test::take",
            vec![
                take_param("resource", HostTypeSchema::Resource(guard_key())),
                take_param("callback", callable_schema()),
            ],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let ping_schema =
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
            ping_then_callable_program(ping_schema, take_schema),
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
        assert!(!is_live(&vm, raw, &guard_key()));
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        assert_eq!(vm.instance.stack, vec![Value::Bool(true)]);
    }

    /// A declared `TakeOwned` that the host did not consume is reported and
    /// reclaimed by the commit: the guest never gets the handle back, the
    /// untaken arguments are restored exactly once, and the resource is only
    /// released by the ordinary scope close.
    #[test]
    fn owned_resource_commit_reclaims_an_unconsumed_resource() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = resource_schema(
            "test::take",
            vec![
                value_param("delay", HostTypeSchema::Int),
                take_param("resource", HostTypeSchema::Resource(guard_key())),
            ],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let ping_schema =
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
            ping_then_int_program(ping_schema, take_schema, 41),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("a declared take that was not consumed must fail the call");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("was not consumed")),
            "expected a structured not-consumed rejection, got: {error}"
        );
        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        assert!(
            is_live(&vm, raw, &guard_key()),
            "the reclaimed resource stays in the scope until the scope close releases it"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(41)],
            "the reclaimed handle is never handed back and the untaken delay argument is \
             restored exactly once"
        );
        assert_eq!(
            closes.load(Ordering::SeqCst),
            0,
            "the reclaim itself must not close the resource twice"
        );
        drop(vm);
        assert_eq!(
            closes.load(Ordering::SeqCst),
            1,
            "the scope close releases the reclaimed resource exactly once"
        );
    }

    /// A `BorrowMut` argument the host function consumed is a structured
    /// access conflict reported by the commit; the illegal consumption itself
    /// is not undone and the untaken borrowed argument is restored once.
    #[test]
    fn owned_resource_commit_reports_a_consumed_borrowed_resource() {
        let closes = Arc::new(AtomicUsize::new(0));
        let ping_raw = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = resource_schema(
            "test::take",
            vec![
                borrow_mut_param("resource", HostTypeSchema::Resource(guard_key())),
                take_param("callback", callable_schema()),
            ],
            HostTypeSchema::Bool,
        );
        let mut registry = HostFunctionRegistry::empty();
        let ping_schema =
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
            ping_then_callable_program(ping_schema, take_schema),
            &registry,
        );
        let error = vm
            .run()
            .expect_err("consuming a borrowed argument must fail the commit");
        assert_eq!(
            resource_error_code(&error),
            Some(ResourceErrorCode::ResourceAccessConflict),
            "expected a structured borrow conflict, got: {error}"
        );
        let raw = ping_raw
            .lock()
            .expect("ping record")
            .expect("ping recorded a handle");
        assert!(
            !is_live(&vm, raw, &guard_key()),
            "the illegal consumption itself is not undone by the commit"
        );
        assert_eq!(
            vm.instance.stack,
            vec![Value::Int(raw as i64)],
            "the untaken (borrowed) argument must be restored exactly once"
        );
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

        let mut vm = bound_vm(int_then_callable_program(take_schema, 41), &registry);
        let error = vm
            .run()
            .expect_err("a non-handle exact resource return must be rejected");
        assert!(
            matches!(&error, VmError::TypeMismatch(message) if *message == "resource"),
            "expected a structured resource-handle rejection, got: {error}"
        );
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
            "the untaken delay argument must be restored exactly once after a rejected exact \
             return; nothing is dropped twice"
        );
    }

    /// The exact return validates as a handle but its resource-key transfer
    /// fails: the taken callable stays consumed, the untaken delay is
    /// restored, and the wrongly-keyed resource is never transferred to the
    /// guest.
    #[test]
    fn owned_dispatch_restores_untaken_arguments_on_a_failed_ownership_transfer() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let take_schema = int_callable_resource_schema();
        let mut registry = HostFunctionRegistry::empty();
        register_bad_return(&mut registry, take_schema.clone(), &seen, |seen| {
            Box::new(BadReturnKey { seen })
        });

        let mut vm = bound_vm(int_then_callable_program(take_schema, 41), &registry);
        let error = vm
            .run()
            .expect_err("a key-mismatched exact resource return must be rejected");
        assert!(
            matches!(&error, VmError::HostError(message) if message.contains("resource_type_key_mismatch")),
            "expected a structured key mismatch, got: {error}"
        );
        let raw = seen
            .lock()
            .expect("seen")
            .handle
            .expect("the handler recorded its returned handle");
        assert!(
            is_live(&vm, raw, &other_key()),
            "a failed ownership transfer must leave the resource live under its own key"
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

        let mut vm = bound_vm(int_then_callable_program(take_schema, 41), &registry);
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

    // ---- nested owned-callable capture graph probes ---------------------------

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

    /// Resource-bearing capture rejection is schema-only, and it applies inside a
    /// nested callable capture too: the host path never reads or moves the source
    /// VM's resource table.
    #[test]
    fn owned_callable_rejects_resource_in_nested_non_move_callable_capture() {
        let key = crate::host_api::ResourceTypeKey::new("test.nested_capture").expect("key");
        let nested_schema = TypeSchema::Callable {
            params: Vec::new(),
            result: Box::new(TypeSchema::Unknown),
        };
        let program = capture_program(
            vec![nested_schema, TypeSchema::Resource(key)],
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

    /// A foreign callable captured *inside* the owned graph is rejected with the
    /// source-provenance rejection: owned graphs are program-local and are never
    /// portable across VM boundaries, however deeply they are nested.
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
        let result = call.spawn_owned_callable_vm(&registry, &outer, |_vm| Ok(()));
        let error = match result {
            Ok(_) => panic!("nested foreign callable must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, VmError::InvalidFrameState(message) if message.contains("source vm")),
            "unexpected nested provenance error: {error}"
        );
    }
}
