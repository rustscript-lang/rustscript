use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::debug_info::{DebugInfo, LocalInfo};
use crate::vm::{Program, Vm, VmError, VmStatus};

mod recording;
mod replay;
#[cfg(test)]
mod tests;

use self::recording::VmRecordingBuilder;
pub use self::recording::{
    VmRecording, VmRecordingError, VmRecordingFrame, VmRecordingReplayResponse,
    VmRecordingReplayState,
};
use self::replay::*;
pub use self::replay::{replay_recording_stdio, run_recording_replay_command};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepMode {
    Running,
    Step,
    StepOver { depth: usize, ip: usize },
    StepOut { depth: usize },
}

pub struct Debugger {
    breakpoints: HashSet<usize>,
    line_breakpoints: HashSet<u32>,
    step_mode: StepMode,
    server: Option<DebugServer>,
    bridge: Option<DebugCommandBridge>,
    recording: Option<VmRecordingBuilder>,
    client_detached: bool,
}

#[derive(Clone)]
pub struct DebugCommandBridge {
    inner: Arc<DebugCommandBridgeInner>,
}

struct DebugCommandBridgeInner {
    state: Mutex<DebugCommandBridgeState>,
    changed: Condvar,
}

struct DebugCommandBridgeState {
    attached: bool,
    current_line: Option<u32>,
    closed: bool,
    next_request_id: u64,
    pending_request: Option<DebugCommandBridgeRequest>,
    pending_response: Option<DebugCommandBridgeResponseInternal>,
}

struct DebugCommandBridgeRequest {
    request_id: u64,
    command: String,
}

#[derive(Clone, Debug)]
pub struct DebugCommandBridgeResponse {
    pub output: String,
    pub current_line: Option<u32>,
    pub attached: bool,
    pub resumed: bool,
}
pub struct DebugCommandBridgeStatus {
    pub attached: bool,
    pub current_line: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugCommandBridgeError {
    NotAttached,
    Timeout,
    Closed,
}

impl Default for Debugger {
    fn default() -> Self {
        Self::new()
    }
}

impl Debugger {
    pub fn new() -> Self {
        Self {
            breakpoints: HashSet::new(),
            line_breakpoints: HashSet::new(),
            step_mode: StepMode::Running,
            server: None,
            bridge: None,
            recording: None,
            client_detached: false,
        }
    }

    pub fn with_tcp(addr: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(false)?;
        Ok(Self {
            breakpoints: HashSet::new(),
            line_breakpoints: HashSet::new(),
            step_mode: StepMode::Running,
            server: Some(DebugServer::new(listener)),
            bridge: None,
            recording: None,
            client_detached: false,
        })
    }

    pub fn with_command_bridge(bridge: DebugCommandBridge) -> Self {
        Self {
            breakpoints: HashSet::new(),
            line_breakpoints: HashSet::new(),
            step_mode: StepMode::Running,
            server: None,
            bridge: Some(bridge),
            recording: None,
            client_detached: false,
        }
    }

    pub fn with_recording(program: Program) -> Self {
        Self {
            breakpoints: HashSet::new(),
            line_breakpoints: HashSet::new(),
            step_mode: StepMode::Running,
            server: None,
            bridge: None,
            recording: Some(VmRecordingBuilder::new(program)),
            client_detached: false,
        }
    }

    pub fn stop_on_entry(&mut self) {
        self.step_mode = StepMode::Step;
    }

    pub fn add_breakpoint(&mut self, offset: usize) {
        self.breakpoints.insert(offset);
    }

    pub fn remove_breakpoint(&mut self, offset: usize) {
        self.breakpoints.remove(&offset);
    }

    pub fn on_instruction(&mut self, vm: &mut Vm) {
        if let Some(recording) = self.recording.as_mut() {
            recording.record_state(vm);
        }

        let ip = vm.ip();
        let mut should_break = self.breakpoints.contains(&ip);

        if !should_break
            && let Some(line) = current_line(vm)
            && self.line_breakpoints.contains(&line)
        {
            should_break = true;
        }

        if !should_break {
            match self.step_mode {
                StepMode::Step => {
                    should_break = true;
                }
                StepMode::StepOver {
                    depth,
                    ip: start_ip,
                } => {
                    if vm.call_depth() <= depth && ip != start_ip {
                        should_break = true;
                    }
                }
                StepMode::StepOut { depth } => {
                    if vm.call_depth() < depth {
                        should_break = true;
                    }
                }
                StepMode::Running => {}
            }
        }
        if should_break {
            self.step_mode = StepMode::Running;
            self.client_detached = self.repl(vm, None);
        }
    }

    pub fn on_vm_status(&mut self, vm: &Vm, status: VmStatus) {
        if let Some(recording) = self.recording.as_mut() {
            recording.on_terminal_status(vm, status);
        }
    }

    pub fn on_vm_error(&mut self, vm: &mut Vm, err: &VmError) -> bool {
        let banner = match err {
            VmError::OutOfFuel { needed, remaining } => Some(format!(
                "execution interrupted: out of fuel (needed {needed}, remaining {remaining}). use `fuel set <n>` or `fuel add <n>`, then `continue`"
            )),
            VmError::EpochDeadlineReached { current, deadline } => Some(format!(
                "execution interrupted: epoch deadline reached (current {current}, deadline {deadline}). `continue` will re-arm the same deadline automatically; use `epoch tick <n>` to advance the global epoch or `epoch deadline <ticks>` to change the slice size first"
            )),
            _ => None,
        };
        self.client_detached = self.repl(vm, banner.as_deref());
        !self.client_detached
    }

    pub fn take_recording(&mut self) -> Option<VmRecording> {
        self.recording.take().map(VmRecordingBuilder::finish)
    }

    pub fn take_detach_event(&mut self) -> bool {
        std::mem::take(&mut self.client_detached)
    }

    fn repl(&mut self, vm: &mut Vm, banner: Option<&str>) -> bool {
        if let Some(server) = self.server.as_mut() {
            return server.repl(
                vm,
                &mut self.breakpoints,
                &mut self.line_breakpoints,
                &mut self.step_mode,
                banner,
            );
        }
        if let Some(bridge) = self.bridge.as_ref() {
            return bridge.repl(
                vm,
                &mut self.breakpoints,
                &mut self.line_breakpoints,
                &mut self.step_mode,
                banner,
            );
        }
        repl_stdio(
            vm,
            &mut self.breakpoints,
            &mut self.line_breakpoints,
            &mut self.step_mode,
            banner,
        );
        false
    }
}

impl DebugCommandBridge {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DebugCommandBridgeInner {
                state: Mutex::new(DebugCommandBridgeState {
                    attached: false,
                    current_line: None,
                    closed: false,
                    next_request_id: 0,
                    pending_request: None,
                    pending_response: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn status(&self) -> DebugCommandBridgeStatus {
        let state = self
            .inner
            .state
            .lock()
            .expect("debug command bridge lock poisoned");
        DebugCommandBridgeStatus {
            attached: state.attached,
            current_line: state.current_line,
        }
    }

    pub fn close(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("debug command bridge lock poisoned");
        state.closed = true;
        state.attached = false;
        state.current_line = None;
        state.pending_request = None;
        state.pending_response = None;
        self.inner.changed.notify_all();
    }

    pub fn execute(
        &self,
        command: impl Into<String>,
        timeout: Duration,
    ) -> Result<DebugCommandBridgeResponse, DebugCommandBridgeError> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("debug command bridge lock poisoned");
        if state.closed {
            return Err(DebugCommandBridgeError::Closed);
        }
        if !state.attached {
            return Err(DebugCommandBridgeError::NotAttached);
        }

        state.next_request_id = state.next_request_id.saturating_add(1);
        let request_id = state.next_request_id;
        state.pending_request = Some(DebugCommandBridgeRequest {
            request_id,
            command: command.into(),
        });
        self.inner.changed.notify_all();

        let deadline = Instant::now() + timeout;
        loop {
            if state.closed {
                return Err(DebugCommandBridgeError::Closed);
            }
            if let Some(response) = state.pending_response.clone()
                && response.request_id == request_id
            {
                state.pending_response = None;
                return Ok(DebugCommandBridgeResponse {
                    output: response.output,
                    current_line: response.current_line,
                    attached: response.attached,
                    resumed: response.resumed,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(DebugCommandBridgeError::Timeout);
            }
            let wait_for = deadline.saturating_duration_since(now);
            let (next_state, wait_result) = self
                .inner
                .changed
                .wait_timeout(state, wait_for)
                .expect("debug command bridge lock poisoned");
            state = next_state;
            if wait_result.timed_out() {
                return Err(DebugCommandBridgeError::Timeout);
            }
        }
    }

    fn repl(
        &self,
        vm: &mut Vm,
        breakpoints: &mut HashSet<usize>,
        line_breakpoints: &mut HashSet<u32>,
        step: &mut StepMode,
        _banner: Option<&str>,
    ) -> bool {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("debug command bridge lock poisoned");
            state.closed = false;
            state.attached = true;
            state.current_line = current_line(vm);
            state.pending_request = None;
            // Keep any in-flight response visible to execute() to avoid
            // races when the debugger resumes and immediately re-attaches.
            self.inner.changed.notify_all();
        }

        loop {
            let request = {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .expect("debug command bridge lock poisoned");
                while !state.closed && state.pending_request.is_none() {
                    state = self
                        .inner
                        .changed
                        .wait(state)
                        .expect("debug command bridge lock poisoned");
                }
                if state.closed {
                    state.attached = false;
                    state.current_line = None;
                    state.pending_request = None;
                    state.pending_response = None;
                    self.inner.changed.notify_all();
                    return true;
                }
                state
                    .pending_request
                    .take()
                    .expect("debug command request missing")
            };

            let mut output = Vec::<u8>::new();
            let action = handle_command(
                &request.command,
                vm,
                breakpoints,
                line_breakpoints,
                step,
                &mut output,
            );
            let resumed = action.is_break();
            let current_line = if resumed { None } else { current_line(vm) };
            let attached = !resumed;
            let output = String::from_utf8_lossy(&output).to_string();

            let mut state = self
                .inner
                .state
                .lock()
                .expect("debug command bridge lock poisoned");
            state.attached = attached;
            state.current_line = current_line;
            state.pending_response = Some(DebugCommandBridgeResponseInternal {
                request_id: request.request_id,
                output,
                current_line,
                attached,
                resumed,
            });
            self.inner.changed.notify_all();

            if resumed {
                return false;
            }
        }
    }
}

impl Default for DebugCommandBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DebugCommandBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DebugCommandBridgeError::NotAttached => write!(f, "debugger is not attached"),
            DebugCommandBridgeError::Timeout => write!(f, "timed out waiting for debugger"),
            DebugCommandBridgeError::Closed => write!(f, "debugger bridge is closed"),
        }
    }
}

impl std::error::Error for DebugCommandBridgeError {}

#[derive(Clone)]
struct DebugCommandBridgeResponseInternal {
    request_id: u64,
    output: String,
    current_line: Option<u32>,
    attached: bool,
    resumed: bool,
}

struct DebugServer {
    listener: TcpListener,
    stream: Option<TcpStream>,
}

impl DebugServer {
    fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            stream: None,
        }
    }

    fn ensure_client(&mut self) -> io::Result<()> {
        if self.stream.is_none() {
            let (stream, _) = self.listener.accept()?;
            self.stream = Some(stream);
        }
        Ok(())
    }

    fn repl(
        &mut self,
        vm: &mut Vm,
        breakpoints: &mut HashSet<usize>,
        line_breakpoints: &mut HashSet<u32>,
        step: &mut StepMode,
        banner: Option<&str>,
    ) -> bool {
        if self.ensure_client().is_err() {
            return false;
        }
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        let _ = writeln!(stream, "debugger attached. type 'help' for commands");
        if let Some(text) = banner {
            let _ = writeln!(stream, "{text}");
        }
        let Ok(clone) = stream.try_clone() else {
            self.stream = None;
            return true;
        };
        let mut reader = io::BufReader::new(clone);
        loop {
            if write_prompt(stream).is_err() {
                self.stream = None;
                return true;
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    self.stream = None;
                    return true;
                }
                Ok(_) => {}
                Err(_) => {
                    self.stream = None;
                    return true;
                }
            }
            if handle_command(&line, vm, breakpoints, line_breakpoints, step, stream).is_break() {
                return false;
            }
        }
    }
}

fn repl_stdio(
    vm: &mut Vm,
    breakpoints: &mut HashSet<usize>,
    line_breakpoints: &mut HashSet<u32>,
    step: &mut StepMode,
    banner: Option<&str>,
) {
    let stdin = io::stdin();
    let mut input = String::new();
    if let Some(text) = banner {
        println!("{text}");
    }
    loop {
        input.clear();
        print!("(pdb) ");
        let _ = io::stdout().flush();
        if stdin.read_line(&mut input).is_err() {
            break;
        }
        if handle_command(
            &input,
            vm,
            breakpoints,
            line_breakpoints,
            step,
            &mut io::stdout(),
        )
        .is_break()
        {
            break;
        }
    }
}

fn write_prompt(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(b"(pdb) ")?;
    stream.flush()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplAction {
    Continue,
    Break,
}

impl ReplAction {
    fn is_break(self) -> bool {
        matches!(self, ReplAction::Break)
    }
}

fn handle_command(
    line: &str,
    vm: &mut Vm,
    breakpoints: &mut HashSet<usize>,
    line_breakpoints: &mut HashSet<u32>,
    step: &mut StepMode,
    out: &mut dyn Write,
) -> ReplAction {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return ReplAction::Continue;
    };
    match cmd {
        "c" | "continue" => return ReplAction::Break,
        "s" | "step" | "stepi" => {
            *step = StepMode::Step;
            return ReplAction::Break;
        }
        "n" | "next" => {
            *step = StepMode::StepOver {
                depth: vm.call_depth(),
                ip: vm.ip(),
            };
            return ReplAction::Break;
        }
        "finish" | "out" => {
            *step = StepMode::StepOut {
                depth: vm.call_depth(),
            };
            return ReplAction::Break;
        }
        "b" | "break" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(requested_line) = parse_u32(parts.next()) {
                        let line = vm
                            .debug_info()
                            .map(|info| resolve_executable_line(info, requested_line))
                            .unwrap_or(requested_line);
                        line_breakpoints.insert(line);
                        if line == requested_line {
                            let _ = writeln!(out, "line breakpoint set at {line}");
                        } else {
                            let _ = writeln!(
                                out,
                                "line breakpoint set at {line} (requested line {requested_line})"
                            );
                        }
                    } else {
                        let _ = writeln!(out, "usage: break line <number>");
                    }
                    return ReplAction::Continue;
                }
                if let Ok(offset) = arg.parse::<usize>() {
                    breakpoints.insert(offset);
                    let _ = writeln!(out, "breakpoint set at {offset}");
                } else {
                    let _ = writeln!(out, "expected instruction offset");
                }
            } else {
                let _ = writeln!(out, "usage: break <offset>");
            }
        }
        "bl" => {
            if let Some(requested_line) = parse_u32(parts.next()) {
                let line = vm
                    .debug_info()
                    .map(|info| resolve_executable_line(info, requested_line))
                    .unwrap_or(requested_line);
                line_breakpoints.insert(line);
                if line == requested_line {
                    let _ = writeln!(out, "line breakpoint set at {line}");
                } else {
                    let _ = writeln!(
                        out,
                        "line breakpoint set at {line} (requested line {requested_line})"
                    );
                }
            } else {
                let _ = writeln!(out, "usage: bl <line>");
            }
        }
        "clear" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(requested_line) = parse_u32(parts.next()) {
                        let line = vm
                            .debug_info()
                            .map(|info| resolve_executable_line(info, requested_line))
                            .unwrap_or(requested_line);
                        line_breakpoints.remove(&line);
                        if line == requested_line {
                            let _ = writeln!(out, "line breakpoint cleared at {line}");
                        } else {
                            let _ = writeln!(
                                out,
                                "line breakpoint cleared at {line} (requested line {requested_line})"
                            );
                        }
                    } else {
                        let _ = writeln!(out, "usage: clear line <number>");
                    }
                    return ReplAction::Continue;
                }
                if let Ok(offset) = arg.parse::<usize>() {
                    breakpoints.remove(&offset);
                    let _ = writeln!(out, "breakpoint cleared at {offset}");
                } else {
                    let _ = writeln!(out, "expected instruction offset");
                }
            } else {
                let _ = writeln!(out, "usage: clear <offset>");
            }
        }
        "cl" => {
            if let Some(requested_line) = parse_u32(parts.next()) {
                let line = vm
                    .debug_info()
                    .map(|info| resolve_executable_line(info, requested_line))
                    .unwrap_or(requested_line);
                line_breakpoints.remove(&line);
                if line == requested_line {
                    let _ = writeln!(out, "line breakpoint cleared at {line}");
                } else {
                    let _ = writeln!(
                        out,
                        "line breakpoint cleared at {line} (requested line {requested_line})"
                    );
                }
            } else {
                let _ = writeln!(out, "usage: cl <line>");
            }
        }
        "breaks" => {
            let _ = writeln!(out, "breakpoints: {:?}", breakpoints);
            let _ = writeln!(out, "line breakpoints: {:?}", line_breakpoints);
        }
        "stack" => {
            let _ = writeln!(out, "stack: {:?}", vm.stack());
        }
        "locals" => {
            print_locals(vm, out);
        }
        "p" | "print" => {
            if let Some(name) = parts.next() {
                print_local_by_name(vm, name, out);
            } else {
                let _ = writeln!(out, "usage: print <local_name>");
            }
        }
        "ip" => {
            let _ = writeln!(out, "ip: {}", vm.ip());
        }
        "fuel" => {
            let Some(sub) = parts.next() else {
                print_fuel_state(vm, out);
                return ReplAction::Continue;
            };
            match sub {
                "set" => {
                    if let Some(value) = parse_u64(parts.next()) {
                        vm.set_fuel(value);
                        let _ = writeln!(out, "fuel set to {value}");
                        print_fuel_state(vm, out);
                    } else {
                        let _ = writeln!(out, "usage: fuel set <amount>");
                    }
                }
                "add" => {
                    if let Some(value) = parse_u64(parts.next()) {
                        match vm.add_fuel(value) {
                            Ok(()) => {
                                let _ = writeln!(out, "fuel added: {value}");
                                print_fuel_state(vm, out);
                            }
                            Err(err) => {
                                let _ = writeln!(out, "fuel add failed: {err}");
                            }
                        }
                    } else {
                        let _ = writeln!(out, "usage: fuel add <amount>");
                    }
                }
                "clear" => {
                    vm.clear_fuel();
                    let _ = writeln!(out, "fuel metering disabled");
                    print_fuel_state(vm, out);
                }
                "interval" => {
                    if let Some(raw) = parts.next() {
                        match raw.parse::<u32>() {
                            Ok(interval) => match vm.set_fuel_check_interval(interval) {
                                Ok(()) => {
                                    let _ = writeln!(out, "fuel check interval set to {interval}");
                                    print_fuel_state(vm, out);
                                }
                                Err(err) => {
                                    let _ = writeln!(out, "fuel interval update failed: {err}");
                                }
                            },
                            Err(_) => {
                                let _ = writeln!(out, "usage: fuel interval <n>");
                            }
                        }
                    } else {
                        let _ = writeln!(out, "fuel check interval: {}", vm.fuel_check_interval());
                    }
                }
                _ => {
                    let _ = writeln!(
                        out,
                        "usage: fuel | fuel set <amount> | fuel add <amount> | fuel clear | fuel interval [n]"
                    );
                }
            }
        }
        "epoch" => {
            let Some(sub) = parts.next() else {
                print_epoch_state(vm, out);
                return ReplAction::Continue;
            };
            match sub {
                "tick" => {
                    let delta = parse_u64(parts.next()).unwrap_or(1);
                    let current = vm.increment_epoch_by(delta);
                    let _ = writeln!(out, "epoch advanced by {delta} to {current}");
                    print_epoch_state(vm, out);
                }
                "deadline" | "set" => {
                    if let Some(value) = parse_u64(parts.next()) {
                        match vm.set_epoch_deadline(value) {
                            Ok(()) => {
                                let _ = writeln!(
                                    out,
                                    "epoch deadline set {value} ticks beyond current epoch"
                                );
                                print_epoch_state(vm, out);
                            }
                            Err(err) => {
                                let _ = writeln!(out, "epoch deadline update failed: {err}");
                            }
                        }
                    } else {
                        let _ = writeln!(out, "usage: epoch deadline <ticks>");
                    }
                }
                "clear" => {
                    vm.clear_epoch_deadline();
                    let _ = writeln!(out, "epoch interruption disabled");
                    print_epoch_state(vm, out);
                }
                "interval" => {
                    if let Some(raw) = parts.next() {
                        match raw.parse::<u32>() {
                            Ok(interval) => match vm.set_epoch_check_interval(interval) {
                                Ok(()) => {
                                    let _ = writeln!(out, "epoch check interval set to {interval}");
                                    print_epoch_state(vm, out);
                                }
                                Err(err) => {
                                    let _ = writeln!(out, "epoch interval update failed: {err}");
                                }
                            },
                            Err(_) => {
                                let _ = writeln!(out, "usage: epoch interval <n>");
                            }
                        }
                    } else {
                        let _ =
                            writeln!(out, "epoch check interval: {}", vm.epoch_check_interval());
                    }
                }
                _ => {
                    let _ = writeln!(
                        out,
                        "usage: epoch | epoch tick [n] | epoch deadline <ticks> | epoch clear | epoch interval [n]"
                    );
                }
            }
        }
        "where" => {
            if let Some(info) = vm.debug_info() {
                let line = info.line_for_offset(vm.ip());
                if let Some(line) = line {
                    if let Some(text) = info.source_line(line) {
                        let _ = writeln!(out, "line {line}: {text}");
                    } else {
                        let _ = writeln!(out, "line: {line}");
                    }
                } else {
                    let _ = writeln!(out, "line: unknown");
                }
            } else {
                let _ = writeln!(out, "no debug info");
            }
        }
        "funcs" => {
            if let Some(info) = vm.debug_info() {
                for func in &info.functions {
                    let _ = writeln!(out, "fn {}({})", func.name, format_args_list(func));
                }
            } else {
                let _ = writeln!(out, "no debug info");
            }
        }
        "help" => {
            let _ = writeln!(
                out,
                "commands: break, break line, bl, clear, clear line, cl, breaks, continue, step, next, out, stack, locals, print, ip, where, funcs, fuel, epoch, help"
            );
        }
        _ => {
            let _ = writeln!(out, "unknown command");
        }
    }
    ReplAction::Continue
}

fn format_args_list(func: &crate::debug_info::DebugFunction) -> String {
    let mut parts = Vec::new();
    for arg in &func.args {
        parts.push(format!("{}:{}", arg.position, arg.name));
    }
    parts.join(", ")
}

fn print_locals(vm: &Vm, out: &mut dyn Write) {
    let Some(info) = vm.debug_info() else {
        let _ = writeln!(out, "locals: {:?}", vm.locals());
        return;
    };

    if info.locals.is_empty() {
        let _ = writeln!(out, "locals: {:?}", vm.locals());
        return;
    }

    let current_line = info.line_for_offset(vm.ip());
    for local in &info.locals {
        if !local_visible_at_line(local, current_line) {
            continue;
        }
        match vm.locals().get(local.index as usize) {
            Some(value) => {
                let _ = writeln!(out, "{} = {:?}", local.name, value);
            }
            None => {
                let _ = writeln!(out, "{} = <unavailable>", local.name);
            }
        }
    }
}

fn print_local_by_name(vm: &Vm, name: &str, out: &mut dyn Write) {
    let Some(info) = vm.debug_info() else {
        let _ = writeln!(out, "no debug info");
        return;
    };

    let Some(local) = info.locals.iter().find(|local| local.name == name) else {
        let _ = writeln!(out, "unknown local '{name}'");
        return;
    };
    let current_line = info.line_for_offset(vm.ip());
    if !local_visible_at_line(local, current_line) {
        let _ = writeln!(out, "local '{name}' is not visible at this location");
        return;
    }

    match vm.locals().get(local.index as usize) {
        Some(value) => {
            let _ = writeln!(out, "{name} = {:?}", value);
        }
        None => {
            let _ = writeln!(out, "local '{name}' is out of range for this VM instance");
        }
    }
}

fn print_fuel_state(vm: &Vm, out: &mut dyn Write) {
    let remaining = vm
        .get_fuel()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    let _ = writeln!(
        out,
        "fuel: {remaining}, check_interval={}",
        vm.fuel_check_interval()
    );
}

fn print_epoch_state(vm: &Vm, out: &mut dyn Write) {
    let deadline = vm
        .epoch_deadline()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    let slice = vm
        .epoch_deadline_delta()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    let _ = writeln!(
        out,
        "epoch: current={}, deadline={}, slice={}, check_interval={}",
        vm.current_epoch(),
        deadline,
        slice,
        vm.epoch_check_interval()
    );
}

pub fn attach_with_debugger(vm: &mut Vm, debugger: &mut Debugger) {
    debugger.on_instruction(vm);
}

pub fn debug_info_from_vm(vm: &Vm) -> Option<&DebugInfo> {
    vm.debug_info()
}

fn current_line(vm: &Vm) -> Option<u32> {
    vm.debug_info()
        .and_then(|info| info.line_for_offset(vm.ip()))
}

fn parse_u32(token: Option<&str>) -> Option<u32> {
    token.and_then(|value| value.parse::<u32>().ok())
}

fn parse_u64(token: Option<&str>) -> Option<u64> {
    token.and_then(|value| value.parse::<u64>().ok())
}

fn resolve_executable_line(info: &DebugInfo, requested_line: u32) -> u32 {
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
