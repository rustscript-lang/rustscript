use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use vm::{
    CallOutcome, FunctionDecl, HostFunction, LocalInfo, PrintHostFunction, PrintlnHostFunction,
    SourceFlavor, SourcePathError, Value, Vm, VmError, VmStatus,
    compile_source_with_flavor_and_options, format_value, render_vm_error,
};

use crate::analyzer::{LintDiagnostic, lint_source_with_flavor};
use crate::stdlib::embedded_stdlib_compile_options;

const MAX_DEBUG_STEPS_PER_COMMAND: usize = 200_000;

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
        name: "add_one",
        arity: 1,
        docs: "Returns input integer + 1.",
    },
    PlaygroundHostFunctionSpec {
        name: "echo",
        arity: 1,
        docs: "Returns the first argument unchanged.",
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
}

impl RunReport {
    pub fn success(output: Vec<String>, stack: Vec<String>) -> Self {
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: None,
        }
    }

    pub fn source_error(source: &str, flavor: SourceFlavor, err: SourcePathError) -> Self {
        let diagnostics = lint_source_with_flavor(source, flavor).diagnostics;
        Self {
            diagnostics,
            output: Vec::new(),
            stack: Vec::new(),
            error: Some(err.to_string()),
        }
    }

    pub fn runtime_error(message: String, output: Vec<String>, stack: Vec<String>) -> Self {
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: Some(message),
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
        }
    }
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
    Error(String),
}

struct DebugSession {
    vm: Vm,
    output_lines: Arc<Mutex<Vec<String>>>,
    line_breakpoints: HashSet<u32>,
    halted: bool,
    error: Option<String>,
}

thread_local! {
    static DEBUG_SESSION: RefCell<Option<DebugSession>> = const { RefCell::new(None) };
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
                if hit_line_breakpoint
                    && let Some(line) = current_line
                {
                    return format!("line breakpoint hit at {line}");
                }
                return String::new();
            }
        }
    }

    fn execute_single_instruction(&mut self) -> StepExecution {
        self.vm.set_fuel(1);
        match self.vm.run() {
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
        return RunReport::runtime_error(err, Vec::new(), Vec::new());
    }

    loop {
        let status = match vm.run() {
            Ok(status) => status,
            Err(err) => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::runtime_error(render_vm_error(&vm, &err), output, stack);
            }
        };
        match status {
            VmStatus::Halted => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::success(output, stack);
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
                );
            }
        }
    }
}

pub fn start_debug_source_with_flavor(source: &str, flavor: SourceFlavor) -> DebugReport {
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
        session.snapshot(Vec::new(), command_output)
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
        "add_one" => vm.bind_function("add_one", Box::new(AddOneFunction)),
        "echo" => vm.bind_function("echo", Box::new(EchoFunction)),
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

struct AddOneFunction;

impl HostFunction for AddOneFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            _ => return Err(VmError::TypeMismatch("int")),
        };
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
    }
}

struct EchoFunction;

impl HostFunction for EchoFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = args.first().cloned().ok_or(VmError::StackUnderflow)?;
        Ok(CallOutcome::Return(vec![value]))
    }
}
