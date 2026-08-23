use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
#[cfg(all(not(target_arch = "wasm32"), test))]
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use vm::{
    CallOutcome, CallReturn, CancellationReason, FunctionDecl, HostAsyncBridge, HostFunction,
    HostFuture, HostFutureOutput, HostOpId, LocalInfo, SourceFlavor, SourcePathError, Value, Vm,
    VmError, VmResult, VmStatus, VmYieldReason, compile_source_with_flavor_and_options,
    format_value, render_vm_error, standard_composition,
};

use crate::analyzer::{LintDiagnostic, lint_source_with_flavor, lint_success_diagnostics};
use crate::stdlib::embedded_stdlib_compile_options;

const MAX_DEBUG_STEPS_PER_COMMAND: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptConfigMode {
    Fuel,
    Epoch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FuelConfig {
    pub mode: Option<InterruptConfigMode>,
    pub fuel: Option<u64>,
    pub fuel_check_interval: Option<u32>,
    pub epoch_deadline: Option<u64>,
    pub epoch_check_interval: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptModeState {
    None,
    Fuel,
    Epoch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuelState {
    pub enabled: bool,
    pub mode: InterruptModeState,
    pub remaining: Option<u64>,
    pub check_interval: u32,
    pub epoch_current: u64,
    pub epoch_deadline: Option<u64>,
    pub epoch_slice: Option<u64>,
}

impl FuelState {
    fn disabled(check_interval: u32) -> Self {
        Self {
            enabled: false,
            mode: InterruptModeState::None,
            remaining: None,
            check_interval,
            epoch_current: 0,
            epoch_deadline: None,
            epoch_slice: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunErrorDetails {
    /// The VM/domain operation that reported the failure.
    pub operation: String,
    /// Structured human-readable detail retained alongside the stable code.
    pub message: String,
    /// Optional configured capacity associated with the failure.
    pub limit: Option<u64>,
    /// Optional offending/observed value associated with the failure.
    pub value: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub diagnostics: Vec<LintDiagnostic>,
    pub output: Vec<String>,
    pub stack: Vec<String>,
    pub error: Option<String>,
    /// Stable machine-readable classification for JavaScript consumers.
    pub error_code: Option<String>,
    /// Structured detail payload; callers must not parse `error` to classify it.
    pub error_details: Option<RunErrorDetails>,
    pub halted: bool,
    pub yielded: bool,
    pub fuel: FuelState,
    pub command_output: String,
}

impl RunReport {
    pub fn source_error(source: &str, flavor: SourceFlavor, err: SourcePathError) -> Self {
        let diagnostics = lint_source_with_flavor(source, flavor).diagnostics;
        Self {
            diagnostics,
            output: Vec::new(),
            stack: Vec::new(),
            error: Some(err.to_string()),
            error_code: Some("source_error".to_string()),
            error_details: Some(RunErrorDetails {
                operation: "source".to_string(),
                message: err.to_string(),
                limit: None,
                value: None,
            }),
            halted: true,
            yielded: false,
            fuel: FuelState::disabled(1),
            command_output: String::new(),
        }
    }

    pub fn runtime_error(
        message: String,
        output: Vec<String>,
        stack: Vec<String>,
        fuel: FuelState,
    ) -> Self {
        let error_details = RunErrorDetails {
            operation: "runtime".to_string(),
            message: message.clone(),
            limit: None,
            value: None,
        };
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: Some(message),
            error_code: Some("runtime_error".to_string()),
            error_details: Some(error_details),
            halted: true,
            yielded: false,
            fuel,
            command_output: String::new(),
        }
    }

    fn runtime_vm_error(
        vm: Option<&Vm>,
        error: &VmError,
        output: Vec<String>,
        stack: Vec<String>,
        fuel: FuelState,
    ) -> Self {
        let (error_code, error_details) = vm_error_info(error);
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: Some(
                vm.map(|vm| render_vm_error(vm, error))
                    .unwrap_or_else(|| error.to_string()),
            ),
            error_code: Some(error_code),
            error_details: Some(error_details),
            halted: true,
            yielded: false,
            fuel,
            command_output: String::new(),
        }
    }

    fn inactive(error: Option<String>, command_output: impl Into<String>) -> Self {
        let error_details = error.as_ref().map(|message| RunErrorDetails {
            operation: "wasm::command".to_string(),
            message: message.clone(),
            limit: None,
            value: None,
        });
        Self {
            diagnostics: Vec::new(),
            output: Vec::new(),
            stack: Vec::new(),
            error,
            error_code: error_details.as_ref().map(|_| "command_error".to_string()),
            error_details,
            halted: true,
            yielded: false,
            fuel: FuelState::disabled(1),
            command_output: command_output.into(),
        }
    }
}

fn vm_error_info(error: &VmError) -> (String, RunErrorDetails) {
    match error {
        VmError::Resource(error) => (
            // ResourceTable arena-ID identity exhaustion carries a dedicated
            // typed variant (`ResourceTableArenaExhausted`), so the stable
            // JS-facing code is derived purely from the enum — never from the
            // free-form operation string. Ordinary resource slot/id push
            // exhaustion keeps `ResourceIdExhausted` (-> `resource_id_exhausted`).
            error.code().as_str().to_string(),
            RunErrorDetails {
                operation: error.operation().to_string(),
                message: error.message().to_string(),
                limit: error.limit().map(|value| value as u64),
                value: error.value(),
            },
        ),
        VmError::Operation(error) => (
            error.code().as_str().to_string(),
            RunErrorDetails {
                operation: error.operation().to_string(),
                message: error.message().to_string(),
                limit: error.limit(),
                value: error.value(),
            },
        ),
        VmError::LegacyRuntime(error) => (
            format!("legacy_runtime_{}", error.code().as_str()),
            RunErrorDetails {
                operation: error.operation().to_string(),
                message: error.message().to_string(),
                limit: error.limit().map(|value| value as u64),
                value: error.value(),
            },
        ),
        VmError::ExecutionScope(error) => (
            "execution_scope_error".to_string(),
            RunErrorDetails {
                operation: "vm::execution_scope".to_string(),
                message: error.to_string(),
                limit: None,
                value: None,
            },
        ),
        _ => (
            "vm_error".to_string(),
            RunErrorDetails {
                operation: "vm".to_string(),
                message: error.to_string(),
                limit: None,
                value: None,
            },
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugReport {
    pub diagnostics: Vec<LintDiagnostic>,
    pub output: Vec<String>,
    pub stack: Vec<String>,
    pub error: Option<String>,
    pub current_line: Option<u32>,
    pub breakpoints: Vec<u32>,
    pub halted: bool,
    pub command_output: String,
    pub fuel: FuelState,
}

impl DebugReport {
    fn source_error(source: &str, flavor: SourceFlavor, err: SourcePathError) -> Self {
        Self {
            diagnostics: lint_source_with_flavor(source, flavor).diagnostics,
            output: Vec::new(),
            stack: Vec::new(),
            error: Some(err.to_string()),
            current_line: None,
            breakpoints: Vec::new(),
            halted: true,
            command_output: String::new(),
            fuel: FuelState::disabled(1),
        }
    }

    fn inactive(error: Option<String>, command_output: impl Into<String>) -> Self {
        Self {
            diagnostics: Vec::new(),
            output: Vec::new(),
            stack: Vec::new(),
            error,
            current_line: None,
            breakpoints: Vec::new(),
            halted: true,
            command_output: command_output.into(),
            fuel: FuelState::disabled(1),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunCommand {
    Resume,
    SetFuel { amount: u64 },
    AddFuel { amount: u64 },
    ClearFuel,
    SetFuelCheckInterval { interval: u32 },
    SetEpochDeadline { ticks: u64 },
    ClearEpochDeadline,
    TickEpoch { amount: u64 },
    SetEpochCheckInterval { interval: u32 },
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DebugCommand {
    State,
    Continue,
    Step,
    Next,
    Out,
    Where,
    Locals,
    Stack,
    PrintVar { name: String },
    BreakLine { line: u32 },
    ClearLine { line: u32 },
    SetFuel { amount: u64 },
    AddFuel { amount: u64 },
    ClearFuel,
    SetFuelCheckInterval { interval: u32 },
    SetEpochDeadline { ticks: u64 },
    ClearEpochDeadline,
    TickEpoch { amount: u64 },
    SetEpochCheckInterval { interval: u32 },
    Stop,
}

enum ResumeMode {
    Continue,
    StepOnce,
    StepOver { depth: usize, ip: usize },
    StepOut { depth: usize },
}

enum StepExecution {
    Advanced,
    Halted,
    Paused(String),
    Error(String),
}

enum DebugStepCheckpoint {
    Fuel(vm::FuelCheckpoint),
    Epoch(vm::EpochCheckpoint),
}

enum RunProgress {
    Halted,
    Yielded,
    Running,
}

struct RunSession {
    vm: Vm,
    output_lines: Arc<Mutex<Vec<String>>>,
    diagnostics: Vec<LintDiagnostic>,
    halted: bool,
    error: Option<String>,
    error_code: Option<String>,
    error_details: Option<RunErrorDetails>,
}

struct DebugSession {
    vm: Vm,
    output_lines: Arc<Mutex<Vec<String>>>,
    diagnostics: Vec<LintDiagnostic>,
    line_breakpoints: HashSet<u32>,
    epoch_resume_rearm_pending: bool,
    halted: bool,
    error: Option<String>,
}

thread_local! {
    static RUN_SESSION: RefCell<Option<RunSession>> = const { RefCell::new(None) };
    static DEBUG_SESSION: RefCell<Option<DebugSession>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct BrowserAsyncState {
    futures: HashMap<HostOpId, HostFuture>,
}

struct BrowserAsyncBridge {
    state: Arc<Mutex<BrowserAsyncState>>,
}

impl BrowserAsyncBridge {
    fn new(state: Arc<Mutex<BrowserAsyncState>>) -> Self {
        Self { state }
    }
}

impl HostAsyncBridge for BrowserAsyncBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        let Ok(mut state) = self.state.lock() else {
            return Err(VmError::HostError(
                "browser async bridge state is unavailable".to_string(),
            ));
        };
        state.futures.insert(op_id, future);
        Ok(())
    }

    fn poll_op(&mut self, op_id: HostOpId, _cx: &mut Context<'_>) -> Poll<VmResult<CallReturn>> {
        let Ok(state) = self.state.lock() else {
            return Poll::Ready(Err(VmError::HostError(
                "browser async bridge state is unavailable".to_string(),
            )));
        };
        if state.futures.contains_key(&op_id) {
            Poll::Pending
        } else {
            Poll::Ready(Err(VmError::HostError(format!(
                "unknown browser async op {op_id}"
            ))))
        }
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<HostFutureOutput>> {
        let Ok(mut state) = self.state.lock() else {
            return Poll::Ready(Err(VmError::HostError(
                "browser async bridge state is unavailable".to_string(),
            )));
        };
        let Some(future) = state.futures.get_mut(&op_id) else {
            return Poll::Ready(Err(VmError::HostError(format!(
                "unknown browser async op {op_id}"
            ))));
        };
        let polled = Pin::new(future).poll(cx);
        // Release the completed future the moment its poll returns `Ready`
        // (success or failure): the entry is retained only while `Pending`, so
        // repeated sequential operations never accumulate completed futures in
        // the bridge map.
        if polled.is_ready() {
            state.futures.remove(&op_id);
        }
        polled
    }

    fn cancel_op_with_reason(&mut self, op_id: HostOpId, _reason: CancellationReason) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.futures.remove(&op_id);
    }
}

struct PlaygroundRuntimeSleepHostFunction;

impl HostFunction for PlaygroundRuntimeSleepHostFunction {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let millis = sleep_millis(args)?;
        let deadline_ms = current_time_ms() + millis as f64;
        // Submit a real HostFuture through the modern scope-operation path
        // (`submit_host_future` registers a HostFutureOperation in the current
        // ExecutionScope and returns its packed scope id). The bridge is the
        // runtime context that polls the future; after the deadline it
        // resolves true.
        let sleep = std::future::poll_fn(move |cx| {
            if current_time_ms() >= deadline_ms {
                Poll::Ready(Ok(HostFutureOutput::returning(CallReturn::one(
                    Value::Bool(true),
                ))))
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        });
        vm.submit_host_future(Box::pin(sleep))
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    #[link_name = "pd_playground_now_ms"]
    fn imported_now_ms() -> f64;
}

fn current_time_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        imported_now_ms()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1_000.0
    }
}

fn sleep_millis(args: &[Value]) -> VmResult<u64> {
    let millis = match args.first() {
        Some(Value::Int(value)) => *value,
        Some(_) => return Err(VmError::TypeMismatch("int")),
        None => {
            return Err(VmError::HostError(
                "missing argument: runtime::sleep milliseconds".to_string(),
            ));
        }
    };
    if millis < 0 {
        return Err(VmError::HostError(format!(
            "runtime::sleep expects non-negative milliseconds, got {millis}",
        )));
    }
    Ok(millis as u64)
}

fn wait_message(op_id: HostOpId) -> String {
    format!("runtime::sleep pending in browser (host op {op_id})")
}

fn poll_waiting_host_op_once(vm: &mut Vm) -> Poll<VmResult<()>> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    vm.poll_waiting_host_op(&mut cx)
}

impl RunSession {
    fn new(
        vm: Vm,
        output_lines: Arc<Mutex<Vec<String>>>,
        diagnostics: Vec<LintDiagnostic>,
    ) -> Self {
        Self {
            vm,
            output_lines,
            diagnostics,
            halted: false,
            error: None,
            error_code: None,
            error_details: None,
        }
    }

    fn snapshot(&self, command_output: String, yielded: bool) -> RunReport {
        RunReport {
            diagnostics: self.diagnostics.clone(),
            output: drain_output(&self.output_lines),
            stack: self.vm.stack().iter().map(format_value).collect(),
            error: self.error.clone(),
            error_code: self.error_code.clone(),
            error_details: self.error_details.clone(),
            halted: self.halted,
            yielded,
            fuel: capture_fuel_state(&self.vm),
            command_output,
        }
    }

    fn resume(&mut self) -> (String, RunProgress) {
        if self.halted {
            return ("program halted".to_string(), RunProgress::Halted);
        }
        if let Some(error) = self.error.as_ref() {
            return (
                format!("run session is unavailable: {error}"),
                RunProgress::Halted,
            );
        }

        loop {
            if let Some(op_id) = self.vm.waiting_host_op_id() {
                match poll_waiting_host_op_once(&mut self.vm) {
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(err)) => {
                        self.halted = true;
                        let message = render_vm_error(&self.vm, &err);
                        let (code, details) = vm_error_info(&err);
                        self.error = Some(message.clone());
                        self.error_code = Some(code);
                        self.error_details = Some(details);
                        return (message, RunProgress::Halted);
                    }
                    Poll::Pending => return (wait_message(op_id), RunProgress::Running),
                }
            }

            match self.vm.run() {
                Ok(VmStatus::Halted) => {
                    self.halted = true;
                    return ("program halted".to_string(), RunProgress::Halted);
                }
                Ok(VmStatus::Yielded) => {
                    let message = match self.vm.last_yield_reason() {
                        Some(VmYieldReason::Fuel) if self.vm.get_fuel() == Some(0) => {
                            "execution interrupted: out of fuel. add more fuel and resume"
                                .to_string()
                        }
                        Some(VmYieldReason::Epoch) => format_epoch_yield_message(&self.vm),
                        _ => "execution yielded; resume to continue".to_string(),
                    };
                    return (message, RunProgress::Yielded);
                }
                Ok(VmStatus::Waiting(_)) => continue,
                Err(err) => {
                    self.halted = true;
                    let message = render_vm_error(&self.vm, &err);
                    let (code, details) = vm_error_info(&err);
                    self.error = Some(message.clone());
                    self.error_code = Some(code);
                    self.error_details = Some(details);
                    return (message, RunProgress::Halted);
                }
            }
        }
    }
}

impl DebugSession {
    fn new(
        vm: Vm,
        output_lines: Arc<Mutex<Vec<String>>>,
        diagnostics: Vec<LintDiagnostic>,
    ) -> Self {
        Self {
            vm,
            output_lines,
            diagnostics,
            line_breakpoints: HashSet::new(),
            epoch_resume_rearm_pending: false,
            halted: false,
            error: None,
        }
    }

    fn run_command(&mut self, command: DebugCommand) -> String {
        match command {
            DebugCommand::State => String::new(),
            DebugCommand::Continue => self.resume_with_mode(ResumeMode::Continue),
            DebugCommand::Step => self.resume_with_mode(ResumeMode::StepOnce),
            DebugCommand::Next => self.resume_with_mode(ResumeMode::StepOver {
                depth: self.vm.call_depth(),
                ip: self.vm.ip(),
            }),
            DebugCommand::Out => self.resume_with_mode(ResumeMode::StepOut {
                depth: self.vm.call_depth(),
            }),
            DebugCommand::Where => self.command_where(),
            DebugCommand::Locals => self.command_locals(),
            DebugCommand::Stack => format!("stack: {:?}", self.vm.stack()),
            DebugCommand::PrintVar { name } => self.command_print_var(&name),
            DebugCommand::BreakLine { line } => self.add_line_breakpoint(line),
            DebugCommand::ClearLine { line } => self.clear_line_breakpoint(line),
            DebugCommand::SetFuel { amount } => self.command_set_fuel(amount),
            DebugCommand::AddFuel { amount } => self.command_add_fuel(amount),
            DebugCommand::ClearFuel => self.command_clear_fuel(),
            DebugCommand::SetFuelCheckInterval { interval } => {
                self.command_set_fuel_check_interval(interval)
            }
            DebugCommand::SetEpochDeadline { ticks } => self.command_set_epoch_deadline(ticks),
            DebugCommand::ClearEpochDeadline => self.command_clear_epoch_deadline(),
            DebugCommand::TickEpoch { amount } => self.command_tick_epoch(amount),
            DebugCommand::SetEpochCheckInterval { interval } => {
                self.command_set_epoch_check_interval(interval)
            }
            DebugCommand::Stop => String::new(),
        }
    }

    fn snapshot(&self, command_output: String) -> DebugReport {
        let mut breakpoints = self.line_breakpoints.iter().copied().collect::<Vec<_>>();
        breakpoints.sort_unstable();
        DebugReport {
            diagnostics: self.diagnostics.clone(),
            output: drain_output(&self.output_lines),
            stack: self.vm.stack().iter().map(format_value).collect(),
            error: self.error.clone(),
            current_line: if self.halted {
                None
            } else {
                self.current_line()
            },
            breakpoints,
            halted: self.halted,
            command_output,
            fuel: capture_fuel_state(&self.vm),
        }
    }

    fn current_line(&self) -> Option<u32> {
        self.vm
            .debug_info()
            .and_then(|info| info.line_for_offset(self.vm.ip()))
    }

    fn add_line_breakpoint(&mut self, requested_line: u32) -> String {
        let line = self.resolve_executable_line(requested_line);
        self.line_breakpoints.insert(line);
        if line == requested_line {
            format!("line breakpoint set at {line}")
        } else {
            format!("line breakpoint set at {line} (requested line {requested_line})")
        }
    }

    fn clear_line_breakpoint(&mut self, requested_line: u32) -> String {
        let line = self.resolve_executable_line(requested_line);
        self.line_breakpoints.remove(&line);
        if line == requested_line {
            format!("line breakpoint cleared at {line}")
        } else {
            format!("line breakpoint cleared at {line} (requested line {requested_line})")
        }
    }

    fn resolve_executable_line(&self, requested_line: u32) -> u32 {
        let Some(info) = self.vm.debug_info() else {
            return requested_line;
        };

        let mut next = None::<u32>;
        let mut prev = None::<u32>;
        for line in info.lines.iter().map(|entry| entry.line) {
            if line >= requested_line && next.is_none_or(|candidate| line < candidate) {
                next = Some(line);
            }
            if line <= requested_line && prev.is_none_or(|candidate| line > candidate) {
                prev = Some(line);
            }
        }
        next.or(prev).unwrap_or(requested_line)
    }

    fn resume_with_mode(&mut self, mode: ResumeMode) -> String {
        if self.halted {
            return "program halted".to_string();
        }
        if let Some(error) = self.error.as_ref() {
            return format!("debugger unavailable: {error}");
        }

        let mut steps = 0usize;
        loop {
            if steps >= MAX_DEBUG_STEPS_PER_COMMAND {
                return format!(
                    "paused after {MAX_DEBUG_STEPS_PER_COMMAND} instructions; run continue again"
                );
            }
            steps = steps.saturating_add(1);

            match self.execute_single_instruction() {
                StepExecution::Advanced => {}
                StepExecution::Halted => return "program halted".to_string(),
                StepExecution::Paused(message) => return message,
                StepExecution::Error(message) => return message,
            }

            let current_line = self.current_line();
            let hit_line_breakpoint =
                current_line.is_some_and(|line| self.line_breakpoints.contains(&line));

            let should_pause = match mode {
                ResumeMode::Continue => hit_line_breakpoint,
                ResumeMode::StepOnce => true,
                ResumeMode::StepOver { depth, ip } => {
                    hit_line_breakpoint || (self.vm.call_depth() <= depth && self.vm.ip() != ip)
                }
                ResumeMode::StepOut { depth } => {
                    hit_line_breakpoint || self.vm.call_depth() < depth
                }
            };

            if should_pause {
                if hit_line_breakpoint && let Some(line) = current_line {
                    return format!("line breakpoint hit at {line}");
                }
                return String::new();
            }
        }
    }

    fn execute_single_instruction(&mut self) -> StepExecution {
        if let Err(message) = self.rearm_epoch_deadline_after_yield_if_needed() {
            return StepExecution::Error(message);
        }

        if let Some(op_id) = self.vm.waiting_host_op_id() {
            return match poll_waiting_host_op_once(&mut self.vm) {
                Poll::Ready(Ok(())) => StepExecution::Advanced,
                Poll::Ready(Err(err)) => {
                    self.halted = true;
                    let message = render_vm_error(&self.vm, &err);
                    self.error = Some(message.clone());
                    StepExecution::Error(message)
                }
                Poll::Pending => StepExecution::Paused(wait_message(op_id)),
            };
        }

        let stepped_interrupt = match self.prepare_debug_step_interrupt() {
            Ok(checkpoint) => checkpoint,
            Err(message) => return StepExecution::Paused(message),
        };

        if matches!(stepped_interrupt, DebugStepCheckpoint::Epoch(_)) {
            self.vm.clear_epoch_deadline();
        }
        self.vm
            .set_fuel_check_interval(1)
            .expect("exact debugger step interval should be valid");
        self.vm.set_fuel(1);

        let outcome = match self.vm.run() {
            Ok(VmStatus::Yielded) => StepExecution::Advanced,
            Ok(VmStatus::Halted) => {
                self.halted = true;
                StepExecution::Halted
            }
            Ok(VmStatus::Waiting(op_id)) => StepExecution::Paused(wait_message(op_id)),
            Err(err) => {
                self.halted = true;
                let message = render_vm_error(&self.vm, &err);
                self.error = Some(message.clone());
                StepExecution::Error(message)
            }
        };

        self.vm.clear_fuel();
        match stepped_interrupt {
            DebugStepCheckpoint::Fuel(checkpoint) => self.vm.restore_fuel(checkpoint),
            DebugStepCheckpoint::Epoch(checkpoint) => self.vm.restore_epoch(checkpoint),
        }
        outcome
    }

    fn rearm_epoch_deadline_after_yield_if_needed(&mut self) -> Result<(), String> {
        if !self.epoch_resume_rearm_pending {
            return Ok(());
        }

        let Some(delta) = self.vm.epoch_deadline_delta() else {
            self.epoch_resume_rearm_pending = false;
            return Ok(());
        };

        match self.vm.set_epoch_deadline(delta) {
            Ok(()) => {
                self.epoch_resume_rearm_pending = false;
                Ok(())
            }
            Err(err) => {
                let message = render_vm_error(&self.vm, &err);
                self.halted = true;
                self.error = Some(message.clone());
                Err(message)
            }
        }
    }

    fn prepare_debug_step_interrupt(&mut self) -> Result<DebugStepCheckpoint, String> {
        if self.vm.epoch_deadline().is_some() {
            let checkpoint = self.vm.epoch_checkpoint();
            return match self.vm.consume_epoch_tick() {
                Ok(()) => {
                    let stepped = self.vm.epoch_checkpoint();
                    self.vm.restore_epoch(checkpoint);
                    Ok(DebugStepCheckpoint::Epoch(stepped))
                }
                Err(VmError::EpochDeadlineReached { current, deadline }) => {
                    self.vm.restore_epoch(checkpoint);
                    self.epoch_resume_rearm_pending = true;
                    Err(format!(
                        "execution interrupted: epoch deadline reached (current {current}, deadline {deadline}). continue will re-arm the same deadline automatically; advance epoch or change the deadline first if needed"
                    ))
                }
                Err(err) => {
                    let message = render_vm_error(&self.vm, &err);
                    self.vm.restore_epoch(checkpoint);
                    self.halted = true;
                    self.error = Some(message.clone());
                    Err(message)
                }
            };
        }

        let checkpoint = self.vm.fuel_checkpoint();
        match self.vm.consume_fuel_tick() {
            Ok(()) => {
                let stepped = self.vm.fuel_checkpoint();
                self.vm.restore_fuel(checkpoint);
                Ok(DebugStepCheckpoint::Fuel(stepped))
            }
            Err(VmError::OutOfFuel { needed, remaining }) => {
                self.vm.restore_fuel(checkpoint);
                Err(format!(
                    "execution interrupted: out of fuel (needed {needed}, remaining {remaining}). add more fuel, then continue"
                ))
            }
            Err(err) => {
                let message = render_vm_error(&self.vm, &err);
                self.vm.restore_fuel(checkpoint);
                self.halted = true;
                self.error = Some(message.clone());
                Err(message)
            }
        }
    }

    fn command_where(&self) -> String {
        if let Some(info) = self.vm.debug_info() {
            if let Some(line) = info.line_for_offset(self.vm.ip()) {
                if let Some(text) = info.source_line(line) {
                    return format!("line {line}: {text}");
                }
                return format!("line: {line}");
            }
            return "line: unknown".to_string();
        }
        "no debug info".to_string()
    }

    fn command_locals(&self) -> String {
        let Some(info) = self.vm.debug_info() else {
            return format!("locals: {:?}", self.vm.locals());
        };
        if info.locals.is_empty() {
            return format!("locals: {:?}", self.vm.locals());
        }

        let current_line = info.line_for_offset(self.vm.ip());
        let mut lines = Vec::new();
        for local in &info.locals {
            if !local_visible_at_line(local, current_line) {
                continue;
            }
            match self.vm.locals().get(local.index as usize) {
                Some(value) => lines.push(format!("{} = {:?}", local.name, value)),
                None => lines.push(format!("{} = <unavailable>", local.name)),
            }
        }

        if lines.is_empty() {
            return "locals: <none visible>".to_string();
        }
        lines.join("\n")
    }

    fn command_print_var(&self, name: &str) -> String {
        let Some(info) = self.vm.debug_info() else {
            return "no debug info".to_string();
        };

        let Some(local) = info.locals.iter().find(|local| local.name == name) else {
            return format!("unknown local '{name}'");
        };

        let current_line = info.line_for_offset(self.vm.ip());
        if !local_visible_at_line(local, current_line) {
            return format!("local '{name}' is not visible at this location");
        }

        match self.vm.locals().get(local.index as usize) {
            Some(value) => format!("{name} = {:?}", value),
            None => format!("local '{name}' is out of range for this VM instance"),
        }
    }

    fn command_set_fuel(&mut self, amount: u64) -> String {
        self.vm.set_fuel(amount);
        format!("fuel set to {amount}\n{}", format_fuel_state(&self.vm))
    }

    fn command_add_fuel(&mut self, amount: u64) -> String {
        match self.vm.add_fuel(amount) {
            Ok(()) => format!("fuel added: {amount}\n{}", format_fuel_state(&self.vm)),
            Err(err) => format!("fuel add failed: {err}"),
        }
    }

    fn command_clear_fuel(&mut self) -> String {
        self.vm.clear_fuel();
        format!("fuel metering disabled\n{}", format_fuel_state(&self.vm))
    }

    fn command_set_fuel_check_interval(&mut self, interval: u32) -> String {
        match self.vm.set_fuel_check_interval(interval) {
            Ok(()) => format!(
                "fuel check interval set to {interval}\n{}",
                format_fuel_state(&self.vm)
            ),
            Err(err) => format!("fuel interval update failed: {err}"),
        }
    }

    fn command_set_epoch_deadline(&mut self, ticks: u64) -> String {
        match self.vm.set_epoch_deadline(ticks) {
            Ok(()) => {
                self.epoch_resume_rearm_pending = false;
                format!(
                    "epoch deadline set {ticks} ticks beyond current epoch\n{}",
                    format_fuel_state(&self.vm)
                )
            }
            Err(err) => format!("epoch deadline update failed: {err}"),
        }
    }

    fn command_clear_epoch_deadline(&mut self) -> String {
        self.epoch_resume_rearm_pending = false;
        self.vm.clear_epoch_deadline();
        format!(
            "epoch interruption disabled\n{}",
            format_fuel_state(&self.vm)
        )
    }

    fn command_tick_epoch(&mut self, amount: u64) -> String {
        if amount == 0 {
            return format!("epoch unchanged\n{}", format_fuel_state(&self.vm));
        }
        let current = self.vm.increment_epoch_by(amount);
        format!(
            "epoch advanced by {amount} to {current}\n{}",
            format_fuel_state(&self.vm)
        )
    }

    fn command_set_epoch_check_interval(&mut self, interval: u32) -> String {
        match self.vm.set_epoch_check_interval(interval) {
            Ok(()) => format!(
                "epoch check interval set to {interval}\n{}",
                format_fuel_state(&self.vm)
            ),
            Err(err) => format!("epoch interval update failed: {err}"),
        }
    }
}

fn local_visible_at_line(local: &LocalInfo, line: Option<u32>) -> bool {
    let Some(line) = line else {
        return true;
    };
    if let Some(declared_line) = local.declared_line
        && line < declared_line
    {
        return false;
    }
    if let Some(last_line) = local.last_line
        && line > last_line
    {
        return false;
    }
    true
}

fn capture_fuel_state(vm: &Vm) -> FuelState {
    let remaining = vm.get_fuel();
    let epoch_deadline = vm.epoch_deadline();
    let epoch_slice = vm.epoch_deadline_delta();
    let mode = if remaining.is_some() {
        InterruptModeState::Fuel
    } else if epoch_deadline.is_some() {
        InterruptModeState::Epoch
    } else {
        InterruptModeState::None
    };
    FuelState {
        enabled: !matches!(mode, InterruptModeState::None),
        mode,
        remaining,
        check_interval: if matches!(mode, InterruptModeState::Epoch) {
            vm.epoch_check_interval()
        } else {
            vm.fuel_check_interval()
        },
        epoch_current: vm.current_epoch(),
        epoch_deadline,
        epoch_slice,
    }
}

fn format_fuel_state(vm: &Vm) -> String {
    let fuel = capture_fuel_state(vm);
    match fuel.mode {
        InterruptModeState::Fuel => {
            format!(
                "fuel: {}, check_interval={}",
                fuel.remaining.unwrap_or(0),
                fuel.check_interval
            )
        }
        InterruptModeState::Epoch => {
            let deadline = fuel
                .epoch_deadline
                .map(|value| value.to_string())
                .unwrap_or_else(|| "disabled".to_string());
            let slice = fuel
                .epoch_slice
                .map(|value| value.to_string())
                .unwrap_or_else(|| "disabled".to_string());
            format!(
                "epoch: current={}, deadline={}, slice={}, check_interval={}",
                fuel.epoch_current, deadline, slice, fuel.check_interval
            )
        }
        InterruptModeState::None => {
            format!(
                "interruption: disabled, check_interval={}",
                fuel.check_interval
            )
        }
    }
}

fn apply_fuel_config(vm: &mut Vm, config: FuelConfig) -> Result<(), String> {
    let mode = config.mode.or_else(|| {
        if config.epoch_deadline.is_some() || config.epoch_check_interval.is_some() {
            Some(InterruptConfigMode::Epoch)
        } else if config.fuel.is_some() || config.fuel_check_interval.is_some() {
            Some(InterruptConfigMode::Fuel)
        } else {
            None
        }
    });

    match mode {
        Some(InterruptConfigMode::Fuel) => {
            if config.epoch_deadline.is_some() || config.epoch_check_interval.is_some() {
                return Err("epoch settings cannot be combined with fuel interruption".to_string());
            }
            if let Some(interval) = config.fuel_check_interval {
                vm.set_fuel_check_interval(interval)
                    .map_err(|err| render_vm_error(vm, &err))?;
            }
            if let Some(fuel) = config.fuel {
                vm.set_fuel(fuel);
            }
        }
        Some(InterruptConfigMode::Epoch) => {
            if config.fuel.is_some() || config.fuel_check_interval.is_some() {
                return Err("fuel settings cannot be combined with epoch interruption".to_string());
            }
            if let Some(interval) = config.epoch_check_interval {
                vm.set_epoch_check_interval(interval)
                    .map_err(|err| render_vm_error(vm, &err))?;
            }
            if let Some(deadline) = config.epoch_deadline {
                vm.set_epoch_deadline(deadline)
                    .map_err(|err| render_vm_error(vm, &err))?;
            }
        }
        None => {}
    }
    Ok(())
}

fn format_epoch_yield_message(vm: &Vm) -> String {
    let deadline = vm
        .epoch_deadline()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    format!(
        "execution interrupted: epoch deadline reached (current {}, deadline {}). resume will re-arm the same deadline automatically; advance epoch or change the deadline first if needed",
        vm.current_epoch(),
        deadline
    )
}

#[cfg(test)]
pub(crate) fn run_source_with_flavor(source: &str, flavor: SourceFlavor) -> RunReport {
    let options = embedded_stdlib_compile_options();
    let compiled = match compile_source_with_flavor_and_options(source, flavor, options.clone()) {
        Ok(compiled) => compiled,
        Err(err) => return RunReport::source_error(source, flavor, err),
    };
    let diagnostics = lint_success_diagnostics(source, flavor, &compiled, None, &options);

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.set_standard_composition(standard_composition());
    if let Err(err) = register_functions(&mut vm, &compiled.functions, &output_lines) {
        return RunReport::runtime_error(err, Vec::new(), Vec::new(), capture_fuel_state(&vm));
    }

    loop {
        if let Some(_op_id) = vm.waiting_host_op_id() {
            match poll_waiting_host_op_once(&mut vm) {
                Poll::Ready(Ok(())) => continue,
                Poll::Ready(Err(err)) => {
                    let output = drain_output(&output_lines);
                    let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                    return RunReport::runtime_error(
                        render_vm_error(&vm, &err),
                        output,
                        stack,
                        capture_fuel_state(&vm),
                    );
                }
                Poll::Pending => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        let output = drain_output(&output_lines);
                        let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                        return RunReport::runtime_error(
                            wait_message(_op_id),
                            output,
                            stack,
                            capture_fuel_state(&vm),
                        );
                    }
                }
            }
        }

        let status = match vm.run() {
            Ok(status) => status,
            Err(err) => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::runtime_error(
                    render_vm_error(&vm, &err),
                    output,
                    stack,
                    capture_fuel_state(&vm),
                );
            }
        };
        match status {
            VmStatus::Halted => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport {
                    diagnostics: diagnostics.clone(),
                    output,
                    stack,
                    error: None,
                    error_code: None,
                    error_details: None,
                    halted: true,
                    yielded: false,
                    fuel: capture_fuel_state(&vm),
                    command_output: String::new(),
                };
            }
            VmStatus::Yielded => {}
            VmStatus::Waiting(_) => {}
        }
    }
}

pub fn start_run_source_with_flavor(
    source: &str,
    flavor: SourceFlavor,
    fuel_config: FuelConfig,
) -> RunReport {
    let options = embedded_stdlib_compile_options();
    let compiled = match compile_source_with_flavor_and_options(source, flavor, options.clone()) {
        Ok(compiled) => compiled,
        Err(err) => {
            RUN_SESSION.with(|state| {
                *state.borrow_mut() = None;
            });
            return RunReport::source_error(source, flavor, err);
        }
    };
    let diagnostics = lint_success_diagnostics(source, flavor, &compiled, None, &options);

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = match Vm::try_new(compiled.program.with_local_count(compiled.locals)) {
        Ok(vm) => vm,
        Err(err) => {
            // Arena-space exhaustion is terminal for the playground embedding:
            // report the typed error instead of panicking.
            RUN_SESSION.with(|state| {
                *state.borrow_mut() = None;
            });
            return RunReport::runtime_vm_error(
                None,
                &err,
                Vec::new(),
                Vec::new(),
                FuelState::disabled(1),
            );
        }
    };
    vm.set_standard_composition(standard_composition());
    if let Err(err) = register_functions(&mut vm, &compiled.functions, &output_lines) {
        RUN_SESSION.with(|state| {
            *state.borrow_mut() = None;
        });
        return RunReport::runtime_error(err, Vec::new(), Vec::new(), capture_fuel_state(&vm));
    }
    if let Err(err) = apply_fuel_config(&mut vm, fuel_config) {
        RUN_SESSION.with(|state| {
            *state.borrow_mut() = None;
        });
        return RunReport::runtime_error(err, Vec::new(), Vec::new(), capture_fuel_state(&vm));
    }

    let mut session = RunSession::new(vm, output_lines, diagnostics);
    let (command_output, progress) = session.resume();
    let report = session.snapshot(command_output, matches!(progress, RunProgress::Yielded));
    RUN_SESSION.with(|state| {
        *state.borrow_mut() = if report.halted || report.error.is_some() {
            None
        } else {
            Some(session)
        };
    });
    report
}

pub fn run_command(command: RunCommand) -> RunReport {
    if matches!(command, RunCommand::Stop) {
        RUN_SESSION.with(|state| {
            *state.borrow_mut() = None;
        });
        return RunReport::inactive(None, "run session stopped");
    }

    RUN_SESSION.with(|state| {
        let mut slot = state.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return RunReport::inactive(
                Some("run session is not active".to_string()),
                String::new(),
            );
        };

        let mut yielded = false;
        let command_output = match command {
            RunCommand::Resume => {
                let (output, progress) = session.resume();
                yielded = matches!(progress, RunProgress::Yielded);
                output
            }
            RunCommand::SetFuel { amount } => {
                session.vm.set_fuel(amount);
                format!("fuel set to {amount}\n{}", format_fuel_state(&session.vm))
            }
            RunCommand::AddFuel { amount } => match session.vm.add_fuel(amount) {
                Ok(()) => format!("fuel added: {amount}\n{}", format_fuel_state(&session.vm)),
                Err(err) => format!("fuel add failed: {err}"),
            },
            RunCommand::ClearFuel => {
                session.vm.clear_fuel();
                format!("fuel metering disabled\n{}", format_fuel_state(&session.vm))
            }
            RunCommand::SetFuelCheckInterval { interval } => {
                match session.vm.set_fuel_check_interval(interval) {
                    Ok(()) => format!(
                        "fuel check interval set to {interval}\n{}",
                        format_fuel_state(&session.vm)
                    ),
                    Err(err) => format!("fuel interval update failed: {err}"),
                }
            }
            RunCommand::SetEpochDeadline { ticks } => match session.vm.set_epoch_deadline(ticks) {
                Ok(()) => format!(
                    "epoch deadline set {ticks} ticks beyond current epoch\n{}",
                    format_fuel_state(&session.vm)
                ),
                Err(err) => format!("epoch deadline update failed: {err}"),
            },
            RunCommand::ClearEpochDeadline => {
                session.vm.clear_epoch_deadline();
                format!(
                    "epoch interruption disabled\n{}",
                    format_fuel_state(&session.vm)
                )
            }
            RunCommand::TickEpoch { amount } => {
                let current = session.vm.increment_epoch_by(amount);
                format!(
                    "epoch advanced by {amount} to {current}\n{}",
                    format_fuel_state(&session.vm)
                )
            }
            RunCommand::SetEpochCheckInterval { interval } => {
                match session.vm.set_epoch_check_interval(interval) {
                    Ok(()) => format!(
                        "epoch check interval set to {interval}\n{}",
                        format_fuel_state(&session.vm)
                    ),
                    Err(err) => format!("epoch interval update failed: {err}"),
                }
            }
            RunCommand::Stop => unreachable!("handled above"),
        };

        let report = session.snapshot(command_output, yielded);
        if report.halted || report.error.is_some() {
            *slot = None;
        }
        report
    })
}

pub fn start_debug_source_with_flavor(
    source: &str,
    flavor: SourceFlavor,
    fuel_config: FuelConfig,
) -> DebugReport {
    let options = embedded_stdlib_compile_options();
    let compiled = match compile_source_with_flavor_and_options(source, flavor, options.clone()) {
        Ok(compiled) => compiled,
        Err(err) => {
            DEBUG_SESSION.with(|state| {
                *state.borrow_mut() = None;
            });
            return DebugReport::source_error(source, flavor, err);
        }
    };
    let diagnostics = lint_success_diagnostics(source, flavor, &compiled, None, &options);

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = match Vm::try_new(compiled.program.with_local_count(compiled.locals)) {
        Ok(vm) => vm,
        Err(err) => {
            // Arena-space exhaustion is terminal for the playground embedding:
            // report the typed error instead of panicking.
            DEBUG_SESSION.with(|state| {
                *state.borrow_mut() = None;
            });
            return DebugReport::inactive(Some(err.to_string()), "debugger initialization failed");
        }
    };
    vm.set_standard_composition(standard_composition());
    if let Err(err) = register_functions(&mut vm, &compiled.functions, &output_lines) {
        DEBUG_SESSION.with(|state| {
            *state.borrow_mut() = None;
        });
        return DebugReport::inactive(Some(err), "debugger initialization failed");
    }
    if let Err(err) = apply_fuel_config(&mut vm, fuel_config) {
        DEBUG_SESSION.with(|state| {
            *state.borrow_mut() = None;
        });
        return DebugReport::inactive(Some(err), "debugger initialization failed");
    }

    let session = DebugSession::new(vm, output_lines, diagnostics);
    let report = session.snapshot("debugger attached".to_string());
    DEBUG_SESSION.with(|state| {
        *state.borrow_mut() = Some(session);
    });
    report
}

pub fn run_debug_command(command: DebugCommand) -> DebugReport {
    if matches!(command, DebugCommand::Stop) {
        DEBUG_SESSION.with(|state| {
            *state.borrow_mut() = None;
        });
        return DebugReport::inactive(None, "debug session stopped");
    }

    DEBUG_SESSION.with(|state| {
        let mut slot = state.borrow_mut();
        let Some(session) = slot.as_mut() else {
            return DebugReport::inactive(
                Some("debug session is not active".to_string()),
                String::new(),
            );
        };
        let command_output = session.run_command(command);
        let report = session.snapshot(command_output);
        if report.halted || report.error.is_some() {
            *slot = None;
        }
        report
    })
}

pub fn debug_state() -> DebugReport {
    DEBUG_SESSION.with(|state| {
        let slot = state.borrow();
        let Some(session) = slot.as_ref() else {
            return DebugReport::inactive(
                Some("debug session is not active".to_string()),
                String::new(),
            );
        };
        session.snapshot(String::new())
    })
}

fn drain_output(lines: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    match lines.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => Vec::new(),
    }
}

fn register_functions(
    vm: &mut Vm,
    functions: &[FunctionDecl],
    print_output: &Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    let lines = Arc::clone(print_output);
    vm.set_runtime_print_sink(move |rendered| {
        push_output_line(&lines, rendered);
    });
    let async_state = functions
        .iter()
        .any(|decl| decl.name == "runtime::sleep")
        .then(|| {
            let state = Arc::new(Mutex::new(BrowserAsyncState::default()));
            vm.set_async_bridge(Box::new(BrowserAsyncBridge::new(Arc::clone(&state))));
            state
        });
    for decl in functions {
        register_named_function(vm, &decl.name, async_state.as_ref())?;
    }
    Ok(())
}

fn register_named_function(
    vm: &mut Vm,
    name: &str,
    async_state: Option<&Arc<Mutex<BrowserAsyncState>>>,
) -> Result<(), String> {
    match name {
        "print" | "println" => {}
        "runtime::sleep" => {
            let Some(_state) = async_state else {
                return Err("runtime::sleep async bridge not initialized".to_string());
            };
            vm.bind_function(
                "runtime::sleep",
                Box::new(PlaygroundRuntimeSleepHostFunction),
            );
        }
        "runtime::exit" => {}
        other => {
            return Err(format!("no host binding for function '{other}'"));
        }
    }
    Ok(())
}

fn push_output_line(lines: &Arc<Mutex<Vec<String>>>, rendered: String) {
    let normalized = rendered.trim_end_matches('\n').to_string();
    if let Ok(mut guard) = lines.lock() {
        guard.push(normalized);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::{
        BrowserAsyncBridge, BrowserAsyncState, FuelState, RunReport, noop_waker, vm_error_info,
    };
    use vm::operation::{OperationError, OperationErrorCode};
    use vm::resource::{ResourceError, ResourceErrorCode};
    use vm::{
        CallReturn, CancellationReason, HostAsyncBridge, HostFuture, HostFutureOutput, HostOpId,
        Value,
    };
    use vm::{RuntimeError, RuntimeErrorCode, VmError};

    fn fuel() -> FuelState {
        FuelState::disabled(1)
    }

    #[test]
    fn run_report_retains_structured_operation_exhaustion_for_json_consumers() {
        let error = OperationError::new(
            OperationErrorCode::OperationRegistryTagExhausted,
            "vm::operation_registry",
            "operation registry tag identity space is exhausted",
        )
        .with_limit(0x00ff_ffff)
        .with_value(0x0100_0000);
        let vm_error = VmError::Operation(error);
        let report = RunReport::runtime_vm_error(None, &vm_error, Vec::new(), Vec::new(), fuel());

        assert_eq!(
            report.error_code.as_deref(),
            Some("operation_registry_tag_exhausted")
        );
        let details = report.error_details.as_ref().expect("structured details");
        assert_eq!(details.operation, "vm::operation_registry");
        assert_eq!(details.limit, Some(0x00ff_ffff));
        assert_eq!(details.value, Some(0x0100_0000));
        let json = serde_json::to_value(details).expect("details serialize");
        assert_eq!(json["operation"], "vm::operation_registry");
        assert_eq!(json["limit"], 0x00ff_ffffu64);
    }

    #[test]
    fn vm_exhaustion_domains_have_distinct_stable_codes() {
        // Arena-ID identity exhaustion of a ResourceTable: dedicated typed
        // variant, classified purely by the enum (never by operation string).
        let arena = VmError::Resource(ResourceError::new(
            ResourceErrorCode::ResourceTableArenaExhausted,
            "resource::table",
            "resource table arena identity space is exhausted",
        ));
        // Ordinary resource slot/id push exhaustion inside an existing table
        // keeps the legacy shared `ResourceIdExhausted` code.
        let resource = VmError::Resource(ResourceError::new(
            ResourceErrorCode::ResourceIdExhausted,
            "resource::push",
            "resource slot identity space is exhausted",
        ));
        let legacy = VmError::LegacyRuntime(RuntimeError::new(
            RuntimeErrorCode::ResourceIdExhausted,
            "legacy::resource_arena",
            "legacy resource identity space is exhausted",
        ));
        let operation = VmError::Operation(OperationError::new(
            OperationErrorCode::OperationIdExhausted,
            "vm::operation_registry",
            "operation identity space is exhausted",
        ));

        assert_eq!(vm_error_info(&arena).0, "resource_arena_id_exhausted");
        assert_eq!(vm_error_info(&resource).0, "resource_id_exhausted");
        assert_eq!(
            vm_error_info(&legacy).0,
            "legacy_runtime_resource_id_exhausted"
        );
        assert_eq!(vm_error_info(&operation).0, "operation_id_exhausted");
        assert_ne!(vm_error_info(&resource).0, vm_error_info(&legacy).0);

        // The arena code is derived from the typed variant, not from the
        // operation string: a `ResourceTableArenaExhausted` error is
        // classified identically regardless of the free-form operation scope,
        // and no operation-string comparison drives the mapping.
        let arena_renamed_operation = VmError::Resource(ResourceError::new(
            ResourceErrorCode::ResourceTableArenaExhausted,
            "some::other::scope",
            "resource table arena identity space is exhausted",
        ));
        assert_eq!(
            vm_error_info(&arena_renamed_operation).0,
            "resource_arena_id_exhausted",
            "arena classification must depend only on the typed variant"
        );
        // And a plain `ResourceIdExhausted` never yields the arena code, even
        // if its operation string happens to look like the arena scope.
        let push_like_table = VmError::Resource(ResourceError::new(
            ResourceErrorCode::ResourceIdExhausted,
            "resource::table",
            "slot identity space is exhausted",
        ));
        assert_eq!(
            vm_error_info(&push_like_table).0,
            "resource_id_exhausted",
            "slot/id exhaustion must not be misclassified as arena exhaustion"
        );
        // The three resource identity-exhaustion domains stay pairwise
        // distinct in their stable codes.
        assert_ne!(vm_error_info(&arena).0, vm_error_info(&resource).0);
        assert_ne!(vm_error_info(&arena).0, vm_error_info(&legacy).0);
    }

    #[test]
    fn source_and_presentation_messages_keep_machine_fields_separate() {
        let report = RunReport::runtime_error(
            "runtime error with user-facing wording".to_string(),
            Vec::new(),
            Vec::new(),
            fuel(),
        );
        assert_eq!(
            report.error.as_deref(),
            Some("runtime error with user-facing wording")
        );
        assert_eq!(report.error_code.as_deref(), Some("runtime_error"));
        assert_eq!(
            report
                .error_details
                .as_ref()
                .map(|details| details.message.as_str()),
            Some("runtime error with user-facing wording")
        );
    }

    /// The browser async bridge must release a completed future the moment its
    /// poll returns `Ready` (success or failure), retaining it only while
    /// `Pending`. Repeated sequential sleeps therefore return the bridge futures
    /// map to zero after each completion instead of accumulating leaked entries.
    #[test]
    fn browser_async_bridge_releases_completed_futures_instead_of_leaking() {
        let state = Arc::new(Mutex::new(BrowserAsyncState::default()));
        let mut bridge = BrowserAsyncBridge::new(Arc::clone(&state));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // A future that resolves Ready immediately on the first poll.
        let ready_future: HostFuture = Box::pin(std::future::ready(Ok(
            HostFutureOutput::returning(CallReturn::one(Value::Bool(true))),
        )));
        let op_id: HostOpId = 1;
        bridge.submit_op(op_id, ready_future).expect("submit");
        assert_eq!(
            state.lock().expect("lock").futures.len(),
            1,
            "pending future is retained"
        );
        match bridge.poll_submitted_op(op_id, &mut cx) {
            Poll::Ready(Ok(_)) => {}
            _ => panic!("expected ready success from the submitted future"),
        }
        assert_eq!(
            state.lock().expect("lock").futures.len(),
            0,
            "a completed future must be removed from the bridge map"
        );
        // The id is now stale: a subsequent poll reports an unknown op rather
        // than re-polling a retained completed future.
        assert!(
            bridge.poll_submitted_op(op_id, &mut cx).is_ready(),
            "a removed op id must immediately resolve as ready/error"
        );

        // Multiple sequential sleeps each return the map to zero after their
        // completion; no completed entry accumulates across sleeps.
        for id in [2u64, 3, 4, 5] {
            let future: HostFuture = Box::pin(std::future::ready(Ok(HostFutureOutput::returning(
                CallReturn::one(Value::Bool(true)),
            ))));
            bridge.submit_op(id, future).expect("submit");
            assert_eq!(state.lock().expect("lock").futures.len(), 1);
            assert!(
                bridge.poll_submitted_op(id, &mut cx).is_ready(),
                "ready future completes on first poll"
            );
            assert_eq!(
                state.lock().expect("lock").futures.len(),
                0,
                "map returns to zero after each completion"
            );
        }
    }

    /// Cancellation also removes the bridge future entry, so the map stays
    /// drained once the operation is cancelled.
    #[test]
    fn browser_async_bridge_cancellation_removes_future_entry() {
        let state = Arc::new(Mutex::new(BrowserAsyncState::default()));
        let mut bridge = BrowserAsyncBridge::new(Arc::clone(&state));
        let op_id: HostOpId = 42;
        let future: HostFuture = Box::pin(std::future::pending());
        bridge.submit_op(op_id, future).expect("submit");
        assert_eq!(state.lock().expect("lock").futures.len(), 1);
        bridge.cancel_op_with_reason(op_id, CancellationReason::Requested);
        assert_eq!(
            state.lock().expect("lock").futures.len(),
            0,
            "cancellation must remove the future entry"
        );
    }
}
