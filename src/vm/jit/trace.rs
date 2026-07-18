use std::collections::{HashMap, HashSet};

use crate::debug_info::DebugInfo;
use crate::vm::native::ROOT_FRAME_KEY;
use crate::vm::{OpCode, Program};

use super::deopt::{SideTraceImport, SideTraceImportError};
use super::ir::{SsaExitId, SsaMaterialization, SsaTrace, SsaValueId, SsaValueRepr};
use super::liveness::{boxed_load_site_count, boxed_store_site_count};
use super::recorder::{RecordedTrace, TraceRecordError, record_trace_with_local_count};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TraceEntryKey {
    pub(crate) frame_key: u64,
    pub(crate) root_ip: usize,
    pub(crate) stack_depth: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TraceExitKey {
    pub(crate) parent_trace_id: usize,
    pub(crate) exit_id: SsaExitId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TraceExitProfile {
    pub(crate) executions: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TraceExitRecordError {
    UnknownParentTrace(usize),
    UnknownExit {
        parent_trace_id: usize,
        exit_id: SsaExitId,
        exit_count: usize,
    },
}

impl TraceExitRecordError {
    pub(crate) fn message(self) -> String {
        match self {
            Self::UnknownParentTrace(parent_trace_id) => {
                format!("native trace exit parent trace {parent_trace_id} does not exist")
            }
            Self::UnknownExit {
                parent_trace_id,
                exit_id,
                exit_count,
            } => format!(
                "native trace {parent_trace_id} returned impossible exit id {} (parent has {exit_count} exits)",
                exit_id.raw()
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SideTraceAdmissionError {
    UnknownParentTrace(usize),
    UnknownChildTrace(usize),
    SameTrace(usize),
    CrossFrame {
        parent_frame_key: u64,
        child_frame_key: u64,
    },
    YieldingTrace(usize),
    Import(SideTraceImportError),
    UnknownParentValue(SsaValueId),
    ReprMismatch {
        index: usize,
        parent: SsaValueRepr,
        child: SsaValueRepr,
    },
    UnsupportedHeapOwnership {
        index: usize,
        value: SsaValueId,
    },
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

#[allow(dead_code)]
fn ssa_value_repr(trace: &SsaTrace, value: SsaValueId) -> Option<SsaValueRepr> {
    trace.blocks.iter().find_map(|block| {
        block
            .params
            .iter()
            .find_map(|param| (param.value.id == value).then_some(param.value.repr))
            .or_else(|| {
                block.insts.iter().find_map(|inst| {
                    inst.output
                        .filter(|output| output.id == value)
                        .map(|output| output.repr)
                })
            })
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JitTraceTerminal {
    LoopBack,
    Halt,
    BranchExit,
    CallValue,
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
    pub frame_key: u64,
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
    pub frame_key: u64,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitExitProfile {
    pub parent_trace_id: usize,
    pub exit_id: u32,
    pub executions: u64,
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

const CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD: u32 = 64;

pub struct TraceJitEngine {
    config: JitConfig,
    hot_counts: HashMap<TraceEntryKey, u32>,
    compiled_by_ip: Vec<Vec<(u64, usize, usize)>>,
    blocked_entries: HashSet<TraceEntryKey>,
    loop_headers: Option<Vec<bool>>,
    non_yielding_host_imports: Vec<bool>,
    traces: Vec<JitTrace>,
    trace_exit_profiles: HashMap<TraceExitKey, TraceExitProfile>,
    callable_side_exit_streaks: Vec<u32>,
    blocked_callable_frames: Vec<bool>,
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
            trace_exit_profiles: HashMap::new(),
            callable_side_exit_streaks: Vec::new(),
            blocked_callable_frames: Vec::new(),
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
        self.trace_exit_profiles.clear();
        self.callable_side_exit_streaks.clear();
        self.blocked_callable_frames.clear();
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
        self.trace_exit_profiles.clear();
        self.callable_side_exit_streaks.clear();
        self.blocked_callable_frames.clear();
        self.attempts.clear();
        true
    }

    pub fn observe_hot_ip(&mut self, ip: usize, program: &Program) -> Option<usize> {
        self.observe_hot_entry(ROOT_FRAME_KEY, ip, 0, program)
    }

    pub(crate) fn observe_hot_entry(
        &mut self,
        frame_key: u64,
        ip: usize,
        stack_depth: usize,
        program: &Program,
    ) -> Option<usize> {
        self.observe_hot_entry_with_local_types(frame_key, ip, stack_depth, None, program)
    }

    pub(crate) fn observe_hot_entry_with_local_types(
        &mut self,
        frame_key: u64,
        ip: usize,
        stack_depth: usize,
        entry_local_types: Option<&[crate::ValueType]>,
        program: &Program,
    ) -> Option<usize> {
        if !self.config.enabled
            || !native_jit_supported()
            || self.callable_frame_is_blocked(frame_key)
        {
            return None;
        }
        let key = TraceEntryKey {
            frame_key,
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
            self.compile_trace(program, key, entry_local_types)
        };
        self.finish_attempt(key, line, result)
    }

    pub fn trace_clone(&self, trace_id: usize) -> Option<JitTrace> {
        self.traces.get(trace_id).cloned()
    }

    #[allow(dead_code)]
    pub(crate) fn side_trace_import(
        &self,
        parent_trace_id: usize,
        parent_exit_id: SsaExitId,
        child_trace_id: usize,
    ) -> Result<SideTraceImport, SideTraceAdmissionError> {
        let parent = self
            .traces
            .get(parent_trace_id)
            .ok_or(SideTraceAdmissionError::UnknownParentTrace(parent_trace_id))?;
        let child = self
            .traces
            .get(child_trace_id)
            .ok_or(SideTraceAdmissionError::UnknownChildTrace(child_trace_id))?;
        if parent_trace_id == child_trace_id {
            return Err(SideTraceAdmissionError::SameTrace(parent_trace_id));
        }
        if parent.frame_key != child.frame_key {
            return Err(SideTraceAdmissionError::CrossFrame {
                parent_frame_key: parent.frame_key,
                child_frame_key: child.frame_key,
            });
        }
        if parent.has_yielding_call {
            return Err(SideTraceAdmissionError::YieldingTrace(parent_trace_id));
        }
        if child.has_yielding_call {
            return Err(SideTraceAdmissionError::YieldingTrace(child_trace_id));
        }
        let import = super::deopt::side_trace_import(&parent.ssa, parent_exit_id, &child.ssa)
            .map_err(SideTraceAdmissionError::Import)?;
        let child_entry = child.ssa.blocks.get(child.ssa.entry.index()).ok_or(
            SideTraceAdmissionError::Import(SideTraceImportError::InvalidChildEntry),
        )?;
        for (index, (materialization, child_param)) in
            import.args.iter().zip(&child_entry.params).enumerate()
        {
            let parent_repr = match materialization {
                SsaMaterialization::Value(value) => ssa_value_repr(&parent.ssa, *value)
                    .ok_or(SideTraceAdmissionError::UnknownParentValue(*value))?,
                SsaMaterialization::BoxInt(_)
                | SsaMaterialization::BoxFloat(_)
                | SsaMaterialization::BoxBool(_) => SsaValueRepr::Tagged,
                SsaMaterialization::BoxHeapPtr { value, .. } => {
                    return Err(SideTraceAdmissionError::UnsupportedHeapOwnership {
                        index,
                        value: *value,
                    });
                }
            };
            let child_repr = child_param.value.repr;
            if parent_repr != child_repr {
                return Err(SideTraceAdmissionError::ReprMismatch {
                    index,
                    parent: parent_repr,
                    child: child_repr,
                });
            }
        }
        Ok(import)
    }

    pub fn observe_exit_ip(&mut self, ip: usize, program: &Program) -> Option<usize> {
        self.observe_exit_entry(ROOT_FRAME_KEY, ip, 0, program)
    }

    pub(crate) fn observe_exit_entry(
        &mut self,
        frame_key: u64,
        ip: usize,
        stack_depth: usize,
        program: &Program,
    ) -> Option<usize> {
        self.observe_exit_entry_with_local_types(frame_key, ip, stack_depth, None, program)
    }

    pub(crate) fn observe_exit_entry_with_local_types(
        &mut self,
        frame_key: u64,
        ip: usize,
        stack_depth: usize,
        entry_local_types: Option<&[crate::ValueType]>,
        program: &Program,
    ) -> Option<usize> {
        if !self.config.enabled
            || !native_jit_supported()
            || self.callable_frame_is_blocked(frame_key)
        {
            return None;
        }
        let key = TraceEntryKey {
            frame_key,
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
            self.compile_trace(program, key, entry_local_types)
        };
        self.finish_attempt(key, line, result)
    }

    pub fn trace_has_call(&self, trace_id: usize) -> bool {
        self.traces
            .get(trace_id)
            .is_some_and(|trace| trace.has_call)
    }

    pub fn compiled_trace_for_ip(&self, ip: usize) -> Option<usize> {
        self.compiled_trace_for_entry(ROOT_FRAME_KEY, ip, 0)
    }

    pub(crate) fn compiled_trace_for_entry(
        &self,
        frame_key: u64,
        ip: usize,
        stack_depth: usize,
    ) -> Option<usize> {
        self.compiled_trace_for_key(TraceEntryKey {
            frame_key,
            root_ip: ip,
            stack_depth,
        })
    }

    pub fn mark_trace_executed(&mut self, trace_id: usize) {
        if let Some(trace) = self.traces.get_mut(trace_id) {
            trace.executions = trace.executions.saturating_add(1);
        }
    }

    pub(crate) fn record_trace_exit(
        &mut self,
        key: TraceExitKey,
    ) -> Result<(), TraceExitRecordError> {
        let trace = self.traces.get(key.parent_trace_id).ok_or(
            TraceExitRecordError::UnknownParentTrace(key.parent_trace_id),
        )?;
        let exit_count = trace.ssa.exits.len();
        if !trace.ssa.exits.iter().any(|exit| exit.id == key.exit_id) {
            return Err(TraceExitRecordError::UnknownExit {
                parent_trace_id: key.parent_trace_id,
                exit_id: key.exit_id,
                exit_count,
            });
        }
        let profile = self.trace_exit_profiles.entry(key).or_default();
        profile.executions = profile.executions.saturating_add(1);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn trace_exit_profile(&self, key: TraceExitKey) -> Option<&TraceExitProfile> {
        self.trace_exit_profiles.get(&key)
    }

    pub(crate) fn record_native_side_exit(&mut self, trace_id: usize) -> bool {
        if self
            .traces
            .get(trace_id)
            .is_none_or(|trace| trace.frame_key == ROOT_FRAME_KEY)
        {
            return false;
        }
        let Some(streak) = self.callable_side_exit_streaks.get_mut(trace_id) else {
            return false;
        };
        *streak = streak.saturating_add(1);
        *streak >= CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD
    }

    pub(crate) fn record_native_loop_back(&mut self, trace_id: usize) {
        if let Some(streak) = self.callable_side_exit_streaks.get_mut(trace_id) {
            *streak = 0;
        }
    }

    pub(crate) fn callable_frame_is_blocked(&self, frame_key: u64) -> bool {
        if frame_key == ROOT_FRAME_KEY {
            return false;
        }
        usize::try_from(frame_key)
            .ok()
            .and_then(|index| self.blocked_callable_frames.get(index))
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn block_callable_frame(&mut self, trace_id: usize) {
        let Some(frame_key) = self.traces.get(trace_id).map(|trace| trace.frame_key) else {
            return;
        };
        if frame_key == ROOT_FRAME_KEY {
            self.block_trace(trace_id);
            return;
        }
        let Ok(frame_index) = usize::try_from(frame_key) else {
            self.block_trace(trace_id);
            return;
        };
        if self.blocked_callable_frames.len() <= frame_index {
            self.blocked_callable_frames.resize(frame_index + 1, false);
        }
        self.blocked_callable_frames[frame_index] = true;
        for entries in &mut self.compiled_by_ip {
            entries.retain(|(entry_frame_key, _, _)| *entry_frame_key != frame_key);
        }
        self.hot_counts.retain(|key, _| key.frame_key != frame_key);
    }

    pub(crate) fn block_trace(&mut self, trace_id: usize) {
        if let Some(trace) = self.traces.get(trace_id) {
            let key = TraceEntryKey {
                frame_key: trace.frame_key,
                root_ip: trace.root_ip,
                stack_depth: trace.entry_stack_depth,
            };
            self.remove_compiled_trace(key);
            self.blocked_entries.insert(key);
        }
    }

    pub fn exit_profiles(&self) -> Vec<JitExitProfile> {
        let mut profiles = self
            .trace_exit_profiles
            .iter()
            .map(|(key, profile)| JitExitProfile {
                parent_trace_id: key.parent_trace_id,
                exit_id: key.exit_id.raw(),
                executions: profile.executions,
            })
            .collect::<Vec<_>>();
        profiles.sort_by_key(|profile| (profile.parent_trace_id, profile.exit_id));
        profiles
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
        out.push_str(&format!(
            "  profiled trace exits: {}\n",
            self.trace_exit_profiles.len()
        ));
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
                    frame_key: key.frame_key,
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
                    frame_key: key.frame_key,
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
        entry_local_types: Option<&[crate::ValueType]>,
    ) -> Result<usize, JitNyiReason> {
        let local_count = if key.frame_key == ROOT_FRAME_KEY {
            program.local_count
        } else {
            program
                .callable_prototypes
                .get(key.frame_key as usize)
                .map(|prototype| prototype.frame_local_count)
                .ok_or_else(|| {
                    JitNyiReason::UnsupportedTrace(format!(
                        "unknown callable prototype frame key {}",
                        key.frame_key
                    ))
                })?
        };
        let recorded = record_trace_with_local_count(
            program,
            key.root_ip,
            key.stack_depth,
            local_count,
            entry_local_types,
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
        self.callable_side_exit_streaks.push(0);
        Ok(id)
    }

    #[inline(always)]
    fn compiled_trace_for_key(&self, key: TraceEntryKey) -> Option<usize> {
        self.compiled_by_ip.get(key.root_ip)?.iter().find_map(
            |(frame_key, stack_depth, trace_id)| {
                (*frame_key == key.frame_key && *stack_depth == key.stack_depth)
                    .then_some(*trace_id)
            },
        )
    }

    fn insert_compiled_trace(&mut self, key: TraceEntryKey, trace_id: usize) {
        if self.compiled_by_ip.len() <= key.root_ip {
            self.compiled_by_ip.resize_with(key.root_ip + 1, Vec::new);
        }
        let entries = &mut self.compiled_by_ip[key.root_ip];
        if let Some((_, _, existing_trace_id)) =
            entries.iter_mut().find(|(frame_key, stack_depth, _)| {
                *frame_key == key.frame_key && *stack_depth == key.stack_depth
            })
        {
            *existing_trace_id = trace_id;
        } else {
            entries.push((key.frame_key, key.stack_depth, trace_id));
        }
    }

    fn remove_compiled_trace(&mut self, key: TraceEntryKey) {
        let Some(entries) = self.compiled_by_ip.get_mut(key.root_ip) else {
            return;
        };
        if let Some(index) = entries.iter().position(|(frame_key, stack_depth, _)| {
            *frame_key == key.frame_key && *stack_depth == key.stack_depth
        }) {
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
        frame_key: key.frame_key,
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
    use crate::vm::jit::ir::{
        SsaExitId, SsaMaterialization, SsaTerminator, SsaTrace, SsaTraceBuilder, SsaValueRepr,
    };
    use crate::{
        BytecodeBuilder, CallableKind, CallablePrototype, CallableTarget, ScriptFunction, Value,
        ValueType,
    };

    fn test_trace(id: usize, frame_key: u64, has_yielding_call: bool, ssa: SsaTrace) -> JitTrace {
        JitTrace {
            id,
            frame_key,
            root_ip: ssa.root_ip,
            entry_stack_depth: ssa.entry_stack_depth,
            start_line: None,
            has_call: false,
            has_yielding_call,
            op_names: Vec::new(),
            terminal: JitTraceTerminal::BranchExit,
            executions: 0,
            ssa,
        }
    }

    fn tagged_side_trace_pair(
        parent_frame_key: u64,
        child_frame_key: u64,
        parent_yields: bool,
        child_yields: bool,
    ) -> (TraceJitEngine, SsaExitId) {
        let mut parent = SsaTraceBuilder::new(0, 1);
        let parent_entry = parent.entry();
        let stack = parent
            .append_param(parent_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        let parent_exit = parent.add_exit(
            12,
            vec![SsaMaterialization::Value(stack.id)],
            Vec::new(),
            Vec::new(),
        );
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: parent_exit })
            .unwrap();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();

        let mut engine = TraceJitEngine::new(JitConfig::default());
        engine.traces.push(test_trace(
            0,
            parent_frame_key,
            parent_yields,
            parent.finish(),
        ));
        engine
            .traces
            .push(test_trace(1, child_frame_key, child_yields, child.finish()));
        (engine, parent_exit)
    }

    #[test]
    fn side_trace_admission_rejects_unknown_and_same_trace_ids() {
        let (engine, parent_exit) = tagged_side_trace_pair(7, 7, false, false);

        assert_eq!(
            engine.side_trace_import(9, parent_exit, 1),
            Err(SideTraceAdmissionError::UnknownParentTrace(9))
        );
        assert_eq!(
            engine.side_trace_import(0, parent_exit, 9),
            Err(SideTraceAdmissionError::UnknownChildTrace(9))
        );
        assert_eq!(
            engine.side_trace_import(0, SsaExitId::new(99), 1),
            Err(SideTraceAdmissionError::Import(
                SideTraceImportError::UnknownParentExit(SsaExitId::new(99))
            ))
        );
        assert_eq!(
            engine.side_trace_import(0, parent_exit, 0),
            Err(SideTraceAdmissionError::SameTrace(0))
        );
    }

    #[test]
    fn side_trace_admission_rejects_exit_entry_shape_mismatch() {
        let (mut engine, parent_exit) = tagged_side_trace_pair(7, 7, false, false);
        engine.traces[1].ssa.root_ip = 13;

        assert_eq!(
            engine.side_trace_import(0, parent_exit, 1),
            Err(SideTraceAdmissionError::Import(
                SideTraceImportError::ExitIpMismatch {
                    parent: 12,
                    child: 13,
                }
            ))
        );
    }

    #[test]
    fn side_trace_admission_rejects_repr_mismatch_at_any_entry_param() {
        let mut parent = SsaTraceBuilder::new(0, 1);
        let parent_entry = parent.entry();
        let stack = parent
            .append_param(parent_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        let local = parent
            .append_param(parent_entry, SsaValueRepr::I64, "local0")
            .unwrap();
        let parent_exit = parent.add_exit(
            12,
            vec![SsaMaterialization::Value(stack.id)],
            vec![SsaMaterialization::BoxInt(local.id)],
            vec![true],
        );
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: parent_exit })
            .unwrap();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        child
            .append_param(child_entry, SsaValueRepr::I64, "local0")
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();

        let mut engine = TraceJitEngine::new(JitConfig::default());
        engine.traces.push(test_trace(0, 7, false, parent.finish()));
        engine.traces.push(test_trace(1, 7, false, child.finish()));

        assert_eq!(
            engine.side_trace_import(0, parent_exit, 1),
            Err(SideTraceAdmissionError::ReprMismatch {
                index: 1,
                parent: SsaValueRepr::Tagged,
                child: SsaValueRepr::I64,
            })
        );
    }

    #[test]
    fn side_trace_admission_uses_defining_repr_for_value_materialization() {
        let mut parent = SsaTraceBuilder::new(0, 1);
        let parent_entry = parent.entry();
        let value = parent
            .append_param(parent_entry, SsaValueRepr::I64, "stack0")
            .unwrap();
        let parent_exit = parent.add_exit(
            12,
            vec![SsaMaterialization::Value(value.id)],
            Vec::new(),
            Vec::new(),
        );
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: parent_exit })
            .unwrap();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();

        let mut engine = TraceJitEngine::new(JitConfig::default());
        engine.traces.push(test_trace(0, 7, false, parent.finish()));
        engine.traces.push(test_trace(1, 7, false, child.finish()));

        assert_eq!(
            engine.side_trace_import(0, parent_exit, 1),
            Err(SideTraceAdmissionError::ReprMismatch {
                index: 0,
                parent: SsaValueRepr::I64,
                child: SsaValueRepr::Tagged,
            })
        );
    }

    #[test]
    fn side_trace_admission_rejects_box_heap_ptr_ownership() {
        let mut parent = SsaTraceBuilder::new(0, 1);
        let parent_entry = parent.entry();
        let heap = parent
            .append_param(
                parent_entry,
                SsaValueRepr::HeapPtr(ValueType::String),
                "stack0",
            )
            .unwrap();
        let parent_exit = parent.add_exit(
            12,
            vec![SsaMaterialization::BoxHeapPtr {
                value: heap.id,
                tag: ValueType::String,
            }],
            Vec::new(),
            Vec::new(),
        );
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: parent_exit })
            .unwrap();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();

        let mut engine = TraceJitEngine::new(JitConfig::default());
        engine.traces.push(test_trace(0, 7, false, parent.finish()));
        engine.traces.push(test_trace(1, 7, false, child.finish()));

        assert_eq!(
            engine.side_trace_import(0, parent_exit, 1),
            Err(SideTraceAdmissionError::UnsupportedHeapOwnership {
                index: 0,
                value: heap.id,
            })
        );
    }

    #[test]
    fn side_trace_admission_rejects_yielding_parent_or_child() {
        for (parent_yields, child_yields, yielding_trace_id) in [(true, false, 0), (false, true, 1)]
        {
            let (engine, parent_exit) = tagged_side_trace_pair(7, 7, parent_yields, child_yields);
            assert_eq!(
                engine.side_trace_import(0, parent_exit, 1),
                Err(SideTraceAdmissionError::YieldingTrace(yielding_trace_id))
            );
        }
    }

    #[test]
    fn side_trace_admission_rejects_cross_frame_traces() {
        let (engine, parent_exit) = tagged_side_trace_pair(7, 8, false, false);

        assert_eq!(
            engine.side_trace_import(0, parent_exit, 1),
            Err(SideTraceAdmissionError::CrossFrame {
                parent_frame_key: 7,
                child_frame_key: 8,
            })
        );
    }

    #[test]
    fn side_trace_admission_returns_validated_import_for_same_frame_traces() {
        let mut parent = SsaTraceBuilder::new(0, 1);
        let parent_entry = parent.entry();
        let stack = parent
            .append_param(parent_entry, SsaValueRepr::Tagged, "stack0")
            .unwrap();
        let int_local = parent
            .append_param(parent_entry, SsaValueRepr::I64, "local0")
            .unwrap();
        let float_local = parent
            .append_param(parent_entry, SsaValueRepr::F64, "local1")
            .unwrap();
        let bool_local = parent
            .append_param(parent_entry, SsaValueRepr::Bool, "local2")
            .unwrap();
        let parent_exit = parent.add_exit(
            12,
            vec![SsaMaterialization::Value(stack.id)],
            vec![
                SsaMaterialization::BoxInt(int_local.id),
                SsaMaterialization::BoxFloat(float_local.id),
                SsaMaterialization::BoxBool(bool_local.id),
            ],
            vec![true, false, true],
        );
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: parent_exit })
            .unwrap();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        for label in ["stack0", "local0", "local1", "local2"] {
            child
                .append_param(child_entry, SsaValueRepr::Tagged, label)
                .unwrap();
        }
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();

        let mut engine = TraceJitEngine::new(JitConfig::default());
        engine.traces.push(test_trace(0, 7, false, parent.finish()));
        engine.traces.push(test_trace(1, 7, false, child.finish()));

        let import = engine.side_trace_import(0, parent_exit, 1).unwrap();

        assert_eq!(import.parent_exit, parent_exit);
        assert_eq!(import.stack_depth, 1);
        assert_eq!(import.local_count, 3);
        assert_eq!(import.dirty_locals, vec![true, false, true]);
        assert_eq!(
            import.args,
            vec![
                SsaMaterialization::Value(stack.id),
                SsaMaterialization::BoxInt(int_local.id),
                SsaMaterialization::BoxFloat(float_local.id),
                SsaMaterialization::BoxBool(bool_local.id),
            ]
        );
    }

    #[test]
    fn trace_exit_profiles_are_keyed_by_parent_and_exit() {
        let mut engine = TraceJitEngine::new(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 64,
        });
        let mut ssa = SsaTraceBuilder::new(0, 0);
        let entry = ssa.entry();
        let first_exit = ssa.add_exit(1, Vec::new(), Vec::new(), Vec::new());
        let second_exit = ssa.add_exit(2, Vec::new(), Vec::new(), Vec::new());
        ssa.set_terminator(entry, SsaTerminator::Exit { exit: first_exit })
            .unwrap();
        engine.traces.push(JitTrace {
            id: 0,
            frame_key: ROOT_FRAME_KEY,
            root_ip: 0,
            entry_stack_depth: 0,
            start_line: None,
            has_call: false,
            has_yielding_call: false,
            op_names: Vec::new(),
            terminal: JitTraceTerminal::BranchExit,
            executions: 0,
            ssa: ssa.finish(),
        });
        let first = TraceExitKey {
            parent_trace_id: 0,
            exit_id: first_exit,
        };
        let second = TraceExitKey {
            parent_trace_id: 0,
            exit_id: second_exit,
        };

        engine.record_trace_exit(first).unwrap();
        engine.record_trace_exit(first).unwrap();
        engine.record_trace_exit(second).unwrap();

        assert_eq!(engine.trace_exit_profile(first).unwrap().executions, 2);
        assert_eq!(engine.trace_exit_profile(second).unwrap().executions, 1);
        assert_ne!(first, second);
        assert_eq!(
            engine.exit_profiles(),
            vec![
                JitExitProfile {
                    parent_trace_id: 0,
                    exit_id: first_exit.raw(),
                    executions: 2,
                },
                JitExitProfile {
                    parent_trace_id: 0,
                    exit_id: second_exit.raw(),
                    executions: 1,
                },
            ]
        );
    }

    #[test]
    fn trace_exit_profiles_reject_exit_missing_from_parent_trace() {
        let mut engine = TraceJitEngine::new(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 64,
        });
        let mut ssa = SsaTraceBuilder::new(0, 0);
        let entry = ssa.entry();
        let exit_id = ssa.add_exit(1, Vec::new(), Vec::new(), Vec::new());
        ssa.set_terminator(entry, SsaTerminator::Exit { exit: exit_id })
            .unwrap();
        engine.traces.push(JitTrace {
            id: 0,
            frame_key: ROOT_FRAME_KEY,
            root_ip: 0,
            entry_stack_depth: 0,
            start_line: None,
            has_call: false,
            has_yielding_call: false,
            op_names: Vec::new(),
            terminal: JitTraceTerminal::BranchExit,
            executions: 0,
            ssa: ssa.finish(),
        });

        assert_eq!(
            engine
                .record_trace_exit(TraceExitKey {
                    parent_trace_id: 0,
                    exit_id: SsaExitId::new(1),
                })
                .unwrap_err(),
            TraceExitRecordError::UnknownExit {
                parent_trace_id: 0,
                exit_id: SsaExitId::new(1),
                exit_count: 1,
            }
        );
        assert!(engine.trace_exit_profiles.is_empty());
    }

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

    #[test]
    fn callable_side_exit_backoff_resets_on_native_loopback() {
        if !native_jit_supported() {
            return;
        }
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(0);
        bc.add();
        bc.stloc(0);
        bc.br(root_ip);
        let mut program = Program::new(vec![Value::Int(1)], bc.finish()).with_local_count(1);
        program.script_functions.push(ScriptFunction {
            entry_ip: root_ip,
            end_ip: program.code.len() as u32,
        });
        program.callable_prototypes.push(CallablePrototype {
            kind: CallableKind::FunctionItem,
            target: CallableTarget::ScriptFunction(0),
            arity: 0,
            frame_local_count: 1,
            parameter_slots: Vec::new(),
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        });
        let mut engine = TraceJitEngine::new(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 64,
        });
        let trace_id = engine
            .observe_hot_entry(0, root_ip as usize, 0, &program)
            .expect("script-frame trace should compile");

        for _ in 1..CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD {
            assert!(!engine.record_native_side_exit(trace_id));
        }
        engine.record_native_loop_back(trace_id);
        for _ in 1..CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD {
            assert!(!engine.record_native_side_exit(trace_id));
        }
        assert!(engine.record_native_side_exit(trace_id));
        engine.block_callable_frame(trace_id);
        assert!(engine.callable_frame_is_blocked(0));
        assert_eq!(
            engine.compiled_trace_for_entry(0, root_ip as usize, 0),
            None
        );

        let root_trace_id = engine
            .observe_hot_entry(ROOT_FRAME_KEY, root_ip as usize, 0, &program)
            .expect("root trace should compile");
        for _ in 0..CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD * 2 {
            assert!(!engine.record_native_side_exit(root_trace_id));
        }
    }

    #[test]
    fn callable_frame_backoff_clears_on_jit_invalidation() {
        if !native_jit_supported() {
            return;
        }
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(0);
        bc.add();
        bc.stloc(0);
        bc.br(root_ip);
        let mut program = Program::new(vec![Value::Int(1)], bc.finish()).with_local_count(1);
        program.script_functions.push(ScriptFunction {
            entry_ip: root_ip,
            end_ip: program.code.len() as u32,
        });
        program.callable_prototypes.push(CallablePrototype {
            kind: CallableKind::FunctionItem,
            target: CallableTarget::ScriptFunction(0),
            arity: 0,
            frame_local_count: 1,
            parameter_slots: Vec::new(),
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        });
        let config = JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 64,
        };
        let mut engine = TraceJitEngine::new(config);

        let trace_id = engine
            .observe_hot_entry(0, root_ip as usize, 0, &program)
            .expect("script-frame trace should compile");
        for _ in 0..CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD {
            engine.record_native_side_exit(trace_id);
        }
        engine.block_callable_frame(trace_id);
        assert!(engine.callable_frame_is_blocked(0));

        engine.set_config(config);
        assert!(!engine.callable_frame_is_blocked(0));

        let trace_id = engine
            .observe_hot_entry(0, root_ip as usize, 0, &program)
            .expect("script-frame trace should recompile after config invalidation");
        for _ in 0..CALLABLE_SIDE_EXIT_BACKOFF_THRESHOLD {
            engine.record_native_side_exit(trace_id);
        }
        engine.block_callable_frame(trace_id);
        assert!(engine.callable_frame_is_blocked(0));

        assert!(engine.set_non_yielding_host_imports(vec![true]));
        assert!(!engine.callable_frame_is_blocked(0));
    }

    #[test]
    fn trace_entry_cache_separates_root_and_script_frames() {
        if !native_jit_supported() {
            return;
        }
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(0);
        bc.add();
        bc.stloc(0);
        bc.br(root_ip);
        let mut program = Program::new(vec![Value::Int(1)], bc.finish()).with_local_count(4);
        program.script_functions.push(ScriptFunction {
            entry_ip: root_ip,
            end_ip: program.code.len() as u32,
        });
        program.callable_prototypes.push(CallablePrototype {
            kind: CallableKind::FunctionItem,
            target: CallableTarget::ScriptFunction(0),
            arity: 0,
            frame_local_count: 1,
            parameter_slots: Vec::new(),
            capture_source_slots: Vec::new(),
            capture_slots: Vec::new(),
            capture_modes: Vec::new(),
            self_slot: None,
            schema: None,
        });
        let mut engine = TraceJitEngine::new(JitConfig {
            enabled: true,
            hot_loop_threshold: 1,
            max_trace_len: 64,
        });

        let root_trace = engine
            .observe_hot_entry(ROOT_FRAME_KEY, root_ip as usize, 0, &program)
            .expect("root trace should compile");
        let frame_trace = engine
            .observe_hot_entry(0, root_ip as usize, 0, &program)
            .expect("script-frame trace should compile separately");
        assert_ne!(root_trace, frame_trace);
        assert_eq!(engine.traces[root_trace].ssa.blocks[0].params.len(), 4);
        assert_eq!(engine.traces[frame_trace].ssa.blocks[0].params.len(), 1);
        assert_eq!(
            engine.compiled_trace_for_entry(ROOT_FRAME_KEY, root_ip as usize, 0),
            Some(root_trace)
        );
        assert_eq!(
            engine.compiled_trace_for_entry(0, root_ip as usize, 0),
            Some(frame_trace)
        );
    }
}
