//! Standard timer host-function module.
//!
//! This module owns the complete generic `timer::*` contract. It transfers
//! callbacks through the function registry's owned-value dispatch, creates a
//! fresh callback VM, and hands a self-contained registration to an
//! embedding-supplied backend. The VM core has no timer state or policy.

use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use crate::host_api::{
    HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
};
use crate::{
    CallOutcome, CallReturn, HostFunctionRegistry, HostOwnedFunction, OwnedHostCall,
    OwnedHostContext, Value, Vm, VmError, VmResult, VmStatus,
};

/// Default maximum number of registered callbacks waiting to begin.
pub const DEFAULT_MAX_PENDING_TIMERS: usize = 1024;
/// Default maximum number of callbacks executing (including Waiting).
pub const DEFAULT_MAX_RUNNING_TIMERS: usize = 256;

/// Admission limits for one installed timer runtime.
///
/// The generic module validates the call itself and forwards these limits with
/// every [`TimerRegistration`]; it keeps no admission state or counters of its
/// own. The installed [`TimerBackend`] is the sole admission authority and
/// **must** enforce both limits, checked atomically with its own
/// insertion/scheduling (no inspect-then-insert race). `timer::pending_count`
/// and `timer::running_count` report that backend's counts and never a
/// module-local approximation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerConfig {
    pub max_pending: usize,
    pub max_running: usize,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            max_pending: DEFAULT_MAX_PENDING_TIMERS,
            max_running: DEFAULT_MAX_RUNNING_TIMERS,
        }
    }
}

/// Backend contract for deadline scheduling and lifecycle accounting.
///
/// A backend owns every accepted registration. Deadline waits may happen on a
/// worker executor, but callback bytecode must be driven by the embedding's
/// designated VM thread through [`OwnedTimerCallback`]. `max_running` is a
/// runtime-wide cap: a callback in `Waiting` counts as running, and excess due
/// callbacks remain pending until a running slot is released.
///
/// The backend is the **sole admission authority and accountant**: the generic
/// module never tracks pending/running counts itself, never admits a
/// registration on the backend's behalf, and never substitutes an estimate for
/// [`Self::pending_count`] / [`Self::running_count`]. The limits carried by
/// each [`TimerRegistration`] are MUST-enforce obligations, checked atomically
/// with the backend's own insertion/scheduling so they behave as hard caps.
pub trait TimerBackend: Send + Sync + 'static {
    /// Accepts one complete registration. Returning success transfers the task
    /// to the backend and lets the script call return immediately.
    fn register(&self, registration: TimerRegistration) -> VmResult<()>;
    /// Registered callbacks waiting to begin (or waiting for a running slot).
    fn pending_count(&self) -> usize;
    /// Live callback executions, including callbacks paused on async host work.
    fn running_count(&self) -> usize;
    /// Reports a callback execution error without unwinding the source VM.
    fn report_callback_error(&self, error: TimerCallbackError);
    /// Stops acceptance, runs pending callbacks once with `premature=true`,
    /// cancels active async operations, and releases callback VMs. Implementations
    /// must make this operation idempotent.
    fn shutdown(&self) -> VmResult<()>;
}

/// One backend-owned registration.
pub struct TimerRegistration {
    pub delay: Duration,
    pub interval: Option<Duration>,
    /// Maximum number of registrations admitted while pending. The backend
    /// **must** enforce this cap as part of its own admission: the check and
    /// the insertion happen under the same synchronization, so the limit is a
    /// hard cap and never a post-hoc observation.
    pub max_pending: usize,
    /// Runtime-wide concurrent callback cap. The backend **must** enforce it
    /// when a due callback starts; a `Waiting` callback counts against it and
    /// excess due callbacks remain pending. The generic module never replaces
    /// the backend's counters with an estimate.
    pub max_running: usize,
    pub callback: OwnedTimerCallback,
}

impl std::fmt::Debug for TimerRegistration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TimerRegistration")
            .field("delay", &self.delay)
            .field("interval", &self.interval)
            .field("max_pending", &self.max_pending)
            .field("max_running", &self.max_running)
            .field("callback", &self.callback)
            .finish()
    }
}

/// Current state of an owned callback VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerCallbackState {
    Ready,
    Running,
    Waiting(u64),
    Yielded,
    Complete,
    Cancelled,
}

/// Public callback-driving status name retained by the runtime API.
pub type TimerCallbackStatus = TimerCallbackState;

/// A callback failure reported by an embedding-owned scheduler.
#[derive(Debug)]
pub struct TimerCallbackError {
    pub error: VmError,
}

impl From<VmError> for TimerCallbackError {
    fn from(error: VmError) -> Self {
        Self { error }
    }
}

/// A transferred callback and its private execution VM.
///
/// The callable value is retained across `every` rounds, so its shared mutable
/// capture cells persist. Each invocation is serialized: [`start`](Self::start)
/// rejects while a prior invocation is Running, Waiting, or Yielded.
pub struct OwnedTimerCallback {
    vm: Vm,
    callable: Value,
    state: TimerCallbackState,
}

impl std::fmt::Debug for OwnedTimerCallback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnedTimerCallback")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl OwnedTimerCallback {
    fn new(vm: Vm, callable: Value) -> Self {
        Self {
            vm,
            callable,
            state: TimerCallbackState::Ready,
        }
    }

    pub fn state(&self) -> TimerCallbackState {
        self.state
    }

    pub fn is_live(&self) -> bool {
        matches!(
            self.state,
            TimerCallbackState::Running
                | TimerCallbackState::Waiting(_)
                | TimerCallbackState::Yielded
        )
    }

    /// Installs the async bridge used by this callback's private VM.
    ///
    /// Every accepted callback owns its own VM, so whenever the callback body
    /// can enter an async host operation the backend must install a fresh
    /// bridge for it **before** the first [`Self::start`] call. A bridge is
    /// never shared with the creating request VM or with another callback.
    /// Once the callback reports [`TimerCallbackState::Waiting`], the backend
    /// drives it through [`Self::poll_reporting`].
    pub fn set_async_bridge(&mut self, bridge: Box<dyn crate::HostAsyncBridge>) -> VmResult<()> {
        self.vm.set_async_bridge(bridge)
    }

    /// Starts a serialized callback round with the OpenResty-compatible
    /// `premature` argument. The callback return value is discarded.
    pub fn start(&mut self, premature: bool) -> VmResult<TimerCallbackState> {
        if self.is_live() {
            return Err(VmError::HostError(
                "timer callback invocation is already live".to_string(),
            ));
        }
        if matches!(self.state, TimerCallbackState::Cancelled) {
            return Err(VmError::HostError(
                "timer callback was cancelled".to_string(),
            ));
        }
        self.state = TimerCallbackState::Running;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.vm
                .start_callable(self.callable.clone(), &[Value::Bool(premature)])
        }));
        match result {
            Ok(Ok(status)) => self.finish_status(status),
            Ok(Err(error)) => self.fail_and_recover(error),
            Err(payload) => self.fail_and_recover(callback_panic_error("start", payload)),
        }
    }

    /// Starts a round and routes any callback failure to the backend's error
    /// sink. The callback source VM remains unaffected.
    pub fn start_reporting(
        &mut self,
        premature: bool,
        backend: &dyn TimerBackend,
    ) -> TimerCallbackState {
        match self.start(premature) {
            Ok(state) => state,
            Err(error) => {
                backend.report_callback_error(error.into());
                self.state
            }
        }
    }

    /// Polls an outstanding async host operation once. If it becomes ready,
    /// resumes callback bytecode until the next Halted/Yielded/Waiting state.
    /// Any returned error has already recovered this callback VM for reuse.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<VmResult<TimerCallbackState>> {
        match self.state {
            TimerCallbackState::Waiting(_) => {
                let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.vm.poll_waiting_host_op(cx)
                }));
                match polled {
                    Err(payload) => Poll::Ready(
                        self.fail_and_recover(callback_panic_error("waiting poll", payload)),
                    ),
                    Ok(Poll::Pending) => Poll::Pending,
                    Ok(Poll::Ready(Err(error))) => Poll::Ready(self.fail_and_recover(error)),
                    Ok(Poll::Ready(Ok(()))) => self.resume_after_poll(),
                }
            }
            TimerCallbackState::Yielded | TimerCallbackState::Running => self.resume_after_poll(),
            state => Poll::Ready(Ok(state)),
        }
    }

    /// Polls one callback step and reports asynchronous callback failures to
    /// the backend. Backends should use this method when they want polling,
    /// callback panic isolation, and error reporting in one operation.
    pub fn poll_reporting(
        &mut self,
        cx: &mut Context<'_>,
        backend: &dyn TimerBackend,
    ) -> Poll<TimerCallbackState> {
        match self.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(state)) => Poll::Ready(state),
            Poll::Ready(Err(error)) => {
                backend.report_callback_error(error.into());
                Poll::Ready(self.state)
            }
        }
    }

    fn resume_after_poll(&mut self) -> Poll<VmResult<TimerCallbackState>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.vm.resume()));
        match result {
            Ok(Ok(status)) => Poll::Ready(self.finish_status(status)),
            Ok(Err(error)) => Poll::Ready(self.fail_and_recover(error)),
            Err(payload) => {
                Poll::Ready(self.fail_and_recover(callback_panic_error("resume", payload)))
            }
        }
    }

    fn fail_and_recover(&mut self, error: VmError) -> VmResult<TimerCallbackState> {
        self.state = TimerCallbackState::Complete;
        let recovery = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.vm.recover_owned_callable(&self.callable)
        }));
        match recovery {
            Ok(Ok(())) => Err(error),
            Ok(Err(recovery_error)) => Err(VmError::HostError(format!(
                "{error}; callback recovery failed: {recovery_error}"
            ))),
            Err(payload) => Err(VmError::HostError(format!(
                "{error}; callback recovery panicked: {}",
                panic_payload_message(&payload)
            ))),
        }
    }

    /// Cancels an active/waiting callback and releases its private VM state.
    ///
    /// The private VM is shut down through the generic VM lifecycle: a waiting
    /// host operation is cancelled through its bridge exactly once and every
    /// run-scoped resource is released. A cancelled callback can never restart,
    /// so the released VM is never executed again.
    pub fn cancel(&mut self) -> VmResult<()> {
        if matches!(self.state, TimerCallbackState::Cancelled) {
            return Ok(());
        }
        self.vm.shutdown();
        self.state = TimerCallbackState::Cancelled;
        Ok(())
    }

    fn finish_status(&mut self, status: VmStatus) -> VmResult<TimerCallbackState> {
        self.state = match status {
            VmStatus::Halted => {
                // Discard the callback result by contract.
                let _ = self.vm.take_callable_result();
                TimerCallbackState::Complete
            }
            VmStatus::Yielded => TimerCallbackState::Yielded,
            VmStatus::Waiting(op_id) => TimerCallbackState::Waiting(op_id),
        };
        Ok(self.state)
    }
}

fn callback_panic_error(stage: &str, payload: Box<dyn std::any::Any + Send>) -> VmError {
    VmError::HostError(format!(
        "timer callback {stage} panicked: {}",
        panic_payload_message(&payload)
    ))
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

impl Drop for OwnedTimerCallback {
    fn drop(&mut self) {
        if !matches!(self.state, TimerCallbackState::Cancelled) {
            self.vm.shutdown();
        }
    }
}

enum TimerBackendHandle {
    Owner(Arc<dyn TimerBackend>),
    Callback(std::sync::Weak<dyn TimerBackend>),
}

pub struct TimerHostState {
    backend: TimerBackendHandle,
    config: TimerConfig,
}

impl TimerHostState {
    fn owner(backend: Arc<dyn TimerBackend>, config: TimerConfig) -> Self {
        Self {
            backend: TimerBackendHandle::Owner(backend),
            config,
        }
    }

    fn callback(backend: &Arc<dyn TimerBackend>, config: TimerConfig) -> Self {
        Self {
            backend: TimerBackendHandle::Callback(Arc::downgrade(backend)),
            config,
        }
    }

    fn backend(&self) -> VmResult<Arc<dyn TimerBackend>> {
        match &self.backend {
            TimerBackendHandle::Owner(backend) => Ok(Arc::clone(backend)),
            TimerBackendHandle::Callback(backend) => backend
                .upgrade()
                .ok_or_else(|| VmError::HostError("timer runtime has shut down".to_string())),
        }
    }
}

/// VM extension helpers for installing/removing one timer runtime.
pub trait TimerHostExt {
    fn install_timer_runtime(&mut self, backend: Arc<dyn TimerBackend>, config: TimerConfig);
    fn clear_timer_runtime(&mut self);
}

impl TimerHostExt for Vm {
    fn install_timer_runtime(&mut self, backend: Arc<dyn TimerBackend>, config: TimerConfig) {
        self.host_context()
            .set_module_state(TimerHostState::owner(backend, config));
    }

    fn clear_timer_runtime(&mut self) {
        let _ = self.host_context().take_module_state::<TimerHostState>();
    }
}

/// Standard extension that registers timer functions and installs a backend.
pub struct TimerExtension {
    backend: Arc<dyn TimerBackend>,
    config: TimerConfig,
}

impl TimerExtension {
    pub fn new(backend: Arc<dyn TimerBackend>, config: TimerConfig) -> Self {
        Self { backend, config }
    }
}

impl crate::vm::HostExtension for TimerExtension {
    fn catalog(&self) -> Option<&HostApiCatalog> {
        Some(
            TIMER_HOST_CATALOG
                .get_or_init(|| {
                    super::host_modules::module_catalog("timer", TIMER_CATALOG_FUNCTIONS, &[], &[])
                })
                .as_ref(),
        )
    }

    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        register_timer_builtin_module(registry)
    }

    fn install(&self, vm: &mut Vm) {
        vm.install_timer_runtime(Arc::clone(&self.backend), self.config);
    }
}

static TIMER_HOST_CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();

/// The typed callback surface: the callback observes the `premature` flag and
/// its return value is discarded by the module, so the declared result is
/// `unknown`: an `int`/`string`/`map`/`bool`/`null` return all compile and
/// execute, and the value is dropped instead of accumulating. This is the one
/// deliberate dynamic occurrence in the public timer surface; the typed-catalog
/// guard records it as its narrow discarded-callable-result policy exception.
fn timer_callback_schema() -> HostTypeSchema {
    HostTypeSchema::Callable {
        params: vec![HostTypeSchema::Bool],
        result: Box::new(HostTypeSchema::Unknown),
    }
}

/// Guest contract for `timer::at` / `timer::every`.
///
/// The timer functions are not `#[pd_host_function]` declarations: they need
/// the exact owned dispatch (`register_exact_owned`) rather than a borrowed
/// adapter, so the module declares explicit descriptors. The descriptor still
/// carries the schema, binding class, adapter factory, and effects, so there is
/// no parallel catalog or registry glue.
fn timer_register_contract(name: &'static str, delay_name: &'static str) -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        name,
        vec![
            HostParamSchema::value(delay_name, HostTypeSchema::Int),
            HostParamSchema::with_passing(
                "callback",
                timer_callback_schema(),
                HostParamPassing::TakeOwned,
            ),
        ],
        HostTypeSchema::Bool,
    )
    .with_description("Registers an owned timer callback")
}

/// The owned-dispatch factory for one timer registration function.
struct RegisterTimerFactory {
    repeating: bool,
}

impl crate::host_extension::HostOwnedAdapterFactory for RegisterTimerFactory {
    fn create(&self, context: OwnedHostContext<'_>) -> Box<dyn HostOwnedFunction> {
        Box::new(RegisterTimer {
            registry: context.registry().clone(),
            repeating: self.repeating,
        })
    }
}

static TIMER_AT_FACTORY: RegisterTimerFactory = RegisterTimerFactory { repeating: false };
static TIMER_EVERY_FACTORY: RegisterTimerFactory = RegisterTimerFactory { repeating: true };

fn timer_at_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    crate::host_extension::HostFunctionDescriptor {
        schema: timer_register_contract("timer::at", "delay_ms"),
        binding: crate::host_extension::HostBindingDescriptor {
            kind: crate::host_extension::HostBindingKind::Owned,
        },
        effects: Vec::new(),
        adapter: crate::host_extension::HostAdapterDescriptor::Owned(&TIMER_AT_FACTORY),
        resource_types: Vec::new(),
    }
}

fn timer_every_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    crate::host_extension::HostFunctionDescriptor {
        schema: timer_register_contract("timer::every", "interval_ms"),
        binding: crate::host_extension::HostBindingDescriptor {
            kind: crate::host_extension::HostBindingKind::Owned,
        },
        effects: Vec::new(),
        adapter: crate::host_extension::HostAdapterDescriptor::Owned(&TIMER_EVERY_FACTORY),
        resource_types: Vec::new(),
    }
}

fn timer_count_descriptor(
    name: &'static str,
    adapter: crate::vm::StaticHostStackFunction,
) -> crate::host_extension::HostFunctionDescriptor {
    crate::host_extension::HostFunctionDescriptor {
        schema: HostFunctionSchema::with_return(name, Vec::new(), HostTypeSchema::Int),
        binding: crate::host_extension::HostBindingDescriptor {
            kind: crate::host_extension::HostBindingKind::StaticStack,
        },
        effects: Vec::new(),
        adapter: crate::host_extension::HostAdapterDescriptor::StaticStack(adapter),
        resource_types: Vec::new(),
    }
}

fn timer_pending_count_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    timer_count_descriptor("timer::pending_count", pending_count)
}

fn timer_running_count_descriptor() -> crate::host_extension::HostFunctionDescriptor {
    timer_count_descriptor("timer::running_count", running_count)
}

/// Exact catalog functions for `timer::{at,every,pending_count,running_count}`.
const TIMER_CATALOG_FUNCTIONS: &[fn() -> crate::host_extension::HostFunctionDescriptor] = &[
    timer_at_descriptor,
    timer_every_descriptor,
    timer_pending_count_descriptor,
    timer_running_count_descriptor,
];

fn timer_catalog_module() -> crate::host_extension::HostModuleDescriptor {
    super::host_modules::catalog_module("timer", TIMER_CATALOG_FUNCTIONS, &[])
}

/// The standard `timer` host module: every timer function is owned here and the
/// whole surface is published.
pub(super) fn timer_host_module() -> super::host_modules::StandardHostModule {
    use super::host_modules::StandardHostModule;

    StandardHostModule {
        name: "timer",
        catalog: timer_catalog_module,
        owned: TIMER_CATALOG_FUNCTIONS,
        named_structs: &[],
    }
}

/// Exact catalog for `timer::{at,every,pending_count,running_count}`, derived
/// from the module descriptors.
pub fn timer_host_catalog() -> Arc<HostApiCatalog> {
    Arc::clone(TIMER_HOST_CATALOG.get_or_init(|| {
        super::host_modules::module_catalog("timer", TIMER_CATALOG_FUNCTIONS, &[], &[])
    }))
}

struct RegisterTimer {
    registry: HostFunctionRegistry,
    repeating: bool,
}

impl HostOwnedFunction for RegisterTimer {
    fn call(&mut self, call: &mut OwnedHostCall<'_>) -> VmResult<CallOutcome> {
        let delay_ms = call
            .arg(0)
            .ok_or(VmError::TypeMismatch("timer delay"))?
            .as_int()?;
        if delay_ms < 0 || (self.repeating && delay_ms == 0) {
            let requirement = if self.repeating {
                "positive"
            } else {
                "non-negative"
            };
            return Err(VmError::HostError(format!(
                "{} expects a {requirement} millisecond duration, got {delay_ms}",
                if self.repeating {
                    "timer::every"
                } else {
                    "timer::at"
                }
            )));
        }
        let delay = Duration::from_millis(delay_ms as u64);
        register_owned_timer(
            call,
            &self.registry,
            TIMER_CALLBACK_ARG,
            delay,
            self.repeating.then_some(delay),
        )
    }
}

/// The call argument that carries the transferred callback in every timer
/// adapter signature (`timer::{at,every}` and downstream milliseconds- or
/// seconds-based siblings).
pub const TIMER_CALLBACK_ARG: usize = 1;

/// Counts read synchronously from the installed timer runtime.
///
/// These are the *embedding's* own counters: the generic module keeps none of
/// its own and never substitutes an estimate for the installed backend's
/// [`TimerBackend::pending_count`] / [`TimerBackend::running_count`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerCounts {
    /// Registrations waiting to begin (or waiting for a running slot).
    pub pending: usize,
    /// Live callback executions, including callbacks paused on async host work.
    pub running: usize,
}

/// Reads the installed backend's synchronous pending/running counts.
///
/// This is the domain-free count surface a thin adapter calls: it resolves the
/// installed [`TimerHostState`] through the VM's module state and returns the
/// backend's own counters, so a downstream adapter (for example a seconds-based
/// `ngx.timer`-shaped host) can implement its count functions without naming
/// `TimerBackend`, `TimerHostState`, or any backend internals.
///
/// Fails with `"timer runtime is not installed"` when no timer runtime state is
/// installed on `vm` — the same error the `timer::*` count builtins report.
pub fn installed_timer_counts(vm: &mut Vm) -> VmResult<TimerCounts> {
    let backend = {
        let context = vm.host_context();
        let state = context
            .module_state::<TimerHostState>()
            .ok_or_else(|| VmError::HostError("timer runtime is not installed".to_string()))?;
        state.backend()?
    };
    Ok(TimerCounts {
        pending: backend.pending_count(),
        running: backend.running_count(),
    })
}

/// Registers one owned timer callback through the shared backend path.
///
/// This is the adapter seam for embeddings that expose the standard timer
/// contract under their *own* exact host name and schema (for example an
/// `ngx.timer`-shaped host whose deadline argument is seconds and whose
/// callback checks its own request provenance): the adapter validates its own
/// arguments, converts them to a checked [`Duration`], and hands the owned call
/// plus the callback argument index here. Everything after that — the installed
/// runtime lookup, the callback argument's type and program provenance, the
/// fresh isolated callback VM, the admission limits travelling with the
/// registration, backend registration, and the transactional rollback of the
/// callback argument on every failure path (returned error *and* panic) — is
/// shared with `timer::at` / `timer::every`.
///
/// * `callback_arg` indexes the callable argument inside `call`. The argument is
///   taken here; on any failure it is restored to the call, so the dispatch
///   returns it to the guest exactly once.
/// * `delay` is the first deadline; `interval` marks a repeating registration
///   and must be positive (a zero repeating interval is rejected).
/// * Registration uses the installed [`TimerConfig`] limits, which travel with
///   every [`TimerRegistration`] as MUST-enforce backend obligations.
///
/// The helper is name-independent and unit-independent: it never inspects the
/// call's host name and never converts units. A milliseconds adapter
/// (`timer::at` / `timer::every`) and a seconds adapter therefore share one
/// implementation of the owned handoff, admission, and rollback contract.
///
/// [`OwnedTimerCallback`] values are only ever created here, so an embedding can
/// never bypass callback provenance or VM isolation by constructing one.
pub fn register_owned_timer(
    call: &mut OwnedHostCall<'_>,
    registry: &HostFunctionRegistry,
    callback_arg: usize,
    delay: Duration,
    interval: Option<Duration>,
) -> VmResult<CallOutcome> {
    if interval.is_some_and(|interval| interval.is_zero()) {
        return Err(VmError::HostError(
            "timer interval must be positive".to_string(),
        ));
    }
    let (backend, config) = {
        let context = call.vm().host_context();
        let state = context
            .module_state::<TimerHostState>()
            .ok_or_else(|| VmError::HostError("timer runtime is not installed".to_string()))?;
        (state.backend()?, state.config)
    };
    let callback_state = TimerHostState::callback(&backend, config);
    let rollback_callback = call
        .arg(callback_arg)
        .cloned()
        .ok_or_else(|| VmError::HostError("timer callback argument is missing".to_string()))?;
    let callback = call.take_arg(callback_arg)?;
    if !matches!(callback, Value::Callable(_)) {
        call.restore_arg(callback_arg, rollback_callback)?;
        return Err(VmError::TypeMismatch("callable"));
    }
    let spawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call.spawn_owned_callable_vm(registry, &callback, move |vm| {
            vm.host_context().set_module_state(callback_state);
            Ok(())
        })
    }));
    let vm = match spawn {
        Ok(Ok(vm)) => vm,
        Ok(Err(error)) => {
            call.restore_arg(callback_arg, rollback_callback)?;
            return Err(error);
        }
        Err(payload) => {
            call.restore_arg(callback_arg, rollback_callback)?;
            std::panic::resume_unwind(payload)
        }
    };
    let registration = TimerRegistration {
        delay,
        interval,
        max_pending: config.max_pending,
        max_running: config.max_running,
        callback: OwnedTimerCallback::new(vm, callback),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        backend.register(registration)
    }));
    match result {
        Ok(Ok(())) => Ok(CallOutcome::Return(CallReturn::one(Value::Bool(true)))),
        Ok(Err(error)) => {
            call.restore_arg(callback_arg, rollback_callback)?;
            Err(error)
        }
        Err(payload) => {
            call.restore_arg(callback_arg, rollback_callback)?;
            std::panic::resume_unwind(payload)
        }
    }
}

fn pending_count(vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    timer_count(vm, true)
}

fn running_count(vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    timer_count(vm, false)
}

/// Returns one installed backend count as an `int`, through the same
/// synchronous count surface downstream adapters use.
fn timer_count(vm: &mut Vm, pending: bool) -> VmResult<CallOutcome> {
    let counts = installed_timer_counts(vm)?;
    let count = if pending {
        counts.pending
    } else {
        counts.running
    };
    let count = i64::try_from(count)
        .map_err(|_| VmError::HostError("timer count exceeds int range".to_string()))?;
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(count))))
}

/// Registers all timer functions against the authoritative standard catalog.
pub fn register_timer_builtin_module(registry: &mut HostFunctionRegistry) -> VmResult<()> {
    let catalog = crate::builtins::runtime::standard_host_catalog();
    register_timer_builtin_module_from_catalog(registry, &catalog)
}

/// Registers all timer functions against a caller-provided catalog snapshot.
///
/// `timer::at` / `timer::every` keep their exact owned dispatch: the installed
/// adapter is the module's owned-descriptor factory, so the callback operand is
/// drained, transferred, and driven by the same isolated owned-value execution
/// VM as before, and the callback provenance guarantees are unchanged. The
/// count functions keep their synchronous stack dispatch.
pub fn register_timer_builtin_module_from_catalog(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> VmResult<()> {
    timer_host_module()
        .catalog_module()
        .expect("the timer module publishes a catalog surface")
        .install_from_catalog(registry, catalog)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::task::Waker;

    use super::*;
    use crate::bytecode::{
        CallableKind, CallablePrototype, CallableTarget, CallableValue, HostImport, Program,
        RootCallableBinding, ScriptFunction, ValueType,
    };
    use crate::host_api::{HostParamPassing, HostTypeSchema};
    use crate::vm::{HostImportParam, HostImportSchema};
    use crate::{BytecodeBuilder, HostAsyncBridge, HostFunction, HostOpId};

    struct NoopWake;

    impl std::task::Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[derive(Default)]
    struct RecordingBackend {
        registrations: Mutex<usize>,
        errors: Mutex<Vec<String>>,
    }

    impl TimerBackend for RecordingBackend {
        fn register(&self, _registration: TimerRegistration) -> VmResult<()> {
            *self.registrations.lock().expect("registrations") += 1;
            Ok(())
        }

        fn pending_count(&self) -> usize {
            *self.registrations.lock().expect("registrations")
        }

        fn running_count(&self) -> usize {
            0
        }

        fn report_callback_error(&self, error: TimerCallbackError) {
            self.errors
                .lock()
                .expect("recording backend errors")
                .push(error.error.to_string());
        }

        fn shutdown(&self) -> VmResult<()> {
            Ok(())
        }
    }

    fn noop_host(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::none()))
    }

    /// Program whose root halts immediately and binds one callable at local 0;
    /// the callable body calls the `test::noop` host three times.
    fn immediate_callback_program() -> Program {
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.call(0, 0);
        code.call(0, 0);
        code.call(0, 0);
        code.ret();
        let mut program = Program::new(Vec::new(), code.finish());
        program.local_count = 1;
        program.imports = vec![HostImport {
            name: "test::noop".to_string(),
            arity: 0,
            return_type: ValueType::Unknown,
        }];
        program.callable_prototypes = vec![CallablePrototype {
            kind: CallableKind::Closure,
            target: CallableTarget::ScriptFunction(0),
            arity: 1,
            frame_local_count: 1,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];
        program
    }

    fn make_immediate_callback() -> OwnedTimerCallback {
        let program = immediate_callback_program();
        let mut vm = Vm::try_new(program).expect("vm");
        let mut registry = HostFunctionRegistry::empty();
        registry.register_static("test::noop", 0, noop_host);
        registry.bind_vm_cached(&mut vm).expect("bind noop host");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        OwnedTimerCallback::new(vm, callable)
    }

    /// The callback state machine reaches `Yielded` deterministically through
    /// fuel exhaustion, blocks an overlapping round, and resumes to `Complete`
    /// on the *same* private VM.
    #[test]
    fn yielded_round_blocks_overlap_then_resumes_to_complete() {
        let mut callback = make_immediate_callback();
        callback.vm.set_fuel(1);
        assert_eq!(
            callback.start(false).expect("first round yields"),
            TimerCallbackState::Yielded
        );
        assert!(callback.is_live(), "a yielded round is still live");
        let overlap = callback
            .start(false)
            .expect_err("an overlapping round must not start while yielded");
        assert!(
            overlap.to_string().contains("already live"),
            "unexpected error: {overlap}"
        );

        callback.vm.add_fuel(16).expect("recharge fuel");
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            callback.poll(&mut cx),
            Poll::Ready(Ok(TimerCallbackState::Complete))
        ));
        assert!(!callback.is_live(), "a completed round is not live");
        assert_eq!(
            callback.start(false).expect("later round completes"),
            TimerCallbackState::Complete
        );
    }

    /// A call without an installed runtime reports the documented installation
    /// error instead of silently dropping the callback, and the callback stays
    /// with the guest.
    #[test]
    fn adapter_helper_reports_a_missing_runtime_and_restores_the_callback() {
        let mut vm = Vm::try_new(Program::new(Vec::new(), vec![])).expect("vm");
        let registry = HostFunctionRegistry::empty();
        let callable = Value::string("not-this-call's-callback");
        let mut call = OwnedHostCall::new(&mut vm, vec![Value::Int(1), callable.clone()]);
        let error = register_owned_timer(
            &mut call,
            &registry,
            TIMER_CALLBACK_ARG,
            Duration::from_secs(1),
            None,
        )
        .expect_err("missing runtime state must fail");
        assert!(
            error.to_string().contains("timer runtime is not installed"),
            "unexpected error: {error}"
        );
        assert!(
            !call.is_taken(TIMER_CALLBACK_ARG),
            "a rejected registration keeps the callback guest-owned"
        );
        assert_eq!(call.arg(TIMER_CALLBACK_ARG), Some(&callable));
    }

    /// The helper never constructs a callback from a value the source VM does
    /// not own: a foreign callable is a structured rejection and the argument
    /// returns to the guest.
    #[test]
    fn adapter_helper_rejects_a_foreign_callback_and_restores_it() {
        let backend: Arc<dyn TimerBackend> = Arc::new(RecordingBackend::default());
        let mut vm = Vm::try_new(Program::new(Vec::new(), vec![])).expect("vm");
        vm.install_timer_runtime(Arc::clone(&backend), TimerConfig::default());
        let registry = HostFunctionRegistry::empty();
        let foreign = Value::Callable(Arc::new(CallableValue {
            prototype_id: 0,
            kind: CallableKind::Closure,
            env: None,
        }));
        let mut call = OwnedHostCall::new(&mut vm, vec![Value::Int(1), foreign.clone()]);
        let error = register_owned_timer(
            &mut call,
            &registry,
            TIMER_CALLBACK_ARG,
            Duration::from_secs(1),
            None,
        )
        .expect_err("a foreign callable must be rejected");
        assert!(
            matches!(&error, VmError::InvalidFrameState(message) if message.contains("source vm")),
            "unexpected error: {error}"
        );
        assert_eq!(
            call.arg(TIMER_CALLBACK_ARG),
            Some(&foreign),
            "the rejected callback argument returns to the guest exactly once"
        );
        assert_eq!(
            backend.pending_count(),
            0,
            "a rejected callback never reaches the backend"
        );
    }

    /// A zero repeating interval is rejected before any ownership handoff.
    #[test]
    fn adapter_helper_rejects_a_zero_repeating_interval() {
        let mut vm = Vm::try_new(Program::new(Vec::new(), vec![])).expect("vm");
        let registry = HostFunctionRegistry::empty();
        let mut call = OwnedHostCall::new(&mut vm, vec![Value::Int(1), Value::Int(0)]);
        let error = register_owned_timer(
            &mut call,
            &registry,
            TIMER_CALLBACK_ARG,
            Duration::from_secs(1),
            Some(Duration::ZERO),
        )
        .expect_err("a zero interval must be rejected");
        assert!(
            error.to_string().contains("interval must be positive"),
            "unexpected error: {error}"
        );
        assert!(!call.is_taken(TIMER_CALLBACK_ARG));
    }

    /// `timer::{at,every}` and a downstream seconds adapter share the same
    /// registration path: the standard module's own schema is expressible and
    /// its callbacks are only ever created through the helper.
    #[test]
    fn standard_module_registers_owned_callbacks_from_the_shared_catalog() {
        let backend: Arc<dyn TimerBackend> = Arc::new(RecordingBackend::default());
        let mut vm = Vm::try_new(Program::new(Vec::new(), vec![])).expect("vm");
        vm.install_timer_runtime(Arc::clone(&backend), TimerConfig::default());
        let mut registry = HostFunctionRegistry::empty();
        register_timer_builtin_module(&mut registry).expect("timer registration");
        let schema = HostImportSchema {
            name: "timer::at".to_string(),
            params: vec![
                HostImportParam {
                    name: "delay_ms".to_string(),
                    schema: HostTypeSchema::Int,
                    passing: HostParamPassing::Value,
                },
                HostImportParam {
                    name: "callback".to_string(),
                    schema: HostTypeSchema::Callable {
                        params: vec![HostTypeSchema::Bool],
                        result: Box::new(HostTypeSchema::Unknown),
                    },
                    passing: HostParamPassing::TakeOwned,
                },
            ],
            return_type: HostTypeSchema::Bool,
            fingerprint: timer_host_catalog().fingerprint(),
        };
        let duplicate = registry
            .register_exact_owned("timer::at", 2, schema, |_context| {
                Box::new(RegisterTimer {
                    registry: HostFunctionRegistry::empty(),
                    repeating: false,
                })
            })
            .expect_err("re-registering the standard adapter is a conflict");
        assert!(matches!(duplicate, VmError::HostError(_)), "{duplicate}");
    }

    /// Submits a host future that never completes, so the callback rests in
    /// the `Waiting` state until its bridge is driven.
    fn pending_wait(vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        vm.submit_host_future(Box::pin(async {
            std::future::pending::<VmResult<crate::HostFutureOutput>>().await
        }))
    }

    // ---- callback driving: waiting rounds, bridges, panics, recovery ----

    /// Bridge stub that accepts futures and never completes them.
    struct PendingBridge;

    impl HostAsyncBridge for PendingBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: crate::HostFuture) -> VmResult<()> {
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut Context<'_>,
        ) -> Poll<VmResult<CallReturn>> {
            Poll::Pending
        }
    }

    /// Bridge stub driven by a test-owned state: it completes, fails, or
    /// panics on demand, and counts acknowledged cancellations.
    #[derive(Default)]
    struct ControlledBridgeState {
        complete: bool,
        poll_error: bool,
        panic_poll: bool,
        cancellations: usize,
    }

    struct ControlledBridge {
        state: Arc<std::sync::Mutex<ControlledBridgeState>>,
    }

    impl HostAsyncBridge for ControlledBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: crate::HostFuture) -> VmResult<()> {
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut Context<'_>,
        ) -> Poll<VmResult<CallReturn>> {
            let state = self.state.lock().expect("bridge state");
            assert!(!state.panic_poll, "registered bridge poll panic");
            if state.poll_error {
                Poll::Ready(Err(VmError::HostError("bridge poll failed".to_string())))
            } else if state.complete {
                Poll::Ready(Ok(CallReturn::none()))
            } else {
                Poll::Pending
            }
        }

        fn cancel_op(&mut self, _op_id: HostOpId) {
            self.state.lock().expect("bridge state").cancellations += 1;
        }
    }

    /// A host function that panics on its first call and succeeds afterwards.
    struct PanicOnceHost {
        should_panic: Arc<std::sync::atomic::AtomicBool>,
    }

    impl HostFunction for PanicOnceHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            if self
                .should_panic
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                panic!("registered callback panic");
            }
            Ok(CallOutcome::Return(CallReturn::none()))
        }
    }

    /// A host function that returns `0` on its first call and `1` afterwards,
    /// so a resumed division by zero fails once and succeeds later.
    struct ResumeDenominatorHost {
        first: Arc<std::sync::atomic::AtomicBool>,
    }

    impl HostFunction for ResumeDenominatorHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            let denominator = if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                0
            } else {
                1
            };
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(
                denominator,
            ))))
        }
    }

    /// The shared import schema of every helper callback program below.
    fn helper_import(name: &str, return_type: ValueType) -> HostImport {
        HostImport {
            name: name.to_string(),
            arity: 0,
            return_type,
        }
    }

    fn helper_prototype() -> CallablePrototype {
        CallablePrototype {
            kind: CallableKind::Closure,
            target: CallableTarget::ScriptFunction(0),
            arity: 1,
            frame_local_count: 1,
            parameter_slots: vec![0],
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        }
    }

    /// One-callable program that waits on `test::wait` and then panics once
    /// through `test::panic_once`, so a later round runs cleanly.
    fn make_panic_once_callback() -> OwnedTimerCallback {
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.call(0, 0);
        code.ret();
        let mut program = Program::new(Vec::new(), code.finish());
        program.local_count = 1;
        program.imports = vec![helper_import("test::panic_once", ValueType::Unknown)];
        program.callable_prototypes = vec![helper_prototype()];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];

        let mut vm = Vm::try_new(program).expect("vm");
        let should_panic = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let host_state = Arc::clone(&should_panic);
        let mut registry = HostFunctionRegistry::empty();
        registry.register("test::panic_once", 0, move || -> Box<dyn HostFunction> {
            Box::new(PanicOnceHost {
                should_panic: Arc::clone(&host_state),
            })
        });
        registry.bind_vm_cached(&mut vm).expect("bind panic host");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        OwnedTimerCallback::new(vm, callable)
    }

    /// Program whose callback body returns `Int(7)`; the module discards the
    /// value, so no result and no stack operand may accumulate across rounds.
    fn make_int_result_callback() -> OwnedTimerCallback {
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.ldc(0);
        code.ret();
        let mut program = Program::new(vec![Value::Int(7)], code.finish());
        program.local_count = 1;
        program.callable_prototypes = vec![helper_prototype()];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];

        let mut vm = Vm::try_new(program).expect("vm");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        OwnedTimerCallback::new(vm, callable)
    }

    /// Program that waits on `test::wait` and then divides by a denominator
    /// that is `0` for the first resumed round and `1` afterwards, so the
    /// resumed round fails once and a later round succeeds.
    fn make_resume_error_once_callback() -> OwnedTimerCallback {
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.call(0, 0);
        code.ldc(0);
        code.call(1, 0);
        code.div();
        code.ret();
        let mut program = Program::new(vec![Value::Int(1)], code.finish());
        program.local_count = 1;
        program.imports = vec![
            helper_import("test::wait", ValueType::Unknown),
            helper_import("test::denominator", ValueType::Int),
        ];
        program.callable_prototypes = vec![helper_prototype()];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];

        let mut vm = Vm::try_new(program).expect("vm");
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let denominator_state = Arc::clone(&first);
        let mut registry = HostFunctionRegistry::empty();
        registry.register_static("test::wait", 0, pending_wait);
        registry.register("test::denominator", 0, move || -> Box<dyn HostFunction> {
            Box::new(ResumeDenominatorHost {
                first: Arc::clone(&denominator_state),
            })
        });
        registry.bind_vm_cached(&mut vm).expect("bind resume hosts");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        OwnedTimerCallback::new(vm, callable)
    }

    /// Program that waits on `test::wait` and then panics once on the resumed
    /// round through `test::panic_once`.
    fn make_resume_panic_once_callback() -> OwnedTimerCallback {
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.call(0, 0);
        code.call(1, 0);
        code.ret();
        let mut program = Program::new(Vec::new(), code.finish());
        program.local_count = 1;
        program.imports = vec![
            helper_import("test::wait", ValueType::Unknown),
            helper_import("test::panic_once", ValueType::Unknown),
        ];
        program.callable_prototypes = vec![helper_prototype()];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];

        let mut vm = Vm::try_new(program).expect("vm");
        let should_panic = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let host_state = Arc::clone(&should_panic);
        let mut registry = HostFunctionRegistry::empty();
        registry.register_static("test::wait", 0, pending_wait);
        registry.register("test::panic_once", 0, move || -> Box<dyn HostFunction> {
            Box::new(PanicOnceHost {
                should_panic: Arc::clone(&host_state),
            })
        });
        registry
            .bind_vm_cached(&mut vm)
            .expect("bind resume panic hosts");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        OwnedTimerCallback::new(vm, callable)
    }

    /// Callback that waits on an unbridged pending future, optionally failing
    /// with a division by zero after it is resumed.
    fn make_waiting_callback() -> OwnedTimerCallback {
        make_callback(false)
    }

    fn make_resumed_error_callback() -> OwnedTimerCallback {
        make_callback(true)
    }

    fn make_callback(resume_error: bool) -> OwnedTimerCallback {
        let constants = if resume_error {
            vec![Value::Int(1), Value::Int(0)]
        } else {
            Vec::new()
        };
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.call(0, 0);
        if resume_error {
            code.ldc(0);
            code.ldc(1);
            code.div();
        }
        code.ret();
        let mut program = Program::new(constants, code.finish());
        program.local_count = 1;
        program.imports = vec![helper_import("test::wait", ValueType::Unknown)];
        program.callable_prototypes = vec![helper_prototype()];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];

        let mut vm = Vm::try_new(program).expect("vm");
        let mut registry = HostFunctionRegistry::empty();
        registry.register_static("test::wait", 0, |vm, _args| {
            vm.submit_host_future(Box::pin(async {
                std::future::pending::<VmResult<crate::HostFutureOutput>>().await
            }))
        });
        registry.bind_vm_cached(&mut vm).expect("bind wait host");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        OwnedTimerCallback::new(vm, callable)
    }

    /// Each waiting callback owns its own bridge: completing one round must
    /// leave the other callback's operation pending.
    #[test]
    fn callback_bridges_are_independent_across_waiting_timers() {
        let first_state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let second_state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut first = make_waiting_callback();
        let mut second = make_waiting_callback();
        first
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&first_state),
            }))
            .expect("install first callback bridge");
        second
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&second_state),
            }))
            .expect("install second callback bridge");

        assert!(matches!(
            first.start(false).expect("first callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        assert!(matches!(
            second.start(false).expect("second callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(first.poll(&mut cx), Poll::Pending));
        assert!(matches!(second.poll(&mut cx), Poll::Pending));

        first_state.lock().expect("first bridge state").complete = true;
        assert!(matches!(
            first.poll(&mut cx),
            Poll::Ready(Ok(TimerCallbackStatus::Complete))
        ));
        assert!(matches!(second.poll(&mut cx), Poll::Pending));

        second_state.lock().expect("second bridge state").complete = true;
        assert!(matches!(
            second.poll(&mut cx),
            Poll::Ready(Ok(TimerCallbackStatus::Complete))
        ));
    }

    /// Cancelling a waiting callback cancels its bridge exactly once; a
    /// repeated cancellation is idempotent.
    #[test]
    fn cancelling_waiting_callback_cancels_its_bridge_once() {
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_waiting_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));

        callback.cancel().expect("cancel waiting callback");
        callback
            .cancel()
            .expect("repeated cancellation is idempotent");
        assert_eq!(callback.state(), TimerCallbackStatus::Cancelled);
        assert_eq!(state.lock().expect("bridge state").cancellations, 1);
    }

    /// A callback that fails while resuming after a completed bridge poll is
    /// recovered to `Complete` before the failure is returned, so a later
    /// round runs on the same private VM.
    #[test]
    fn resumed_async_error_completes_callback_before_reporting() {
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_resumed_error_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").complete = true;

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let result = callback.poll(&mut cx);
        assert!(matches!(result, Poll::Ready(Err(_))));
        assert_eq!(callback.state(), TimerCallbackStatus::Complete);
    }

    /// A bridge poll error completes the callback round before the failure is
    /// returned to the caller.
    #[test]
    fn poll_error_completes_callback_before_returning_failure() {
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_waiting_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").poll_error = true;

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(callback.poll(&mut cx), Poll::Ready(Err(_))));
        assert_eq!(callback.state(), TimerCallbackStatus::Complete);
    }

    /// A start failure (no installed bridge) leaves the callback schedulable:
    /// every later round may report the error, and installing a bridge makes
    /// the next round succeed.
    #[test]
    fn start_error_leaves_callback_schedulable_for_a_later_round() {
        let mut callback = make_waiting_callback();
        assert!(
            callback.start(false).is_err(),
            "missing bridge must fail start"
        );
        assert_eq!(callback.state(), TimerCallbackStatus::Complete);
        assert!(
            callback.start(false).is_err(),
            "each later round may report the error"
        );
        assert_eq!(callback.state(), TimerCallbackStatus::Complete);

        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install bridge after start errors");
        assert!(matches!(
            callback.start(false).expect("recovered round waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").complete = true;
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            callback.poll(&mut cx),
            Poll::Ready(Ok(TimerCallbackStatus::Complete))
        ));
    }

    /// A terminal poll error is reported exactly once, and the recovered
    /// callback runs a later round successfully.
    #[test]
    fn poll_error_recovers_callback_for_a_successful_later_round() {
        let backend = RecordingBackend::default();
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_waiting_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").poll_error = true;

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(
            backend.errors.lock().expect("errors").len(),
            1,
            "a terminal poll error is reported once"
        );

        {
            let mut bridge_state = state.lock().expect("bridge state");
            bridge_state.poll_error = false;
            bridge_state.complete = true;
        }
        assert!(matches!(
            callback.start(false).expect("later round waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
    }

    /// A bridge poll panic is isolated, recovered, and reported once; the
    /// recovered callback is not re-reported.
    #[test]
    fn waiting_poll_panic_recovers_callback_and_reports_once() {
        let backend = RecordingBackend::default();
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_waiting_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").panic_poll = true;

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
        assert!(
            backend.errors.lock().expect("errors")[0].contains("panicked"),
            "a poll panic must be reported through the backend sink"
        );

        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
    }

    /// A callback panic is isolated and reported once; a later round runs on
    /// the same private VM without duplicating the report.
    #[test]
    fn callback_panic_is_reported_once_and_later_round_runs() {
        let backend = RecordingBackend::default();
        let mut callback = make_panic_once_callback();

        assert_eq!(
            callback.start_reporting(false, &backend),
            TimerCallbackStatus::Complete
        );
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
        assert!(backend.errors.lock().expect("errors")[0].contains("panicked"));

        assert_eq!(
            callback.start_reporting(false, &backend),
            TimerCallbackStatus::Complete
        );
        assert_eq!(
            backend.errors.lock().expect("errors").len(),
            1,
            "a successful later round must not duplicate the prior report"
        );
    }

    /// A failure while resuming is recovered for a later round: the first
    /// resumed round fails once, the next round runs cleanly, and the failure
    /// is reported exactly once.
    #[test]
    fn resume_error_recovers_callback_for_a_successful_later_round() {
        let backend = RecordingBackend::default();
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_resume_error_once_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").complete = true;

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);

        state.lock().expect("bridge state").complete = true;
        assert!(matches!(
            callback.start(false).expect("later round waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
    }

    /// A panic while resuming is isolated and recovered; the later round runs
    /// cleanly and the panic is reported once.
    #[test]
    fn resume_panic_recovers_callback_for_a_successful_later_round() {
        let backend = RecordingBackend::default();
        let state = Arc::new(std::sync::Mutex::new(ControlledBridgeState::default()));
        let mut callback = make_resume_panic_once_callback();
        callback
            .set_async_bridge(Box::new(ControlledBridge {
                state: Arc::clone(&state),
            }))
            .expect("install callback bridge");
        assert!(matches!(
            callback.start(false).expect("callback waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        state.lock().expect("bridge state").complete = true;

        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
        assert!(backend.errors.lock().expect("errors")[0].contains("panicked"));

        state.lock().expect("bridge state").complete = true;
        assert!(matches!(
            callback.start(false).expect("later round waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        assert!(matches!(
            callback.poll_reporting(&mut cx, &backend),
            Poll::Ready(TimerCallbackStatus::Complete)
        ));
        assert_eq!(backend.errors.lock().expect("errors").len(), 1);
    }

    /// The callback return value is discarded: nothing accumulates on the
    /// private VM's operand stack and no callable result is retained, across
    /// repeated rounds.
    #[test]
    fn discarded_callback_return_does_not_accumulate_results_or_stack() {
        let mut callback = make_int_result_callback();
        for _ in 0..3 {
            assert_eq!(
                callback.start(false).expect("callback completes"),
                TimerCallbackState::Complete
            );
            assert!(callback.vm.stack().is_empty());
            assert_eq!(callback.vm.take_callable_result(), None);
        }
    }

    /// A waiting callback rejects an overlapping round: the live round must be
    /// cancelled before another interval round can start.
    #[test]
    fn waiting_callback_rejects_overlapping_interval_round() {
        let mut code = BytecodeBuilder::new();
        code.ret();
        code.call(0, 0);
        code.ret();
        let mut program = Program::new(Vec::new(), code.finish());
        program.local_count = 1;
        program.imports = vec![helper_import("test::wait", ValueType::Unknown)];
        program.callable_prototypes = vec![helper_prototype()];
        program.script_functions = vec![ScriptFunction {
            entry_ip: 1,
            end_ip: program.code.len() as u32,
        }];
        program.root_callable_bindings = vec![RootCallableBinding {
            local_slot: 0,
            prototype_id: 0,
        }];

        let mut vm = Vm::try_new(program).expect("vm");
        vm.set_async_bridge(Box::new(PendingBridge))
            .expect("install pending bridge");
        let mut registry = HostFunctionRegistry::empty();
        registry.register_static("test::wait", 0, |vm, _args| {
            vm.submit_host_future(Box::pin(async {
                std::future::pending::<VmResult<crate::HostFutureOutput>>().await
            }))
        });
        registry.bind_vm_cached(&mut vm).expect("bind wait host");
        assert_eq!(vm.run().expect("halt root"), VmStatus::Halted);
        let callable = vm.locals()[0].clone();
        let mut callback = OwnedTimerCallback::new(vm, callable);

        assert!(matches!(
            callback.start(false).expect("first round waits"),
            TimerCallbackStatus::Waiting(_)
        ));
        let overlap = callback.start(false).expect_err("overlap rejected");
        assert!(overlap.to_string().contains("already live"), "{overlap}");
        callback.cancel().expect("cancel waiting callback");
        assert_eq!(callback.state(), TimerCallbackStatus::Cancelled);
    }
}
