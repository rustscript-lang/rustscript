use std::collections::{HashMap, HashSet};

use crate::debug_info::DebugInfo;
use crate::vm::{OpCode, Program};

use super::ir::SsaTrace;
use super::liveness::{boxed_load_site_count, boxed_store_site_count};
use super::recorder::{RecordedTrace, TraceRecordError, record_trace};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TraceEntryKey {
    pub(crate) root_ip: usize,
    pub(crate) stack_depth: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    UnsupportedTrace(String),
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
            JitNyiReason::UnsupportedTrace(detail) => detail.clone(),
            JitNyiReason::InvalidJumpTarget { target } => {
                format!("jump target {target} is invalid or out of bytecode bounds")
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

#[derive(Clone, Debug, PartialEq)]
pub struct JitTrace {
    pub id: usize,
    pub root_ip: usize,
    pub entry_stack_depth: usize,
    pub start_line: Option<u32>,
    pub has_call: bool,
    pub has_yielding_call: bool,
    pub op_names: Vec<String>,
    pub terminal: JitTraceTerminal,
    pub executions: u64,
    pub(crate) ssa: SsaTrace,
}

impl JitTrace {
    pub fn op_names(&self) -> &[String] {
        &self.op_names
    }

    pub fn ssa_text(&self) -> String {
        self.ssa.render_text()
    }

    pub fn ssa_block_count(&self) -> usize {
        self.ssa.blocks.len()
    }

    pub fn ssa_exit_count(&self) -> usize {
        self.ssa.exits.len()
    }

    pub fn boxed_load_site_count(&self) -> u64 {
        boxed_load_site_count(&self.ssa)
    }

    pub fn boxed_store_site_count(&self) -> u64 {
        boxed_store_site_count(&self.ssa)
    }

    pub fn ssa_dirty_local_materialization_count(&self) -> u64 {
        self.ssa
            .exits
            .iter()
            .flat_map(|exit| exit.dirty_locals.iter())
            .filter(|dirty| **dirty)
            .count() as u64
    }

    pub fn terminal_call_exit_ip(&self) -> Option<usize> {
        (self.op_names.last().map(String::as_str) == Some("call"))
            .then(|| self.ssa.exits.last().map(|exit| exit.exit_ip))
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitAttempt {
    pub root_ip: usize,
    pub entry_stack_depth: usize,
    pub line: Option<u32>,
    pub result: Result<usize, JitNyiReason>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitMetrics {
    pub boxed_load_site_count: u64,
    pub boxed_store_site_count: u64,
    pub trace_exit_count: u64,
    pub native_loop_back_count: u64,
    pub helper_fallback_count: u64,
    pub native_trace_exec_count: u64,
}

impl JitMetrics {
    pub fn guard_exit_count(self) -> u64 {
        self.trace_exit_count
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JitSnapshot {
    pub arch: &'static str,
    pub config: JitConfig,
    pub traces: Vec<JitTrace>,
    pub attempts: Vec<JitAttempt>,
    pub metrics: JitMetrics,
    pub nyi_reference: Vec<JitNyiDoc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitNyiDoc {
    pub item: &'static str,
    pub reason: &'static str,
}

pub struct TraceJitEngine {
    config: JitConfig,
    hot_counts: HashMap<TraceEntryKey, u32>,
    compiled_by_ip: Vec<Vec<(usize, usize)>>,
    blocked_entries: HashSet<TraceEntryKey>,
    loop_headers: Option<Vec<bool>>,
    non_yielding_host_imports: Vec<bool>,
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
            compiled_by_ip: Vec::new(),
            blocked_entries: HashSet::new(),
            loop_headers: None,
            non_yielding_host_imports: Vec::new(),
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
        self.compiled_by_ip.clear();
        self.blocked_entries.clear();
        self.loop_headers = None;
        self.traces.clear();
        self.attempts.clear();
    }

    pub(crate) fn set_non_yielding_host_imports(&mut self, imports: Vec<bool>) -> bool {
        if self.non_yielding_host_imports == imports {
            return false;
        }
        self.non_yielding_host_imports = imports;
        self.hot_counts.clear();
        self.compiled_by_ip.clear();
        self.blocked_entries.clear();
        self.loop_headers = None;
        self.traces.clear();
        self.attempts.clear();
        true
    }

    pub fn observe_hot_ip(&mut self, ip: usize, program: &Program) -> Option<usize> {
        self.observe_hot_entry(ip, 0, program)
    }

    pub(crate) fn observe_hot_entry(
        &mut self,
        ip: usize,
        stack_depth: usize,
        program: &Program,
    ) -> Option<usize> {
        if !self.config.enabled || !native_jit_supported() {
            return None;
        }
        let key = TraceEntryKey {
            root_ip: ip,
            stack_depth,
        };
        if let Some(trace_id) = self.compiled_trace_for_key(key) {
            return Some(trace_id);
        }
        if !self.is_loop_header(program, ip)
            || (!self.blocked_entries.is_empty() && self.blocked_entries.contains(&key))
        {
            return None;
        }

        let count = self.hot_counts.entry(key).or_insert(0);
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
        } else {
            self.compile_trace(program, key)
        };
        self.finish_attempt(key, line, result)
    }

    pub fn trace_clone(&self, trace_id: usize) -> Option<JitTrace> {
        self.traces.get(trace_id).cloned()
    }

    pub fn observe_exit_ip(&mut self, ip: usize, program: &Program) -> Option<usize> {
        self.observe_exit_entry(ip, 0, program)
    }

    pub(crate) fn observe_exit_entry(
        &mut self,
        ip: usize,
        stack_depth: usize,
        program: &Program,
    ) -> Option<usize> {
        if !self.config.enabled || !native_jit_supported() {
            return None;
        }
        let key = TraceEntryKey {
            root_ip: ip,
            stack_depth,
        };
        if let Some(trace_id) = self.compiled_trace_for_key(key) {
            return Some(trace_id);
        }
        if !self.blocked_entries.is_empty() && self.blocked_entries.contains(&key) {
            return None;
        }

        let count = self.hot_counts.entry(key).or_insert(0);
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
        } else {
            self.compile_trace(program, key)
        };
        self.finish_attempt(key, line, result)
    }

    pub fn trace_has_call(&self, trace_id: usize) -> bool {
        self.traces
            .get(trace_id)
            .is_some_and(|trace| trace.has_call)
    }

    pub fn compiled_trace_for_ip(&self, ip: usize) -> Option<usize> {
        self.compiled_trace_for_entry(ip, 0)
    }

    pub(crate) fn compiled_trace_for_entry(&self, ip: usize, stack_depth: usize) -> Option<usize> {
        self.compiled_trace_for_key(TraceEntryKey {
            root_ip: ip,
            stack_depth,
        })
    }

    pub fn mark_trace_executed(&mut self, trace_id: usize) {
        if let Some(trace) = self.traces.get_mut(trace_id) {
            trace.executions = trace.executions.saturating_add(1);
        }
    }

    pub(crate) fn block_trace(&mut self, trace_id: usize) {
        if let Some(trace) = self.traces.get(trace_id) {
            let key = TraceEntryKey {
                root_ip: trace.root_ip,
                stack_depth: trace.entry_stack_depth,
            };
            self.remove_compiled_trace(key);
            self.blocked_entries.insert(key);
        }
    }

    pub fn snapshot(&self, runtime_metrics: JitMetrics) -> JitSnapshot {
        JitSnapshot {
            arch: std::env::consts::ARCH,
            config: self.config,
            traces: self.traces.clone(),
            attempts: self.attempts.clone(),
            metrics: self.aggregate_metrics(runtime_metrics),
            nyi_reference: nyi_reference(),
        }
    }

    pub fn dump_text(&self, debug: Option<&DebugInfo>, runtime_metrics: JitMetrics) -> String {
        let mut out = String::new();
        let metrics = self.aggregate_metrics(runtime_metrics);
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
        out.push_str(&format!(
            "  boxed value sites: loads={} stores={}\n",
            metrics.boxed_load_site_count, metrics.boxed_store_site_count
        ));
        out.push_str(&format!(
            "  trace exits: total={} guard_like={} loop_backs={}\n",
            metrics.trace_exit_count,
            metrics.guard_exit_count(),
            metrics.native_loop_back_count
        ));
        out.push_str(&format!(
            "  interpreter fallbacks: {}\n",
            metrics.helper_fallback_count
        ));

        for trace in &self.traces {
            let line = trace
                .start_line
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string());
            let source = debug
                .and_then(|info| trace.start_line.and_then(|l| info.source_line(l)))
                .unwrap_or_default();
            out.push_str(&format!(
                "  trace#{} root_ip={} entry_stack_depth={} line={} terminal={:?} ops={} executions={}\n",
                trace.id,
                trace.root_ip,
                trace.entry_stack_depth,
                line,
                trace.terminal,
                trace.op_names.len(),
                trace.executions
            ));
            if !source.is_empty() {
                out.push_str(&format!("    source: {}\n", source.trim()));
            }
            out.push_str("    ops:");
            for op in &trace.op_names {
                out.push_str(&format!(" {}", op));
            }
            out.push('\n');
            out.push_str(&format!(
                "    ssa: blocks={} exits={}\n",
                trace.ssa.blocks.len(),
                trace.ssa.exits.len()
            ));
            for line in trace.ssa.render_text().lines() {
                out.push_str("      ");
                out.push_str(line);
                out.push('\n');
            }
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
                    "  nyi root_ip={} entry_stack_depth={} line={} reason={}\n",
                    attempt.root_ip,
                    attempt.entry_stack_depth,
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

    fn finish_attempt(
        &mut self,
        key: TraceEntryKey,
        line: Option<u32>,
        result: Result<usize, JitNyiReason>,
    ) -> Option<usize> {
        match result {
            Ok(trace_id) => {
                self.attempts.push(JitAttempt {
                    root_ip: key.root_ip,
                    entry_stack_depth: key.stack_depth,
                    line,
                    result: Ok(trace_id),
                });
                self.insert_compiled_trace(key, trace_id);
                Some(trace_id)
            }
            Err(reason) => {
                self.attempts.push(JitAttempt {
                    root_ip: key.root_ip,
                    entry_stack_depth: key.stack_depth,
                    line,
                    result: Err(reason),
                });
                self.blocked_entries.insert(key);
                None
            }
        }
    }

    fn compile_trace(
        &mut self,
        program: &Program,
        key: TraceEntryKey,
    ) -> Result<usize, JitNyiReason> {
        let recorded = record_trace(
            program,
            key.root_ip,
            key.stack_depth,
            self.config.max_trace_len,
            &self.non_yielding_host_imports,
        )
        .map_err(to_nyi)?;
        let id = self.traces.len();
        let start_line = program
            .debug
            .as_ref()
            .and_then(|debug| debug.line_for_offset(key.root_ip));
        let trace = build_jit_trace(id, key, start_line, recorded);
        self.traces.push(trace);
        Ok(id)
    }

    #[inline(always)]
    fn compiled_trace_for_key(&self, key: TraceEntryKey) -> Option<usize> {
        self.compiled_by_ip
            .get(key.root_ip)?
            .iter()
            .find_map(|(stack_depth, trace_id)| {
                (*stack_depth == key.stack_depth).then_some(*trace_id)
            })
    }

    fn insert_compiled_trace(&mut self, key: TraceEntryKey, trace_id: usize) {
        if self.compiled_by_ip.len() <= key.root_ip {
            self.compiled_by_ip.resize_with(key.root_ip + 1, Vec::new);
        }
        let entries = &mut self.compiled_by_ip[key.root_ip];
        if let Some((_, existing_trace_id)) = entries
            .iter_mut()
            .find(|(stack_depth, _)| *stack_depth == key.stack_depth)
        {
            *existing_trace_id = trace_id;
        } else {
            entries.push((key.stack_depth, trace_id));
        }
    }

    fn remove_compiled_trace(&mut self, key: TraceEntryKey) {
        let Some(entries) = self.compiled_by_ip.get_mut(key.root_ip) else {
            return;
        };
        if let Some(index) = entries
            .iter()
            .position(|(stack_depth, _)| *stack_depth == key.stack_depth)
        {
            entries.swap_remove(index);
        }
    }

    fn is_loop_header(&mut self, program: &Program, ip: usize) -> bool {
        if self.loop_headers.is_none() {
            self.loop_headers = Some(scan_loop_headers(program));
        }
        self.loop_headers
            .as_ref()
            .and_then(|headers| headers.get(ip))
            .copied()
            .unwrap_or(false)
    }

    fn aggregate_metrics(&self, mut runtime_metrics: JitMetrics) -> JitMetrics {
        for trace in &self.traces {
            runtime_metrics.boxed_load_site_count = runtime_metrics
                .boxed_load_site_count
                .saturating_add(boxed_load_site_count(&trace.ssa));
            runtime_metrics.boxed_store_site_count = runtime_metrics
                .boxed_store_site_count
                .saturating_add(boxed_store_site_count(&trace.ssa));
            if trace.terminal == JitTraceTerminal::LoopBack {
                runtime_metrics.native_loop_back_count =
                    runtime_metrics.native_loop_back_count.saturating_add(1);
            }
        }
        runtime_metrics
    }
}

fn build_jit_trace(
    id: usize,
    key: TraceEntryKey,
    start_line: Option<u32>,
    recorded: RecordedTrace,
) -> JitTrace {
    JitTrace {
        id,
        root_ip: key.root_ip,
        entry_stack_depth: key.stack_depth,
        start_line,
        has_call: recorded.has_call,
        has_yielding_call: recorded.has_yielding_call,
        op_names: recorded.op_names,
        terminal: recorded.terminal,
        executions: 0,
        ssa: recorded.ssa,
    }
}

fn to_nyi(err: TraceRecordError) -> JitNyiReason {
    match err {
        TraceRecordError::UnsupportedOpcode(op) => JitNyiReason::UnsupportedOpcode(op),
        TraceRecordError::UnsupportedTrace(detail) => JitNyiReason::UnsupportedTrace(detail),
        TraceRecordError::InvalidJumpTarget { target } => {
            JitNyiReason::InvalidJumpTarget { target }
        }
        TraceRecordError::InvalidImmediate(kind) => JitNyiReason::InvalidImmediate(kind),
        TraceRecordError::TraceTooLong { limit } => JitNyiReason::TraceTooLong { limit },
        TraceRecordError::MissingTerminal => JitNyiReason::MissingTerminal,
        TraceRecordError::InvalidLocal(_)
        | TraceRecordError::StackUnderflow
        | TraceRecordError::TypeMismatch { .. }
        | TraceRecordError::StackDepthMismatch { .. }
        | TraceRecordError::InvalidIr(_) => JitNyiReason::UnsupportedTrace(err.to_string()),
    }
}

fn scan_loop_headers(program: &Program) -> Vec<bool> {
    let code = &program.code;
    let mut headers = vec![false; code.len()];
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
                if target <= instr_ip && target < headers.len() {
                    headers[target] = true;
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

fn read_u8(code: &[u8], ip: &mut usize) -> Option<u8> {
    let value = *code.get(*ip)?;
    *ip = ip.saturating_add(1);
    Some(value)
}

fn read_u16(code: &[u8], ip: &mut usize) -> Option<u16> {
    if ip.saturating_add(2) > code.len() {
        return None;
    }
    let bytes = [code[*ip], code[*ip + 1]];
    *ip = ip.saturating_add(2);
    Some(u16::from_le_bytes(bytes))
}

fn read_u32(code: &[u8], ip: &mut usize) -> Option<u32> {
    if ip.saturating_add(4) > code.len() {
        return None;
    }
    let bytes = [code[*ip], code[*ip + 1], code[*ip + 2], code[*ip + 3]];
    *ip = ip.saturating_add(4);
    Some(u32::from_le_bytes(bytes))
}

fn nyi_reference() -> Vec<JitNyiDoc> {
    vec![
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, Value};

    #[test]
    fn scan_loop_headers_finds_backward_targets() {
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldc(0);
        let branch_ip = bc.position();
        bc.br(root_ip);
        let program = Program::new(vec![Value::Int(1)], bc.finish());

        let headers = scan_loop_headers(&program);
        assert!(headers[root_ip as usize]);
        assert!(!headers[branch_ip as usize]);
    }
}
