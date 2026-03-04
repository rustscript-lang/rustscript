use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::debug_info::{DebugInfo, LocalInfo};
use crate::vm::{Program, Value, Vm, VmError, VmStatus};

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
        const VERSION: u16 = 2;

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
            Some(VmStatus::Waiting(_)) => 3u8,
            None => 0u8,
        };
        out.push(status_tag);
        if let Some(VmStatus::Waiting(op_id)) = self.terminal_status {
            out.extend_from_slice(&op_id.to_le_bytes());
        }

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
        const VERSION_LEGACY: u16 = 1;
        const VERSION: u16 = 2;

        let mut cursor = RecordingCursor::new(bytes);

        let magic = cursor.read_exact(4)?;
        if magic != MAGIC {
            return Err(VmRecordingError::InvalidFormat("invalid recording magic"));
        }

        let version = cursor.read_u16()?;
        if version != VERSION && version != VERSION_LEGACY {
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
            3 if version >= VERSION => {
                let op_id = cursor.read_u64()?;
                Some(VmStatus::Waiting(op_id))
            }
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
                "commands: break, break line, bl, clear, clear line, cl, breaks, continue, step, next, out, stack, locals, print, ip, where, funcs, fuel, help"
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
                    if let Some(requested_line) = parse_u32(parts.next()) {
                        let line = recording
                            .program
                            .debug
                            .as_ref()
                            .map(|info| resolve_executable_line(info, requested_line))
                            .unwrap_or(requested_line);
                        replay_breakpoints.line_breakpoints.insert(line);
                        if line == requested_line {
                            let _ = writeln!(out, "replay pause point set at line {line}");
                        } else {
                            let _ = writeln!(
                                out,
                                "replay pause point set at line {line} (requested line {requested_line})"
                            );
                        }
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
            if let Some(requested_line) = parse_u32(parts.next()) {
                let line = recording
                    .program
                    .debug
                    .as_ref()
                    .map(|info| resolve_executable_line(info, requested_line))
                    .unwrap_or(requested_line);
                replay_breakpoints.line_breakpoints.insert(line);
                if line == requested_line {
                    let _ = writeln!(out, "replay pause point set at line {line}");
                } else {
                    let _ = writeln!(
                        out,
                        "replay pause point set at line {line} (requested line {requested_line})"
                    );
                }
            } else {
                let _ = writeln!(out, "usage: bl <line>");
            }
            return ReplayAction::Continue;
        }
        "clear" => {
            if let Some(arg) = parts.next() {
                if arg == "line" {
                    if let Some(requested_line) = parse_u32(parts.next()) {
                        let line = recording
                            .program
                            .debug
                            .as_ref()
                            .map(|info| resolve_executable_line(info, requested_line))
                            .unwrap_or(requested_line);
                        replay_breakpoints.line_breakpoints.remove(&line);
                        if line == requested_line {
                            let _ = writeln!(out, "replay pause point cleared at line {line}");
                        } else {
                            let _ = writeln!(
                                out,
                                "replay pause point cleared at line {line} (requested line {requested_line})"
                            );
                        }
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
            if let Some(requested_line) = parse_u32(parts.next()) {
                let line = recording
                    .program
                    .debug
                    .as_ref()
                    .map(|info| resolve_executable_line(info, requested_line))
                    .unwrap_or(requested_line);
                replay_breakpoints.line_breakpoints.remove(&line);
                if line == requested_line {
                    let _ = writeln!(out, "replay pause point cleared at line {line}");
                } else {
                    let _ = writeln!(
                        out,
                        "replay pause point cleared at line {line} (requested line {requested_line})"
                    );
                }
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

    let current_line = info.line_for_offset(frame.ip);
    for local in &info.locals {
        if !local_visible_at_line(local, current_line) {
            continue;
        }
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

    let Some(local) = info.locals.iter().find(|local| local.name == name) else {
        let _ = writeln!(out, "unknown local '{name}'");
        return;
    };
    let current_line = info.line_for_offset(frame.ip);
    if !local_visible_at_line(local, current_line) {
        let _ = writeln!(out, "local '{name}' is not visible in this frame");
        return;
    }

    match frame.locals.get(local.index as usize) {
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

    fn read_u64(&mut self) -> Result<u64, VmRecordingError> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
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
    use std::time::{Duration, Instant};

    use crate::debug_info::{DebugInfo, LocalInfo};
    use crate::vm::{Program, Value, Vm, VmStatus};

    use super::{
        DebugCommandBridgeError, Debugger, ReplAction, ReplayBreakpoints, StepMode, VmRecording,
        VmRecordingFrame, handle_command, handle_replay_command,
    };

    fn wait_for_bridge_attachment(bridge: &super::DebugCommandBridge, attached: bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if bridge.status().attached == attached {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for debugger attached={attached}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

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
                    declared_line: None,
                    last_line: None,
                }],
            }),
        );
        let mut vm = Vm::new(program.with_local_count(1));
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
                    declared_line: None,
                    last_line: None,
                }],
            }),
        );
        Vm::new(program.with_local_count(1))
    }

    fn vm_with_scoped_named_locals() -> Vm {
        let program = Program::with_debug(
            vec![Value::Int(1)],
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
                lines: vec![crate::debug_info::LineInfo { offset: 0, line: 1 }],
                functions: vec![],
                locals: vec![
                    LocalInfo {
                        name: "a".to_string(),
                        index: 0,
                        declared_line: Some(1),
                        last_line: Some(1),
                    },
                    LocalInfo {
                        name: "b".to_string(),
                        index: 0,
                        declared_line: Some(2),
                        last_line: Some(3),
                    },
                ],
            }),
        );
        let mut vm = Vm::new(program.with_local_count(1));
        let status = vm.run().expect("vm should run");
        assert_eq!(status, crate::vm::VmStatus::Halted);
        vm
    }

    #[test]
    fn print_local_by_name_uses_debug_name() {
        let mut vm = vm_with_named_local("counter", Value::Int(42));
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        let action = handle_command(
            "print counter",
            &mut vm,
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
        let mut vm = vm_with_named_local("counter", Value::Int(42));
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        handle_command(
            "p missing",
            &mut vm,
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
        let mut vm = vm_with_named_unassigned_local("counter");
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        handle_command(
            "p counter",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("counter = Null"));
    }

    #[test]
    fn locals_command_filters_by_debug_line_visibility() {
        let mut vm = vm_with_scoped_named_locals();
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        handle_command(
            "locals",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );

        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("a = Int(1)"));
        assert!(!text.contains("b = "));
    }

    #[test]
    fn print_local_by_name_reports_not_visible_when_outside_debug_window() {
        let mut vm = vm_with_scoped_named_locals();
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        handle_command(
            "p b",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );

        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(text.contains("local 'b' is not visible"));
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
                    declared_line: None,
                    last_line: None,
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
    fn replay_break_line_on_non_executable_source_line_resolves_forward() {
        let program = Program::with_debug(
            vec![],
            vec![],
            Some(DebugInfo {
                source: None,
                lines: vec![
                    crate::debug_info::LineInfo { offset: 0, line: 8 },
                    crate::debug_info::LineInfo {
                        offset: 10,
                        line: 13,
                    },
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
                    ip: 10,
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
            "break line 11",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        let set_text = String::from_utf8(out.clone()).expect("output should be utf-8");
        assert!(
            set_text.contains("line 13 (requested line 11)"),
            "expected resolved-line message, got: {set_text}"
        );

        out.clear();
        handle_replay_command(
            "continue",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        assert_eq!(
            cursor, 1,
            "continue should pause at resolved executable line"
        );
    }

    #[test]
    fn replay_offset_breakpoint_persists_until_cleared() {
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
                VmRecordingFrame {
                    ip: 1,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 3,
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
        assert_eq!(
            cursor, 1,
            "first continue should stop at the first matching ip"
        );

        out.clear();
        handle_replay_command(
            "continue",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        assert_eq!(
            cursor, 3,
            "second continue should stop at the next matching ip while breakpoint is still set"
        );
    }

    #[test]
    fn replay_line_breakpoint_persists_until_cleared() {
        let program = Program::with_debug(
            vec![],
            vec![],
            Some(DebugInfo {
                source: None,
                lines: vec![
                    crate::debug_info::LineInfo { offset: 0, line: 1 },
                    crate::debug_info::LineInfo { offset: 1, line: 7 },
                    crate::debug_info::LineInfo { offset: 2, line: 2 },
                    crate::debug_info::LineInfo { offset: 3, line: 7 },
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
                VmRecordingFrame {
                    ip: 3,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 4,
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
            "break line 7",
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
        assert_eq!(
            cursor, 1,
            "first continue should stop at the first matching line"
        );

        out.clear();
        handle_replay_command(
            "continue",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        assert_eq!(
            cursor, 3,
            "second continue should stop at the next matching line while breakpoint is still set"
        );
    }

    #[test]
    fn bridge_continue_stops_again_at_line_breakpoint() {
        let source = "let a = 1;\nlet b = 2;\nlet c = 3;\n".to_string();
        let program = Program::with_debug(
            vec![Value::Int(1), Value::Int(2), Value::Int(3)],
            vec![
                crate::vm::OpCode::Ldc as u8,
                0,
                0,
                0,
                0,
                crate::vm::OpCode::Stloc as u8,
                0,
                crate::vm::OpCode::Ldc as u8,
                1,
                0,
                0,
                0,
                crate::vm::OpCode::Stloc as u8,
                1,
                crate::vm::OpCode::Ldc as u8,
                2,
                0,
                0,
                0,
                crate::vm::OpCode::Stloc as u8,
                2,
                crate::vm::OpCode::Ret as u8,
            ],
            Some(DebugInfo {
                source: Some(source),
                lines: vec![
                    crate::debug_info::LineInfo { offset: 0, line: 1 },
                    crate::debug_info::LineInfo { offset: 7, line: 2 },
                    crate::debug_info::LineInfo {
                        offset: 14,
                        line: 3,
                    },
                    crate::debug_info::LineInfo {
                        offset: 19,
                        line: 4,
                    },
                ],
                functions: vec![],
                locals: vec![
                    LocalInfo {
                        name: "a".to_string(),
                        index: 0,
                        declared_line: None,
                        last_line: None,
                    },
                    LocalInfo {
                        name: "b".to_string(),
                        index: 1,
                        declared_line: None,
                        last_line: None,
                    },
                    LocalInfo {
                        name: "c".to_string(),
                        index: 2,
                        declared_line: None,
                        last_line: None,
                    },
                ],
            }),
        );
        let bridge = super::DebugCommandBridge::new();
        let mut debugger = Debugger::with_command_bridge(bridge.clone());
        debugger.stop_on_entry();

        let join = std::thread::spawn(move || {
            let mut vm = Vm::new(program.with_local_count(3));
            vm.run_with_debugger(&mut debugger)
                .expect("debugged vm run should succeed")
        });

        wait_for_bridge_attachment(&bridge, true);
        let set_response = bridge
            .execute("break line 3", Duration::from_millis(200))
            .expect("set breakpoint command should succeed");
        assert!(set_response.attached);

        let continue_response = bridge
            .execute("continue", Duration::from_millis(200))
            .expect("continue command should succeed");
        assert!(continue_response.resumed);
        assert!(!continue_response.attached);

        wait_for_bridge_attachment(&bridge, true);
        let where_response = bridge
            .execute("where", Duration::from_millis(200))
            .expect("where command should succeed");
        assert!(where_response.attached);
        assert!(
            where_response.output.contains("line 3"),
            "expected to pause at line 3, got: {}",
            where_response.output
        );

        let final_continue = bridge
            .execute("continue", Duration::from_millis(200))
            .expect("final continue should succeed");
        assert!(final_continue.resumed);
        assert!(!final_continue.attached);

        let status = join.join().expect("debugger thread should join");
        assert_eq!(status, VmStatus::Halted);
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

    #[test]
    fn handle_command_next_and_out_set_expected_step_modes() {
        let mut vm = Vm::new(Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]));
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        let action = handle_command(
            "next",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(action, ReplAction::Break);
        assert!(
            matches!(step_mode, StepMode::StepOver { depth: 0, ip: 0 }),
            "unexpected step mode after next: {step_mode:?}"
        );

        let action = handle_command(
            "out",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(action, ReplAction::Break);
        assert!(
            matches!(step_mode, StepMode::StepOut { depth: 0 }),
            "unexpected step mode after out: {step_mode:?}"
        );
    }

    #[test]
    fn break_line_on_non_executable_source_line_resolves_forward() {
        let mut vm = Vm::new(Program::with_debug(
            vec![],
            vec![crate::vm::OpCode::Nop as u8, crate::vm::OpCode::Ret as u8],
            Some(DebugInfo {
                source: None,
                lines: vec![
                    crate::debug_info::LineInfo { offset: 0, line: 8 },
                    crate::debug_info::LineInfo {
                        offset: 1,
                        line: 13,
                    },
                ],
                functions: vec![],
                locals: vec![],
            }),
        ));
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        let action = handle_command(
            "break line 11",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(action, ReplAction::Continue);
        assert!(
            line_breakpoints.contains(&13),
            "breakpoint should resolve to next executable line"
        );
        assert!(
            !line_breakpoints.contains(&11),
            "raw non-executable line should not remain as unresolved breakpoint"
        );
        let text = String::from_utf8(out).expect("output should be utf-8");
        assert!(
            text.contains("line breakpoint set at 13 (requested line 11)"),
            "expected resolved-line message, got: {text}"
        );
    }

    #[test]
    fn handle_command_fuel_queries_and_updates_budget() {
        let mut vm = Vm::new(Program::new(vec![], vec![crate::vm::OpCode::Ret as u8]));
        vm.set_fuel(9);
        let mut out = Vec::<u8>::new();
        let mut breakpoints = HashSet::new();
        let mut line_breakpoints = HashSet::new();
        let mut step_mode = StepMode::Running;

        let action = handle_command(
            "fuel",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(action, ReplAction::Continue);
        let text = String::from_utf8(out.clone()).expect("output should be utf-8");
        assert!(text.contains("fuel: 9"), "{text}");

        out.clear();
        handle_command(
            "fuel set 4",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(vm.get_fuel(), Some(4));
        let text = String::from_utf8(out.clone()).expect("output should be utf-8");
        assert!(text.contains("fuel set to 4"), "{text}");

        out.clear();
        handle_command(
            "fuel interval 5",
            &mut vm,
            &mut breakpoints,
            &mut line_breakpoints,
            &mut step_mode,
            &mut out,
        );
        assert_eq!(vm.fuel_check_interval(), 5);
    }

    #[test]
    fn debugger_bridge_can_recover_from_out_of_fuel_by_adding_fuel() {
        let program = Program::new(
            vec![],
            vec![
                crate::vm::OpCode::Nop as u8,
                crate::vm::OpCode::Nop as u8,
                crate::vm::OpCode::Ret as u8,
            ],
        );
        let bridge = super::DebugCommandBridge::new();
        let mut debugger = Debugger::with_command_bridge(bridge.clone());

        let join = std::thread::spawn(move || {
            let mut vm = Vm::new(program);
            vm.set_fuel(1);
            vm.run_with_debugger(&mut debugger)
                .expect("debugged vm run should recover and succeed")
        });

        wait_for_bridge_attachment(&bridge, true);

        let fuel_response = bridge
            .execute("fuel", Duration::from_millis(200))
            .expect("fuel command should succeed");
        assert!(
            fuel_response.output.contains("fuel: 0"),
            "{}",
            fuel_response.output
        );

        let add_response = bridge
            .execute("fuel add 2", Duration::from_millis(200))
            .expect("fuel add command should succeed");
        assert!(
            add_response.output.contains("fuel added: 2"),
            "{}",
            add_response.output
        );

        let continue_response = bridge
            .execute("continue", Duration::from_millis(200))
            .expect("continue command should succeed");
        assert!(continue_response.resumed);
        assert!(!continue_response.attached);

        let status = join.join().expect("debugger thread should join");
        assert_eq!(status, VmStatus::Halted);
    }

    #[test]
    fn replay_next_and_out_follow_call_depth_transitions() {
        let recording = VmRecording {
            program: Program::new(vec![], vec![]),
            frames: vec![
                VmRecordingFrame {
                    ip: 10,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 10,
                    call_depth: 1,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 11,
                    call_depth: 1,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 10,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
                VmRecordingFrame {
                    ip: 12,
                    call_depth: 0,
                    stack: vec![],
                    locals: vec![],
                },
            ],
            terminal_status: Some(VmStatus::Halted),
        };
        let mut replay_breakpoints = ReplayBreakpoints::default();
        let mut out = Vec::<u8>::new();

        let mut cursor = 0usize;
        handle_replay_command(
            "next",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        assert_eq!(cursor, 4, "next should step over nested call frames");

        out.clear();
        cursor = 1;
        handle_replay_command(
            "out",
            &recording,
            &mut cursor,
            &mut replay_breakpoints,
            &mut out,
        );
        assert_eq!(cursor, 3, "out should stop when call depth decreases");
    }

    #[test]
    fn replay_api_reports_empty_recording_without_crashing() {
        let recording = VmRecording {
            program: Program::new(vec![], vec![]),
            frames: vec![],
            terminal_status: None,
        };
        let mut state = super::VmRecordingReplayState::default();

        let response = super::run_recording_replay_command(&recording, &mut state, "continue");
        assert_eq!(response.output, "recording has no captured frames");
        assert!(response.at_end);
        assert!(!response.exited);
        assert_eq!(response.current_line, None);
    }

    #[test]
    fn debug_command_bridge_reports_not_attached_timeout_and_closed_states() {
        let bridge = super::DebugCommandBridge::new();
        assert!(!bridge.status().attached);

        let not_attached = bridge
            .execute("where", Duration::from_millis(5))
            .expect_err("bridge should reject commands before attach");
        assert_eq!(not_attached, DebugCommandBridgeError::NotAttached);

        {
            let mut state = bridge
                .inner
                .state
                .lock()
                .expect("debug command bridge lock poisoned");
            state.attached = true;
        }
        let timeout = bridge
            .execute("where", Duration::from_millis(5))
            .expect_err("bridge should time out with no debugger repl consumer");
        assert_eq!(timeout, DebugCommandBridgeError::Timeout);

        bridge.close();
        let closed = bridge
            .execute("where", Duration::from_millis(5))
            .expect_err("closed bridge should reject commands");
        assert_eq!(closed, DebugCommandBridgeError::Closed);
        let status = bridge.status();
        assert!(!status.attached);
        assert_eq!(status.current_line, None);
    }
}
