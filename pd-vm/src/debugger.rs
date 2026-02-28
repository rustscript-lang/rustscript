use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::debug_info::DebugInfo;
use crate::vm::{Program, Value, Vm, VmStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepMode {
    Running,
    Step,
    StepOver { depth: usize, ip: usize },
    StepOut { depth: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub struct VmRecordingFrame {
    pub ip: usize,
    pub call_depth: usize,
    pub stack: Vec<Value>,
    pub locals: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct VmRecording {
    pub program: Program,
    pub frames: Vec<VmRecordingFrame>,
    pub terminal_status: Option<VmStatus>,
}

#[derive(Clone, Debug, Default)]
pub struct VmRecordingReplayState {
    pub cursor: usize,
    pub offset_breakpoints: HashSet<usize>,
    pub line_breakpoints: HashSet<u32>,
}

#[derive(Clone, Debug)]
pub struct VmRecordingReplayResponse {
    pub output: String,
    pub current_line: Option<u32>,
    pub at_end: bool,
    pub exited: bool,
}

#[derive(Debug)]
pub enum VmRecordingError {
    Io(io::Error),
    Wire(crate::vmbc::WireError),
    InvalidFormat(&'static str),
    Message(String),
}

struct VmRecordingBuilder {
    recording: VmRecording,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

impl VmRecordingFrame {
    fn from_vm(vm: &Vm) -> Self {
        Self {
            ip: vm.ip(),
            call_depth: vm.call_depth(),
            stack: vm.stack().to_vec(),
            locals: vm.locals().to_vec(),
        }
    }
}

impl VmRecordingBuilder {
    fn new(program: Program) -> Self {
        Self {
            recording: VmRecording {
                program,
                frames: Vec::new(),
                terminal_status: None,
            },
        }
    }

    fn record_state(&mut self, vm: &Vm) {
        let frame = VmRecordingFrame::from_vm(vm);
        if self.recording.frames.last() == Some(&frame) {
            return;
        }
        self.recording.frames.push(frame);
    }

    fn on_terminal_status(&mut self, vm: &Vm, status: VmStatus) {
        self.record_state(vm);
        self.recording.terminal_status = Some(status);
    }

    fn finish(self) -> VmRecording {
        self.recording
    }
}

impl VmRecording {
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), VmRecordingError> {
        let bytes = self.encode()?;
        std::fs::write(path, bytes).map_err(VmRecordingError::Io)
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, VmRecordingError> {
        let bytes = std::fs::read(path).map_err(VmRecordingError::Io)?;
        Self::decode(&bytes)
    }

    pub fn encode(&self) -> Result<Vec<u8>, VmRecordingError> {
        const MAGIC: [u8; 4] = *b"PDRC";
        const VERSION: u16 = 1;

        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());

        let program_bytes =
            crate::vmbc::encode_program(&self.program).map_err(VmRecordingError::Wire)?;
        write_u32_len(program_bytes.len(), &mut out)?;
        out.extend_from_slice(&program_bytes);

        let status_tag = match self.terminal_status {
            Some(VmStatus::Halted) => 1u8,
            Some(VmStatus::Yielded) => 2u8,
            None => 0u8,
        };
        out.push(status_tag);

        write_u32_len(self.frames.len(), &mut out)?;
        for frame in &self.frames {
            write_u32_from_usize(frame.ip, &mut out)?;
            write_u32_from_usize(frame.call_depth, &mut out)?;

            write_u32_len(frame.stack.len(), &mut out)?;
            for value in &frame.stack {
                encode_value(value, &mut out)?;
            }

            write_u32_len(frame.locals.len(), &mut out)?;
            for value in &frame.locals {
                encode_value(value, &mut out)?;
            }
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, VmRecordingError> {
        const MAGIC: [u8; 4] = *b"PDRC";
        const VERSION: u16 = 1;

        let mut cursor = RecordingCursor::new(bytes);

        let magic = cursor.read_exact(4)?;
        if magic != MAGIC {
            return Err(VmRecordingError::InvalidFormat("invalid recording magic"));
        }

        let version = cursor.read_u16()?;
        if version != VERSION {
            return Err(VmRecordingError::Message(format!(
                "unsupported recording version {version}"
            )));
        }

        let program_len = cursor.read_u32()? as usize;
        let program_bytes = cursor.read_exact(program_len)?;
        let program = crate::vmbc::decode_program(program_bytes).map_err(VmRecordingError::Wire)?;

        let terminal_status = match cursor.read_u8()? {
            0 => None,
            1 => Some(VmStatus::Halted),
            2 => Some(VmStatus::Yielded),
            _ => {
                return Err(VmRecordingError::InvalidFormat(
                    "invalid terminal status tag",
                ));
            }
        };

        let frame_count = cursor.read_u32()? as usize;
        let mut frames = Vec::with_capacity(frame_count);
        for _ in 0..frame_count {
            let ip = cursor.read_u32()? as usize;
            let call_depth = cursor.read_u32()? as usize;

            let stack_len = cursor.read_u32()? as usize;
            let mut stack = Vec::with_capacity(stack_len);
            for _ in 0..stack_len {
                stack.push(decode_value(&mut cursor)?);
            }

            let locals_len = cursor.read_u32()? as usize;
            let mut locals = Vec::with_capacity(locals_len);
            for _ in 0..locals_len {
                locals.push(decode_value(&mut cursor)?);
            }

            frames.push(VmRecordingFrame {
                ip,
                call_depth,
                stack,
                locals,
            });
        }

        if !cursor.is_eof() {
            return Err(VmRecordingError::InvalidFormat(
                "trailing bytes in recording payload",
            ));
        }

        Ok(Self {
            program,
            frames,
            terminal_status,
        })
    }
}

impl std::fmt::Display for VmRecordingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmRecordingError::Io(err) => write!(f, "{err}"),
            VmRecordingError::Wire(err) => write!(f, "{err}"),
            VmRecordingError::InvalidFormat(message) => write!(f, "{message}"),
            VmRecordingError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for VmRecordingError {}

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

    pub fn on_instruction(&mut self, vm: &Vm) {
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
            self.client_detached = self.repl(vm);
        }
    }

    pub fn on_vm_status(&mut self, vm: &Vm, status: VmStatus) {
        if let Some(recording) = self.recording.as_mut() {
            recording.on_terminal_status(vm, status);
        }
    }

    pub fn take_recording(&mut self) -> Option<VmRecording> {
        self.recording.take().map(VmRecordingBuilder::finish)
    }

    pub fn take_detach_event(&mut self) -> bool {
        std::mem::take(&mut self.client_detached)
    }

    fn repl(&mut self, vm: &Vm) -> bool {
        if let Some(server) = self.server.as_mut() {
            return server.repl(
                vm,
                &mut self.breakpoints,
                &mut self.line_breakpoints,
                &mut self.step_mode,
            );
        }
        if let Some(bridge) = self.bridge.as_ref() {
            return bridge.repl(
                vm,
                &mut self.breakpoints,
                &mut self.line_breakpoints,
                &mut self.step_mode,
            );
        }
        repl_stdio(
            vm,
            &mut self.breakpoints,
            &mut self.line_breakpoints,
            &mut self.step_mode,
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
        vm: &Vm,
        breakpoints: &mut HashSet<usize>,
        line_breakpoints: &mut HashSet<u32>,
        step: &mut StepMode,
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
            state.pending_response = None;
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
        vm: &Vm,
        breakpoints: &mut HashSet<usize>,
        line_breakpoints: &mut HashSet<u32>,
        step: &mut StepMode,
    ) -> bool {
        if self.ensure_client().is_err() {
            return false;
        }
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        let _ = writeln!(stream, "debugger attached. type 'help' for commands");
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
    vm: &Vm,
    breakpoints: &mut HashSet<usize>,
    line_breakpoints: &mut HashSet<u32>,
    step: &mut StepMode,
) {
    let stdin = io::stdin();
    let mut input = String::new();
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
    vm: &Vm,
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
                    if let Some(line) = parse_u32(parts.next()) {
                        line_breakpoints.insert(line);
                        let _ = writeln!(out, "line breakpoint set at {line}");
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
            if let Some(line) = parse_u32(parts.next()) {
                line_breakpoints.insert(line);
                let _ = writeln!(out, "line breakpoint set at {line}");
            } else {
                let _ = writeln!(out, "usage: bl <line>");
            }
        }
        "clear" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(line) = parse_u32(parts.next()) {
                        line_breakpoints.remove(&line);
                        let _ = writeln!(out, "line breakpoint cleared at {line}");
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
            if let Some(line) = parse_u32(parts.next()) {
                line_breakpoints.remove(&line);
                let _ = writeln!(out, "line breakpoint cleared at {line}");
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
                "commands: break, break line, bl, clear, clear line, cl, breaks, continue, step, next, out, stack, locals, print, ip, where, funcs, help"
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

    for local in &info.locals {
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

    let Some(index) = info.local_index(name) else {
        let _ = writeln!(out, "unknown local '{name}'");
        return;
    };

    match vm.locals().get(index as usize) {
        Some(value) => {
            let _ = writeln!(out, "{name} = {:?}", value);
        }
        None => {
            let _ = writeln!(out, "local '{name}' is out of range for this VM instance");
        }
    }
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

pub fn replay_recording_stdio(recording: &VmRecording) {
    if recording.frames.is_empty() {
        println!("recording has no captured frames");
        return;
    }

    let mut cursor = 0usize;
    let mut replay_breakpoints = ReplayBreakpoints::default();
    println!(
        "recording loaded: frames={}, terminal={:?}",
        recording.frames.len(),
        recording.terminal_status
    );
    let _ = write_replay_position(recording, cursor, &mut io::stdout());

    let stdin = io::stdin();
    let mut input = String::new();
    loop {
        input.clear();
        if replay_at_end(recording, cursor) {
            print!("(pdb-rec:end) ");
        } else {
            print!("(pdb-rec) ");
        }
        let _ = io::stdout().flush();
        if stdin.read_line(&mut input).is_err() {
            break;
        }
        if handle_replay_command(
            &input,
            recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut io::stdout(),
        )
        .is_exit()
        {
            break;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayAction {
    Continue,
    Exit,
}

impl ReplayAction {
    fn is_exit(self) -> bool {
        matches!(self, ReplayAction::Exit)
    }
}

fn handle_replay_command(
    line: &str,
    recording: &VmRecording,
    cursor: &mut usize,
    replay_breakpoints: &mut ReplayBreakpoints,
    out: &mut dyn Write,
) -> ReplayAction {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return ReplayAction::Continue;
    };
    match cmd {
        "q" | "quit" | "exit" => return ReplayAction::Exit,
        "c" | "continue" => {
            if let Some(next) =
                replay_breakpoints.next_pause_frame(recording, cursor.saturating_add(1))
            {
                *cursor = next;
                replay_breakpoints.consume_pause_at_frame(recording, *cursor);
            } else {
                *cursor = recording.frames.len().saturating_sub(1);
            }
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "s" | "step" | "stepi" => {
            if *cursor + 1 < recording.frames.len() {
                *cursor += 1;
            }
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "n" | "next" => {
            *cursor = replay_step_over(recording, *cursor);
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "finish" | "out" => {
            *cursor = replay_step_out(recording, *cursor);
            let _ = write_replay_position(recording, *cursor, out);
            return ReplayAction::Continue;
        }
        "b" | "break" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(line) = parse_u32(parts.next()) {
                        replay_breakpoints.line_breakpoints.insert(line);
                        let _ = writeln!(out, "replay pause point set at line {line}");
                    } else {
                        let _ = writeln!(out, "usage: break line <number>");
                    }
                    return ReplayAction::Continue;
                }
                if let Ok(offset) = arg.parse::<usize>() {
                    replay_breakpoints.offset_breakpoints.insert(offset);
                    let _ = writeln!(out, "replay pause point set at offset {offset}");
                } else {
                    let _ = writeln!(out, "expected instruction offset");
                }
            } else {
                let _ = writeln!(out, "usage: break <offset>");
            }
            return ReplayAction::Continue;
        }
        "bl" => {
            if let Some(line) = parse_u32(parts.next()) {
                replay_breakpoints.line_breakpoints.insert(line);
                let _ = writeln!(out, "replay pause point set at line {line}");
            } else {
                let _ = writeln!(out, "usage: bl <line>");
            }
            return ReplayAction::Continue;
        }
        "clear" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(line) = parse_u32(parts.next()) {
                        replay_breakpoints.line_breakpoints.remove(&line);
                        let _ = writeln!(out, "replay pause point cleared at line {line}");
                    } else {
                        let _ = writeln!(out, "usage: clear line <number>");
                    }
                    return ReplayAction::Continue;
                }
                if let Ok(offset) = arg.parse::<usize>() {
                    replay_breakpoints.offset_breakpoints.remove(&offset);
                    let _ = writeln!(out, "replay pause point cleared at offset {offset}");
                } else {
                    let _ = writeln!(out, "expected instruction offset");
                }
            } else {
                let _ = writeln!(out, "usage: clear <offset>");
            }
            return ReplayAction::Continue;
        }
        "cl" => {
            if let Some(line) = parse_u32(parts.next()) {
                replay_breakpoints.line_breakpoints.remove(&line);
                let _ = writeln!(out, "replay pause point cleared at line {line}");
            } else {
                let _ = writeln!(out, "usage: cl <line>");
            }
            return ReplayAction::Continue;
        }
        "breaks" => {
            let _ = writeln!(
                out,
                "replay pause offsets: {:?}",
                replay_breakpoints.offset_breakpoints
            );
            let _ = writeln!(
                out,
                "replay pause lines: {:?}",
                replay_breakpoints.line_breakpoints
            );
            return ReplayAction::Continue;
        }
        _ => {}
    }

    let frame = &recording.frames[*cursor];
    match cmd {
        "stack" => {
            let _ = writeln!(out, "stack: {:?}", frame.stack);
        }
        "locals" => {
            print_replay_locals(recording, frame, out);
        }
        "p" | "print" => {
            if let Some(name) = parts.next() {
                print_replay_local_by_name(recording, frame, name, out);
            } else {
                let _ = writeln!(out, "usage: print <local_name>");
            }
        }
        "ip" => {
            let _ = writeln!(out, "ip: {}", frame.ip);
        }
        "where" => {
            print_replay_where(recording, frame, out);
        }
        "funcs" => {
            if let Some(info) = recording.program.debug.as_ref() {
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
                "commands: break, break line, bl, clear, clear line, cl, breaks, continue, step, next, out, stack, locals, print, ip, where, funcs, help, quit"
            );
        }
        _ => {
            let _ = writeln!(out, "unknown command");
        }
    }
    ReplayAction::Continue
}

fn replay_step_over(recording: &VmRecording, cursor: usize) -> usize {
    if cursor + 1 >= recording.frames.len() {
        return cursor;
    }
    let start = &recording.frames[cursor];
    for index in (cursor + 1)..recording.frames.len() {
        let frame = &recording.frames[index];
        if frame.call_depth <= start.call_depth && frame.ip != start.ip {
            return index;
        }
    }
    recording.frames.len().saturating_sub(1)
}

fn replay_step_out(recording: &VmRecording, cursor: usize) -> usize {
    if cursor + 1 >= recording.frames.len() {
        return cursor;
    }
    let start = &recording.frames[cursor];
    for index in (cursor + 1)..recording.frames.len() {
        let frame = &recording.frames[index];
        if frame.call_depth < start.call_depth {
            return index;
        }
    }
    recording.frames.len().saturating_sub(1)
}

fn write_replay_position(
    recording: &VmRecording,
    cursor: usize,
    out: &mut dyn Write,
) -> io::Result<()> {
    let last = recording.frames.len().saturating_sub(1);
    let frame = &recording.frames[cursor];
    write!(out, "frame {cursor}/{last}")?;
    if replay_at_end(recording, cursor) {
        match recording.terminal_status {
            Some(status) => write!(out, " [end:{status:?}]")?,
            None => write!(out, " [end]")?,
        }
    }
    write!(out, ": ip={} depth={}", frame.ip, frame.call_depth)?;
    if let Some(info) = recording.program.debug.as_ref()
        && let Some(line) = info.line_for_offset(frame.ip)
    {
        if let Some(text) = info.source_line(line) {
            writeln!(out, " line {line}: {text}")?;
            return Ok(());
        }
        writeln!(out, " line: {line}")?;
        return Ok(());
    }
    writeln!(out)?;
    Ok(())
}

fn replay_at_end(recording: &VmRecording, cursor: usize) -> bool {
    cursor + 1 >= recording.frames.len()
}

#[derive(Default)]
struct ReplayBreakpoints {
    offset_breakpoints: HashSet<usize>,
    line_breakpoints: HashSet<u32>,
}

impl ReplayBreakpoints {
    fn next_pause_frame(&self, recording: &VmRecording, start_index: usize) -> Option<usize> {
        if self.offset_breakpoints.is_empty() && self.line_breakpoints.is_empty() {
            return None;
        }
        for index in start_index..recording.frames.len() {
            let frame = &recording.frames[index];
            if self.offset_breakpoints.contains(&frame.ip) {
                return Some(index);
            }
            if let Some(info) = recording.program.debug.as_ref()
                && let Some(line) = info.line_for_offset(frame.ip)
                && self.line_breakpoints.contains(&line)
            {
                return Some(index);
            }
        }
        None
    }

    fn consume_pause_at_frame(&mut self, recording: &VmRecording, frame_index: usize) {
        let Some(frame) = recording.frames.get(frame_index) else {
            return;
        };
        self.offset_breakpoints.remove(&frame.ip);
        if let Some(info) = recording.program.debug.as_ref()
            && let Some(line) = info.line_for_offset(frame.ip)
        {
            self.line_breakpoints.remove(&line);
        }
    }
}

pub fn run_recording_replay_command(
    recording: &VmRecording,
    state: &mut VmRecordingReplayState,
    command: &str,
) -> VmRecordingReplayResponse {
    if recording.frames.is_empty() {
        return VmRecordingReplayResponse {
            output: "recording has no captured frames".to_string(),
            current_line: None,
            at_end: true,
            exited: false,
        };
    }

    if state.cursor >= recording.frames.len() {
        state.cursor = recording.frames.len().saturating_sub(1);
    }

    let mut replay_breakpoints = ReplayBreakpoints {
        offset_breakpoints: state.offset_breakpoints.clone(),
        line_breakpoints: state.line_breakpoints.clone(),
    };
    let mut output = Vec::<u8>::new();
    let action = handle_replay_command(
        command,
        recording,
        &mut state.cursor,
        &mut replay_breakpoints,
        &mut output,
    );

    state.offset_breakpoints = replay_breakpoints.offset_breakpoints;
    state.line_breakpoints = replay_breakpoints.line_breakpoints;
    let current_line = replay_current_line(recording, state.cursor);

    VmRecordingReplayResponse {
        output: String::from_utf8_lossy(&output).to_string(),
        current_line,
        at_end: replay_at_end(recording, state.cursor),
        exited: action.is_exit(),
    }
}

fn replay_current_line(recording: &VmRecording, cursor: usize) -> Option<u32> {
    let frame = recording.frames.get(cursor)?;
    recording
        .program
        .debug
        .as_ref()
        .and_then(|info| info.line_for_offset(frame.ip))
}

fn print_replay_locals(recording: &VmRecording, frame: &VmRecordingFrame, out: &mut dyn Write) {
    let Some(info) = recording.program.debug.as_ref() else {
        let _ = writeln!(out, "locals: {:?}", frame.locals);
        return;
    };

    if info.locals.is_empty() {
        let _ = writeln!(out, "locals: {:?}", frame.locals);
        return;
    }

    for local in &info.locals {
        match frame.locals.get(local.index as usize) {
            Some(value) => {
                let _ = writeln!(out, "{} = {:?}", local.name, value);
            }
            None => {
                let _ = writeln!(out, "{} = <unavailable>", local.name);
            }
        }
    }
}

fn print_replay_local_by_name(
    recording: &VmRecording,
    frame: &VmRecordingFrame,
    name: &str,
    out: &mut dyn Write,
) {
    let Some(info) = recording.program.debug.as_ref() else {
        let _ = writeln!(out, "no debug info");
        return;
    };

    let Some(index) = info.local_index(name) else {
        let _ = writeln!(out, "unknown local '{name}'");
        return;
    };

    match frame.locals.get(index as usize) {
        Some(value) => {
            let _ = writeln!(out, "{name} = {:?}", value);
        }
        None => {
            let _ = writeln!(out, "local '{name}' is out of range for this frame");
        }
    }
}

fn print_replay_where(recording: &VmRecording, frame: &VmRecordingFrame, out: &mut dyn Write) {
    if let Some(info) = recording.program.debug.as_ref() {
        if let Some(line) = info.line_for_offset(frame.ip) {
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

fn write_u32_len(len: usize, out: &mut Vec<u8>) -> Result<(), VmRecordingError> {
    let value = u32::try_from(len)
        .map_err(|_| VmRecordingError::Message(format!("length too large: {len}")))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32_from_usize(value: usize, out: &mut Vec<u8>) -> Result<(), VmRecordingError> {
    let value = u32::try_from(value)
        .map_err(|_| VmRecordingError::Message(format!("value too large: {value}")))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn encode_value(value: &Value, out: &mut Vec<u8>) -> Result<(), VmRecordingError> {
    match value {
        Value::Null => out.push(0),
        Value::Int(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Float(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        Value::Bool(value) => {
            out.push(3);
            out.push(u8::from(*value));
        }
        Value::String(value) => {
            out.push(4);
            write_u32_len(value.len(), out)?;
            out.extend_from_slice(value.as_bytes());
        }
        Value::Array(values) => {
            out.push(5);
            write_u32_len(values.len(), out)?;
            for value in values {
                encode_value(value, out)?;
            }
        }
        Value::Map(entries) => {
            out.push(6);
            write_u32_len(entries.len(), out)?;
            for (key, value) in entries {
                encode_value(key, out)?;
                encode_value(value, out)?;
            }
        }
    }
    Ok(())
}

fn decode_value(cursor: &mut RecordingCursor<'_>) -> Result<Value, VmRecordingError> {
    let tag = cursor.read_u8()?;
    match tag {
        0 => Ok(Value::Null),
        1 => Ok(Value::Int(cursor.read_i64()?)),
        2 => Ok(Value::Float(cursor.read_f64()?)),
        3 => match cursor.read_u8()? {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            _ => Err(VmRecordingError::InvalidFormat("invalid bool value")),
        },
        4 => {
            let len = cursor.read_u32()? as usize;
            let bytes = cursor.read_exact(len)?;
            let text = String::from_utf8(bytes.to_vec())
                .map_err(|_| VmRecordingError::InvalidFormat("invalid utf-8 string"))?;
            Ok(Value::String(text))
        }
        5 => {
            let len = cursor.read_u32()? as usize;
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(decode_value(cursor)?);
            }
            Ok(Value::Array(values))
        }
        6 => {
            let len = cursor.read_u32()? as usize;
            let mut entries = Vec::with_capacity(len);
            for _ in 0..len {
                let key = decode_value(cursor)?;
                let value = decode_value(cursor)?;
                entries.push((key, value));
            }
            Ok(Value::Map(entries))
        }
        _ => Err(VmRecordingError::InvalidFormat("invalid value tag")),
    }
}

struct RecordingCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> RecordingCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_eof(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], VmRecordingError> {
        if self.offset.saturating_add(len) > self.bytes.len() {
            return Err(VmRecordingError::InvalidFormat(
                "unexpected end of recording",
            ));
        }
        let start = self.offset;
        self.offset += len;
        Ok(&self.bytes[start..self.offset])
    }

    fn read_u8(&mut self) -> Result<u8, VmRecordingError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, VmRecordingError> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, VmRecordingError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i64(&mut self) -> Result<i64, VmRecordingError> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_f64(&mut self) -> Result<f64, VmRecordingError> {
        let bytes = self.read_exact(8)?;
        Ok(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::debug_info::{DebugInfo, LocalInfo};
    use crate::vm::{Program, Value, Vm, VmStatus};

    use super::{
        Debugger, ReplAction, ReplayBreakpoints, StepMode, VmRecording, VmRecordingFrame,
        handle_command, handle_replay_command,
    };

    fn vm_with_named_local(name: &str, value: Value) -> Vm {
        let program = Program::with_debug(
            vec![value],
            vec![
                crate::vm::OpCode::Ldc as u8,
                0,
                0,
                0,
                0,
                crate::vm::OpCode::Stloc as u8,
                0,
                crate::vm::OpCode::Ret as u8,
            ],
            Some(DebugInfo {
                source: None,
                lines: vec![],
                functions: vec![],
                locals: vec![LocalInfo {
                    name: name.to_string(),
                    index: 0,
                }],
            }),
        );
        let mut vm = Vm::with_locals(program, 1);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, crate::vm::VmStatus::Halted);
        vm
    }

    fn vm_with_named_unassigned_local(name: &str) -> Vm {
        let program = Program::with_debug(
            vec![],
            vec![crate::vm::OpCode::Ret as u8],
            Some(DebugInfo {
                source: None,
                lines: vec![],
                functions: vec![],
                locals: vec![LocalInfo {
                    name: name.to_string(),
                    index: 0,
                }],
            }),
        );
        Vm::with_locals(program, 1)
    }

    #[test]
    fn print_local_by_name_uses_debug_name() {
        let vm = vm_with_named_local("counter", Value::Int(42));
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        let action = handle_command(
            "print counter",
            &vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(action, ReplAction::Continue);
        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("counter = Int(42)"));
    }

    #[test]
    fn print_local_by_name_reports_unknown_local() {
        let vm = vm_with_named_local("counter", Value::Int(42));
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        handle_command(
            "p missing",
            &vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("unknown local 'missing'"));
    }

    #[test]
    fn print_local_by_name_shows_null_for_unassigned_local() {
        let vm = vm_with_named_unassigned_local("counter");
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        handle_command(
            "p counter",
            &vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("counter = Null"));
    }

    #[test]
    fn recording_encode_decode_roundtrip() {
        let program = Program::with_debug(
            vec![Value::Int(1)],
            vec![crate::vm::OpCode::Ret as u8],
            Some(DebugInfo {
                source: Some("let x = 1;".to_string()),
                lines: vec![crate::debug_info::LineInfo { offset: 0, line: 1 }],
                functions: vec![],
                locals: vec![LocalInfo {
                    name: "x".to_string(),
                    index: 0,
                }],
            }),
        );
        let recording = VmRecording {
            program: program.clone(),
            frames: vec![
                VmRecordingFrame {
                    ip: 0,
                    call_depth: 0,
                    stack: vec![Value::Array(vec![Value::Int(7), Value::Bool(true)])],
                    locals: vec![Value::Map(vec![(
                        Value::String("k".to_string()),
                        Value::Int(9),
                    )])],
                },
                VmRecordingFrame {
                    ip: 1,
                    call_depth: 0,
                    stack: vec![Value::Int(42)],
                    locals: vec![Value::Int(42)],
                },
            ],
            terminal_status: Some(VmStatus::Halted),
        };

        let bytes = recording.encode().expect("encode should succeed");
        let decoded = VmRecording::decode(&bytes).expect("decode should succeed");

        assert_eq!(decoded.frames, recording.frames);
        assert_eq!(decoded.terminal_status, recording.terminal_status);
        assert_eq!(decoded.program.code, program.code);
        assert_eq!(decoded.program.constants, program.constants);
        assert_eq!(decoded.program.imports, program.imports);
        assert_eq!(decoded.program.debug, program.debug);
    }

    #[test]
    fn recording_debugger_captures_initial_and_terminal_frames() {
        let program = Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]);
        let mut vm = Vm::new(program.clone());
        let mut debugger = Debugger::with_recording(program);

        let status = vm
            .run_with_debugger(&mut debugger)
            .expect("recorded run should succeed");
        assert_eq!(status, VmStatus::Halted);

        let recording = debugger
            .take_recording()
            .expect("recording should be available");
        assert!(recording.frames.len() >= 2);
        assert_eq!(recording.frames.first().expect("first frame").ip, 0);
        assert_eq!(recording.terminal_status, Some(VmStatus::Halted));
    }

    #[test]
    fn replay_break_sets_pause_point_for_continue() {
        let recording = VmRecording {
            program: Program::new(vec![], vec![]),
            frames: vec![
                VmRecordingFrame {
                    ip: 0,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 1,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 2,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
            ],
            terminal_status: Some(VmStatus::Halted),
        };
        let mut cursor = 0usize;
        let mut replay_breakpoints = ReplayBreakpoints::default();
        let mut out = Vec::<u8>::new();

        handle_replay_command(
            "break 1",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        out.clear();
        handle_replay_command(
            "continue",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("frame 1/2"));
        assert_eq!(cursor, 1);
    }

    #[test]
    fn replay_continue_marks_end_of_recording() {
        let recording = VmRecording {
            program: Program::new(vec![], vec![]),
            frames: vec![
                VmRecordingFrame {
                    ip: 0,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 1,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
            ],
            terminal_status: Some(VmStatus::Halted),
        };
        let mut cursor = 0usize;
        let mut replay_breakpoints = ReplayBreakpoints::default();
        let mut out = Vec::<u8>::new();

        handle_replay_command(
            "continue",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );

        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("[end:Halted]"));
        assert_eq!(cursor, 1);
    }

    #[test]
    fn replay_break_line_sets_pause_point_for_continue() {
        let program = Program::with_debug(
            vec![],
            vec![],
            Some(DebugInfo {
                source: None,
                lines: vec![
                    crate::debug_info::LineInfo { offset: 0, line: 1 },
                    crate::debug_info::LineInfo { offset: 5, line: 2 },
                ],
                functions: vec![],
                locals: vec![],
            }),
        );
        let recording = VmRecording {
            program,
            frames: vec![
                VmRecordingFrame {
                    ip: 0,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 5,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 9,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
            ],
            terminal_status: Some(VmStatus::Halted),
        };
        let mut cursor = 0usize;
        let mut replay_breakpoints = ReplayBreakpoints::default();
        let mut out = Vec::<u8>::new();

        handle_replay_command(
            "break line 2",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        out.clear();
        handle_replay_command(
            "continue",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );

        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("frame 1/2"));
        assert_eq!(cursor, 1);
    }

    #[test]
    fn public_replay_api_updates_cursor_and_reports_line() {
        let program = Program::with_debug(
            vec![],
            vec![],
            Some(DebugInfo {
                source: None,
                lines: vec![crate::debug_info::LineInfo {
                    offset: 0,
                    line: 11,
                }],
                functions: vec![],
                locals: vec![],
            }),
        );
        let recording = VmRecording {
            program,
            frames: vec![
                VmRecordingFrame {
                    ip: 0,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 5,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
            ],
            terminal_status: Some(VmStatus::Halted),
        };
        let mut state = super::VmRecordingReplayState::default();

        let response = super::run_recording_replay_command(&recording, &mut state, "step");
        assert!(!response.exited);
        assert_eq!(state.cursor, 1);
        assert!(response.at_end);
        assert_eq!(response.current_line, Some(11));
    }
}
