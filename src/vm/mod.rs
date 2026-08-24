use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

pub(crate) mod aot;
mod async_host;
mod capability;
pub mod diagnostics;
mod engine;
mod epoch;
pub mod execution_scope;
mod fuel;
pub(crate) mod host;
mod host_context;
pub mod host_extension;
mod host_runtime;
#[cfg(test)]
mod host_stream_tests;
mod instance;
pub mod invocation;
pub(crate) mod jit;
mod map_iter;
pub(crate) mod native;
pub mod operation;
pub mod program;
pub mod resource;
mod run_context;
pub mod standard_composition;
mod store;
mod superinstructions;
#[cfg(test)]
mod tests;
pub use self::aot::AotArtifactError;

pub use self::async_host::{
    CaptureAsyncHostContext, HostAsyncBridge, HostFuture, HostFutureOutput,
};
#[cfg_attr(not(feature = "http-client"), allow(unused_imports))]
pub(crate) use self::async_host::{HostStreamAction, HostStreamDriver, HostStreamPoll};
pub use self::capability::{CapabilityProfile, CapabilityProfileBuilder};
use self::engine::Engine;
pub use self::epoch::{EpochCheckpoint, EpochHandle};
use self::execution_scope::{ExecutionScopeError, ScopeCloseFailure, ScopeCloseOutcome};
pub use self::fuel::FuelCheckpoint;
pub use self::host::{
    CallOutcome, CallReturn, HostArgsFunction, HostBindingPlan, HostFunction, HostFunctionRegistry,
    HostOpId, HostStackFunction, StaticHostArgsFunction, StaticHostFunction,
    StaticHostStackFunction,
};
use self::host::{HostCallExecOutcome, VmHostFunction};
pub use self::host_context::{
    HostContext, HostContextError, HostContextErrorKind, HostContextResult, HostModule,
};
pub use self::host_extension::{HostExtension, HostModuleState, catalog_import_schemas};
use self::host_runtime::HostRuntime;
use self::instance::{ExecutionFrame, FrameContinuation, Instance, QueuedCallable};
pub use self::invocation::{Invocation, InvocationError, InvocationItem, InvocationPoll};
use self::operation::{OperationError, OperationErrorCode};
use self::resource::ResourceCloseReason;
pub use self::resource::{
    CloseProgress, GuestReleaseOutcome, HostResource, OwnershipRelease, Resource,
    ResourceAccessFrame, ResourceAccessMode, ResourceAccessRequest, ResourceError,
    ResourceErrorCode, ResourceHandle, ResourceMut, ResourceOwned, ResourceOwnership, ResourceRef,
    ResourceTable,
};
use self::run_context::{InterruptMode, RunContext};
pub use self::standard_composition::StandardSurfaceComposition;
pub use crate::builtins::BuiltinFunction;
pub use crate::builtins::runtime::cancellation::CancellationReason;
pub use crate::builtins::runtime::error::{RuntimeError, RuntimeErrorCode};
pub use crate::host_api::HostParamPassing;
pub use crate::host_api::ResourceTypeKey;

pub use crate::bytecode::{
    CallableTarget, CallableValue, HostImport, HostImportParam, HostImportSchema, OpCode, Program,
    Value, ValueType,
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
    /// A JIT bridge received a callable prototype id outside the valid `u32`
    /// index range (e.g. a negative value). Carries the raw value so the
    /// error stays accurate instead of masquerading as a truncated id.
    InvalidCallablePrototypeId(i64),
    InvalidBranchTarget {
        target: usize,
    },
    CallableArityMismatch {
        prototype_id: u32,
        expected: u8,
        got: u8,
    },
    /// `CallScript` targeted a prototype whose capture layout requires a
    /// callable environment, which a static script call cannot supply.
    CallScriptRequiresEnvironment(u32),
    CallStackOverflow {
        limit: usize,
    },
    InvalidCallStackLimit(usize),
    UnboundImport(String),
    InvalidOpcode(u8),
    BytecodeBounds,
    HostError(String),
    /// A structured resource capability failure. This variant is preserved
    /// across host-context, macro adapter, and VM boundaries.
    Resource(ResourceError),
    /// A structured failure from a legacy runtime identity space surfaced at
    /// the VM boundary.
    ///
    /// Retained for public-SDK and wasm compatibility; the execution scope is
    /// now the single authority for resources and operations, so the modern
    /// construction path reports identity exhaustion through the typed
    /// [`VmError::Resource`] / [`VmError::Operation`] variants instead.
    LegacyRuntime(RuntimeError),
    /// A structured failure from the modern operation registry, including
    /// process-unique tag identity exhaustion.
    Operation(OperationError),
    /// A structured execution-scope state/close failure that is not a direct
    /// resource or operation error.
    ExecutionScope(ExecutionScopeError),
    /// A structured error from exact host-import binding / registration.
    HostImportBinding(HostImportBindingError),
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
    /// A structured VM reset/reuse contract failure.
    Reset(VmResetError),
}

/// Structured error for exact host-import binding and registration.
///
/// These replace the stringly-typed `VmError::HostError(String)` failures so
/// callers and tests can match on fields (import/name, expected vs. got
/// values, capacity limit) instead of parsing messages. The legacy
/// [`VmError::HostError`] variant remains for pre-existing string-based errors
/// and stays fully compatible.
///
/// This public enum is `non_exhaustive` so adding structured binding
/// diagnostics remains source-compatible for downstream embedders.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostImportBindingError {
    /// The supplied catalog does not declare an adapter-required member.
    MissingCatalogMember {
        import: String,
        expected: Vec<HostImportSchema>,
    },
    /// The supplied catalog declares a member, but none of its overloads has
    /// the adapter-compatible parameter labels, passing modes, parameter
    /// schemas and return schema. Catalog fingerprints are intentionally not
    /// compared here: custom and combined catalogs may have their own identity.
    IncompatibleCatalogSchema {
        import: String,
        expected: Vec<HostImportSchema>,
        got: Vec<HostImportSchema>,
    },
    /// A registered exact name + schema conflicts with an existing binding.
    Duplicate { import: String },
    /// A program import carrying an exact schema has no matching registered
    /// exact binding; it never falls back to a legacy by-name slot.
    MissingExact { import: String },
    /// The registered arity disagrees with the schema's parameter count.
    SchemaArityMismatch {
        import: String,
        expected: u8,
        got: u8,
    },
    /// The import's coarse return type disagrees with the schema's coarse
    /// return type at bind time.
    ReturnTypeMismatch {
        import: String,
        expected: ValueType,
        got: ValueType,
    },
    /// The exact registry's `u16` slot space is exhausted.
    CapacityExceeded { import: String, limit: usize },
    /// The supplied schema is internally inconsistent at registration time.
    InvalidSchema { import: String, reason: String },
}

impl std::fmt::Display for HostImportBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCatalogMember { import, expected } => write!(
                f,
                "catalog is missing required host member '{import}' (expected schemas: {expected:?})"
            ),
            Self::IncompatibleCatalogSchema {
                import,
                expected,
                got,
            } => write!(
                f,
                "catalog schema for '{import}' is incompatible with its adapter (expected: {expected:?}, got: {got:?})"
            ),
            Self::Duplicate { import } => {
                write!(
                    f,
                    "duplicate exact host binding for '{import}' (same import schema)"
                )
            }
            Self::MissingExact { import } => write!(
                f,
                "host import '{import}' has no exact binding matching its import schema"
            ),
            Self::SchemaArityMismatch {
                import,
                expected,
                got,
            } => write!(
                f,
                "exact host binding '{import}' arity {got} does not match its schema parameter count {expected}"
            ),
            Self::ReturnTypeMismatch {
                import,
                expected,
                got,
            } => write!(
                f,
                "exact host binding '{import}' return schema mismatch: expected {expected:?}, got {got:?}"
            ),
            Self::CapacityExceeded { import, limit } => write!(
                f,
                "exact host binding registry capacity exceeded registering '{import}': limit {limit} slots"
            ),
            Self::InvalidSchema { import, reason } => {
                write!(
                    f,
                    "invalid exact host binding schema for '{import}': {reason}"
                )
            }
        }
    }
}

impl std::error::Error for HostImportBindingError {}

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
            VmError::InvalidCallablePrototypeId(id) => {
                write!(f, "invalid callable prototype id {id}")
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
            VmError::CallScriptRequiresEnvironment(prototype_id) => write!(
                f,
                "callscript prototype {prototype_id} requires a callable environment"
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
            VmError::Resource(error) => write!(f, "resource error: {error}"),
            VmError::LegacyRuntime(error) => write!(f, "legacy runtime error: {error}"),
            VmError::Operation(error) => write!(f, "operation error: {error}"),
            VmError::ExecutionScope(error) => write!(f, "execution scope error: {error}"),
            VmError::HostImportBinding(error) => write!(f, "host import binding error: {error}"),
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
            VmError::Reset(error) => write!(f, "vm reset error: {error}"),
        }
    }
}

impl std::error::Error for VmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resource(error) => Some(error),
            Self::LegacyRuntime(error) => Some(error),
            Self::Operation(error) => Some(error),
            Self::ExecutionScope(error) => Some(error),
            Self::HostImportBinding(error) => Some(error),
            Self::Reset(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ResourceError> for VmError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

impl From<ExecutionScopeError> for VmError {
    fn from(error: ExecutionScopeError) -> Self {
        match error {
            ExecutionScopeError::Resource(error) => Self::Resource(error),
            ExecutionScopeError::ArenaExhausted(error) => Self::Resource(error),
            ExecutionScopeError::Operation(error) => Self::Operation(error),
            other => Self::ExecutionScope(other),
        }
    }
}

impl VmError {
    /// Returns the structured resource error without requiring callers to
    /// parse the legacy `HostError` display string.
    pub fn resource_error(&self) -> Option<&ResourceError> {
        match self {
            Self::Resource(error) => Some(error),
            _ => None,
        }
    }

    /// Returns the stable resource error category, when this is a resource
    /// failure.
    pub fn resource_error_code(&self) -> Option<ResourceErrorCode> {
        self.resource_error().map(ResourceError::code)
    }

    /// Returns the structured modern operation-registry error without
    /// requiring callers to parse the presentation string.
    pub fn operation_error(&self) -> Option<&OperationError> {
        match self {
            Self::Operation(error) => Some(error),
            _ => None,
        }
    }

    /// Returns the stable modern operation error category, when present.
    pub fn operation_error_code(&self) -> Option<OperationErrorCode> {
        self.operation_error().map(OperationError::code)
    }

    /// Returns the structured legacy runtime error (retained for public-SDK
    /// and wasm compatibility) without requiring callers to parse the
    /// `HostError` display string.
    ///
    /// Callers can pattern-match on the stable [`RuntimeErrorCode`] via
    /// [`RuntimeError::code`] and read the operation / message / limit / value
    /// payloads through the structured accessors.
    pub fn legacy_runtime_error(&self) -> Option<&RuntimeError> {
        match self {
            Self::LegacyRuntime(error) => Some(error),
            _ => None,
        }
    }

    /// Returns the stable legacy runtime error category, when this is a
    /// legacy runtime identity-space failure.
    pub fn legacy_runtime_error_code(&self) -> Option<RuntimeErrorCode> {
        self.legacy_runtime_error().map(RuntimeError::code)
    }
}

pub type VmResult<T> = Result<T, VmError>;

/// Reuse/reset lifecycle state of a [`Vm`].
///
/// A fresh `Vm` starts [`Ready`](Self::Ready): it is executable and may be
/// lent out of a reuse pool. [`Vm::begin_reset_for_reuse`] moves it to
/// [`Resetting`](Self::Resetting) while the execution-scope close is driven
/// to quiescence; run/resume and pool reuse are rejected until the reset
/// completes and the `Vm` returns to `Ready`. Any terminal reset failure
/// (scope cleanup error, scope recycle/arena exhaustion, or deadline) moves
/// the `Vm` to [`Poisoned`](Self::Poisoned), which is permanent: it never
/// auto-returns to `Ready`, the old scope and the recorded error are
/// preserved for diagnostics, and the `Vm` is never lent out again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmResetState {
    /// The VM is executable and reusable (a pool may lend it out).
    Ready,
    /// A reset is in progress; run/resume and reuse are rejected.
    Resetting,
    /// A previous reset failed terminally; the VM is permanently unusable.
    Poisoned,
}

/// Outcome of [`Vm::begin_reset_for_reuse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginResetOutcome {
    /// The reset was started by this call; the first reason/deadline were
    /// bound now.
    Started,
    /// A reset was already in progress; the first reason/deadline are
    /// retained unchanged (idempotent repeat).
    AlreadyStarted,
}

/// Structured failure for the VM reset / reuse contract.
///
/// Replaces stringly-typed reset failures so callers and tests can match on
/// fields (state, deadline timestamps, scope close outcome) instead of
/// parsing messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmResetError {
    /// Execution (`run` / `resume` / `start_callable`) was attempted while
    /// the VM was not Ready. `stage` names the blocked entry point.
    NotReusable {
        state: VmResetState,
        stage: &'static str,
    },
    /// The synchronous compat [`Vm::reset_for_reuse`] began a scope close
    /// that cannot complete inline: a genuinely pending resource/operation
    /// still blocks quiescence. The VM stays `Resetting`; drive
    /// [`Vm::poll_reset_for_reuse`] to completion.
    ResetPending {
        resource_count: usize,
        operation_count: usize,
    },
    /// The embedding/pool recycle deadline passed before the scope reached
    /// quiescence. Typed [`ScopeCleanupDeadline`] per the pool contract: the
    /// VM is permanently discarded/poisoned and no further reuse is
    /// attempted.
    ScopeCleanupDeadline { deadline: Instant, now: Instant },
    /// Scope shutdown finished but at least one cleanup failed; the VM is
    /// poisoned and the old scope is preserved for diagnostics. Carries the
    /// first (earliest) typed failure plus the total failure count observed
    /// across the whole shutdown.
    ScopeCleanup(ScopeCloseFailure),
    /// `take_quiescent_scope` was requested but the scope was not quiescent
    /// (defensive; cannot normally fire after a driven close).
    ScopeNotQuiescent(ExecutionScopeError),
    /// The quiescent scope could not be recycled into a fresh Active scope
    /// because a fresh execution scope could not be constructed (for example,
    /// process-unique resource-arena or operation-registry identity space is
    /// exhausted). The old scope stays installed and intact for diagnostics;
    /// the VM is poisoned and never reused.
    ScopeRecycle(ExecutionScopeError),
    /// A reset/reuse API was exercised on an already-poisoned VM.
    AlreadyPoisoned { reason: String },
}

impl std::fmt::Display for VmResetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReusable { state, stage } => {
                write!(f, "{stage} requires a ready vm, but the vm is {state:?}")
            }
            Self::ResetPending {
                resource_count,
                operation_count,
            } => write!(
                f,
                "reset is pending: {resource_count} resource(s) and {operation_count} operation(s) still closing",
            ),
            Self::ScopeCleanupDeadline { deadline, now } => write!(
                f,
                "scope cleanup recycle deadline {deadline:?} passed at {now:?}; the vm is permanently discarded",
            ),
            Self::ScopeCleanup(failure) => {
                write!(
                    f,
                    "execution scope cleanup failed ({} failure(s), first: {:?}); the vm is poisoned",
                    failure.failed, failure.first
                )
            }
            Self::ScopeNotQuiescent(error) => {
                write!(f, "execution scope is not quiescent: {error}")
            }
            Self::ScopeRecycle(error) => write!(
                f,
                "execution scope recycle failed: {error}; the vm is poisoned"
            ),
            Self::AlreadyPoisoned { reason } => write!(f, "vm is permanently poisoned: {reason}"),
        }
    }
}

impl std::error::Error for VmResetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ScopeNotQuiescent(error) | Self::ScopeRecycle(error) => Some(error),
            _ => None,
        }
    }
}

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

pub struct Vm {
    program: Arc<Program>,
    pub(crate) engine: Engine,
    pub(crate) instance: Instance,
    pub(crate) run_ctx: RunContext,
    pub(crate) host: HostRuntime,
    /// Reuse/reset lifecycle state (Ready → Resetting → Ready | Poisoned).
    reset_state: VmResetState,
    /// Deadline bound by the first `begin_reset_for_reuse` call, if any.
    reset_deadline: Option<Instant>,
    /// Structured failure that poisoned the VM (or the pending indicator for
    /// a compat reset still in progress), preserved for diagnostics.
    reset_error: Option<VmResetError>,
    /// First reset reason bound by the first `begin_reset_for_reuse` call
    /// (first-reason-wins; repeated begins are idempotent).
    reset_first_reason: Option<ResourceCloseReason>,
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
        // A resource schema admits exactly the `Value::Int` carriers that
        // decode as a structurally valid resource handle token. The check is
        // deliberately structural (`from_raw` reserved-space decode), never a
        // table/key lookup: nominal identity and liveness are later scopes.
        TypeSchema::Resource(_) => ResourceHandle::from_value(value).is_ok(),
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
        TypeSchema::Resource(key) => {
            17u8.hash(state);
            key.hash(state);
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
    /// Creates a VM from a [`Program`] with the default JIT configuration.
    ///
    /// Fallible: initial construction allocates a process-unique execution
    /// scope arena identity, which can be exhausted. See
    /// [`Vm::try_new`]. Embeddings and pools must propagate this error.
    pub fn try_new(program: Program) -> VmResult<Self> {
        Self::try_new_shared_with_jit_config(Arc::new(program), jit::JitConfig::default())
    }

    /// Creates a VM from a [`Program`] with an explicit JIT configuration.
    ///
    /// Fallible: see [`Vm::try_new`].
    pub fn try_new_with_jit_config(program: Program, jit_config: jit::JitConfig) -> VmResult<Self> {
        Self::try_new_shared_with_jit_config(Arc::new(program), jit_config)
    }

    /// Creates a VM sharing a [`Program`] with the default JIT configuration.
    ///
    /// Fallible: see [`Vm::try_new`].
    pub fn try_new_shared(program: Arc<Program>) -> VmResult<Self> {
        Self::try_new_shared_with_jit_config(program, jit::JitConfig::default())
    }

    /// The core fallible construction path: builds a VM sharing `program` with
    /// `jit_config` and a fresh host runtime / execution scope.
    ///
    /// # Arena exhaustion
    ///
    /// A VM always owns an execution scope backed by a process-unique arena
    /// identity (20-bit, ~1,048,575 handouts per process). When that space is
    /// exhausted, the typed construction path surfaces a
    /// [`VmError::Resource`] with code
    /// [`ResourceErrorCode::ResourceTableArenaExhausted`] — never a panic.
    /// (The retained [`VmError::LegacyRuntime`] variant is an API-compat
    /// legacy identity-space error and is not produced by this path.)
    ///
    /// There is **no** infallible construction path: every long-lived / embedding
    /// / pool construction inside this crate (the CLI, the REPL, the replay/AOT
    /// loaders, the WASM playground, and the scope-recycle/reset path) uses
    /// these `try_*` constructors and interprets a
    /// `ResourceTableArenaExhausted` as a terminal, non-reusable failure.
    /// Tests call `try_*().expect("...")` locally. New embedding and pool
    /// code must call `try_*`.
    pub fn try_new_shared_with_jit_config(
        program: Arc<Program>,
        jit_config: jit::JitConfig,
    ) -> VmResult<Self> {
        let engine = Engine::new(jit_config, &program);
        let mut instance = Instance::new(&program);
        instance.initialize_root_callable_bindings(&program);
        let host = HostRuntime::new().map_err(VmError::from)?;
        Ok(Self {
            program,
            engine,
            instance,
            run_ctx: RunContext::default(),
            host,
            reset_state: VmResetState::Ready,
            reset_deadline: None,
            reset_error: None,
            reset_first_reason: None,
        })
    }

    /// Returns the generic host boundary for this VM.
    ///
    /// External host extensions register typed, per-VM module state without
    /// ever touching the underlying host runtime internals or a builtin domain
    /// module. The returned [`HostContext`] borrows this VM mutably.
    pub fn host_context(&mut self) -> HostContext<'_> {
        HostContext::new(&mut self.host)
    }

    /// Installs the caller-provided standard-surface composition on this VM
    /// (explicit per-instance state, never a process global).
    ///
    /// The outer standard-runtime constructor calls this so the VM's
    /// default-fallback paths can compose the standard surfaces (auto-stage,
    /// default-registry construction, legacy by-name default binding) without
    /// the core knowing concrete domains. A VM bound by a registry that
    /// carries a composition also receives it automatically through
    /// [`HostFunctionRegistry::bind_vm_with_plan`]; this setter is for
    /// bare VMs constructed outside a registry bind.
    #[cfg(feature = "runtime")]
    pub fn set_standard_composition(
        &mut self,
        composition: std::sync::Arc<dyn StandardSurfaceComposition>,
    ) {
        // Changing the VM's composition is a resolved-binding mutation: mark
        // the resolved-call cache dirty so the next `ensure_call_bindings`
        // re-resolves under the new composition with deterministic semantics
        // rather than reusing a resolution made under a previous strategy.
        self.host.standard_composition = Some(composition);
        self.host.resolved_calls_dirty = true;
    }

    /// Installs a [`HostExtension`] into this VM through its standard
    /// register / install lifecycle.
    ///
    /// [`HostExtension::register`] runs against the standard host-function
    /// registry (builtin defaults plus the extension's exact functions) and
    /// that registry is bound with
    /// [`HostFunctionRegistry::bind_vm_cached`]. Both are fallible; then the
    /// now-infallible [`HostExtension::install`] installs persistent per-VM
    /// module state. Because every fallible step runs before `install`, the
    /// call is **transactional**: on any registration or binding failure the
    /// VM is left exactly as it was — unbound and with no module state — so a
    /// corrected `install_extension` can be retried on the same VM. A
    /// successful call fully binds the VM and installs its module state.
    ///
    /// Because `bind_vm_cached` requires an unbound VM, call this before the
    /// first `run` (and before any other registry binding); controls needing a
    /// restricted/capability-granted registry should instead call
    /// [`HostExtension::register`] directly and bind the registry themselves.
    pub fn install_extension(&mut self, extension: &dyn HostExtension) -> VmResult<()> {
        let mut registry = HostFunctionRegistry::new();
        extension.register(&mut registry)?;
        registry.bind_vm_cached(self)?;
        extension.install(self);
        Ok(())
    }

    /// Begins an operation-aware resource frame and preserves resource errors
    /// as the structured [`VmError::Resource`] variant.
    pub fn begin_resource_access(
        &mut self,
        requests: Vec<ResourceAccessRequest>,
    ) -> VmResult<ResourceAccessFrame<'_>> {
        self.host
            .execution_scope_begin_resource_access(requests)
            .map_err(VmError::from)
    }
    /// Returns the maximum number of simultaneously active script call frames.
    pub fn max_script_call_depth(&self) -> usize {
        self.instance.max_script_call_depth
    }

    /// Sets the maximum number of simultaneously active script call frames.
    ///
    /// The limit must be greater than zero. Existing active frames are not unwound;
    /// the new limit is checked before the next script frame is entered.
    pub fn set_max_script_call_depth(&mut self, limit: usize) -> VmResult<()> {
        if limit == 0 {
            return Err(VmError::InvalidCallStackLimit(limit));
        }
        self.instance.max_script_call_depth = limit;
        Ok(())
    }

    fn ensure_program_cache_key(&mut self) -> u64 {
        self.engine.ensure_program_cache_key(&self.program)
    }

    #[inline(always)]
    fn fuel_metering_enabled(&self) -> bool {
        self.run_ctx.interrupt_mode == InterruptMode::Fuel
    }

    #[inline(always)]
    fn epoch_interruption_enabled(&self) -> bool {
        self.run_ctx.interrupt_mode == InterruptMode::Epoch
    }

    #[inline(always)]
    fn interruption_enabled(&self) -> bool {
        self.run_ctx.interrupt_mode != InterruptMode::None
    }

    /// Returns the maximum number of compiled regular expressions retained by this VM.
    ///
    /// New VMs default to 512 entries. A capacity of zero disables caching.
    pub fn regex_cache_capacity(&self) -> usize {
        self.engine.regex_cache.capacity()
    }

    /// Changes this VM's compiled regular-expression cache capacity.
    ///
    /// Shrinking evicts least-recently-used entries immediately. Setting zero clears
    /// all entries and disables caching until a positive capacity is configured.
    pub fn set_regex_cache_capacity(&mut self, capacity: usize) {
        self.engine.regex_cache.set_capacity(capacity);
    }

    pub fn regex_cache_entry_count(&self) -> usize {
        self.engine.regex_cache.len()
    }

    pub fn regex_cache_compile_count(&self) -> u64 {
        self.engine.regex_cache.compile_count()
    }

    pub fn regex_cache_hit_count(&self) -> u64 {
        self.engine.regex_cache.hit_count()
    }

    pub(crate) fn cached_regex(
        &mut self,
        pattern: &str,
    ) -> Result<std::sync::Arc<regex::Regex>, regex::Error> {
        self.engine.regex_cache.get_or_compile(pattern)
    }

    pub fn set_jit_native_bridge_stats_enabled(&mut self, enabled: bool) {
        self.engine.jit_native_bridge_stats_enabled = enabled;
        if !enabled {
            self.engine.jit_native_bridge_counts.clear();
        }
    }

    pub fn jit_native_bridge_stats_enabled(&self) -> bool {
        self.engine.jit_native_bridge_stats_enabled
    }

    pub fn clear_jit_native_bridge_stats(&mut self) {
        self.engine.jit_native_bridge_counts.clear();
    }

    pub fn interpreter_metrics_snapshot(&self) -> InterpreterMetrics {
        InterpreterMetrics {
            operand_hint_hit_count: self.instance.operand_hint_hit_count,
            operand_hint_miss_count: self.instance.operand_hint_miss_count,
            typed_builtin_fast_path_count: self.instance.typed_builtin_fast_path_count,
            projection_fast_path_count: self.instance.projection_fast_path_count,
            generic_builtin_call_count: self.instance.generic_builtin_call_count,
            scalar_superinstruction_count: self.instance.scalar_superinstruction_count,
            local_type_hint_hit_count: self.instance.local_type_hint_hit_count,
        }
    }

    pub fn clear_interpreter_metrics(&mut self) {
        self.instance.operand_hint_hit_count = 0;
        self.instance.operand_hint_miss_count = 0;
        self.instance.typed_builtin_fast_path_count = 0;
        self.instance.projection_fast_path_count = 0;
        self.instance.generic_builtin_call_count = 0;
        self.instance.scalar_superinstruction_count = 0;
        self.instance.local_type_hint_hit_count = 0;
    }

    pub fn jit_native_bridge_stats_snapshot(&self) -> Vec<(&'static str, u64)> {
        let mut entries: Vec<(&'static str, u64)> = self
            .engine
            .jit_native_bridge_counts
            .iter()
            .map(|(name, count)| (*name, *count))
            .collect();
        entries.sort_unstable_by_key(|(name, _)| *name);
        entries
    }

    #[allow(dead_code)]
    pub(in crate::vm) fn record_native_bridge_hit(&mut self, bridge_name: &'static str) {
        if !self.engine.jit_native_bridge_stats_enabled {
            return;
        }
        let entry = self
            .engine
            .jit_native_bridge_counts
            .entry(bridge_name)
            .or_insert(0);
        *entry = entry.saturating_add(1);
    }

    /// Reset VM execution state to allow rerunning the same program instance while
    /// preserving JIT artifacts and registered host bindings.
    ///
    /// Compat path: when the execution scope is empty (the common case for
    /// existing callers) the reset completes synchronously and the VM returns
    /// to [`VmResetState::Ready`]. When a genuinely pending scope
    /// resource/operation blocks quiescence this method does **not**
    /// busy-loop: it begins the close and moves the VM to
    /// [`VmResetState::Resetting`] without clearing interpreter state; the
    /// reset must then be driven to completion through
    /// [`Vm::poll_reset_for_reuse`] (the structured
    /// [`VmResetError::ResetPending`] indicator is observable via
    /// [`Vm::reset_error`] / [`Vm::reset_state`]).
    ///
    /// Locals are reset to `Null`, stack is cleared, and instruction pointer is
    /// rewound to the program entry — but only once the reset *completes*
    /// successfully (never while pending, never after poisoning).
    pub fn reset_for_reuse(&mut self) {
        match self.reset_state {
            VmResetState::Poisoned => {
                // Permanently poisoned: never re-attempted. The caller must
                // consult reset_state()/reset_error() and replace the VM.
            }
            VmResetState::Resetting => {
                // Drive the in-progress reset by a single poll; a still
                // pending scope simply keeps the VM Resetting.
                self.drive_reset_once();
            }
            VmResetState::Ready => {
                let _ = self.begin_reset_for_reuse(ResourceCloseReason::VmReset, None);
                self.drive_reset_once();
            }
        }
    }

    /// The current reuse/reset lifecycle state of this VM.
    pub fn reset_state(&self) -> VmResetState {
        self.reset_state
    }

    /// Whether this VM is Ready: executable and eligible to be lent out of a
    /// reuse pool. A `Resetting` or `Poisoned` VM is never reusable.
    pub fn is_reusable(&self) -> bool {
        self.reset_state == VmResetState::Ready
    }

    /// The structured error that poisoned this VM (or the `ResetPending`
    /// indicator while a compat reset is still in progress), preserved for
    /// diagnostics.
    pub fn reset_error(&self) -> Option<&VmResetError> {
        self.reset_error.as_ref()
    }

    /// The first reset reason bound by the first
    /// [`begin_reset_for_reuse`](Self::begin_reset_for_reuse) call
    /// (first-reason-wins; `None` when no reset is in progress or the reset
    /// already completed).
    pub fn reset_reason(&self) -> Option<ResourceCloseReason> {
        self.reset_first_reason
    }

    /// The reset deadline bound by the first
    /// [`begin_reset_for_reuse`](Self::begin_reset_for_reuse) call, if any.
    pub fn reset_deadline(&self) -> Option<Instant> {
        self.reset_deadline
    }

    /// Begins the two-phase reset for reuse.
    ///
    /// First-reason/deadline-wins and idempotent: the first call binds
    /// `reason`/`deadline` and starts the execution-scope close
    /// (Active → Closing, sealing new inserts); every later call returns
    /// [`BeginResetOutcome::AlreadyStarted`] and leaves the bound
    /// reason/deadline unchanged. It never clears interpreter state, never
    /// creates a new scope, and never marks the VM reusable — completion
    /// happens only through [`Vm::poll_reset_for_reuse`].
    pub fn begin_reset_for_reuse(
        &mut self,
        reason: ResourceCloseReason,
        deadline: Option<Instant>,
    ) -> VmResult<BeginResetOutcome> {
        match self.reset_state {
            VmResetState::Poisoned => Err(VmError::Reset(VmResetError::AlreadyPoisoned {
                reason: self.poison_diagnostic(),
            })),
            VmResetState::Ready | VmResetState::Resetting => {
                let starting = self.reset_first_reason.is_none();
                if starting {
                    self.reset_first_reason = Some(reason);
                    self.reset_deadline = deadline;
                    self.reset_state = VmResetState::Resetting;
                    self.reset_error = None;
                    // Start the scope close exactly once (first-reason-wins at
                    // the scope level too). If the scope is already Closing
                    // with a different reason (a prior HostContext
                    // begin_close), we still drive that close to quiescence;
                    // the Vm-level first reason governs the reset contract.
                    let _ = self.host.execution_scope_begin_close(reason);
                }
                Ok(if starting {
                    BeginResetOutcome::Started
                } else {
                    BeginResetOutcome::AlreadyStarted
                })
            }
        }
    }

    /// Polls the in-progress reset, using the passed-in `now` as the current
    /// time for the deadline (deterministic, never sleeps).
    ///
    /// - [`Poll::Pending`]: scope cleanup is still running; the VM stays
    ///   [`VmResetState::Resetting`] (interpreter state untouched, no new
    ///   scope, not reusable). Poll again later with a fresh `now`.
    /// - [`Poll::Ready`]`(Ok(()))`: the reset completed (or the VM was
    ///   already Ready); the VM is `Ready` and reusable. Idempotent — a
    ///   repeated poll after success returns the same `Ready(Ok(()))`.
    /// - [`Poll::Ready`]`(Err(_))`: a terminal failure (deadline, scope
    ///   cleanup error, or scope recycle failure) poisoned the VM; the old
    ///   scope and the error are preserved and the VM is never reusable
    ///   again.
    pub fn poll_reset_for_reuse(
        &mut self,
        cx: &mut Context<'_>,
        now: Instant,
    ) -> Poll<VmResult<()>> {
        match self.reset_state {
            VmResetState::Poisoned => Poll::Ready(Err(VmError::Reset(
                self.reset_error
                    .clone()
                    .unwrap_or_else(|| VmResetError::AlreadyPoisoned {
                        reason: "vm is permanently poisoned".to_string(),
                    }),
            ))),
            VmResetState::Ready => Poll::Ready(Ok(())),
            VmResetState::Resetting => {
                if let Some(deadline) = self.reset_deadline {
                    let timeout = now >= deadline;
                    if timeout {
                        // Recycle deadline: poison without pretending cleanup
                        // ran, and report the typed ScopeCleanupDeadline per
                        // the pool contract (the VM is permanently discarded).
                        // The old scope and error stay in place for
                        // diagnostics.
                        let error = VmResetError::ScopeCleanupDeadline { deadline, now };
                        self.poison(error.clone());
                        return Poll::Ready(Err(VmError::Reset(error)));
                    }
                }
                match self.host.execution_scope_poll_close(cx) {
                    Poll::Pending => {
                        // Record the current blocking counts as the structured
                        // pending diagnostic (observable via reset_error()).
                        self.reset_error = Some(VmResetError::ResetPending {
                            resource_count: self.host.execution_scope_resource_count(),
                            operation_count: self.host.execution_scope_operation_count(),
                        });
                        Poll::Pending
                    }
                    Poll::Ready(Ok(ScopeCloseOutcome::Success)) => {
                        match self.finish_reset_to_ready() {
                            Ok(()) => Poll::Ready(Ok(())),
                            Err(error) => {
                                self.poison(error.clone());
                                Poll::Ready(Err(VmError::Reset(error)))
                            }
                        }
                    }
                    Poll::Ready(Ok(ScopeCloseOutcome::SuccessWithErrors(first))) => {
                        // Best-effort cleanup finished with a preserved
                        // failure: poison, keep the old scope, never swap.
                        let error = VmResetError::ScopeCleanup(first);
                        self.poison(error.clone());
                        Poll::Ready(Err(VmError::Reset(error)))
                    }
                    Poll::Ready(Err(scope_error)) => {
                        // Defensive: a scope-level failure (e.g. close never
                        // begun) is treated as terminal.
                        let error = VmResetError::ScopeNotQuiescent(scope_error);
                        self.poison(error.clone());
                        Poll::Ready(Err(VmError::Reset(error)))
                    }
                }
            }
        }
    }

    /// Drives one round of the reset with a no-op waker (used by the compat
    /// [`reset_for_reuse`](Self::reset_for_reuse)). Never loops: a still
    /// pending scope simply keeps the VM `Resetting`.
    fn drive_reset_once(&mut self) {
        struct ResetNoopWake;
        impl std::task::Wake for ResetNoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Arc::new(ResetNoopWake).into();
        let mut cx = Context::from_waker(&waker);
        let _ = self.poll_reset_for_reuse(&mut cx, Instant::now());
    }

    /// Executes the post-quiescence reset sequence: the HostRuntime reset
    /// (clears cross-run bridge/stream/pending-result state), then the
    /// R2A-safe scope recycle into a fresh Active empty scope, then the
    /// existing interpreter rewinding. The module store is deliberately
    /// preserved (never cleared by scope cleanup or reset).
    ///
    /// On any failure the caller must poison: this method never swaps the
    /// scope on error.
    fn finish_reset_to_ready(&mut self) -> Result<(), VmResetError> {
        // Scope close already delivered VmReset to every pending operation and
        // consumed all operation slots. Clear only VM-side continuations/maps;
        // attempting a second cancellation here would overwrite stream
        // semantics with Requested and target a stale id.
        self.instance.waiting_host_op = None;
        self.clear_callable_stream_after_scope_close();
        self.host.reset_for_reuse();
        // R2A-safe: recycle only a Quiescent scope into a fresh Active scope.
        // Arena exhaustion during the replacement is a terminal recycle
        // failure: the old (quiescent) scope stays installed for diagnostics,
        // no malformed scope is installed, and the caller poisons the VM.
        let old_scope = self
            .host
            .take_quiescent_scope()
            .map_err(|error| match error {
                ExecutionScopeError::ArenaExhausted(_) | ExecutionScopeError::Operation(_) => {
                    VmResetError::ScopeRecycle(error)
                }
                other => VmResetError::ScopeNotQuiescent(other),
            })?;
        drop(old_scope);
        self.run_ctx.reset_for_reuse();
        // Guest-owned release of every owned local still holding a live
        // handle (an aborted / never-halted run): the release is an idempotent
        // no-op for already-released locals and launches exactly-once closes
        // for any that survived without a frame-exit/Halt.
        let base = self.active_local_base();
        let count = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.local_count)
            .unwrap_or(self.program.local_count);
        self.release_owned_locals_range(base, count);
        self.instance.reset(&self.program);
        self.engine.reset_runtime_state(&self.program);
        self.reset_state = VmResetState::Ready;
        self.reset_deadline = None;
        self.reset_error = None;
        self.reset_first_reason = None;
        Ok(())
    }

    /// Moves the VM to the permanent `Poisoned` state: the old scope and the
    /// recorded error are kept for diagnostics, interpreter state is left
    /// untouched, and the VM is never marked reusable again.
    fn poison(&mut self, error: VmResetError) {
        self.reset_state = VmResetState::Poisoned;
        self.reset_error = Some(error);
    }

    fn poison_diagnostic(&self) -> String {
        self.reset_error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "vm is permanently poisoned".to_string())
    }

    fn ensure_executable(&self, stage: &'static str) -> VmResult<()> {
        if self.reset_state == VmResetState::Ready {
            Ok(())
        } else {
            Err(VmError::Reset(VmResetError::NotReusable {
                state: self.reset_state,
                stage,
            }))
        }
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
        let depth = self.instance.call_depth;
        if self.instance.map_iterators.len() <= depth {
            self.instance.map_iterators.resize_with(depth + 1, Vec::new);
        }
        let frame = &mut self.instance.map_iterators[depth];
        if frame.len() <= slot {
            frame.resize_with(slot + 1, || None);
        }
        frame[slot] = Some(map_iter::MapIteratorState::new(map));
        Ok(())
    }

    pub(crate) fn advance_map_iterator(&mut self, slot: usize) -> VmResult<bool> {
        self.validate_map_iterator_slot(slot)?;
        let frame = self
            .instance
            .map_iterators
            .get_mut(self.instance.call_depth)
            .ok_or_else(|| {
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
        self.instance
            .map_iterators
            .get_mut(self.instance.call_depth)
            .and_then(|frame| frame.get_mut(slot))
            .and_then(Option::as_mut)
            .and_then(map_iter::MapIteratorState::take_key)
            .ok_or_else(|| VmError::HostError("map iterator has no current key".to_string()))
    }

    pub(crate) fn take_map_iterator_value(&mut self, slot: usize) -> VmResult<Value> {
        self.validate_map_iterator_slot(slot)?;
        self.instance
            .map_iterators
            .get_mut(self.instance.call_depth)
            .and_then(|frame| frame.get_mut(slot))
            .and_then(Option::as_mut)
            .and_then(map_iter::MapIteratorState::take_value)
            .ok_or_else(|| VmError::HostError("map iterator has no current value".to_string()))
    }

    pub(crate) fn close_map_iterator(&mut self, slot: usize) -> VmResult<()> {
        self.validate_map_iterator_slot(slot)?;
        if let Some(state) = self
            .instance
            .map_iterators
            .get_mut(self.instance.call_depth)
            .and_then(|frame| frame.get_mut(slot))
        {
            *state = None;
        }
        Ok(())
    }

    fn close_all_map_iterators(&mut self) {
        for frame in &mut self.instance.map_iterators {
            for state in frame {
                state.take();
            }
        }
    }

    #[inline(always)]
    pub(super) fn active_operand_stack_base(&self) -> usize {
        self.instance.active_operand_stack_base_cache
    }

    #[inline(always)]
    pub(super) fn active_operand_stack_len(&self) -> usize {
        self.instance
            .stack
            .len()
            .saturating_sub(self.active_operand_stack_base())
    }

    #[inline(always)]
    pub(super) fn active_frame_key(&self) -> u64 {
        self.instance
            .execution_frames
            .last()
            .and_then(|frame| frame.prototype_id)
            .map(u64::from)
            .unwrap_or(crate::vm::native::ROOT_FRAME_KEY)
    }

    #[inline(always)]
    pub(super) fn active_local_base(&self) -> usize {
        self.instance.active_local_base_cache
    }

    pub(super) fn active_local_types(&self) -> Vec<ValueType> {
        self.instance.locals[self.active_local_base()..]
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
        let mut prototypes = Vec::with_capacity(self.instance.locals.len().saturating_sub(base));
        for (offset, value) in self.instance.locals[base..].iter().enumerate() {
            let prototype_id = if let Some(cell) = self.instance.capture_cells.get(&(base + offset))
            {
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
        if self.instance.shared_capture_slots.is_empty() {
            return false;
        }
        let Some(frame) = self.instance.execution_frames.last() else {
            return false;
        };
        let base = frame.local_base;
        let end = base.saturating_add(frame.local_count);
        self.instance
            .shared_capture_slots
            .iter()
            .any(|absolute| base <= *absolute && *absolute < end)
    }

    fn script_frame_depth(&self) -> usize {
        self.instance
            .execution_frames
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
        self.instance
            .locals
            .get(absolute)
            .map(|_| absolute)
            .ok_or(VmError::InvalidLocal(index))
    }

    #[inline(always)]
    fn load_local_value(&self, index: u8) -> VmResult<Value> {
        let absolute = self.absolute_local_index(index)?;
        if self.instance.capture_cells.is_empty() {
            return Ok(self.instance.locals[absolute].clone());
        }
        self.load_local_value_with_captures(absolute, index)
    }

    #[cold]
    #[inline(never)]
    fn load_local_value_with_captures(&self, absolute: usize, index: u8) -> VmResult<Value> {
        if let Some(cell) = self.instance.capture_cells.get(&absolute) {
            return cell
                .lock()
                .map(|value| value.clone())
                .map_err(|_| VmError::InvalidFrameState("capture cell lock is poisoned"));
        }
        self.instance
            .locals
            .get(absolute)
            .cloned()
            .ok_or(VmError::InvalidLocal(index))
    }

    #[inline(always)]
    pub(super) fn local_numeric_value(&self, index: u8) -> Option<NumericValue> {
        let absolute = self.absolute_local_index(index).ok()?;
        if self.instance.capture_cells.is_empty() {
            return match self.instance.locals.get(absolute)? {
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
            .instance
            .capture_cells
            .get(&absolute)
            .and_then(|cell| cell.lock().ok().map(|value| value.clone()));
        match captured
            .as_ref()
            .or_else(|| self.instance.locals.get(absolute))?
        {
            Value::Int(value) => Some(NumericValue::Int(*value)),
            Value::Float(value) => Some(NumericValue::Float(*value)),
            _ => None,
        }
    }

    pub fn drop_contract_event_count(&self) -> u64 {
        self.instance.drop_contract_events
    }

    pub fn set_drop_contract_events_enabled(&mut self, enabled: bool) {
        if self.instance.drop_contract_events_enabled != enabled {
            self.disconnect_native_regions();
            self.engine.invalidate_codegen_caches();
        }
        self.instance.drop_contract_events_enabled = enabled;
        if !enabled {
            self.instance.drop_contract_events = 0;
        }
    }

    pub fn drop_contract_events_enabled(&self) -> bool {
        self.instance.drop_contract_events_enabled
    }

    fn interruption_mode_conflict(&self, requested: InterruptMode) -> VmError {
        VmError::InterruptionModeConflict {
            active: self.run_ctx.interrupt_mode.label(),
            requested: requested.label(),
        }
    }

    fn reset_interrupt_countdown(&mut self) {
        self.run_ctx.fuel_ops_until_check = self.run_ctx.fuel_check_interval.max(1);
    }

    pub fn run(&mut self) -> VmResult<VmStatus> {
        self.ensure_executable("run")?;
        let status = match self.run_internal(None, true) {
            Ok(status) => status,
            Err(error) => {
                self.abort_callable_stream_on_run_error(&error)?;
                return Err(error);
            }
        };
        self.resume_callable_stream_after_run(status)
    }

    pub fn run_with_debugger(
        &mut self,
        debugger: &mut crate::debugger::Debugger,
    ) -> VmResult<VmStatus> {
        self.ensure_executable("run_with_debugger")?;
        let status = match self.run_internal(Some(debugger), false) {
            Ok(status) => status,
            Err(error) => {
                self.abort_callable_stream_on_run_error(&error)?;
                return Err(error);
            }
        };
        self.resume_callable_stream_after_run(status)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.cancel_waiting_host_op_with_reason(
            crate::builtins::runtime::cancellation::CancellationReason::VmDrop,
        );
        let _ = self.cancel_callable_stream(
            crate::builtins::runtime::cancellation::CancellationReason::VmDrop,
        );
        // Guest-owned release before the interpreter values are dropped: a
        // `Vm` being dropped without a prior halt/reset/shutdown still owes
        // exactly-once closes for any guest-owned local handles.
        let base = self.active_local_base();
        let count = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.local_count)
            .unwrap_or(self.program.local_count);
        self.release_owned_locals_range(base, count);
        // Execution-scope shutdown (plan section 5.3): dropping the Vm must
        // synchronously begin the execution-scope close with the VmDrop
        // reason and drive one round of the close pipeline with a no-op
        // waker. That synchronously cancels every pending operation (with the
        // parallel OperationCancelReason::VmDrop) and issues child-first
        // `begin_close` to every live resource (with ResourceCloseReason::
        // VmDrop), so a Pending child can never prevent its parent's
        // begin_close from running before the owned tables fall through to
        // their Drop guards. Genuinely event-driven Pending resources stay
        // Closing here and are released by their own Drop guards — Drop never
        // blocks, never claims quiescence, and the pool never recycles a
        // dropped Vm.
        let _ = self
            .host
            .execution_scope_begin_close(ResourceCloseReason::VmDrop);
        let _ = self.host.drive_execution_scope_close_once_with_noop_waker();
        self.host.reset_for_reuse();
        self.instance.drop_cleanup();
    }
}

impl Vm {
    pub(super) fn pop_value(&mut self) -> VmResult<Value> {
        self.instance.stack.pop().ok_or(VmError::StackUnderflow)
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
                if absolute >= self.instance.locals.len() {
                    return Err(VmError::InvalidFrameState(
                        "capture source exceeds active frame locals",
                    ));
                }
                let cell = self
                    .instance
                    .capture_cells
                    .entry(absolute)
                    .or_insert_with(|| Arc::new(Mutex::new(value)))
                    .clone();
                self.instance.shared_capture_slots.insert(absolute);
                self.instance.locals[absolute] = cell
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
        self.instance
            .owned_callables
            .push(Arc::downgrade(&callable));
        Ok(Value::Callable(callable))
    }

    fn execute_call_value(
        &mut self,
        argc: u8,
        call_site_ip: Option<usize>,
    ) -> VmResult<ExecOutcome> {
        let operand_count = argc as usize + 1;
        if self.instance.stack.len() < operand_count {
            return Err(VmError::StackUnderflow);
        }
        let operand_stack_base = self.instance.stack.len() - operand_count;
        let mut operands = self.instance.stack.split_off(operand_stack_base);
        let callee = operands.remove(0);
        let Value::Callable(callable) = callee else {
            return Err(VmError::InvalidCallable);
        };
        let prototype_id = callable.prototype_id;
        let continuation = FrameContinuation::ResumeBytecode {
            return_ip: self.instance.ip,
        };
        self.enter_script_frame(
            prototype_id,
            Some(callable),
            operands,
            operand_stack_base,
            call_site_ip,
            continuation,
        )
    }

    /// Execute a static `CallScript(prototype_id, argc)` instruction.
    ///
    /// The operands are split off the stack and the frame is entered through
    /// the shared [`Self::enter_script_frame`] helper with no callable value:
    /// `CallScript` can never supply a callable environment, so capture- or
    /// self-requiring prototypes are rejected there with a typed error.
    fn execute_call_script(
        &mut self,
        prototype_id: u32,
        argc: u8,
        call_ip: usize,
    ) -> VmResult<ExecOutcome> {
        let operand_count = argc as usize;
        if self.instance.stack.len() < operand_count {
            return Err(VmError::StackUnderflow);
        }
        let operand_stack_base = self.instance.stack.len() - operand_count;
        let operands = self.instance.stack.split_off(operand_stack_base);
        let continuation = FrameContinuation::ResumeBytecode {
            return_ip: self.instance.ip,
        };
        self.enter_script_frame(
            prototype_id,
            None,
            operands,
            operand_stack_base,
            Some(call_ip),
            continuation,
        )
    }

    /// Shared script-frame entry for `CallValue` and `CallScript`.
    ///
    /// Enters a callable frame from `(prototype_id, optional callable value,
    /// operands, continuation)`. `CallValue` passes the runtime callable
    /// value, which carries the environment and provides the self binding;
    /// `CallScript` passes `None` and must only reach environment-free
    /// function prototypes. The helper preserves arity validation, schema
    /// checks, depth limits, interruption ticks, the return continuation,
    /// operand stack cleanup, root callable binding initialization, capture
    /// cell wiring, and self-slot binding.
    fn enter_script_frame(
        &mut self,
        prototype_id: u32,
        callable: Option<Arc<CallableValue>>,
        operands: Vec<Value>,
        operand_stack_base: usize,
        call_site_ip: Option<usize>,
        continuation: FrameContinuation,
    ) -> VmResult<ExecOutcome> {
        let prototype = self
            .program
            .callable_prototypes
            .get(prototype_id as usize)
            .cloned()
            .ok_or(VmError::InvalidCallablePrototype(prototype_id))?;
        // A call without a runtime callable value (`CallScript`) cannot
        // populate capture cells or bind the function's self identity.
        if callable.is_none()
            && (!prototype.capture_slots.is_empty() || prototype.self_slot.is_some())
        {
            return Err(VmError::CallScriptRequiresEnvironment(prototype_id));
        }
        if prototype.arity != operands.len() as u8 {
            return Err(VmError::CallableArityMismatch {
                prototype_id,
                expected: prototype.arity,
                got: operands.len() as u8,
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
                    self.engine.jit.observe_script_call_target(
                        self.active_frame_key(),
                        call_ip,
                        prototype_id,
                    );
                }
                if self.instance.call_depth >= self.instance.max_script_call_depth {
                    return Err(VmError::CallStackOverflow {
                        limit: self.instance.max_script_call_depth,
                    });
                }
                let function = self
                    .program
                    .script_functions
                    .get(function_id as usize)
                    .cloned()
                    .ok_or(VmError::InvalidCallablePrototype(prototype_id))?;
                if prototype.parameter_slots.len() != operands.len() {
                    return Err(VmError::CallableArityMismatch {
                        prototype_id,
                        expected: prototype.parameter_slots.len() as u8,
                        got: operands.len() as u8,
                    });
                }
                let inherited_callables = self
                    .instance
                    .execution_frames
                    .last()
                    .map(|frame| {
                        self.instance.locals[frame.local_base..frame.local_base + frame.local_count]
                            .iter()
                            .enumerate()
                            .filter(|(_, value)| matches!(value, Value::Callable(_)))
                            .map(|(slot, value)| (slot, value.clone()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let local_base = self.instance.locals.len();
                let local_count = prototype.frame_local_count;
                self.instance
                    .locals
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
                    self.instance
                        .owned_callables
                        .push(Arc::downgrade(&callable));
                    self.instance.locals[local_base + relative] = Value::Callable(callable);
                }
                for (slot, value) in inherited_callables {
                    if slot < local_count {
                        self.instance.locals[local_base + slot] = value;
                    }
                }
                for (slot, argument) in prototype.parameter_slots.iter().zip(operands) {
                    let relative = *slot as usize;
                    if relative >= local_count {
                        return Err(VmError::InvalidFrameState(
                            "parameter slot is outside the script frame",
                        ));
                    }
                    self.instance.locals[local_base + relative] = argument;
                }
                if let Some(environment) =
                    callable.as_ref().and_then(|callable| callable.env.as_ref())
                {
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
                        self.instance.locals[absolute] = cell
                            .lock()
                            .map_err(|_| {
                                VmError::InvalidFrameState("capture cell lock is poisoned")
                            })?
                            .clone();
                        if prototype.self_slot != Some(*slot) {
                            self.instance.capture_cells.insert(absolute, cell.clone());
                            if matches!(
                                mode,
                                crate::CaptureBindingMode::Borrow
                                    | crate::CaptureBindingMode::BorrowMut
                            ) {
                                self.instance.shared_capture_slots.insert(absolute);
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
                    let Some(callable) = callable else {
                        return Err(VmError::InvalidFrameState(
                            "self slot requires a callable value",
                        ));
                    };
                    self.instance.locals[local_base + relative] = Value::Callable(callable.clone());
                }
                self.instance.execution_frames.push(ExecutionFrame {
                    continuation,
                    operand_stack_base,
                    local_base,
                    local_count,
                    prototype_id: Some(prototype_id),
                });
                self.instance.active_local_base_cache = local_base;
                self.instance.active_operand_stack_base_cache = operand_stack_base;
                self.instance.call_depth = self.script_frame_depth();
                self.instance.ip = function.entry_ip as usize;
                self.charge_interrupt_tick()?;
                Ok(ExecOutcome::Continue)
            }
            CallableTarget::HostImport(import_index) => {
                let Some(callable) = callable else {
                    // `CallScript` is a static script-function call and must
                    // never route a host-import prototype to the host path.
                    return Err(VmError::InvalidCallablePrototype(prototype_id));
                };
                let argc = operands.len() as u8;
                self.instance.stack.extend(operands);
                let call_ip = self.instance.ip.saturating_sub(2);
                match self.execute_host_call(import_index, argc, call_ip)? {
                    HostCallExecOutcome::Returned => Ok(ExecOutcome::Continue),
                    HostCallExecOutcome::Halted => Ok(ExecOutcome::Halted),
                    HostCallExecOutcome::Yielded => {
                        self.instance
                            .stack
                            .insert(operand_stack_base, Value::Callable(callable.clone()));
                        Ok(ExecOutcome::Yielded)
                    }
                    HostCallExecOutcome::Pending(op_id) => Ok(ExecOutcome::Waiting(op_id)),
                }
            }
        }
    }

    fn complete_active_frame(&mut self) -> VmResult<ExecOutcome> {
        let frame = self
            .instance
            .execution_frames
            .pop()
            .ok_or(VmError::InvalidFrameState("missing active frame"))?;
        self.instance.active_local_base_cache = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.local_base)
            .unwrap_or(0);
        self.instance.active_operand_stack_base_cache = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.operand_stack_base)
            .unwrap_or(0);
        if self.instance.stack.len() < frame.operand_stack_base {
            return Err(VmError::InvalidFrameState(
                "operand stack is below the active frame base",
            ));
        }
        if matches!(frame.continuation, FrameContinuation::Halt) {
            self.instance.call_depth = self.script_frame_depth();
            // Root Halt: the program finished; release every guest-owned
            // local of the root frame before the VM returns to the host (the
            // root locals stay in `instance.locals` for host inspection, but
            // their guest-owned resources die with the program).
            self.release_owned_locals_range(frame.local_base, frame.local_count);
            return Ok(ExecOutcome::Halted);
        }

        let result = if self.instance.stack.len() > frame.operand_stack_base {
            self.instance
                .stack
                .pop()
                .expect("stack length checked above")
        } else {
            Value::Null
        };
        while self.instance.stack.len() > frame.operand_stack_base {
            let value = self
                .instance
                .stack
                .pop()
                .expect("stack length checked above");
            self.drop_value_with_contract(value);
        }
        self.instance.call_depth = self.script_frame_depth();

        // Guest-owned release of every owned local in the exiting frame
        // (script function return / `ReturnToHost` completion). The release
        // runs BEFORE the locals are drained, so a `Pending` close stays in
        // the table's `Closing` state and the scope poll machinery drives it.
        self.release_owned_locals_range(frame.local_base, frame.local_count);

        if frame.prototype_id.is_some() {
            let frame_end = frame.local_base.saturating_add(frame.local_count);
            self.instance
                .capture_cells
                .retain(|absolute, _| *absolute < frame.local_base || *absolute >= frame_end);
            self.instance
                .shared_capture_slots
                .retain(|absolute| *absolute < frame.local_base || *absolute >= frame_end);
        }

        if !matches!(frame.continuation, FrameContinuation::Halt) {
            let frame_end = frame
                .local_base
                .checked_add(frame.local_count)
                .ok_or(VmError::InvalidFrameState("local frame range overflow"))?;
            if frame_end != self.instance.locals.len() {
                return Err(VmError::InvalidFrameState(
                    "active local frame does not end at the local stack tail",
                ));
            }
            let drained = self
                .instance
                .locals
                .drain(frame.local_base..)
                .collect::<Vec<_>>();
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
                self.instance.stack.push(result);
                Ok(ExecOutcome::Halted)
            }
            FrameContinuation::ResumeBytecode { return_ip } => {
                self.instance.ip = return_ip;
                self.instance.stack.push(result);
                Ok(ExecOutcome::Continue)
            }
            FrameContinuation::ReturnToHost => {
                self.instance.host_return = Some(result);
                Ok(ExecOutcome::Halted)
            }
        }
    }

    pub(super) fn can_fuse_call_ret_pattern(&self) -> bool {
        let code = &self.program.code;
        self.instance.ip < code.len() && code[self.instance.ip] == OpCode::Ret as u8
    }

    pub(super) fn clear_stack_with_drop_contract(&mut self) {
        let drained = self.instance.stack.drain(..).collect::<Vec<_>>();
        for value in drained {
            self.drop_value_with_contract(value);
        }
    }

    pub(super) fn clear_locals_with_drop_contract(&mut self) {
        for slot in 0..self.instance.locals.len() {
            let previous = std::mem::replace(&mut self.instance.locals[slot], Value::Null);
            self.drop_value_with_contract(previous);
        }
    }

    pub(super) fn drop_value_with_contract(&mut self, value: Value) {
        if self.instance.drop_contract_events_enabled {
            self.count_value_drop_contract(&value);
        }
    }

    // ---- guest-owned local release (C2-C1) ---------------------------------

    /// Whether this program has any resource-containing local slot. When true,
    /// the VM must never let a native backend bypass the interpreter's
    /// ownership release (Stloc overwrite / Drop / frame exit): JIT tracing
    /// and AOT native lowering are disabled for the whole run.
    pub(super) fn program_has_owned_locals(&self) -> bool {
        self.program.owned_local_slots().iter().any(|owned| *owned)
    }

    /// Releases every guest-owned resource reachable from one local slot by
    /// walking the slot's runtime `Value` against the program's exact local
    /// schema. Non-resource locals (and `schema:None` legacy programs) do
    /// nothing, exactly like the pre-ownership VM.
    ///
    /// - The walk is schema-driven: a handle is only released when the schema
    ///   says the current position contains a resource, so plain `Int`s are
    ///   never mistaken for handles and malformed runtime shapes are skipped.
    /// - Each handle is released at most once per walk (same-handle alias
    ///   dedup via a `HashSet`), and cycle/depth protection bounds recursion.
    /// - `Pending` closes are left in the table's `Closing` state; the scope
    ///   poll machinery drives them later. Synchronous close failures are
    ///   recorded in the scope's first-error latch (never panicked from a
    ///   frame unwind).
    fn release_owned_local(&mut self, local_base: usize, relative: usize) {
        let owned = self
            .program
            .owned_local_slots()
            .get(relative)
            .copied()
            .unwrap_or(false);
        if !owned {
            return;
        }
        let Some(absolute) = local_base.checked_add(relative) else {
            return;
        };
        let Some(schema) = self
            .program
            .type_map
            .as_ref()
            .and_then(|type_map| type_map.local_schemas.get(relative))
            .and_then(|schema| schema.as_ref())
            .cloned()
        else {
            return;
        };
        let value = self
            .instance
            .locals
            .get(absolute)
            .cloned()
            .unwrap_or(Value::Null);
        if self.host.execution_scope().resources().is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        let mut depth = 0usize;
        self.release_owned_value(&schema, &value, &mut seen, &mut depth);
    }

    /// Schema-driven recursive release walk over one runtime `Value`.
    fn release_owned_value(
        &mut self,
        schema: &crate::compiler::TypeSchema,
        value: &Value,
        seen: &mut HashSet<u64>,
        depth: &mut usize,
    ) {
        const MAX_RELEASE_DEPTH: usize = 256;
        if *depth >= MAX_RELEASE_DEPTH {
            return;
        }
        *depth += 1;
        self.release_owned_value_inner(schema, value, seen, depth);
        *depth -= 1;
    }

    fn release_owned_value_inner(
        &mut self,
        schema: &crate::compiler::TypeSchema,
        value: &Value,
        seen: &mut HashSet<u64>,
        depth: &mut usize,
    ) {
        use crate::compiler::TypeSchema;
        match schema {
            TypeSchema::Resource(_) => {
                let Ok(handle) = ResourceHandle::from_value(value) else {
                    return;
                };
                if !seen.insert(handle.raw()) {
                    return;
                }
                let release = OwnershipRelease::close();
                match self
                    .host
                    .execution_scope_release_guest_owner(handle, release)
                {
                    Ok(GuestReleaseOutcome::Released(_)) => {}
                    Ok(GuestReleaseOutcome::NotGuestOwned) => {}
                    Err(crate::vm::execution_scope::ExecutionScopeError::Resource(error)) => {
                        self.host.execution_scope_record_release_error(error);
                    }
                    Err(other) => {
                        // Defensive: a non-resource scope error during a
                        // release is treated as a recorded first-error so it
                        // is never silently dropped.
                        let error = ResourceError::new(
                            crate::vm::resource::ResourceErrorCode::ResourceCleanupFailed,
                            "vm::release_owned_local",
                            format!("guest ownership release failed: {other}"),
                        );
                        self.host.execution_scope_record_release_error(error);
                    }
                }
            }
            TypeSchema::Optional(inner) => {
                if !matches!(value, Value::Null) {
                    self.release_owned_value(inner, value, seen, depth);
                }
            }
            TypeSchema::Array(item) | TypeSchema::ArrayTupleRest { rest: item, .. } => {
                if let Value::Array(items) = value {
                    for item_value in items.iter() {
                        self.release_owned_value(item, item_value, seen, depth);
                    }
                }
            }
            TypeSchema::ArrayTuple(items) => {
                if let Value::Array(values) = value {
                    for (item_schema, item_value) in items.iter().zip(values.iter()) {
                        self.release_owned_value(item_schema, item_value, seen, depth);
                    }
                }
            }
            TypeSchema::Map(item) => {
                if let Value::Map(entries) = value {
                    for (_, map_value) in entries.iter() {
                        self.release_owned_value(item, map_value, seen, depth);
                    }
                }
            }
            TypeSchema::Object(fields) => {
                if let Value::Map(entries) = value {
                    for (key, map_value) in entries.iter() {
                        let Value::String(key) = key else {
                            continue;
                        };
                        if let Some(field_schema) = fields.get(key.as_str()) {
                            self.release_owned_value(field_schema, map_value, seen, depth);
                        }
                    }
                }
            }
            TypeSchema::Named(_, type_args) => {
                // Named (struct-like) schemas carry positional type args; the
                // runtime representation is a Map. Match by position against
                // the sorted field order when the map keys are strings.
                if let Value::Map(entries) = value {
                    let mut entries = entries
                        .iter()
                        .filter_map(|(key, map_value)| {
                            let Value::String(key) = key else {
                                return None;
                            };
                            Some((key.clone(), map_value))
                        })
                        .collect::<Vec<_>>();
                    entries.sort_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
                    for (arg_schema, (_, map_value)) in type_args.iter().zip(entries.iter()) {
                        self.release_owned_value(arg_schema, map_value, seen, depth);
                    }
                }
            }
            // Plain scalars, callables, and unknown/generic schemas never
            // release anything (a resource can only appear where the schema
            // names one).
            TypeSchema::Unknown
            | TypeSchema::Null
            | TypeSchema::Int
            | TypeSchema::Float
            | TypeSchema::Number
            | TypeSchema::Bool
            | TypeSchema::String
            | TypeSchema::Bytes
            | TypeSchema::GenericParam(_)
            | TypeSchema::Callable { .. } => {}
        }
    }

    /// Release walk over every owned local of one frame's slot range. Used by
    /// frame exit / root Halt / abort paths before the slots are drained.
    fn release_owned_locals_range(&mut self, local_base: usize, local_count: usize) {
        if !self.program_has_owned_locals() {
            return;
        }
        let owned = self.program.owned_local_slots().to_vec();
        for relative in 0..local_count {
            if owned.get(relative).copied().unwrap_or(false) {
                self.release_owned_local(local_base, relative);
            }
        }
    }

    pub(super) fn count_value_drop_contract(&mut self, value: &Value) {
        match value {
            Value::Null => {}
            Value::Array(values) => {
                self.instance.drop_contract_events =
                    self.instance.drop_contract_events.saturating_add(1);
                for item in values.iter() {
                    self.count_value_drop_contract(item);
                }
            }
            Value::Map(entries) => {
                self.instance.drop_contract_events =
                    self.instance.drop_contract_events.saturating_add(1);
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
                self.instance.drop_contract_events =
                    self.instance.drop_contract_events.saturating_add(1);
            }
        }
    }

    #[inline(always)]
    pub(in crate::vm) fn charge_interrupt_tick(&mut self) -> VmResult<()> {
        match self.run_ctx.interrupt_mode {
            InterruptMode::None => Ok(()),
            InterruptMode::Fuel => self.charge_fuel_tick(),
            InterruptMode::Epoch => self.charge_epoch_tick(),
        }
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub(in crate::vm) fn charge_aot_call_boundary_interrupt(&mut self) -> VmResult<()> {
        match self.run_ctx.interrupt_mode {
            InterruptMode::None => Ok(()),
            InterruptMode::Fuel => self.charge_fuel(1),
            InterruptMode::Epoch => {
                let current = self.current_epoch();
                if current >= self.run_ctx.epoch_deadline {
                    return Err(VmError::EpochDeadlineReached {
                        current,
                        deadline: self.run_ctx.epoch_deadline,
                    });
                }
                Ok(())
            }
        }
    }

    pub(super) fn peek_value(&self) -> VmResult<&Value> {
        self.instance.stack.last().ok_or(VmError::StackUnderflow)
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
        self.engine
            .operand_type_hints
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
        self.instance.local_type_hint_hit_count =
            self.instance.local_type_hint_hit_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_scalar_superinstruction(&mut self) {
        self.instance.scalar_superinstruction_count = self
            .instance
            .scalar_superinstruction_count
            .saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_typed_builtin_fast_path(&mut self) {
        self.instance.typed_builtin_fast_path_count = self
            .instance
            .typed_builtin_fast_path_count
            .saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_projection_fast_path(&mut self) {
        self.instance.projection_fast_path_count =
            self.instance.projection_fast_path_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn record_generic_builtin_call(&mut self) {
        self.instance.generic_builtin_call_count =
            self.instance.generic_builtin_call_count.saturating_add(1);
    }

    #[inline(always)]
    fn record_operand_hint_hit(&mut self) {
        self.instance.operand_hint_hit_count =
            self.instance.operand_hint_hit_count.saturating_add(1);
    }

    #[inline(always)]
    fn record_operand_hint_miss(&mut self) {
        self.instance.operand_hint_miss_count =
            self.instance.operand_hint_miss_count.saturating_add(1);
    }

    #[inline(always)]
    pub(super) fn unary_not_op(&mut self) -> VmResult<()> {
        let value = self.pop_bool()?;
        self.instance.stack.push(Value::Bool(!value));
        Ok(())
    }

    pub(super) fn int_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.instance.stack.push(Value::Int(lhs.wrapping_add(rhs)));
        Ok(())
    }

    pub(super) fn float_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.instance.stack.push(Value::Float(lhs + rhs));
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
        self.instance.stack.push(Value::string(out));
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
        self.instance.stack.push(Value::bytes(out));
        Ok(())
    }

    pub(super) fn int_binary_numeric_op(
        &mut self,
        op: impl FnOnce(i64, i64) -> VmResult<i64>,
    ) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.instance.stack.push(Value::Int(op(lhs, rhs)?));
        Ok(())
    }

    pub(super) fn float_binary_numeric_op(
        &mut self,
        op: impl FnOnce(f64, f64) -> VmResult<f64>,
    ) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.instance.stack.push(Value::Float(op(lhs, rhs)?));
        Ok(())
    }

    pub(super) fn int_neg_op(&mut self) -> VmResult<()> {
        let value = self.pop_int()?;
        self.instance.stack.push(Value::Int(value.wrapping_neg()));
        Ok(())
    }

    pub(super) fn float_neg_op(&mut self) -> VmResult<()> {
        let value = self.pop_float_exact()?;
        self.instance.stack.push(Value::Float(-value));
        Ok(())
    }

    pub(super) fn int_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.instance.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn float_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.instance.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn bool_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_bool()?;
        let lhs = self.pop_bool()?;
        self.instance.stack.push(Value::Bool(lhs == rhs));
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
        self.instance.stack.push(Value::Bool(lhs == rhs));
        Ok(())
    }

    pub(super) fn null_eq_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_value()?;
        let lhs = self.pop_value()?;
        match (lhs, rhs) {
            (Value::Null, Value::Null) => {
                self.instance.stack.push(Value::Bool(true));
                Ok(())
            }
            _ => Err(VmError::TypeMismatch("null")),
        }
    }

    pub(super) fn int_compare_op(&mut self, op: impl FnOnce(i64, i64) -> bool) -> VmResult<()> {
        let rhs = self.pop_int()?;
        let lhs = self.pop_int()?;
        self.instance.stack.push(Value::Bool(op(lhs, rhs)));
        Ok(())
    }

    pub(super) fn float_compare_op(&mut self, op: impl FnOnce(f64, f64) -> bool) -> VmResult<()> {
        let rhs = self.pop_float_exact()?;
        let lhs = self.pop_float_exact()?;
        self.instance.stack.push(Value::Bool(op(lhs, rhs)));
        Ok(())
    }

    pub(super) fn binary_add_op(&mut self) -> VmResult<()> {
        let rhs = self.pop_value()?;
        let lhs = self.pop_value()?;
        match (lhs, rhs) {
            (Value::Int(lhs), Value::Int(rhs)) => {
                self.instance.stack.push(Value::Int(lhs.wrapping_add(rhs)))
            }
            (Value::Int(lhs), Value::Float(rhs)) => {
                self.instance.stack.push(Value::Float(lhs as f64 + rhs))
            }
            (Value::Float(lhs), Value::Int(rhs)) => {
                self.instance.stack.push(Value::Float(lhs + rhs as f64))
            }
            (Value::Float(lhs), Value::Float(rhs)) => {
                self.instance.stack.push(Value::Float(lhs + rhs))
            }
            (Value::String(lhs), Value::String(rhs)) => {
                let mut out = String::with_capacity(lhs.len() + rhs.len());
                out.push_str(lhs.as_str());
                out.push_str(rhs.as_str());
                self.instance.stack.push(Value::string(out));
            }
            (Value::Bytes(lhs), Value::Bytes(rhs)) => {
                let mut out = crate::bytecode::unwrap_or_clone_shared(lhs);
                out.extend(crate::bytecode::unwrap_or_clone_shared(rhs));
                self.instance.stack.push(Value::bytes(out));
            }
            (Value::Array(lhs), Value::Array(rhs)) => {
                let mut out = crate::bytecode::unwrap_or_clone_shared(lhs);
                out.extend(crate::bytecode::unwrap_or_clone_shared(rhs));
                self.instance.stack.push(Value::array(out));
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
                self.instance.stack.push(Value::Int(int_op(lhs, rhs)?));
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
                self.instance.stack.push(Value::Float(float_op(lhs, rhs)?));
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
        self.instance.stack.push(Value::Bool(result));
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
        // Guest-owned release of the overwritten value (Stloc overwrite /
        // liveness-scheduled Drop both land here). The same-local collection
        // rebind (`files = push(files, r)` lowers to `ldc Null; stloc files;
        // call Set; stloc files`) temporarily nulls the slot while the
        // collection Arc is still live on the stack: the walker must skip
        // that null-store so the rebind never double-releases the handles
        // that stay inside the still-live collection.
        if matches!(value, Value::Null) && self.is_same_local_collection_rebind(absolute) {
            // The old value stays alive on the stack; no release.
        } else {
            let base = self.active_local_base();
            let relative = absolute.saturating_sub(base);
            self.release_owned_local(base, relative);
        }
        if self.instance.capture_cells.is_empty() {
            let slot = self
                .instance
                .locals
                .get_mut(absolute)
                .ok_or(VmError::InvalidLocal(index))?;
            let previous = std::mem::replace(slot, value);
            self.drop_value_with_contract(previous);
            return Ok(());
        }
        self.store_local_with_captures(absolute, index, value)
    }

    /// Detects the codegen same-local collection rebind pattern:
    /// `ldc Null; stloc S; call <Set|ArrayPush>; stloc S` — the bytecode at
    /// `self.instance.ip` is the `Call` and the `Stloc` that follows it
    /// (Call occupies `[opcode][u16 index][u8 argc]`, so the trailing Stloc
    /// is at `ip + 4`) retargets the same absolute slot. When true, the
    /// just-nulled slot's previous value is still the live container on the
    /// stack and must not be released.
    fn is_same_local_collection_rebind(&self, absolute: usize) -> bool {
        let code = &self.program.code;
        let Some(&call_opcode) = code.get(self.instance.ip) else {
            return false;
        };
        if call_opcode != OpCode::Call as u8 {
            return false;
        }
        let Some(index_bytes) = code.get(self.instance.ip + 1..self.instance.ip + 3) else {
            return false;
        };
        let call_index = u16::from_le_bytes([index_bytes[0], index_bytes[1]]);
        let is_collection_mutation = matches!(
            BuiltinFunction::from_call_index(call_index),
            Some(BuiltinFunction::Set | BuiltinFunction::ArrayPush)
        );
        if !is_collection_mutation {
            return false;
        }
        // The Call operand is (u16 index, u8 argc) — four bytes in total —
        // so the following Stloc's opcode sits at ip + 4 and its operand at
        // ip + 5; the operand must name the same absolute local.
        let Some(&stloc_opcode) = code.get(self.instance.ip + 4) else {
            return false;
        };
        if stloc_opcode != OpCode::Stloc as u8 {
            return false;
        }
        let Some(&target) = code.get(self.instance.ip + 5) else {
            return false;
        };
        let Some(base) = self.instance.execution_frames.last().map(|f| f.local_base) else {
            return false;
        };
        base + usize::from(target) == absolute
    }

    #[cold]
    #[inline(never)]
    fn store_local_with_captures(
        &mut self,
        absolute: usize,
        index: u8,
        value: Value,
    ) -> VmResult<()> {
        if let Some(cell) = self.instance.capture_cells.get(&absolute).cloned() {
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
            self.instance.locals[absolute] = value;
            self.drop_value_with_contract(previous);
            return Ok(());
        }
        let slot = self
            .instance
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
        self.instance.capture_cells.remove(&absolute);
        let slot = self
            .instance
            .locals
            .get_mut(absolute)
            .ok_or(VmError::InvalidLocal(index))?;
        let previous = std::mem::replace(slot, Value::Null);
        self.drop_value_with_contract(previous);
        Ok(())
    }

    pub(super) fn read_u8(&mut self) -> VmResult<u8> {
        if self.instance.ip >= self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        let value = self.program.code[self.instance.ip];
        self.instance.ip += 1;
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
        if self.instance.ip + count > self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        let mut buf = [0u8; 4];
        buf[..count]
            .copy_from_slice(&self.program.code[self.instance.ip..self.instance.ip + count]);
        self.instance.ip += count;
        Ok(buf)
    }

    pub(super) fn jump_to(&mut self, target: usize) -> VmResult<()> {
        if target >= self.program.code.len() {
            return Err(VmError::BytecodeBounds);
        }
        if !self.program.function_regions.is_empty() {
            let active_prototype = self
                .instance
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
        self.instance.ip = target;
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
            ExecOutcome::Halted | ExecOutcome::Waiting(_) => self.instance.last_yield_reason = None,
            ExecOutcome::Yielded => {
                if self.instance.last_yield_reason.is_none() {
                    self.instance.last_yield_reason = Some(VmYieldReason::Host);
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
            if self.instance.ip >= self.program.code.len() {
                return Err(VmError::BytecodeBounds);
            }
            let opcode = self.read_u8()?;
            let outcome = self.execute_interpreter_instruction(opcode, true)?;
            match outcome {
                ExecOutcome::Continue => {}
                ExecOutcome::Halted => {
                    self.instance.last_yield_reason = None;
                    return Ok(Some(VmStatus::Halted));
                }
                ExecOutcome::Yielded => {
                    if self.instance.last_yield_reason.is_none() {
                        self.instance.last_yield_reason = Some(VmYieldReason::Host);
                    }
                    return Ok(Some(VmStatus::Yielded));
                }
                ExecOutcome::Waiting(op_id) => {
                    self.instance.last_yield_reason = None;
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
        if let Some(waiting) = self.instance.waiting_host_op.clone() {
            self.instance.last_yield_reason = None;
            let status = VmStatus::Waiting(waiting.op_id);
            self.notify_debugger_status(&mut debugger, status);
            return Ok(status);
        }
        self.instance.last_yield_reason = None;
        if self.run_ctx.epoch_rearm_pending {
            self.rearm_epoch_after_yield_if_needed();
        }
        if debugger.is_none()
            && !self.interruption_enabled()
            && (!allow_jit
                || (!self.jit_config().enabled
                    && (!self.has_aot_program() || self.engine.aot_interpreter_boundary_hit)))
            && let Some(status) = self.run_fast_interpreter(allow_jit)?
        {
            return Ok(status);
        }

        loop {
            if self.run_ctx.epoch_rearm_pending {
                self.rearm_epoch_after_yield_if_needed();
            }
            if let Some(active_debugger) = debugger.as_deref_mut() {
                active_debugger.on_instruction(self);
            }

            if allow_jit
                && self.has_aot_program()
                && !self.engine.aot_interpreter_boundary_hit
                && !self.drop_contract_events_enabled()
                && !self.program_has_owned_locals()
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

            if self.engine.aot_interpreter_boundary_hit
                && debugger.is_none()
                && !self.interruption_enabled()
                && !self.jit_config().enabled
                && let Some(status) = self.run_fast_interpreter(false)?
            {
                return Ok(status);
            }

            if allow_jit
                && self.jit_config().enabled
                && self.host.allow_default_builtin_capabilities
                && self.host.allow_default_host_capabilities
                && self.host.builtin_overrides.is_empty()
                && !self.drop_contract_events_enabled()
                && !self.program_has_owned_locals()
                && !self.active_frame_has_shared_capture_cells()
            {
                let frame_key = self.active_frame_key();
                let trace_id = if self.engine.jit.callable_frame_is_blocked(frame_key) {
                    None
                } else {
                    let stack_depth = self.active_operand_stack_len();
                    let entry_local_types = (frame_key != crate::vm::native::ROOT_FRAME_KEY)
                        .then(|| self.active_local_types());
                    let entry_callable_prototypes = self.active_local_callable_prototypes();
                    let program = &self.program;
                    self.engine.jit.observe_hot_entry_with_local_types(
                        frame_key,
                        self.instance.ip,
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

            if self.instance.ip >= self.program.code.len() {
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
                let opcode_ip = self.instance.ip - 1;
                let value = if let Some(value) = self.decoded_ldc_value_at(opcode_ip).cloned() {
                    self.instance.ip += 4;
                    value
                } else {
                    let index = self.read_u32()?;
                    self.program
                        .constants
                        .get(index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidConstant(index))?
                };
                self.instance.stack.push(value);
            }
            x if x == OpCode::Add as u8 => {
                let ip = self.instance.ip - 1;
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
                let ip = self.instance.ip - 1;
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
                let ip = self.instance.ip - 1;
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
                let ip = self.instance.ip - 1;
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
                self.instance.stack.push(Value::Int(lhs.wrapping_shl(rhs)));
            }
            x if x == OpCode::Shr as u8 => {
                let rhs = self.pop_shift_amount()?;
                let lhs = self.pop_int()?;
                self.instance.stack.push(Value::Int(lhs.wrapping_shr(rhs)));
            }
            x if x == OpCode::Lshr as u8 => {
                let rhs = self.pop_shift_amount()?;
                let lhs = self.pop_int()?;
                self.instance
                    .stack
                    .push(Value::Int(logical_shr_i64(lhs, rhs)));
            }
            x if x == OpCode::Mod as u8 => {
                let ip = self.instance.ip - 1;
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
                self.instance.stack.push(Value::Bool(lhs && rhs));
            }
            x if x == OpCode::Or as u8 => {
                let rhs = self.pop_bool()?;
                let lhs = self.pop_bool()?;
                self.instance.stack.push(Value::Bool(lhs || rhs));
            }
            x if x == OpCode::Not as u8 => self.unary_not_op()?,
            x if x == OpCode::Neg as u8 => {
                let ip = self.instance.ip - 1;
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
                                self.instance.stack.push(Value::Int(value.wrapping_neg()))
                            }
                            NumericValue::Float(value) => {
                                self.instance.stack.push(Value::Float(-value))
                            }
                        }
                    }
                }
            }
            x if x == OpCode::Ceq as u8 => {
                let ip = self.instance.ip - 1;
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
                        self.instance.stack.push(Value::Bool(lhs == rhs));
                    }
                }
            }
            x if x == OpCode::Clt as u8 => {
                let ip = self.instance.ip - 1;
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
                let ip = self.instance.ip - 1;
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
                let opcode_ip = self.instance.ip - 1;
                let target = if let Some(target) = self.decoded_jump_target_at(opcode_ip) {
                    self.instance.ip += 4;
                    target
                } else {
                    self.read_u32()? as usize
                };
                if self.decoded_jump_target_is_valid_at(opcode_ip) {
                    self.instance.ip = target;
                } else {
                    self.jump_to(target)?;
                }
            }
            x if x == OpCode::Brfalse as u8 => {
                let opcode_ip = self.instance.ip - 1;
                let target = if let Some(target) = self.decoded_jump_target_at(opcode_ip) {
                    self.instance.ip += 4;
                    target
                } else {
                    self.read_u32()? as usize
                };
                let condition = self.pop_bool()?;
                if !condition {
                    if self.decoded_jump_target_is_valid_at(opcode_ip) {
                        self.instance.ip = target;
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
                self.instance.stack.push(value);
            }
            x if x == OpCode::Ldloc as u8 => {
                let opcode_ip = self.instance.ip - 1;
                let index = if let Some(index) = self.decoded_local_index_at(opcode_ip) {
                    self.instance.ip += 1;
                    index
                } else {
                    self.read_u8()?
                };
                if self.try_fuse_scalar_sequence(index, allow_superinstructions)? {
                    return Ok(ExecOutcome::Continue);
                }
                let value = self.load_local_value(index)?;
                self.instance.stack.push(value);
            }
            x if x == OpCode::Stloc as u8 => {
                let opcode_ip = self.instance.ip - 1;
                let index = if let Some(index) = self.decoded_local_index_at(opcode_ip) {
                    self.instance.ip += 1;
                    index
                } else {
                    self.read_u8()?
                };
                let value = self.pop_value()?;
                self.store_local_with_drop_contract(index, value)?;
            }
            x if x == OpCode::Call as u8 => {
                let call_ip = self.instance.ip - 1;
                let index = self.read_u16()?;
                let argc_u8 = self.read_u8()?;
                let can_fuse_tail_halt = self.can_fuse_call_ret_pattern();
                match self.execute_host_call(index, argc_u8, call_ip)? {
                    HostCallExecOutcome::Returned => {
                        if can_fuse_tail_halt {
                            if self.interruption_enabled() {
                                self.charge_interrupt_tick()?;
                            }
                            self.instance.ip = self.instance.ip.saturating_add(1);
                            return self.complete_active_frame();
                        }
                    }
                    HostCallExecOutcome::Halted => return Ok(ExecOutcome::Halted),
                    HostCallExecOutcome::Yielded => {
                        self.instance.last_yield_reason = Some(VmYieldReason::Host);
                        return Ok(ExecOutcome::Yielded);
                    }
                    HostCallExecOutcome::Pending(op_id) => return Ok(ExecOutcome::Waiting(op_id)),
                }
            }

            x if x == OpCode::CallValue as u8 => {
                let call_ip = self.instance.ip.saturating_sub(1);
                let argc = self.read_u8()?;
                return self.execute_call_value(argc, Some(call_ip));
            }
            x if x == OpCode::CallScript as u8 => {
                let call_ip = self.instance.ip.saturating_sub(1);
                let prototype_id = self.read_u32()?;
                let argc = self.read_u8()?;
                return self.execute_call_script(prototype_id, argc, call_ip);
            }
            other => return Err(VmError::InvalidOpcode(other)),
        }
        Ok(ExecOutcome::Continue)
    }

    pub fn resume(&mut self) -> VmResult<VmStatus> {
        self.ensure_executable("resume")?;
        let allow_jit = !matches!(
            self.instance
                .execution_frames
                .last()
                .map(|frame| &frame.continuation),
            Some(FrameContinuation::ReturnToHost)
        );
        let status = match self.run_internal(None, allow_jit) {
            Ok(status) => status,
            Err(error) => {
                self.abort_callable_stream_on_run_error(&error)?;
                return Err(error);
            }
        };
        self.resume_callable_stream_after_run(status)
    }

    pub fn stack(&self) -> &[Value] {
        &self.instance.stack
    }

    pub fn locals(&self) -> &[Value] {
        &self.instance.locals
    }

    pub fn set_local(&mut self, index: u8, value: Value) -> VmResult<()> {
        self.store_local_with_drop_contract(index, value)?;
        let config = *self.engine.jit.config();
        self.set_jit_config(config);
        Ok(())
    }

    pub fn program(&self) -> &Program {
        self.program.as_ref()
    }

    pub fn bound_function_count(&self) -> usize {
        self.host.host_functions.len()
    }

    pub fn has_bound_function(&self, name: &str) -> bool {
        self.host.host_function_symbols.contains_key(name)
    }

    pub fn ip(&self) -> usize {
        self.instance.ip
    }

    pub(super) fn owns_callable(&self, value: &Value) -> bool {
        let Value::Callable(target) = value else {
            return false;
        };
        self.instance.owned_callables.iter().any(|owned| {
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
            .instance
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
        self.instance.call_depth
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
        if self.instance.shutdown {
            return Err(VmError::InvalidFrameState("vm is shut down"));
        }
        if !matches!(&callable, Value::Callable(_)) {
            return Err(VmError::InvalidCallable);
        }
        if !self.owns_callable(&callable) {
            return Err(VmError::InvalidFrameState(
                "callable does not belong to this vm",
            ));
        }
        self.instance.queued_callables.push_back(QueuedCallable {
            callable,
            args,
            subscription,
        });
        Ok(())
    }

    pub fn queued_callable_count(&self) -> usize {
        self.instance.queued_callables.len()
    }

    pub fn drain_callable_queue(&mut self) -> VmResult<Vec<Value>> {
        if self.instance.draining_queued_callables {
            return Err(VmError::InvalidFrameState(
                "callable queue is already being drained",
            ));
        }
        if !self.instance.execution_frames.is_empty() {
            return Err(VmError::InvalidFrameState(
                "queued callables can only run after the root frame halts",
            ));
        }
        self.instance.draining_queued_callables = true;
        let mut results = Vec::with_capacity(self.instance.queued_callables.len());
        while let Some(queued) = self.instance.queued_callables.pop_front() {
            if queued
                .subscription
                .as_ref()
                .is_some_and(|active| !active.load(Ordering::Acquire))
            {
                continue;
            }
            match self.start_callable(queued.callable, &queued.args) {
                Ok(VmStatus::Halted) => {
                    let Some(result) = self.instance.host_return.take() else {
                        self.instance.completed_callable_results.extend(results);
                        self.instance.draining_queued_callables = false;
                        return Err(VmError::InvalidFrameState(
                            "queued invocation completed without a result",
                        ));
                    };
                    results.push(result);
                }
                Ok(VmStatus::Yielded) => {
                    self.instance.completed_callable_results.extend(results);
                    self.instance.draining_queued_callables = false;
                    return Err(VmError::InvalidFrameState(
                        "queued invocation yielded; resume it before draining again",
                    ));
                }
                Ok(VmStatus::Waiting(_)) => {
                    self.instance.completed_callable_results.extend(results);
                    self.instance.draining_queued_callables = false;
                    return Err(VmError::InvalidFrameState(
                        "queued invocation is waiting; resume it before draining again",
                    ));
                }
                Err(err) => {
                    self.instance.completed_callable_results.extend(results);
                    self.instance.draining_queued_callables = false;
                    return Err(err);
                }
            }
        }
        self.instance.draining_queued_callables = false;
        Ok(results)
    }

    pub fn shutdown(&mut self) {
        self.invalidate_callback_registries();
        let _ = self.cancel_waiting_host_op();
        let _ = self.cancel_callable_stream(
            crate::builtins::runtime::cancellation::CancellationReason::Requested,
        );
        self.instance.queued_callables.clear();
        self.instance.completed_callable_results.clear();
        self.instance.owned_callables.clear();
        self.instance.draining_queued_callables = false;
        self.clear_stack_with_drop_contract();
        self.instance.capture_cells.clear();
        self.instance.shared_capture_slots.clear();
        // Guest-owned release of every owned local before the interpreter
        // values are dropped (scope shutdown falls back to closing anything
        // still guest-owned; releasing here launches each close exactly once
        // with the ownership-release reason).
        let base = self.active_local_base();
        let count = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.local_count)
            .unwrap_or(self.program.local_count);
        self.release_owned_locals_range(base, count);
        self.clear_locals_with_drop_contract();
        self.instance.execution_frames.clear();
        self.instance.active_local_base_cache = 0;
        self.instance.active_operand_stack_base_cache = 0;
        self.instance.call_depth = 0;
        self.instance.host_return = None;
        self.instance.waiting_host_op = None;
        crate::builtins::runtime::close_all_handles(self);
        self.instance.shutdown = true;
    }

    pub(super) fn register_callback_registry(&mut self, active: &Arc<AtomicBool>) {
        self.instance.register_callback_registry(active);
    }

    fn invalidate_callback_registries(&mut self) {
        self.instance.invalidate_callback_registries();
    }

    pub fn start_callable(&mut self, callable: Value, args: &[Value]) -> VmResult<VmStatus> {
        self.ensure_executable("start_callable")?;
        if self.instance.shutdown {
            return Err(VmError::InvalidFrameState("vm is shut down"));
        }
        if !matches!(&callable, Value::Callable(_)) {
            return Err(VmError::InvalidCallable);
        }
        if !self.owns_callable(&callable) {
            return Err(VmError::InvalidFrameState(
                "callable does not belong to this vm",
            ));
        }
        if !self.instance.execution_frames.is_empty() {
            return Err(VmError::InvalidFrameState(
                "host invocation requires a halted VM",
            ));
        }
        let argc = u8::try_from(args.len())
            .map_err(|_| VmError::InvalidFrameState("too many arguments"))?;
        let stack_base = self.instance.stack.len();
        let frame_count = self.instance.execution_frames.len();
        self.instance.stack.push(callable);
        self.instance.stack.extend_from_slice(args);
        self.instance.host_return = None;
        let outcome = match self.execute_call_value(argc, None) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.abort_host_invocation(stack_base, frame_count);
                return Err(error);
            }
        };
        if self.instance.execution_frames.len() == frame_count {
            let result = match outcome {
                ExecOutcome::Continue | ExecOutcome::Halted => {
                    self.instance.stack.pop().unwrap_or(Value::Null)
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
            self.instance.stack.truncate(stack_base);
            self.instance.host_return = Some(result);
            return Ok(VmStatus::Halted);
        }
        if let Some(frame) = self.instance.execution_frames.last_mut() {
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
        let stack_base = self.instance.stack.len();
        let frame_count = self.instance.execution_frames.len();
        match self.start_callable(callable, args)? {
            VmStatus::Halted => self
                .instance
                .host_return
                .take()
                .ok_or(VmError::InvalidFrameState(
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
        while self.instance.execution_frames.len() > frame_count {
            let Some(frame) = self.instance.execution_frames.pop() else {
                break;
            };
            let frame_end = frame.local_base.saturating_add(frame.local_count);
            // Guest-owned release of every owned local in the aborted frame
            // before its slots are drained.
            self.release_owned_locals_range(frame.local_base, frame.local_count);
            self.instance
                .capture_cells
                .retain(|absolute, _| *absolute < frame.local_base || *absolute >= frame_end);
            self.instance
                .shared_capture_slots
                .retain(|absolute| *absolute < frame.local_base || *absolute >= frame_end);
            if frame.local_base <= self.instance.locals.len() {
                let drained = self
                    .instance
                    .locals
                    .drain(frame.local_base..)
                    .collect::<Vec<_>>();
                for value in drained {
                    self.drop_value_with_contract(value);
                }
            }
        }
        self.instance.active_local_base_cache = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.local_base)
            .unwrap_or(0);
        self.instance.active_operand_stack_base_cache = self
            .instance
            .execution_frames
            .last()
            .map(|frame| frame.operand_stack_base)
            .unwrap_or(0);
        while self.instance.stack.len() > stack_base {
            if let Some(value) = self.instance.stack.pop() {
                self.drop_value_with_contract(value);
            }
        }
        self.instance.call_depth = self.script_frame_depth();
        self.instance.host_return = None;
        let _ = self.cancel_waiting_host_op();
        self.instance.last_yield_reason = None;
        self.instance
            .map_iterators
            .truncate(self.instance.call_depth.saturating_add(1));
    }

    pub fn take_callable_result(&mut self) -> Option<Value> {
        self.instance
            .completed_callable_results
            .pop_front()
            .or_else(|| self.instance.host_return.take())
    }

    pub fn execution_frames(&self) -> Vec<VmExecutionFrameSnapshot> {
        self.instance
            .execution_frames
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
