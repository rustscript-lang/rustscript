use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use vm::{
    FunctionDecl, LocalInfo, PrintHostFunction, PrintlnHostFunction, SourceFlavor,
    SourcePathError, Vm, VmError, VmStatus,
    compile_source_with_flavor_and_options, format_value, render_vm_error,
};

use crate::analyzer::{LintDiagnostic, lint_source_with_flavor};
use crate::stdlib::embedded_stdlib_compile_options;

const MAX_DEBUG_STEPS_PER_COMMAND: usize = 200_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FuelConfig {
    pub fuel: Option<u64>,
    pub fuel_check_interval: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuelState {
    pub enabled: bool,
    pub remaining: Option<u64>,
    pub check_interval: u32,
}

impl FuelState {
    fn disabled(check_interval: u32) -> Self {
        Self {
            enabled: false,
            remaining: None,
            check_interval,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaygroundHostFunctionSpec {
    pub name: &'static str,
    pub arity: usize,
    pub docs: &'static str,
}

const HOST_FUNCTION_SPECS: &[PlaygroundHostFunctionSpec] = &[
    PlaygroundHostFunctionSpec {
        name: "print",
        arity: 1,
        docs: "Writes a value to playground print output.",
    },
    PlaygroundHostFunctionSpec {
        name: "runtime::sleep",
        arity: 1,
        docs: "Sleeps for the requested milliseconds on native runtimes. In the wasm playground it validates the argument and returns immediately.",
    },
];

pub(crate) fn host_function_specs() -> &'static [PlaygroundHostFunctionSpec] {
    HOST_FUNCTION_SPECS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub diagnostics: Vec<LintDiagnostic>,
    pub output: Vec<String>,
    pub stack: Vec<String>,
    pub error: Option<String>,
    pub halted: bool,
    pub yielded: bool,
    pub fuel: FuelState,
    pub command_output: String,
}

impl RunReport {
    pub fn success(output: Vec<String>, stack: Vec<String>, fuel: FuelState) -> Self {
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: None,
            halted: true,
            yielded: false,
            fuel,
            command_output: String::new(),
        }
    }

    pub fn source_error(source: &str, flavor: SourceFlavor, err: SourcePathError) -> Self {
        let diagnostics = lint_source_with_flavor(source, flavor).diagnostics;
        Self {
            diagnostics,
            output: Vec::new(),
            stack: Vec::new(),
            error: Some(err.to_string()),
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
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: Some(message),
            halted: true,
            yielded: false,
            fuel,
            command_output: String::new(),
        }
    }

    fn inactive(error: Option<String>, command_output: impl Into<String>) -> Self {
        Self {
            diagnostics: Vec::new(),
            output: Vec::new(),
            stack: Vec::new(),
            error,
            halted: true,
            yielded: false,
            fuel: FuelState::disabled(1),
            command_output: command_output.into(),
        }
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

struct RunSession {
    vm: Vm,
    output_lines: Arc<Mutex<Vec<String>>>,
    halted: bool,
    error: Option<String>,
}

struct DebugSession {
    vm: Vm,
    output_lines: Arc<Mutex<Vec<String>>>,
    line_breakpoints: HashSet<u32>,
    halted: bool,
    error: Option<String>,
}

thread_local! {
    static RUN_SESSION: RefCell<Option<RunSession>> = const { RefCell::new(None) };
    static DEBUG_SESSION: RefCell<Option<DebugSession>> = const { RefCell::new(None) };
}

impl RunSession {
    fn new(vm: Vm, output_lines: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            vm,
            output_lines,
            halted: false,
            error: None,
        }
    }

    fn snapshot(
        &self,
        diagnostics: Vec<LintDiagnostic>,
        command_output: String,
        yielded: bool,
    ) -> RunReport {
        RunReport {
            diagnostics,
            output: drain_output(&self.output_lines),
            stack: self.vm.stack().iter().map(format_value).collect(),
            error: self.error.clone(),
            halted: self.halted,
            yielded,
            fuel: capture_fuel_state(&self.vm),
            command_output,
        }
    }

    fn resume(&mut self) -> (String, bool) {
        if self.halted {
            return ("program halted".to_string(), false);
        }
        if let Some(error) = self.error.as_ref() {
            return (format!("run session is unavailable: {error}"), false);
        }

        match self.vm.run() {
            Ok(VmStatus::Halted) => {
                self.halted = true;
                ("program halted".to_string(), false)
            }
            Ok(VmStatus::Yielded) => {
                let message = match self.vm.get_fuel() {
                    Some(0) => {
                        "execution interrupted: out of fuel. add more fuel and resume".to_string()
                    }
                    _ => "execution yielded; resume to continue".to_string(),
                };
                (message, true)
            }
            Ok(VmStatus::Waiting(op_id)) => {
                self.halted = true;
                let message = format!(
                    "vm is waiting on host op {op_id}; asynchronous host ops are unavailable in the wasm playground runtime"
                );
                self.error = Some(message.clone());
                (message, false)
            }
            Err(err) => {
                self.halted = true;
                let message = render_vm_error(&self.vm, &err);
                self.error = Some(message.clone());
                (message, false)
            }
        }
    }
}

impl DebugSession {
    fn new(vm: Vm, output_lines: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            vm,
            output_lines,
            line_breakpoints: HashSet::new(),
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
            DebugCommand::Stop => String::new(),
        }
    }

    fn snapshot(&self, diagnostics: Vec<LintDiagnostic>, command_output: String) -> DebugReport {
        let mut breakpoints = self.line_breakpoints.iter().copied().collect::<Vec<_>>();
        breakpoints.sort_unstable();
        DebugReport {
            diagnostics,
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
        let original_fuel = self.vm.fuel_checkpoint();
        let stepped_fuel = match self.prepare_debug_step_fuel() {
            Ok(checkpoint) => checkpoint,
            Err(message) => return StepExecution::Paused(message),
        };

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
            Ok(VmStatus::Waiting(op_id)) => {
                self.halted = true;
                let message = format!(
                    "vm is waiting on host op {op_id}; asynchronous host ops are unavailable in the wasm playground runtime"
                );
                self.error = Some(message.clone());
                StepExecution::Error(message)
            }
            Err(err) => {
                self.halted = true;
                let message = render_vm_error(&self.vm, &err);
                self.error = Some(message.clone());
                StepExecution::Error(message)
            }
        };

        self.vm.restore_fuel(stepped_fuel.unwrap_or(original_fuel));
        outcome
    }

    fn prepare_debug_step_fuel(&mut self) -> Result<Option<vm::FuelCheckpoint>, String> {
        let checkpoint = self.vm.fuel_checkpoint();
        if checkpoint.fuel().is_none() {
            return Ok(None);
        }

        match self.vm.consume_fuel_tick() {
            Ok(()) => {
                let stepped = self.vm.fuel_checkpoint();
                self.vm.restore_fuel(checkpoint);
                Ok(Some(stepped))
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
    FuelState {
        enabled: remaining.is_some(),
        remaining,
        check_interval: vm.fuel_check_interval(),
    }
}

fn format_fuel_state(vm: &Vm) -> String {
    let fuel = capture_fuel_state(vm);
    match fuel.remaining {
        Some(remaining) => format!("fuel: {remaining}, check_interval={}", fuel.check_interval),
        None => format!("fuel: disabled, check_interval={}", fuel.check_interval),
    }
}

fn apply_fuel_config(vm: &mut Vm, config: FuelConfig) -> Result<(), String> {
    if let Some(interval) = config.fuel_check_interval {
        vm.set_fuel_check_interval(interval)
            .map_err(|err| render_vm_error(vm, &err))?;
    }
    if let Some(fuel) = config.fuel {
        vm.set_fuel(fuel);
    }
    Ok(())
}

pub fn run_source_with_flavor(source: &str, flavor: SourceFlavor) -> RunReport {
    let compiled = match compile_source_with_flavor_and_options(
        source,
        flavor,
        embedded_stdlib_compile_options(),
    ) {
        Ok(compiled) => compiled,
        Err(err) => return RunReport::source_error(source, flavor, err),
    };

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    if let Err(err) = register_functions(&mut vm, &compiled.functions, &output_lines) {
        return RunReport::runtime_error(err, Vec::new(), Vec::new(), capture_fuel_state(&vm));
    }

    loop {
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
                return RunReport::success(output, stack, capture_fuel_state(&vm));
            }
            VmStatus::Yielded => {}
            VmStatus::Waiting(op_id) => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::runtime_error(
                    format!(
                        "vm is waiting on host op {op_id}; asynchronous host ops are unavailable in the wasm playground runtime"
                    ),
                    output,
                    stack,
                    capture_fuel_state(&vm),
                );
            }
        }
    }
}

pub fn start_run_source_with_flavor(
    source: &str,
    flavor: SourceFlavor,
    fuel_config: FuelConfig,
) -> RunReport {
    let compiled = match compile_source_with_flavor_and_options(
        source,
        flavor,
        embedded_stdlib_compile_options(),
    ) {
        Ok(compiled) => compiled,
        Err(err) => {
            RUN_SESSION.with(|state| {
                *state.borrow_mut() = None;
            });
            return RunReport::source_error(source, flavor, err);
        }
    };

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
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

    let mut session = RunSession::new(vm, output_lines);
    let (command_output, yielded) = session.resume();
    let report = session.snapshot(Vec::new(), command_output, yielded);
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
                let (output, resumed_yielded) = session.resume();
                yielded = resumed_yielded;
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
            RunCommand::Stop => unreachable!("handled above"),
        };

        let report = session.snapshot(Vec::new(), command_output, yielded);
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
    let compiled = match compile_source_with_flavor_and_options(
        source,
        flavor,
        embedded_stdlib_compile_options(),
    ) {
        Ok(compiled) => compiled,
        Err(err) => {
            DEBUG_SESSION.with(|state| {
                *state.borrow_mut() = None;
            });
            return DebugReport::source_error(source, flavor, err);
        }
    };

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
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

    let session = DebugSession::new(vm, output_lines);
    let report = session.snapshot(Vec::new(), "debugger attached".to_string());
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
        let report = session.snapshot(Vec::new(), command_output);
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
        session.snapshot(Vec::new(), String::new())
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
    for decl in functions {
        register_named_function(vm, &decl.name, print_output)?;
    }
    Ok(())
}

fn register_named_function(
    vm: &mut Vm,
    name: &str,
    print_output: &Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    match name {
        "print" => {
            let lines = Arc::clone(print_output);
            vm.bind_function(
                "print",
                Box::new(PrintHostFunction::new(move |rendered| {
                    push_output_line(&lines, rendered);
                })),
            );
        }
        "println" => {
            let lines = Arc::clone(print_output);
            vm.bind_function(
                "println",
                Box::new(PrintlnHostFunction::new(move |rendered| {
                    push_output_line(&lines, rendered);
                })),
            );
        }
        "runtime::sleep" => {}
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
