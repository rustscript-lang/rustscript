//! Standard timer host-module integration tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use vm::{
    CallOutcome, CallReturn, CompileSourceFileOptions, HostApiBuilder, HostApiCatalog,
    HostAsyncBridge, HostFunctionRegistry, HostFunctionSchema, HostImportSchema, HostOpId,
    HostParamPassing, HostParamSchema, HostTypeSchema, SourceError, SourceFlavor, SourcePathError,
    TimerBackend, TimerConfig, TimerExtension, TimerHostExt, TimerRegistration, Value, Vm,
    VmResult, VmStatus, compile_source_with_flavor_and_options, standard_host_catalog,
};

#[derive(Default)]
struct ManualBackend {
    registrations: Mutex<Vec<TimerRegistration>>,
    running: Mutex<usize>,
    callback_errors: Mutex<Vec<String>>,
}

impl ManualBackend {
    fn pop(&self) -> TimerRegistration {
        self.registrations.lock().expect("registrations").remove(0)
    }
}

impl TimerBackend for ManualBackend {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        let mut registrations = self.registrations.lock().expect("registrations");
        if registrations.len() >= registration.max_pending {
            return Err(vm::VmError::HostError(format!(
                "timer pending limit {} reached",
                registration.max_pending
            )));
        }
        registrations.push(registration);
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.registrations.lock().expect("registrations").len()
    }

    fn running_count(&self) -> usize {
        *self.running.lock().expect("running")
    }

    fn report_callback_error(&self, error: vm::TimerCallbackError) {
        self.callback_errors
            .lock()
            .expect("callback errors")
            .push(error.error.to_string());
    }

    fn shutdown(&self) -> VmResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct RejectingBackend {
    attempts: AtomicUsize,
}

impl TimerBackend for RejectingBackend {
    fn register(&self, _registration: TimerRegistration) -> VmResult<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(vm::VmError::HostError(
            "backend rejected registration".to_string(),
        ))
    }

    fn pending_count(&self) -> usize {
        0
    }

    fn running_count(&self) -> usize {
        0
    }

    fn report_callback_error(&self, _error: vm::TimerCallbackError) {}

    fn shutdown(&self) -> VmResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct PanickingBackend {
    attempts: AtomicUsize,
}

impl TimerBackend for PanickingBackend {
    fn register(&self, _registration: TimerRegistration) -> VmResult<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        panic!("backend registration panic");
    }

    fn pending_count(&self) -> usize {
        0
    }

    fn running_count(&self) -> usize {
        0
    }

    fn report_callback_error(&self, _error: vm::TimerCallbackError) {}

    fn shutdown(&self) -> VmResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct SchedulerState {
    pending: VecDeque<TimerRegistration>,
    active: Vec<TimerRegistration>,
    running: usize,
    starts: usize,
}

struct LimitedSchedulerBackend {
    state: Mutex<SchedulerState>,
    errors: Mutex<Vec<String>>,
    max_running: usize,
}

impl LimitedSchedulerBackend {
    fn new(max_running: usize) -> Self {
        Self {
            state: Mutex::new(SchedulerState::default()),
            errors: Mutex::new(Vec::new()),
            max_running,
        }
    }

    fn install_bridge(&self, index: usize, bridge: Box<dyn HostAsyncBridge>) -> VmResult<()> {
        let mut state = self.state.lock().expect("scheduler state");
        state
            .pending
            .get_mut(index)
            .ok_or_else(|| vm::VmError::HostError("missing pending registration".to_string()))?
            .callback
            .set_async_bridge(bridge)
    }

    fn starts(&self) -> usize {
        self.state.lock().expect("scheduler state").starts
    }

    fn start_next(&self) -> bool {
        let mut registration = {
            let mut state = self.state.lock().expect("scheduler state");
            if state.running >= self.max_running {
                return false;
            }
            let Some(registration) = state.pending.pop_front() else {
                return false;
            };
            state.running += 1;
            state.starts += 1;
            registration
        };
        let status = registration.callback.start_reporting(false, self);
        let mut state = self.state.lock().expect("scheduler state");
        if matches!(
            status,
            vm::TimerCallbackStatus::Running
                | vm::TimerCallbackStatus::Waiting(_)
                | vm::TimerCallbackStatus::Yielded
        ) {
            state.active.push(registration);
        } else {
            state.running -= 1;
            if registration.interval.is_some() && status != vm::TimerCallbackStatus::Cancelled {
                state.pending.push_back(registration);
            }
        }
        true
    }

    fn poll_active(&self, index: usize, cx: &mut Context<'_>) -> Poll<vm::TimerCallbackStatus> {
        let mut registration = {
            let mut state = self.state.lock().expect("scheduler state");
            if index >= state.active.len() {
                return Poll::Ready(vm::TimerCallbackStatus::Complete);
            }
            state.active.remove(index)
        };
        let result = registration.callback.poll_reporting(cx, self);
        if result.is_pending() {
            self.state
                .lock()
                .expect("scheduler state")
                .active
                .insert(index, registration);
            return Poll::Pending;
        }
        let status = match result {
            Poll::Ready(status) => status,
            Poll::Pending => unreachable!(),
        };
        let mut state = self.state.lock().expect("scheduler state");
        state.running -= 1;
        if registration.interval.is_some() && status != vm::TimerCallbackStatus::Cancelled {
            state.pending.push_back(registration);
        }
        Poll::Ready(status)
    }
}

impl TimerBackend for LimitedSchedulerBackend {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        let mut state = self.state.lock().expect("scheduler state");
        if state.pending.len() + state.active.len() >= registration.max_pending {
            return Err(vm::VmError::HostError(
                "timer pending limit reached".to_string(),
            ));
        }
        if registration.max_running != self.max_running {
            return Err(vm::VmError::HostError(
                "scheduler max_running mismatch".to_string(),
            ));
        }
        state.pending.push_back(registration);
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.state.lock().expect("scheduler state").pending.len()
    }

    fn running_count(&self) -> usize {
        self.state.lock().expect("scheduler state").running
    }

    fn report_callback_error(&self, error: vm::TimerCallbackError) {
        self.errors
            .lock()
            .expect("scheduler errors")
            .push(error.error.to_string());
    }

    fn shutdown(&self) -> VmResult<()> {
        let (pending, active) = {
            let mut state = self.state.lock().expect("scheduler state");
            state.running = 0;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.active),
            )
        };
        for mut registration in pending {
            let _ = registration.callback.start_reporting(true, self);
            registration.callback.cancel()?;
        }
        for mut registration in active {
            registration.callback.cancel()?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct BridgeState {
    complete: bool,
    cancellations: usize,
}

struct ControlledBridge {
    state: Arc<Mutex<BridgeState>>,
}

impl HostAsyncBridge for ControlledBridge {
    fn submit_op(&mut self, _op_id: HostOpId, _future: vm::HostFuture) -> VmResult<()> {
        Ok(())
    }

    fn poll_op(&mut self, _op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        if self.state.lock().expect("bridge state").complete {
            Poll::Ready(Ok(CallReturn::one(Value::Bool(false))))
        } else {
            Poll::Pending
        }
    }

    fn cancel_op(&mut self, _op_id: HostOpId) {
        self.state.lock().expect("bridge state").cancellations += 1;
    }
}

struct NoopWake;

impl std::task::Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

struct AdmissionBackend {
    barrier: Barrier,
    registrations: Mutex<Vec<TimerRegistration>>,
}

impl AdmissionBackend {
    fn new(participants: usize) -> Self {
        Self {
            barrier: Barrier::new(participants),
            registrations: Mutex::new(Vec::new()),
        }
    }
}

impl TimerBackend for AdmissionBackend {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        self.barrier.wait();
        let mut registrations = self.registrations.lock().expect("registrations");
        if registrations.len() >= registration.max_pending {
            return Err(vm::VmError::HostError(
                "timer pending limit reached".to_string(),
            ));
        }
        registrations.push(registration);
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.registrations.lock().expect("registrations").len()
    }

    fn running_count(&self) -> usize {
        0
    }

    fn report_callback_error(&self, _error: vm::TimerCallbackError) {}

    fn shutdown(&self) -> VmResult<()> {
        let registrations = std::mem::take(&mut *self.registrations.lock().expect("registrations"));
        for mut registration in registrations {
            registration.callback.start(true)?;
            registration.callback.cancel()?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ShutdownBackend {
    registrations: Mutex<Vec<TimerRegistration>>,
    premature_runs: Mutex<usize>,
}

impl TimerBackend for ShutdownBackend {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        let mut registrations = self.registrations.lock().expect("registrations");
        if registrations.len() >= registration.max_pending {
            return Err(vm::VmError::HostError(format!(
                "timer pending limit {} reached",
                registration.max_pending
            )));
        }
        registrations.push(registration);
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.registrations.lock().expect("registrations").len()
    }

    fn running_count(&self) -> usize {
        0
    }

    fn report_callback_error(&self, _error: vm::TimerCallbackError) {}

    fn shutdown(&self) -> VmResult<()> {
        let registrations = std::mem::take(&mut *self.registrations.lock().expect("registrations"));
        for mut registration in registrations {
            registration.callback.start(true)?;
            *self.premature_runs.lock().expect("premature runs") += 1;
            registration.callback.cancel()?;
        }
        Ok(())
    }
}

fn compile_standard(source: &str) -> Result<vm::CompiledProgram, vm::SourcePathError> {
    let catalog = standard_host_catalog();
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
}

fn run_with_backend<B: TimerBackend>(
    source: &str,
    backend: Arc<B>,
    config: TimerConfig,
) -> VmResult<Vm> {
    let mut vm = build_vm_with_backend(source, &backend, config)?;
    assert_eq!(vm.run()?, VmStatus::Halted);
    Ok(vm)
}

fn build_vm_with_backend<B: TimerBackend>(
    source: &str,
    backend: &Arc<B>,
    config: TimerConfig,
) -> VmResult<Vm> {
    let compiled = compile_standard(source).expect("timer script must compile");
    let mut vm = Vm::try_new(compiled.program)?;
    let runtime: Arc<dyn TimerBackend> = backend.clone();
    vm.install_extension(&TimerExtension::new(runtime, config))?;
    Ok(vm)
}

struct TimerTestExtension {
    timer: TimerExtension,
    catalog: Arc<HostApiCatalog>,
    recorded: Arc<Mutex<Vec<i64>>>,
    /// When set, `test::probe` is registered with one Drop-observable host
    /// instance per bound VM, so a test can count exactly how often a private
    /// execution state (the source VM or a callback VM) is released.
    probe: Option<Arc<ProbeCounters>>,
}

/// Release/call counters for the per-VM `test::probe` host instances.
#[derive(Default)]
struct ProbeCounters {
    drops: AtomicUsize,
    calls: AtomicUsize,
}

/// A host-function instance that counts its own release. Every bound VM owns
/// one instance, so `drops` is the number of execution states that were
/// released — a value below the expected count is a leak and any value above
/// it is an extra state (or a double release).
struct ProbeHost {
    counters: Arc<ProbeCounters>,
}

impl Drop for ProbeHost {
    fn drop(&mut self) {
        self.counters.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl vm::HostFunction for ProbeHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        self.counters.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::none()))
    }
}

fn panic_host(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    panic!("registered timer callback panic");
}

/// Records the integer argument of every `test::record(value)` call so a test
/// can observe callback state that the discarded callback return value hides.
struct RecordingHost {
    recorded: Arc<Mutex<Vec<i64>>>,
}

impl vm::HostFunction for RecordingHost {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        if let Some(Value::Int(value)) = args.first() {
            self.recorded.lock().expect("recorded values").push(*value);
        }
        Ok(CallOutcome::Return(CallReturn::none()))
    }
}

impl vm::HostExtension for TimerTestExtension {
    fn catalog(&self) -> Option<&vm::HostApiCatalog> {
        Some(self.catalog.as_ref())
    }

    fn register(&self, registry: &mut vm::HostFunctionRegistry) -> VmResult<()> {
        vm::register_timer_builtin_module_from_catalog(registry, self.catalog.as_ref())?;
        registry.register_exact_static(
            "test::wait",
            0,
            HostImportSchema {
                params: Vec::new(),
                return_type: vm::compiler::TypeSchema::Unknown,
                fingerprint: self.catalog.fingerprint(),
            },
            |vm, _args| {
                vm.submit_host_future(Box::pin(async {
                    std::future::pending::<VmResult<vm::HostFutureOutput>>().await
                }))
            },
        )?;
        registry.register_exact_static(
            "test::panic",
            0,
            HostImportSchema {
                params: Vec::new(),
                return_type: vm::compiler::TypeSchema::Unknown,
                fingerprint: self.catalog.fingerprint(),
            },
            panic_host,
        )?;
        let recorded = Arc::clone(&self.recorded);
        registry.register_exact(
            "test::record",
            1,
            HostImportSchema {
                params: vec![vm::HostImportParam {
                    name: "value".to_string(),
                    schema: vm::compiler::TypeSchema::Int,
                    passing: HostParamPassing::Value,
                }],
                return_type: vm::compiler::TypeSchema::Unknown,
                fingerprint: self.catalog.fingerprint(),
            },
            move || -> Box<dyn vm::HostFunction> {
                Box::new(RecordingHost {
                    recorded: Arc::clone(&recorded),
                })
            },
        )?;
        if let Some(probe) = self.probe.as_ref() {
            let probe = Arc::clone(probe);
            registry.register_exact(
                "test::probe",
                0,
                HostImportSchema {
                    params: Vec::new(),
                    return_type: vm::compiler::TypeSchema::Unknown,
                    fingerprint: self.catalog.fingerprint(),
                },
                move || -> Box<dyn vm::HostFunction> {
                    Box::new(ProbeHost {
                        counters: Arc::clone(&probe),
                    })
                },
            )?;
        }
        Ok(())
    }

    fn install(&self, vm: &mut Vm) {
        self.timer.install(vm);
    }
}

fn test_wait_catalog() -> Arc<HostApiCatalog> {
    let standard = standard_host_catalog();
    let mut builder = HostApiBuilder::new();
    for resource in standard.resources() {
        builder.resource(resource.clone());
    }
    for structure in standard.structs() {
        builder.named_struct(structure.clone());
    }
    for function in standard.functions() {
        builder.function(function.clone());
    }
    builder.function(HostFunctionSchema::with_return(
        "test::wait",
        Vec::new(),
        HostTypeSchema::Unknown,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::panic",
        Vec::new(),
        HostTypeSchema::Unknown,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::record",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Unknown,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::probe",
        Vec::new(),
        HostTypeSchema::Unknown,
    ));
    Arc::new(builder.build().expect("test host catalog"))
}

fn run_with_test_wait_backend<B: TimerBackend>(
    source: &str,
    backend: Arc<B>,
    config: TimerConfig,
) -> VmResult<Vm> {
    run_with_test_recorder(source, backend, config).map(|(vm, _recorded)| vm)
}

/// Runs a script against the test-wait catalog and returns the values recorded
/// by `test::record`.
fn run_with_test_recorder<B: TimerBackend>(
    source: &str,
    backend: Arc<B>,
    config: TimerConfig,
) -> VmResult<(Vm, Arc<Mutex<Vec<i64>>>)> {
    let catalog = test_wait_catalog();
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("timer script must compile");
    let mut vm = Vm::try_new(compiled.program)?;
    let runtime: Arc<dyn TimerBackend> = backend;
    let recorded = Arc::new(Mutex::new(Vec::new()));
    vm.install_extension(&TimerTestExtension {
        timer: TimerExtension::new(runtime, config),
        catalog,
        recorded: Arc::clone(&recorded),
        probe: None,
    })?;
    assert_eq!(vm.run()?, VmStatus::Halted);
    Ok((vm, recorded))
}

/// Builds (without running) a VM whose registry carries the Drop-observable
/// `test::probe` host, so each bound VM owns one probe instance.
fn build_vm_with_probe<B: TimerBackend>(
    source: &str,
    backend: &Arc<B>,
    config: TimerConfig,
    probe: Arc<ProbeCounters>,
) -> VmResult<Vm> {
    let catalog = test_wait_catalog();
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("timer script must compile");
    let mut vm = Vm::try_new(compiled.program)?;
    let runtime: Arc<dyn TimerBackend> = backend.clone();
    vm.install_extension(&TimerTestExtension {
        timer: TimerExtension::new(runtime, config),
        catalog,
        recorded: Arc::new(Mutex::new(Vec::new())),
        probe: Some(probe),
    })?;
    Ok(vm)
}

/// [`build_vm_with_probe`] run to completion.
fn run_with_probe<B: TimerBackend>(
    source: &str,
    backend: Arc<B>,
    config: TimerConfig,
    probe: Arc<ProbeCounters>,
) -> VmResult<Vm> {
    let mut vm = build_vm_with_probe(source, &backend, config, probe)?;
    assert_eq!(vm.run()?, VmStatus::Halted);
    Ok(vm)
}

#[test]
fn at_compile_contract_uses_callable_take_owned() {
    let compiled = compile_standard("use timer; timer::at(10, |premature| { premature });")
        .expect("timer::at must compile");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "timer::at")
        .expect("timer::at exact import");
    let schema = import.schema.as_ref().expect("exact schema");
    assert_eq!(schema.params[1].passing, HostParamPassing::TakeOwned);
}

#[test]
fn at_registers_and_returns_then_callback_runs() {
    let backend = Arc::new(ManualBackend::default());
    let mut vm = run_with_backend(
        "use timer; timer::at(25, |premature| { premature });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    assert_eq!(vm.take_callable_result(), None);
    assert_eq!(backend.pending_count(), 1);

    let mut registration = backend.pop();
    assert_eq!(registration.delay, Duration::from_millis(25));
    assert_eq!(registration.interval, None);
    assert_eq!(
        registration.callback.start(false).expect("callback runs"),
        vm::TimerCallbackStatus::Complete
    );
}

/// A normal (non-shutdown) round passes `premature = false`, and a callback
/// that observes anything else reports through the backend error sink.
#[test]
fn at_callback_observes_a_non_premature_round() {
    let backend = Arc::new(ManualBackend::default());
    run_with_backend(
        "use timer; timer::at(10, |premature| { if premature => { assert(false); false } else => { true } });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    assert_eq!(
        registration
            .callback
            .start(false)
            .expect("callback completes"),
        vm::TimerCallbackStatus::Complete
    );
    assert!(
        backend.callback_errors.lock().expect("errors").is_empty(),
        "a normal round must pass premature=false"
    );
}

/// The documented defaults are the installed limits and travel with every
/// registration.
#[test]
fn default_limits_match_the_documented_contract() {
    let config = TimerConfig::default();
    assert_eq!(config.max_pending, vm::DEFAULT_MAX_PENDING_TIMERS);
    assert_eq!(config.max_running, vm::DEFAULT_MAX_RUNNING_TIMERS);
    assert_eq!(vm::DEFAULT_MAX_PENDING_TIMERS, 1024);
    assert_eq!(vm::DEFAULT_MAX_RUNNING_TIMERS, 256);

    let backend = Arc::new(ManualBackend::default());
    run_with_backend(
        "use timer; timer::at(1, |premature| {});",
        Arc::clone(&backend),
        config,
    )
    .expect("registration succeeds");
    let registration = backend.pop();
    assert_eq!(registration.max_pending, 1024);
    assert_eq!(registration.max_running, 256);
}

#[test]
fn every_marks_interval_and_preserves_callable_between_rounds() {
    let backend = Arc::new(ManualBackend::default());
    run_with_backend(
        "use timer; timer::every(7, |premature| { premature });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    assert_eq!(registration.delay, Duration::from_millis(7));
    assert_eq!(registration.interval, Some(Duration::from_millis(7)));
    assert_eq!(registration.max_running, TimerConfig::default().max_running);
    assert_eq!(
        registration.callback.start(false).expect("first round"),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(
        registration.callback.start(false).expect("second round"),
        vm::TimerCallbackStatus::Complete
    );
}

#[test]
fn every_preserves_captured_callable_graph_across_rounds() {
    let backend = Arc::new(ManualBackend::default());
    let source_vm = run_with_backend(
        "use timer; let expected = false; timer::every(1, |premature| { expected == premature });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("capturing interval registers");
    let mut registration = backend.pop();

    assert_eq!(
        registration.callback.start(false).expect("first round"),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(
        registration.callback.start(false).expect("second round"),
        vm::TimerCallbackStatus::Complete
    );
    assert!(backend.callback_errors.lock().expect("errors").is_empty());
    drop(source_vm);
}

/// `every` serializes one closure whose capture cells survive between rounds:
/// each round reads and writes the *same* mutable capture cell.
#[test]
fn every_rounds_share_one_mutable_capture_cell() {
    let backend = Arc::new(ManualBackend::default());
    let (_vm, recorded) = run_with_test_recorder(
        "use test;\nuse timer;\nlet mut rounds = 0;\ntimer::every(5, |premature| if true => { rounds = rounds + 1; test::record(rounds) } else => { test::record(rounds) });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("mutable capture registers");
    let mut registration = backend.pop();
    for _ in 0..3 {
        assert_eq!(
            registration.callback.start(false).expect("round completes"),
            vm::TimerCallbackStatus::Complete
        );
    }
    assert_eq!(
        recorded.lock().expect("recorded values").as_slice(),
        &[1, 2, 3],
        "each round must observe the value the previous round wrote"
    );
    assert!(backend.callback_errors.lock().expect("errors").is_empty());
}

#[test]
fn counts_are_synchronous_and_reflect_backend_state() {
    let backend = Arc::new(ManualBackend::default());
    *backend.running.lock().expect("running") = 3;
    let vm = run_with_backend(
        "use timer; timer::at(1, |premature| {}); timer::pending_count() + timer::running_count();",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("count query succeeds");
    assert_eq!(vm.stack().last(), Some(&Value::Int(4)));
}

#[test]
fn invalid_durations_and_pending_limit_are_rejected() {
    for source in [
        "use timer; let callback = |premature| { premature }; timer::at(-1, callback);",
        "use timer; let callback = |premature| { premature }; timer::every(0, callback);",
    ] {
        let backend = Arc::new(ManualBackend::default());
        let mut vm = build_vm_with_backend(source, &backend, TimerConfig::default())
            .expect("vm setup succeeds");
        let error = vm
            .run()
            .expect_err("an invalid duration must fail the call");
        assert!(
            error.to_string().contains("millisecond duration"),
            "{error}"
        );
        assert_eq!(backend.pending_count(), 0);
        assert!(
            vm.stack()
                .iter()
                .any(|value| matches!(value, Value::Callable(_))),
            "a preflight duration rejection must restore the callback value"
        );
    }

    let backend = Arc::new(ManualBackend::default());
    run_with_backend(
        "use timer; timer::at(1, |premature| {});",
        Arc::clone(&backend),
        TimerConfig {
            max_pending: 1,
            max_running: 1,
        },
    )
    .expect("first timer accepted");
    let mut vm = build_vm_with_backend(
        "use timer; let callback = |premature| { premature }; timer::at(2, callback);",
        &backend,
        TimerConfig {
            max_pending: 1,
            max_running: 1,
        },
    )
    .expect("vm setup succeeds before the pending-limit rejection");
    let error = vm
        .run()
        .expect_err("second timer must exceed pending limit");
    assert!(error.to_string().contains("pending limit"), "{error}");
    assert_eq!(backend.pending_count(), 1);
    assert!(
        vm.stack()
            .iter()
            .any(|value| matches!(value, Value::Callable(_))),
        "a rejected registration must not consume its callback"
    );
}

#[test]
fn backend_rejection_restores_callback_argument_and_allows_shutdown() {
    let backend = Arc::new(RejectingBackend::default());
    let mut vm = build_vm_with_backend(
        "use timer; timer::at(1, |premature| { premature });",
        &backend,
        TimerConfig::default(),
    )
    .expect("vm setup succeeds before backend rejection");
    let error = vm.run().expect_err("backend rejection must fail the call");
    assert!(error.to_string().contains("backend rejected"), "{error}");
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    assert!(
        vm.stack()
            .iter()
            .any(|value| matches!(value, Value::Callable(_))),
        "the failed call stack must retain the callback value"
    );
    backend.shutdown().expect("shutdown after rejection");
    assert_eq!(backend.pending_count(), 0);
}

#[test]
fn backend_panic_restores_callback_argument_before_unwinding() {
    let backend = Arc::new(PanickingBackend::default());
    let mut vm = build_vm_with_backend(
        "use timer; timer::at(1, |premature| { premature });",
        &backend,
        TimerConfig::default(),
    )
    .expect("vm setup succeeds before backend panic");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.run()));
    assert!(
        result.is_err(),
        "backend panic must preserve panic semantics"
    );
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    assert!(
        vm.stack()
            .iter()
            .any(|value| matches!(value, Value::Callable(_))),
        "the panic path must restore the callback value before unwinding"
    );
    backend.shutdown().expect("shutdown after backend panic");
}

#[test]
fn scheduler_leaves_due_callback_pending_until_running_slot_is_free() {
    let backend = Arc::new(LimitedSchedulerBackend::new(1));
    let config = TimerConfig {
        max_pending: 8,
        max_running: 1,
    };
    run_with_test_wait_backend(
        "use test;\nuse timer;\nfn wait() -> bool { test::wait() }\ntimer::every(1, |premature| { wait() });\ntimer::every(1, |premature| { wait() });\n",
        Arc::clone(&backend),
        config,
    )
    .expect("scheduler registrations succeed");
    let first_state = Arc::new(Mutex::new(BridgeState::default()));
    let second_state = Arc::new(Mutex::new(BridgeState::default()));
    backend
        .install_bridge(
            0,
            Box::new(ControlledBridge {
                state: Arc::clone(&first_state),
            }),
        )
        .expect("install first bridge");
    backend
        .install_bridge(
            1,
            Box::new(ControlledBridge {
                state: Arc::clone(&second_state),
            }),
        )
        .expect("install second bridge");

    assert_eq!(backend.pending_count(), 2);
    assert_eq!(backend.running_count(), 0);
    assert_eq!(backend.starts(), 0);
    assert!(backend.start_next(), "first due callback starts");
    assert_eq!(backend.pending_count(), 1);
    assert_eq!(backend.running_count(), 1);
    assert_eq!(backend.starts(), 1);
    assert!(
        !backend.start_next(),
        "the second due callback remains pending"
    );
    assert_eq!(backend.pending_count(), 1);
    assert_eq!(backend.running_count(), 1);

    first_state.lock().expect("first bridge state").complete = true;
    let waker = std::task::Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(
        backend.poll_active(0, &mut cx),
        Poll::Ready(vm::TimerCallbackStatus::Complete)
    ));
    assert_eq!(backend.running_count(), 0);
    assert_eq!(backend.pending_count(), 2);

    assert!(
        backend.start_next(),
        "a pending callback starts after release"
    );
    assert_eq!(backend.running_count(), 1);
    assert_eq!(backend.pending_count(), 1);
    assert_eq!(backend.starts(), 2);
    assert_eq!(backend.errors.lock().expect("scheduler errors").len(), 0);
    backend.shutdown().expect("scheduler shutdown");
    assert_eq!(backend.pending_count(), 0);
    assert_eq!(backend.running_count(), 0);
}

#[test]
fn scheduler_survives_callback_panic_and_repeats_after_recovery() {
    let backend = Arc::new(LimitedSchedulerBackend::new(1));
    run_with_test_wait_backend(
        "use test;\nuse timer;\nfn invoke_panic() -> bool { test::panic() }\ntimer::every(1, |premature| { invoke_panic() });\n",
        Arc::clone(&backend),
        TimerConfig {
            max_pending: 4,
            max_running: 1,
        },
    )
    .expect("panic callback registration succeeds");
    assert_eq!(backend.pending_count(), 1);
    assert!(backend.start_next(), "first repeating round starts");
    assert_eq!(backend.running_count(), 0);
    assert_eq!(backend.pending_count(), 1);
    assert_eq!(backend.starts(), 1);
    assert_eq!(backend.errors.lock().expect("scheduler errors").len(), 1);

    assert!(
        backend.start_next(),
        "scheduler survives first callback panic"
    );
    assert_eq!(backend.running_count(), 0);
    assert_eq!(backend.pending_count(), 1);
    assert_eq!(backend.starts(), 2);
    assert_eq!(backend.errors.lock().expect("scheduler errors").len(), 2);
    backend.shutdown().expect("scheduler shutdown");
}

#[test]
fn backend_installs_independent_bridges_for_waiting_timer_callbacks() {
    let backend = Arc::new(ManualBackend::default());
    run_with_test_wait_backend(
        "use test;\nuse timer;\nfn wait() -> bool { test::wait() }\ntimer::at(1, |premature| { wait() });\ntimer::at(1, |premature| { wait() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registrations succeed");
    let mut first = backend.pop();
    let mut second = backend.pop();
    let first_state = Arc::new(Mutex::new(BridgeState::default()));
    let second_state = Arc::new(Mutex::new(BridgeState::default()));
    first
        .callback
        .set_async_bridge(Box::new(ControlledBridge {
            state: Arc::clone(&first_state),
        }))
        .expect("install first bridge");
    second
        .callback
        .set_async_bridge(Box::new(ControlledBridge {
            state: Arc::clone(&second_state),
        }))
        .expect("install second bridge");

    assert!(matches!(
        first.callback.start(false).expect("first waits"),
        vm::TimerCallbackStatus::Waiting(_)
    ));
    assert!(matches!(
        second.callback.start(false).expect("second waits"),
        vm::TimerCallbackStatus::Waiting(_)
    ));

    let waker = std::task::Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(first.callback.poll(&mut cx), Poll::Pending));
    assert!(matches!(second.callback.poll(&mut cx), Poll::Pending));

    first_state.lock().expect("first bridge state").complete = true;
    assert!(matches!(
        first.callback.poll(&mut cx),
        Poll::Ready(Ok(vm::TimerCallbackStatus::Complete))
    ));
    assert!(matches!(second.callback.poll(&mut cx), Poll::Pending));

    second_state.lock().expect("second bridge state").complete = true;
    assert!(matches!(
        second.callback.poll(&mut cx),
        Poll::Ready(Ok(vm::TimerCallbackStatus::Complete))
    ));
}

#[test]
fn backend_cancellation_of_waiting_timer_calls_bridge_once() {
    let backend = Arc::new(ManualBackend::default());
    run_with_test_wait_backend(
        "use test;\nuse timer;\nfn wait() -> bool { test::wait() }\ntimer::at(1, |premature| { wait() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    let state = Arc::new(Mutex::new(BridgeState::default()));
    registration
        .callback
        .set_async_bridge(Box::new(ControlledBridge {
            state: Arc::clone(&state),
        }))
        .expect("install bridge");
    assert!(matches!(
        registration.callback.start(false).expect("callback waits"),
        vm::TimerCallbackStatus::Waiting(_)
    ));
    registration.callback.cancel().expect("cancel callback");
    registration
        .callback
        .cancel()
        .expect("repeated cancellation");
    assert_eq!(state.lock().expect("bridge state").cancellations, 1);
}

#[test]
fn callback_errors_go_to_backend_sink_without_touching_source_vm() {
    let backend = Arc::new(ManualBackend::default());
    let source_vm = run_with_backend(
        "use timer; timer::at(1, |premature| { 1 / 0 });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    let source_stack = source_vm.stack().to_vec();

    assert_eq!(
        registration
            .callback
            .start_reporting(false, backend.as_ref()),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(backend.callback_errors.lock().expect("errors").len(), 1);
    assert_eq!(source_vm.stack(), source_stack);
}

#[test]
fn every_callback_can_report_each_round_after_an_error() {
    let backend = Arc::new(ManualBackend::default());
    run_with_backend(
        "use timer; timer::every(1, |premature| { 1 / 0 });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();

    assert_eq!(
        registration
            .callback
            .start_reporting(false, backend.as_ref()),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(
        registration
            .callback
            .start_reporting(false, backend.as_ref()),
        vm::TimerCallbackStatus::Complete
    );
    assert_eq!(backend.callback_errors.lock().expect("errors").len(), 2);
}

#[test]
fn source_vm_drop_leaves_pending_callbacks_until_explicit_shutdown() {
    let backend = Arc::new(ShutdownBackend::default());
    let vm = run_with_backend(
        "use timer; timer::at(100, |premature| { if !premature => { assert(false); false } else => { true } });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    assert_eq!(backend.pending_count(), 1);

    drop(vm);

    assert_eq!(backend.pending_count(), 1);
    assert_eq!(*backend.premature_runs.lock().expect("premature runs"), 0);
    backend.shutdown().expect("explicit worker shutdown");
    assert_eq!(backend.pending_count(), 0);
    assert_eq!(*backend.premature_runs.lock().expect("premature runs"), 1);
}

/// Worker shutdown runs each pending `every` callback exactly once with
/// `premature = true` and never reschedules a further round.
#[test]
fn every_shutdown_runs_once_premature_and_stops_rescheduling() {
    let backend = Arc::new(ShutdownBackend::default());
    run_with_backend(
        "use timer; timer::every(50, |premature| { if !premature => { assert(false); false } else => { true } });",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    assert_eq!(backend.pending_count(), 1);

    backend.shutdown().expect("explicit worker shutdown");
    assert_eq!(backend.pending_count(), 0);
    assert_eq!(*backend.premature_runs.lock().expect("premature runs"), 1);

    backend
        .shutdown()
        .expect("a repeated shutdown stays idempotent");
    assert_eq!(
        *backend.premature_runs.lock().expect("premature runs"),
        1,
        "a shutdown must not run or reschedule the repeating callback again"
    );
}

#[test]
fn concurrent_backend_admission_allows_only_the_configured_pending_count() {
    let backend = Arc::new(AdmissionBackend::new(2));
    let config = TimerConfig {
        max_pending: 1,
        max_running: 1,
    };
    let mut handles = Vec::new();
    for source in [
        "use timer; timer::at(1, |premature| { false });",
        "use timer; timer::at(2, |premature| { false });",
    ] {
        let backend = Arc::clone(&backend);
        handles.push(std::thread::spawn(move || {
            run_with_backend(source, backend, config).map(drop)
        }));
    }
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("admission worker").map(drop))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(backend.pending_count(), 1);
    backend.shutdown().expect("explicit shutdown");
    assert_eq!(backend.pending_count(), 0);
}

#[test]
fn cancellation_is_idempotent_and_blocks_restart() {
    let backend = Arc::new(ManualBackend::default());
    run_with_backend(
        "use timer; timer::at(1, |premature| {});",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    registration.callback.cancel().expect("first cancel");
    registration.callback.cancel().expect("second cancel");
    assert_eq!(
        registration.callback.state(),
        vm::TimerCallbackStatus::Cancelled
    );
    assert!(registration.callback.start(true).is_err());
}

/// Unwraps a compile failure and asserts the stable diagnostic code.
fn expect_compile_code(
    result: Result<vm::CompiledProgram, SourcePathError>,
    code: &str,
) -> SourceError {
    match result {
        Ok(_) => panic!("expected a {code} compile diagnostic, got success"),
        Err(SourcePathError::Source(error)) | Err(SourcePathError::SourceWithMap { error, .. }) => {
            assert!(
                matches!(&error, SourceError::Parse(parse) if parse.code.as_deref() == Some(code)),
                "expected {code}, got: {error:?}"
            );
            error
        }
        Err(other) => panic!("unexpected source path error: {other:?}"),
    }
}

/// The callback parameter is `TakeOwned`, so the compiler's move contract must
/// reject a second use of a callback already transferred to a timer.
#[test]
fn timer_move_contract_rejects_reusing_a_moved_callback() {
    for register in ["timer::at(1, callback);", "timer::every(1, callback);"] {
        let source = format!(
            "use timer;\nlet callback = |premature| {{ premature }};\n{register}\ntimer::at(2, callback);\n"
        );
        let error = expect_compile_code(compile_standard(&source), "E_LOCAL_MOVED");
        if let SourceError::Parse(parse) = &error {
            assert!(
                parse.message.contains("callback"),
                "diagnostic must name the moved local: {parse:?}"
            );
        }
    }
}

/// The standard catalog exposes the timer imports; until the backend state is
/// installed, every timer call fails with the documented installation error.
#[test]
fn timer_calls_without_installed_runtime_state_report_missing_backend() {
    for source in [
        "use timer; timer::at(1, |premature| {});",
        "use timer; timer::pending_count();",
        "use timer; timer::running_count();",
    ] {
        let compiled = compile_standard(source).expect("timer script must compile");
        let mut registry = HostFunctionRegistry::empty();
        vm::register_timer_builtin_module(&mut registry).expect("timer registration");
        let mut vm = Vm::try_new(compiled.program).expect("vm construction");
        registry
            .bind_vm_cached(&mut vm)
            .expect("bind timer registry");
        let error = vm
            .run()
            .expect_err("a timer call without installed state must fail");
        assert!(
            error.to_string().contains("timer runtime is not installed"),
            "unexpected error: {error}"
        );
    }
}

/// Clearing the runtime removes backend state without unregistering the timer
/// functions: later calls fail with the installation error again.
#[test]
fn clearing_the_timer_runtime_blocks_further_timer_calls() {
    let backend = Arc::new(ManualBackend::default());
    let mut vm = build_vm_with_backend(
        "use timer; timer::at(1, |premature| {});",
        &backend,
        TimerConfig::default(),
    )
    .expect("vm setup succeeds");
    vm.clear_timer_runtime();
    let error = vm
        .run()
        .expect_err("a cleared runtime must reject timer calls");
    assert!(
        error.to_string().contains("timer runtime is not installed"),
        "unexpected error: {error}"
    );
    assert_eq!(backend.pending_count(), 0);
}

/// A callback VM holds only a weak reference to its backend, so the timer
/// registration outlives its creating VM. Once the owning backend is gone,
/// starting the callback reports the shutdown error instead of reusing state.
#[test]
fn timer_callback_with_a_dropped_backend_reports_shutdown() {
    let backend = Arc::new(ManualBackend::default());
    let vm = run_with_backend(
        "use timer;\nfn count() -> int { timer::pending_count() }\ntimer::at(1, |premature| { count() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    drop(vm);
    drop(backend);
    let error = registration
        .callback
        .start(false)
        .expect_err("the callback backend is gone");
    assert!(
        error.to_string().contains("timer runtime has shut down"),
        "unexpected error: {error}"
    );
}

/// Dropping a callback paused on an async host operation must cancel its
/// bridge exactly once through `OwnedTimerCallback::Drop`.
#[test]
fn drop_while_waiting_cancels_the_bridge_exactly_once() {
    let backend = Arc::new(ManualBackend::default());
    run_with_test_wait_backend(
        "use test;\nuse timer;\nfn wait() -> bool { test::wait() }\ntimer::at(1, |premature| { wait() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    let state = Arc::new(Mutex::new(BridgeState::default()));
    registration
        .callback
        .set_async_bridge(Box::new(ControlledBridge {
            state: Arc::clone(&state),
        }))
        .expect("install bridge");
    assert!(matches!(
        registration.callback.start(false).expect("callback waits"),
        vm::TimerCallbackStatus::Waiting(_)
    ));
    drop(registration);
    assert_eq!(
        state.lock().expect("bridge state").cancellations,
        1,
        "dropping a waiting callback must cancel its host operation exactly once"
    );
}

/// A completed `at` round keeps its private callback VM (and the transferred
/// callable graph) alive until the registration is released, and the release
/// happens exactly once. The Drop-observable `test::probe` instance is bound
/// once per VM, so its counter witnesses exactly how many execution states
/// were released.
#[test]
fn completed_callback_vm_is_released_exactly_once() {
    let counters = Arc::new(ProbeCounters::default());
    let backend = Arc::new(ManualBackend::default());
    let source_vm = run_with_probe(
        "use test;\nuse timer;\ntimer::at(1, |premature| if true => { test::probe() } else => { test::probe() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
        Arc::clone(&counters),
    )
    .expect("registration succeeds");
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        0,
        "the source VM and its private callback VM must both be alive"
    );
    let mut registration = backend.pop();

    drop(source_vm);
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        1,
        "releasing the source VM releases exactly its own execution state"
    );

    assert_eq!(
        registration
            .callback
            .start(false)
            .expect("callback completes"),
        vm::TimerCallbackStatus::Complete
    );
    assert!(
        counters.calls.load(Ordering::SeqCst) >= 1,
        "the callback VM must have bound and run its own probe instance"
    );
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        1,
        "a completed round keeps the callback VM alive for the registration"
    );

    drop(registration);
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        2,
        "the private callback VM that owns the transferred callable graph must \
         be released exactly once"
    );
}

/// A backend rejection must release the callback VM it refused exactly once:
/// the rejected registration neither leaks its private execution state nor
/// releases the source VM's.
#[test]
fn rejected_registration_releases_the_callback_vm_exactly_once() {
    let counters = Arc::new(ProbeCounters::default());
    let backend = Arc::new(RejectingBackend::default());
    let mut vm = build_vm_with_probe(
        "use test;\nuse timer;\ntimer::at(1, |premature| if true => { test::probe() } else => { test::probe() });\n",
        &backend,
        TimerConfig::default(),
        Arc::clone(&counters),
    )
    .expect("vm setup succeeds before backend rejection");
    let error = vm.run().expect_err("backend rejection must fail the call");
    assert!(error.to_string().contains("backend rejected"), "{error}");
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        1,
        "the rejected callback VM must be released exactly once by the \
         rejection path"
    );
    assert_eq!(
        counters.calls.load(Ordering::SeqCst),
        0,
        "the rejected callback VM never ran a round"
    );
    drop(vm);
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        2,
        "releasing the source VM afterwards adds exactly one release"
    );
}

/// Callback failures (`every` division-by-zero rounds) reset the *same*
/// private VM for reuse: no round leaks a state, no round creates an extra
/// one, and the final release happens exactly once.
#[test]
fn error_rounds_reuse_one_callback_vm_released_exactly_once() {
    let counters = Arc::new(ProbeCounters::default());
    let backend = Arc::new(ManualBackend::default());
    let source_vm = run_with_probe(
        "use test;\nuse timer;\ntimer::every(1, |premature| if true => { test::probe(); let unused = 1 / 0; test::probe() } else => { test::probe() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
        Arc::clone(&counters),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();

    for round in 1..=2 {
        assert_eq!(
            registration
                .callback
                .start_reporting(false, backend.as_ref()),
            vm::TimerCallbackStatus::Complete,
            "error round {round} must end the round without poisoning the \
             scheduler"
        );
    }
    assert_eq!(
        backend.callback_errors.lock().expect("errors").len(),
        2,
        "each failing round reports one callback error"
    );
    assert_eq!(
        counters.calls.load(Ordering::SeqCst),
        2,
        "both rounds ran on the same bound callback VM"
    );
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        0,
        "failing rounds must not release the callback VM"
    );

    drop(registration);
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        1,
        "the recovered callback VM must be released exactly once"
    );
    drop(source_vm);
    assert_eq!(counters.drops.load(Ordering::SeqCst), 2);
}

/// Cancelling a callback paused on async host work releases the operation
/// through the bridge exactly once, keeps the private VM until the
/// registration is released, and then releases it exactly once.
#[test]
fn cancelled_waiting_callback_releases_its_vm_exactly_once() {
    let counters = Arc::new(ProbeCounters::default());
    let backend = Arc::new(ManualBackend::default());
    let source_vm = run_with_probe(
        "use test;\nuse timer;\nfn wait() -> bool { test::wait() }\ntimer::at(1, |premature| if true => { test::probe(); wait() } else => { wait() });\n",
        Arc::clone(&backend),
        TimerConfig::default(),
        Arc::clone(&counters),
    )
    .expect("registration succeeds");
    let mut registration = backend.pop();
    let state = Arc::new(Mutex::new(BridgeState::default()));
    registration
        .callback
        .set_async_bridge(Box::new(ControlledBridge {
            state: Arc::clone(&state),
        }))
        .expect("install bridge");
    assert!(matches!(
        registration.callback.start(false).expect("callback waits"),
        vm::TimerCallbackStatus::Waiting(_)
    ));
    assert_eq!(counters.calls.load(Ordering::SeqCst), 1);

    registration.callback.cancel().expect("cancel callback");
    assert_eq!(
        state.lock().expect("bridge state").cancellations,
        1,
        "cancellation must release the waiting operation exactly once"
    );
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        0,
        "cancelling a callback keeps its private VM until the registration is \
         released"
    );

    drop(registration);
    assert_eq!(
        counters.drops.load(Ordering::SeqCst),
        1,
        "the cancelled callback VM must be released exactly once"
    );
    drop(source_vm);
    assert_eq!(counters.drops.load(Ordering::SeqCst), 2);
}
