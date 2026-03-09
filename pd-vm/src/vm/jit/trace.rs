use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;
use crate::debug_info::DebugInfo;
use crate::vm::{OpCode, Program, ValueType};

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
    IAdd,
    FAdd,
    SConcat,
    Sub,
    ISub,
    FSub,
    Mul,
    IMul,
    FMul,
    Div,
    IDiv,
    FDiv,
    Mod,
    IMod,
    FMod,
    Shl,
    Shr,
    Lshr,
    And,
    Or,
    Not,
    Neg,
    INeg,
    FNeg,
    Ceq,
    FCeq,
    Clt,
    FClt,
    Cgt,
    FCgt,
    Pop,
    Dup,
    Ldloc(u8),
    Stloc(u8),
    BuiltinCall {
        index: u16,
        argc: u8,
        call_ip: usize,
    },
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
    pub step_ips: Vec<usize>,
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

    pub fn prepare_aot(&mut self, program: &Program) -> Vec<usize> {
        self.hot_counts.clear();
        self.compiled_by_root.clear();
        self.blocked_roots.clear();
        self.loop_headers = Some(scan_loop_headers(program));
        self.traces.clear();
        self.attempts.clear();

        if !self.config.enabled || !native_jit_supported() {
            return Vec::new();
        }

        let mut roots = scan_program_block_roots(program)
            .into_iter()
            .collect::<Vec<_>>();
        roots.sort_unstable();

        let mut compiled = Vec::with_capacity(roots.len());
        for root_ip in roots {
            let line = program
                .debug
                .as_ref()
                .and_then(|debug| debug.line_for_offset(root_ip));
            match self.compile_aot_block(program, root_ip) {
                Ok(trace_id) => {
                    self.attempts.push(JitAttempt {
                        root_ip,
                        line,
                        result: Ok(trace_id),
                    });
                    self.compiled_by_root.insert(root_ip, trace_id);
                    compiled.push(trace_id);
                }
                Err(reason) => {
                    self.attempts.push(JitAttempt {
                        root_ip,
                        line,
                        result: Err(reason),
                    });
                    self.blocked_roots.insert(root_ip);
                }
            }
        }

        compiled
    }

    pub(crate) fn ensure_aot_root(&mut self, program: &Program, root_ip: usize) -> Option<usize> {
        if !self.config.enabled || !native_jit_supported() {
            return None;
        }
        if let Some(&trace_id) = self.compiled_by_root.get(&root_ip) {
            return Some(trace_id);
        }
        if self.blocked_roots.contains(&root_ip) {
            return None;
        }

        let line = program
            .debug
            .as_ref()
            .and_then(|debug| debug.line_for_offset(root_ip));
        match self.compile_aot_block(program, root_ip) {
            Ok(trace_id) => {
                self.attempts.push(JitAttempt {
                    root_ip,
                    line,
                    result: Ok(trace_id),
                });
                self.compiled_by_root.insert(root_ip, trace_id);
                Some(trace_id)
            }
            Err(reason) => {
                self.attempts.push(JitAttempt {
                    root_ip,
                    line,
                    result: Err(reason),
                });
                self.blocked_roots.insert(root_ip);
                None
            }
        }
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

    pub fn observe_exit_ip(&mut self, ip: usize, program: &Program) -> Option<usize> {
        if !self.config.enabled || !native_jit_supported() {
            return None;
        }
        // Keep default behavior unchanged: only aggressively chain-compile exit roots when the
        // user requested the most aggressive hotness policy.
        if self.config.hot_loop_threshold > 1 {
            return None;
        }
        if let Some(&trace_id) = self.compiled_by_root.get(&ip) {
            return Some(trace_id);
        }
        if self.blocked_roots.contains(&ip) {
            return None;
        }

        let line = program
            .debug
            .as_ref()
            .and_then(|debug| debug.line_for_offset(ip));
        let result = if self.config.hot_loop_threshold == 0 {
            Err(JitNyiReason::HotLoopThresholdZero)
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

    pub fn trace_has_call(&self, trace_id: usize) -> bool {
        self.traces
            .get(trace_id)
            .is_some_and(|trace| trace.has_call)
    }

    pub fn compiled_trace_for_ip(&self, ip: usize) -> Option<usize> {
        self.compiled_by_root.get(&ip).copied()
    }

    pub fn mark_trace_executed(&mut self, trace_id: usize) {
        if let Some(trace) = self.traces.get_mut(trace_id) {
            trace.executions = trace.executions.saturating_add(1);
        }
    }

    pub(crate) fn install_precompiled_traces(
        &mut self,
        traces: Vec<JitTrace>,
    ) -> Result<(), String> {
        let mut compiled_by_root = HashMap::with_capacity(traces.len());
        for (expected_id, trace) in traces.iter().enumerate() {
            if trace.id != expected_id {
                return Err(format!(
                    "invalid precompiled trace id {}, expected {}",
                    trace.id, expected_id
                ));
            }
            if trace.steps.len() != trace.step_ips.len() {
                return Err(format!(
                    "precompiled trace {} has misaligned steps and step_ips",
                    trace.id
                ));
            }
            if compiled_by_root.insert(trace.root_ip, trace.id).is_some() {
                return Err(format!(
                    "duplicate precompiled trace root_ip {}",
                    trace.root_ip
                ));
            }
        }

        let max_trace_len = traces
            .iter()
            .map(|trace| trace.steps.len())
            .max()
            .unwrap_or(1);
        self.config.enabled = true;
        self.config.hot_loop_threshold = self.config.hot_loop_threshold.max(1);
        self.config.max_trace_len = self.config.max_trace_len.max(max_trace_len);
        self.hot_counts.clear();
        self.compiled_by_root = compiled_by_root;
        self.blocked_roots.clear();
        self.loop_headers = None;
        self.attempts = traces
            .iter()
            .map(|trace| JitAttempt {
                root_ip: trace.root_ip,
                line: trace.start_line,
                result: Ok(trace.id),
            })
            .collect();
        self.traces = traces;
        Ok(())
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

    fn compile_aot_block(
        &mut self,
        program: &Program,
        root_ip: usize,
    ) -> Result<usize, JitNyiReason> {
        let code = &program.code;
        let mut ip = root_ip;
        let mut steps = Vec::new();
        let mut step_ips = Vec::new();

        while steps.len() < self.config.max_trace_len {
            let instr_ip = ip;
            let opcode = *code
                .get(ip)
                .ok_or(JitNyiReason::InvalidJumpTarget { target: ip })?;
            ip = ip.saturating_add(1);

            if opcode == OpCode::Nop as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Nop);
                continue;
            }
            if opcode == OpCode::Ret as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Ret);
                return Ok(self.finish_trace(
                    program,
                    root_ip,
                    steps,
                    step_ips,
                    JitTraceTerminal::Halt,
                ));
            }
            if opcode == OpCode::Ldc as u8 {
                let value = read_u32(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("ldc"))?;
                step_ips.push(instr_ip);
                steps.push(TraceStep::Ldc(value));
                continue;
            }
            if opcode == OpCode::Add as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Sub as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Mul as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Div as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Mod as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Shl as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Shl);
                continue;
            }
            if opcode == OpCode::Shr as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Shr);
                continue;
            }
            if opcode == OpCode::Lshr as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Lshr);
                continue;
            }
            if opcode == OpCode::And as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::And);
                continue;
            }
            if opcode == OpCode::Or as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Or);
                continue;
            }
            if opcode == OpCode::Not as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Not);
                continue;
            }
            if opcode == OpCode::Neg as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Ceq as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Clt as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Cgt as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Pop as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Pop);
                continue;
            }
            if opcode == OpCode::Dup as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Dup);
                continue;
            }
            if opcode == OpCode::Ldloc as u8 {
                let index =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("ldloc"))?;
                step_ips.push(instr_ip);
                steps.push(TraceStep::Ldloc(index));
                continue;
            }
            if opcode == OpCode::Stloc as u8 {
                let index =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("stloc"))?;
                step_ips.push(instr_ip);
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
                step_ips.push(instr_ip);
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
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::JumpToRoot);
                    return Ok(self.finish_trace(
                        program,
                        root_ip,
                        steps,
                        step_ips,
                        JitTraceTerminal::LoopBack,
                    ));
                }
                if target < ip {
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::JumpToIp { target_ip: target });
                    return Ok(self.finish_trace(
                        program,
                        root_ip,
                        steps,
                        step_ips,
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
                if let Some(builtin) = BuiltinFunction::from_call_index(index)
                    && argc == builtin.arity()
                {
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::BuiltinCall {
                        index,
                        argc,
                        call_ip,
                    });
                } else {
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::Call {
                        index,
                        argc,
                        call_ip,
                    });
                }
                continue;
            }

            return Err(JitNyiReason::UnsupportedOpcode(opcode));
        }

        Err(JitNyiReason::TraceTooLong {
            limit: self.config.max_trace_len,
        })
    }

    fn compile_trace(&mut self, program: &Program, root_ip: usize) -> Result<usize, JitNyiReason> {
        let code = &program.code;
        let mut ip = root_ip;
        let mut steps = Vec::new();
        let mut step_ips = Vec::new();

        while steps.len() < self.config.max_trace_len {
            let instr_ip = ip;
            let opcode = *code
                .get(ip)
                .ok_or(JitNyiReason::InvalidJumpTarget { target: ip })?;
            ip = ip.saturating_add(1);

            if opcode == OpCode::Nop as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Nop);
                continue;
            }
            if opcode == OpCode::Ret as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Ret);
                return Ok(self.finish_trace(
                    program,
                    root_ip,
                    steps,
                    step_ips,
                    JitTraceTerminal::Halt,
                ));
            }
            if opcode == OpCode::Ldc as u8 {
                let value = read_u32(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("ldc"))?;
                step_ips.push(instr_ip);
                steps.push(TraceStep::Ldc(value));
                continue;
            }
            if opcode == OpCode::Add as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Sub as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Mul as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Div as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Mod as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Shl as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Shl);
                continue;
            }
            if opcode == OpCode::Shr as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Shr);
                continue;
            }
            if opcode == OpCode::Lshr as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Lshr);
                continue;
            }
            if opcode == OpCode::And as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::And);
                continue;
            }
            if opcode == OpCode::Or as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Or);
                continue;
            }
            if opcode == OpCode::Not as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Not);
                continue;
            }
            if opcode == OpCode::Neg as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Ceq as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Clt as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Cgt as u8 {
                step_ips.push(instr_ip);
                steps.push(typed_trace_step(program, instr_ip, opcode));
                continue;
            }
            if opcode == OpCode::Pop as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Pop);
                continue;
            }
            if opcode == OpCode::Dup as u8 {
                step_ips.push(instr_ip);
                steps.push(TraceStep::Dup);
                continue;
            }
            if opcode == OpCode::Ldloc as u8 {
                let index =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("ldloc"))?;
                step_ips.push(instr_ip);
                steps.push(TraceStep::Ldloc(index));
                continue;
            }
            if opcode == OpCode::Stloc as u8 {
                let index =
                    read_u8(code, &mut ip).ok_or(JitNyiReason::InvalidImmediate("stloc"))?;
                step_ips.push(instr_ip);
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
                step_ips.push(instr_ip);
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
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::JumpToRoot);
                    return Ok(self.finish_trace(
                        program,
                        root_ip,
                        steps,
                        step_ips,
                        JitTraceTerminal::LoopBack,
                    ));
                }
                if target < ip {
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::JumpToIp { target_ip: target });
                    return Ok(self.finish_trace(
                        program,
                        root_ip,
                        steps,
                        step_ips,
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
                if let Some(builtin) = BuiltinFunction::from_call_index(index)
                    && argc == builtin.arity()
                {
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::BuiltinCall {
                        index,
                        argc,
                        call_ip,
                    });
                } else {
                    step_ips.push(instr_ip);
                    steps.push(TraceStep::Call {
                        index,
                        argc,
                        call_ip,
                    });
                }
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
        step_ips: Vec<usize>,
        terminal: JitTraceTerminal,
    ) -> usize {
        debug_assert_eq!(
            steps.len(),
            step_ips.len(),
            "trace steps and step_ips must stay aligned"
        );
        let id = self.traces.len();
        let start_line = program
            .debug
            .as_ref()
            .and_then(|debug| debug.line_for_offset(root_ip));
        let has_call = steps
            .iter()
            .any(|step| matches!(step, TraceStep::Call { .. } | TraceStep::BuiltinCall { .. }));
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
            step_ips,
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

fn typed_trace_step(program: &Program, ip: usize, opcode: u8) -> TraceStep {
    let operand_types = program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.operand_types.get(&ip))
        .copied()
        .unwrap_or((ValueType::Unknown, ValueType::Unknown));
    match (opcode, operand_types) {
        (x, (ValueType::Int, ValueType::Int)) if x == OpCode::Add as u8 => TraceStep::IAdd,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Add as u8 => TraceStep::FAdd,
        (x, (ValueType::String, ValueType::String)) if x == OpCode::Add as u8 => TraceStep::SConcat,
        (x, (ValueType::Int, ValueType::Int)) if x == OpCode::Sub as u8 => TraceStep::ISub,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Sub as u8 => TraceStep::FSub,
        (x, (ValueType::Int, ValueType::Int)) if x == OpCode::Mul as u8 => TraceStep::IMul,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Mul as u8 => TraceStep::FMul,
        (x, (ValueType::Int, ValueType::Int)) if x == OpCode::Div as u8 => TraceStep::IDiv,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Div as u8 => TraceStep::FDiv,
        (x, (ValueType::Int, ValueType::Int)) if x == OpCode::Mod as u8 => TraceStep::IMod,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Mod as u8 => TraceStep::FMod,
        (x, (ValueType::Int, _)) if x == OpCode::Neg as u8 => TraceStep::INeg,
        (x, (ValueType::Float, _)) if x == OpCode::Neg as u8 => TraceStep::FNeg,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Ceq as u8 => TraceStep::FCeq,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Clt as u8 => TraceStep::FClt,
        (x, (ValueType::Float, ValueType::Float)) if x == OpCode::Cgt as u8 => TraceStep::FCgt,
        (x, _) if x == OpCode::Add as u8 => TraceStep::Add,
        (x, _) if x == OpCode::Sub as u8 => TraceStep::Sub,
        (x, _) if x == OpCode::Mul as u8 => TraceStep::Mul,
        (x, _) if x == OpCode::Div as u8 => TraceStep::Div,
        (x, _) if x == OpCode::Mod as u8 => TraceStep::Mod,
        (x, _) if x == OpCode::Neg as u8 => TraceStep::Neg,
        (x, _) if x == OpCode::Ceq as u8 => TraceStep::Ceq,
        (x, _) if x == OpCode::Clt as u8 => TraceStep::Clt,
        (x, _) if x == OpCode::Cgt as u8 => TraceStep::Cgt,
        _ => unreachable!("typed_trace_step only supports arithmetic/comparison opcodes"),
    }
}

fn trace_step_name(step: &TraceStep) -> &'static str {
    match step {
        TraceStep::Nop => "nop",
        TraceStep::Ldc(_) => "ldc",
        TraceStep::Add => "add",
        TraceStep::IAdd => "add",
        TraceStep::FAdd => "add",
        TraceStep::SConcat => "add",
        TraceStep::Sub => "sub",
        TraceStep::ISub => "sub",
        TraceStep::FSub => "sub",
        TraceStep::Mul => "mul",
        TraceStep::IMul => "mul",
        TraceStep::FMul => "mul",
        TraceStep::Div => "div",
        TraceStep::IDiv => "div",
        TraceStep::FDiv => "div",
        TraceStep::Mod => "mod",
        TraceStep::IMod => "mod",
        TraceStep::FMod => "mod",
        TraceStep::Shl => "shl",
        TraceStep::Shr => "shr",
        TraceStep::Lshr => "lshr",
        TraceStep::And => "and",
        TraceStep::Or => "or",
        TraceStep::Not => "not",
        TraceStep::Neg => "neg",
        TraceStep::INeg => "neg",
        TraceStep::FNeg => "neg",
        TraceStep::Ceq => "ceq",
        TraceStep::FCeq => "ceq",
        TraceStep::Clt => "clt",
        TraceStep::FClt => "clt",
        TraceStep::Cgt => "cgt",
        TraceStep::FCgt => "cgt",
        TraceStep::Pop => "pop",
        TraceStep::Dup => "dup",
        TraceStep::Ldloc(_) => "ldloc",
        TraceStep::Stloc(_) => "stloc",
        TraceStep::BuiltinCall { .. } => "call",
        TraceStep::Call { .. } => "call",
        TraceStep::GuardFalse { .. } => "guard_false",
        TraceStep::JumpToIp { .. } => "jump_ip",
        TraceStep::JumpToRoot => "jump_root",
        TraceStep::Ret => "ret",
    }
}

fn scan_program_block_roots(program: &Program) -> HashSet<usize> {
    let mut roots = HashSet::new();
    if program.code.is_empty() {
        return roots;
    }
    roots.insert(0);

    let code = &program.code;
    let mut ip = 0usize;
    while ip < code.len() {
        let opcode = code[ip];
        ip = ip.saturating_add(1);
        match opcode {
            x if x == OpCode::Ldc as u8 => {
                if read_u32(code, &mut ip).is_none() {
                    break;
                }
            }
            x if x == OpCode::Br as u8 => {
                let Some(target) = read_u32(code, &mut ip) else {
                    break;
                };
                let target = target as usize;
                if target < code.len() {
                    roots.insert(target);
                }
                if ip < code.len() {
                    roots.insert(ip);
                }
            }
            x if x == OpCode::Brfalse as u8 => {
                let Some(target) = read_u32(code, &mut ip) else {
                    break;
                };
                let target = target as usize;
                if target < code.len() {
                    roots.insert(target);
                }
                if ip < code.len() {
                    roots.insert(ip);
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
                if ip < code.len() {
                    roots.insert(ip);
                }
            }
            _ => {}
        }
    }

    roots
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
