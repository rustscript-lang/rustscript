use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;
use crate::debug_info::DebugInfo;
use crate::vm::{OpCode, Program};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitConfig {
    pub enabled: bool,
    pub hot_loop_threshold: u32,
    pub max_trace_len: usize,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            enabled: native_jit_supported(),
            hot_loop_threshold: 8,
            max_trace_len: 256,
        }
    }
}

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum JitTraceTerminal {
    LoopBack,
    BranchExit,
    Halt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JitNyiReason {
    UnsupportedArch,
    HotLoopThresholdZero,
    UnsupportedOpcode(u8),
    BackwardGuard { target: usize },
    InvalidJumpTarget { target: usize },
    InvalidImmediate(&'static str),
    TraceTooLong { limit: usize },
    MissingTerminal,
}

impl JitNyiReason {
    pub fn message(&self) -> String {
        match self {
            JitNyiReason::UnsupportedArch => {
                "target architecture is not x86_64-unix-non-macos/x86_64-windows/aarch64-linux/aarch64-macos".to_string()
            }
            JitNyiReason::HotLoopThresholdZero => "hot_loop_threshold must be > 0".to_string(),
            JitNyiReason::UnsupportedOpcode(op) => format!("unsupported opcode 0x{op:02X}"),
            JitNyiReason::BackwardGuard { target } => {
                format!("opcode brfalse with backward target {target} is NYI")
            }
            JitNyiReason::InvalidJumpTarget { target } => {
                format!("jump target {target} is out of bytecode bounds")
            }
            JitNyiReason::InvalidImmediate(kind) => {
                format!("failed to decode immediate operand for {kind}")
            }
            JitNyiReason::TraceTooLong { limit } => {
                format!("trace length exceeded configured limit {limit}")
            }
            JitNyiReason::MissingTerminal => {
                "trace recorder reached end without loopback/ret terminal".to_string()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TraceStep {
    Nop,
    Ldc(u32),
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    And,
    Or,
    Neg,
    Ceq,
    Clt,
    Cgt,
    Pop,
    Dup,
    Ldloc(u8),
    Stloc(u8),
    Call {
        index: u16,
        argc: u8,
        call_ip: usize,
    },
    GuardFalse {
        exit_ip: usize,
    },
    JumpToIp {
        target_ip: usize,
    },
    JumpToRoot,
    Ret,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitTrace {
    pub id: usize,
    pub root_ip: usize,
    pub start_line: Option<u32>,
    pub has_call: bool,
    pub has_yielding_call: bool,
    pub steps: Vec<TraceStep>,
    pub terminal: JitTraceTerminal,
    pub executions: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitAttempt {
    pub root_ip: usize,
    pub line: Option<u32>,
    pub result: Result<usize, JitNyiReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitSnapshot {
    pub arch: &'static str,
    pub config: JitConfig,
    pub traces: Vec<JitTrace>,
    pub attempts: Vec<JitAttempt>,
    pub nyi_reference: Vec<JitNyiDoc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitNyiDoc {
    pub item: &'static str,
    pub reason: &'static str,
}

pub struct TraceJitEngine {
    config: JitConfig,
    hot_counts: HashMap<usize, u32>,
    compiled_by_root: HashMap<usize, usize>,
    blocked_roots: HashSet<usize>,
    loop_headers: Option<HashSet<usize>>,
    traces: Vec<JitTrace>,
    attempts: Vec<JitAttempt>,
}

impl Default for TraceJitEngine {
    fn default() -> Self {
        Self::new(JitConfig::default())
    }
}

impl TraceJitEngine {
    pub fn new(config: JitConfig) -> Self {
        Self {
            config,
            hot_counts: HashMap::new(),
            compiled_by_root: HashMap::new(),
            blocked_roots: HashSet::new(),
            loop_headers: None,
            traces: Vec::new(),
            attempts: Vec::new(),
        }
    }

    pub fn config(&self) -> &JitConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: JitConfig) {
        self.config = config;
        self.hot_counts.clear();
        self.compiled_by_root.clear();
        self.blocked_roots.clear();
        self.loop_headers = None;
        self.traces.clear();
        self.attempts.clear();
    }

    pub fn observe_hot_ip(&mut self, ip: usize, program: &Program) -> Option<usize> {
        if !self.config.enabled {
            return None;
        }
        if !native_jit_supported() {
            return None;
        }
        if let Some(&trace_id) = self.compiled_by_root.get(&ip) {
            return Some(trace_id);
        }
        if self.blocked_roots.contains(&ip) {
            return None;
        }
        if !self.is_loop_header(program, ip) {
            return None;
        }

        let count = self.hot_counts.entry(ip).or_insert(0);
        *count = count.saturating_add(1);
        if *count < self.config.hot_loop_threshold {
            return None;
        }

        let line = program
            .debug
            .as_ref()
            .and_then(|debug| debug.line_for_offset(ip));
        let result = if self.config.hot_loop_threshold == 0 {
            Err(JitNyiReason::HotLoopThresholdZero)
        } else if !native_jit_supported() {
            Err(JitNyiReason::UnsupportedArch)
        } else {
            self.compile_trace(program, ip)
        };

        match result {
            Ok(trace_id) => {
                self.attempts.push(JitAttempt {
                    root_ip: ip,
                    line,
                    result: Ok(trace_id),
                });
                self.compiled_by_root.insert(ip, trace_id);
                Some(trace_id)
            }
            Err(reason) => {
                self.attempts.push(JitAttempt {
                    root_ip: ip,
                    line,
                    result: Err(reason),
                });
                self.blocked_roots.insert(ip);
                None
            }
        }
    }

    pub fn trace_clone(&self, trace_id: usize) -> Option<JitTrace> {
        self.traces.get(trace_id).cloned()
    }

    pub fn trace_has_call(&self, trace_id: usize) -> bool {
        self.traces
            .get(trace_id)
            .is_some_and(|trace| trace.has_call)
    }

    pub fn mark_trace_executed(&mut self, trace_id: usize) {
        if let Some(trace) = self.traces.get_mut(trace_id) {
            trace.executions = trace.executions.saturating_add(1);
        }
    }

    pub fn snapshot(&self) -> JitSnapshot {
        JitSnapshot {
            arch: std::env::consts::ARCH,
            config: self.config.clone(),
            traces: self.traces.clone(),
            attempts: self.attempts.clone(),
            nyi_reference: nyi_reference(),
        }
    }

    pub fn dump_text(&self, debug: Option<&DebugInfo>) -> String {
        let mut out = String::new();
        out.push_str("trace-jit:\n");
        out.push_str(&format!("  arch: {}\n", std::env::consts::ARCH));
        out.push_str(&format!("  enabled: {}\n", self.config.enabled));
        out.push_str(&format!(
            "  hot_loop_threshold: {}\n",
            self.config.hot_loop_threshold
        ));
        out.push_str(&format!("  max_trace_len: {}\n", self.config.max_trace_len));
        out.push_str(&format!("  compiled traces: {}\n", self.traces.len()));
        out.push_str(&format!("  compile attempts: {}\n", self.attempts.len()));

        for trace in &self.traces {
            let line = trace
                .start_line
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string());
            let source = debug
                .and_then(|info| trace.start_line.and_then(|l| info.source_line(l)))
                .unwrap_or_default();
            out.push_str(&format!(
                "  trace#{} root_ip={} line={} terminal={:?} steps={} executions={}\n",
                trace.id,
                trace.root_ip,
                line,
                trace.terminal,
                trace.steps.len(),
                trace.executions
            ));
            if !source.is_empty() {
                out.push_str(&format!("    source: {}\n", source.trim()));
            }
            out.push_str("    ops:");
            for step in &trace.steps {
                out.push_str(&format!(" {}", trace_step_name(step)));
            }
            out.push('\n');
        }

        let mut nyi = 0usize;
        for attempt in &self.attempts {
            if let Err(reason) = &attempt.result {
                nyi = nyi.saturating_add(1);
                let line = attempt
                    .line
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string());
                out.push_str(&format!(
                    "  nyi root_ip={} line={} reason={}\n",
                    attempt.root_ip,
                    line,
                    reason.message()
                ));
            }
        }
        out.push_str(&format!("  nyi attempts: {nyi}\n"));

        out.push_str("  nyi reference:\n");
        for doc in nyi_reference() {
            out.push_str(&format!("    - {}: {}\n", doc.item, doc.reason));
        }

        out
    }

    fn compile_trace(&mut self, program: &Program, root_ip: usize) -> Result<usize, JitNyiReason> {
        let code = &program.code;
        let mut ip = root_ip;
        let mut steps = Vec::new();

        while steps.len() < self.config.max_trace_len {
            let opcode = *code
                .get(ip)
                .ok_or(JitNyiReason::InvalidJumpTarget { target: ip })?;
            ip = ip.saturating_add(1);

            if opcode == OpCode::Nop as u8 {
                steps.push(TraceStep::Nop);
                continue;
            }
            if opcode == OpCode::Ret as u8 {
                steps.push(TraceStep::Ret);
                return Ok(self.finish_trace(program, root_ip, steps, JitTraceTerminal::Halt));
            }
            if opcode == OpCode::Ldc as u8 {
                let value = read_u32(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("ldc"))?;
                steps.push(TraceStep::Ldc(value));
                continue;
            }
            if opcode == OpCode::Add as u8 {
                steps.push(TraceStep::Add);
                continue;
            }
            if opcode == OpCode::Sub as u8 {
                steps.push(TraceStep::Sub);
                continue;
            }
            if opcode == OpCode::Mul as u8 {
                steps.push(TraceStep::Mul);
                continue;
            }
            if opcode == OpCode::Div as u8 {
                steps.push(TraceStep::Div);
                continue;
            }
            if opcode == OpCode::Mod as u8 {
                steps.push(TraceStep::Mod);
                continue;
            }
            if opcode == OpCode::Shl as u8 {
                steps.push(TraceStep::Shl);
                continue;
            }
            if opcode == OpCode::Shr as u8 {
                steps.push(TraceStep::Shr);
                continue;
            }
            if opcode == OpCode::And as u8 {
                steps.push(TraceStep::And);
                continue;
            }
            if opcode == OpCode::Or as u8 {
                steps.push(TraceStep::Or);
                continue;
            }
            if opcode == OpCode::Neg as u8 {
                steps.push(TraceStep::Neg);
                continue;
            }
            if opcode == OpCode::Ceq as u8 {
                steps.push(TraceStep::Ceq);
                continue;
            }
            if opcode == OpCode::Clt as u8 {
                steps.push(TraceStep::Clt);
                continue;
            }
            if opcode == OpCode::Cgt as u8 {
                steps.push(TraceStep::Cgt);
                continue;
            }
            if opcode == OpCode::Pop as u8 {
                steps.push(TraceStep::Pop);
                continue;
            }
            if opcode == OpCode::Dup as u8 {
                steps.push(TraceStep::Dup);
                continue;
            }
            if opcode == OpCode::Ldloc as u8 {
                let index =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("ldloc"))?;
                steps.push(TraceStep::Ldloc(index));
                continue;
            }
            if opcode == OpCode::Stloc as u8 {
                let index =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("stloc"))?;
                steps.push(TraceStep::Stloc(index));
                continue;
            }
            if opcode == OpCode::Brfalse as u8 {
                let target_u32 =
                    read_u32(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("brfalse"))?;
                let target = target_u32 as usize;
                if target <= ip {
                    return Err(JitNyiReason::BackwardGuard { target });
                }
                if target >= code.len() {
                    return Err(JitNyiReason::InvalidJumpTarget { target });
                }
                steps.push(TraceStep::GuardFalse { exit_ip: target });
                continue;
            }
            if opcode == OpCode::Br as u8 {
                let target_u32 =
                    read_u32(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("br"))?;
                let target = target_u32 as usize;
                if target >= code.len() {
                    return Err(JitNyiReason::InvalidJumpTarget { target });
                }
                if target == root_ip {
                    steps.push(TraceStep::JumpToRoot);
                    return Ok(self.finish_trace(
                        program,
                        root_ip,
                        steps,
                        JitTraceTerminal::LoopBack,
                    ));
                }
                if target < ip {
                    steps.push(TraceStep::JumpToIp { target_ip: target });
                    return Ok(self.finish_trace(
                        program,
                        root_ip,
                        steps,
                        JitTraceTerminal::BranchExit,
                    ));
                }
                // Follow forward unconditional branches to avoid creating tiny branch-exit traces.
                ip = target;
                continue;
            }
            if opcode == OpCode::Call as u8 {
                let call_ip = ip.saturating_sub(1);
                let index =
                    read_u16(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("call index"))?;
                let argc =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("call argc"))?;
                steps.push(TraceStep::Call {
                    index,
                    argc,
                    call_ip,
                });
                continue;
            }

            return Err(JitNyiReason::UnsupportedOpcode(opcode));
        }

        Err(JitNyiReason::TraceTooLong {
            limit: self.config.max_trace_len,
        })
    }

    fn finish_trace(
        &mut self,
        program: &Program,
        root_ip: usize,
        steps: Vec<TraceStep>,
        terminal: JitTraceTerminal,
    ) -> usize {
        let id = self.traces.len();
        let start_line = program
            .debug
            .as_ref()
            .and_then(|debug| debug.line_for_offset(root_ip));
        let has_call = steps
            .iter()
            .any(|step| matches!(step, TraceStep::Call { .. }));
        let has_yielding_call = steps.iter().any(|step| {
            if let TraceStep::Call { index, .. } = step {
                BuiltinFunction::from_call_index(*index).is_none()
            } else {
                false
            }
        });
        self.traces.push(JitTrace {
            id,
            root_ip,
            start_line,
            has_call,
            has_yielding_call,
            steps,
            terminal,
            executions: 0,
        });
        id
    }

    fn is_loop_header(&mut self, program: &Program, ip: usize) -> bool {
        if self.loop_headers.is_none() {
            self.loop_headers = Some(scan_loop_headers(program));
        }
        self.loop_headers
            .as_ref()
            .is_some_and(|headers| headers.contains(&ip))
    }
}

fn read_u8(code: &[u8], ip: &mut usize) -> Option<u8> {
    let value = *code.get(*ip)?;
    *ip = ip.saturating_add(1);
    Some(value)
}

fn read_u32(code: &[u8], ip: &mut usize) -> Option<u32> {
    if ip.saturating_add(4) > code.len() {
        return None;
    }
    let bytes = [code[*ip], code[*ip + 1], code[*ip + 2], code[*ip + 3]];
    *ip = ip.saturating_add(4);
    Some(u32::from_le_bytes(bytes))
}

fn trace_step_name(step: &TraceStep) -> &'static str {
    match step {
        TraceStep::Nop => "nop",
        TraceStep::Ldc(_) => "ldc",
        TraceStep::Add => "add",
        TraceStep::Sub => "sub",
        TraceStep::Mul => "mul",
        TraceStep::Div => "div",
        TraceStep::Mod => "mod",
        TraceStep::Shl => "shl",
        TraceStep::Shr => "shr",
        TraceStep::And => "and",
        TraceStep::Or => "or",
        TraceStep::Neg => "neg",
        TraceStep::Ceq => "ceq",
        TraceStep::Clt => "clt",
        TraceStep::Cgt => "cgt",
        TraceStep::Pop => "pop",
        TraceStep::Dup => "dup",
        TraceStep::Ldloc(_) => "ldloc",
        TraceStep::Stloc(_) => "stloc",
        TraceStep::Call { .. } => "call",
        TraceStep::GuardFalse { .. } => "guard_false",
        TraceStep::JumpToIp { .. } => "jump_ip",
        TraceStep::JumpToRoot => "jump_root",
        TraceStep::Ret => "ret",
    }
}

fn scan_loop_headers(program: &Program) -> HashSet<usize> {
    let mut headers = HashSet::new();
    let code = &program.code;
    let mut ip = 0usize;

    while ip < code.len() {
        let opcode = code[ip];
        let instr_ip = ip;
        ip = ip.saturating_add(1);
        match opcode {
            x if x == OpCode::Ldc as u8 => {
                if read_u32(code, &mut ip).is_none() {
                    break;
                }
            }
            x if x == OpCode::Br as u8 || x == OpCode::Brfalse as u8 => {
                let Some(target_u32) = read_u32(code, &mut ip) else {
                    break;
                };
                let target = target_u32 as usize;
                if target <= instr_ip {
                    headers.insert(target);
                }
            }
            x if x == OpCode::Ldloc as u8 || x == OpCode::Stloc as u8 => {
                if read_u8(code, &mut ip).is_none() {
                    break;
                }
            }
            x if x == OpCode::Call as u8 => {
                if read_u16(code, &mut ip).is_none() {
                    break;
                }
                if read_u8(code, &mut ip).is_none() {
                    break;
                }
            }
            _ => {}
        }
    }

    headers
}

fn read_u16(code: &[u8], ip: &mut usize) -> Option<u16> {
    if ip.saturating_add(2) > code.len() {
        return None;
    }
    let bytes = [code[*ip], code[*ip + 1]];
    *ip = ip.saturating_add(2);
    Some(u16::from_le_bytes(bytes))
}

fn nyi_reference() -> Vec<JitNyiDoc> {
    vec![
        JitNyiDoc {
            item: "brfalse (backward target)",
            reason: "only forward guard exits are supported",
        },
        JitNyiDoc {
            item: "Oversized traces",
            reason: "trace recording stops at max_trace_len",
        },
        JitNyiDoc {
            item: "Unsupported native JIT targets",
            reason: "native emission currently supports x86_64 on windows plus unix non-macos, and aarch64 on linux/macos",
        },
    ]
}
