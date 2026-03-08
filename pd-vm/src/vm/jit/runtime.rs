use super::super::{ExecOutcome, HostCallExecOutcome, Vm, VmError, VmResult};
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use super::JitTrace;
use super::{JitTraceTerminal, TraceStep, native};
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use std::{cell::RefCell, thread_local};

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
type NativeTraceEntry = unsafe extern "C" fn(*mut Vm) -> i32;

#[cfg(not(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
)))]
type NativeTraceEntry = fn(*mut Vm) -> i32;

pub(crate) struct NativeTrace {
    _keepalive: Arc<Mutex<native::TraceKeepAlive>>,
    entry: NativeTraceEntry,
    pub(super) code: Arc<[u8]>,
    root_ip: usize,
    terminal: JitTraceTerminal,
    has_yielding_call: bool,
    interrupt_settings: Option<native::NativeInterruptSettings>,
    compile_profile: native::NativeCompileProfile,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NativeTraceCacheKey {
    interrupt_settings: Option<native::NativeInterruptSettings>,
    compile_profile: native::NativeCompileProfile,
    root_ip: usize,
    terminal: JitTraceTerminal,
    steps: Vec<TraceStep>,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
#[derive(Clone)]
struct NativeTraceCacheEntry {
    entry: NativeTraceEntry,
    keepalive: Arc<Mutex<native::TraceKeepAlive>>,
    code: Arc<[u8]>,
    compile_profile: native::NativeCompileProfile,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
struct NativeTraceCache {
    active_program_key: Option<u64>,
    entries: HashMap<NativeTraceCacheKey, NativeTraceCacheEntry>,
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
thread_local! {
    static NATIVE_TRACE_CACHE: RefCell<NativeTraceCache> = RefCell::new(
        NativeTraceCache {
            active_program_key: None,
            entries: HashMap::new(),
        }
    );
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn with_native_trace_cache<R>(f: impl FnOnce(&mut NativeTraceCache) -> R) -> R {
    NATIVE_TRACE_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        f(&mut cache)
    })
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_key(
    trace: &JitTrace,
    interrupt_settings: Option<native::NativeInterruptSettings>,
    compile_profile: native::NativeCompileProfile,
) -> NativeTraceCacheKey {
    NativeTraceCacheKey {
        interrupt_settings,
        compile_profile,
        root_ip: trace.root_ip,
        terminal: trace.terminal.clone(),
        steps: trace.steps.clone(),
    }
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn compile_profile_satisfies(
    compiled: native::NativeCompileProfile,
    requested: native::NativeCompileProfile,
) -> bool {
    compiled == requested
        || matches!(
            (compiled, requested),
            (
                native::NativeCompileProfile::Aot,
                native::NativeCompileProfile::Jit
            )
        )
}

impl Vm {
    fn active_native_interrupt_settings(&self) -> Option<native::NativeInterruptSettings> {
        match self.interrupt_mode {
            super::super::InterruptMode::None => None,
            super::super::InterruptMode::Fuel => {
                Some(native::NativeInterruptSettings::fuel(self.fuel_check_interval))
            }
            super::super::InterruptMode::Epoch => {
                Some(native::NativeInterruptSettings::epoch(self.fuel_check_interval))
            }
        }
    }

    pub fn set_jit_config(&mut self, config: super::JitConfig) {
        if config.enabled {
            self.ensure_program_cache_key();
        }
        self.native_traces.clear();
        self.jit.set_config(config);
    }

    pub fn jit_config(&self) -> &super::JitConfig {
        self.jit.config()
    }

    pub fn jit_snapshot(&self) -> super::JitSnapshot {
        self.jit.snapshot()
    }

    pub fn dump_jit_info(&self) -> String {
        self.dump_jit_info_with_machine_code(true)
    }

    pub fn dump_jit_info_with_machine_code(&self, include_machine_code: bool) -> String {
        let mut out = self.jit.dump_text(self.program.debug.as_ref());
        out.push_str(&format!(
            "  native codegen backend: {}\n",
            native::selected_codegen_backend()
        ));
        out.push_str(&format!(
            "  native trace executions: {}\n",
            self.native_trace_exec_count
        ));
        if self.jit_native_bridge_stats_enabled {
            let mut bridge_entries: Vec<(&'static str, u64)> = self
                .jit_native_bridge_counts
                .iter()
                .map(|(name, count)| (*name, *count))
                .collect();
            bridge_entries.sort_unstable_by_key(|(name, _)| *name);
            let total_bridge_hits = bridge_entries
                .iter()
                .fold(0u64, |acc, (_, count)| acc.saturating_add(*count));
            out.push_str(&format!(
                "  native bridge hits: {} (helpers={})\n",
                total_bridge_hits,
                bridge_entries.len()
            ));
            for (name, count) in bridge_entries {
                out.push_str(&format!("    bridge {}: {}\n", name, count));
            }
        }
        if self.native_traces.is_empty() {
            out.push_str("  native traces: 0\n");
            return out;
        }

        out.push_str(&format!("  native traces: {}\n", self.native_traces.len()));
        let mut ids: Vec<usize> = self.native_traces.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(native) = self.native_traces.get(&id) {
                out.push_str(&format!(
                    "  native trace#{} entry=0x{:X} code_bytes={}\n",
                    id,
                    native.entry as usize,
                    native.code.len()
                ));
                if include_machine_code {
                    out.push_str("    code:");
                    for byte in native.code.iter() {
                        out.push_str(&format!(" {:02X}", byte));
                    }
                    out.push('\n');
                }
            }
        }
        out
    }

    pub fn prepare_aot(&mut self) -> VmResult<usize> {
        if !self.jit_config().enabled {
            return Ok(0);
        }
        self.ensure_program_cache_key();
        self.native_traces.clear();
        let trace_ids = {
            let program = &self.program;
            self.jit.prepare_aot(program)
        };
        #[cfg(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ))]
        {
            for trace_id in trace_ids.iter().copied() {
                self.ensure_native_trace(trace_id, native::NativeCompileProfile::Aot)?;
            }
        }
        Ok(trace_ids.len())
    }

    fn execute_jit_trace(&mut self, trace_id: usize) -> VmResult<ExecOutcome> {
        let Some(trace) = self.jit.trace_clone(trace_id) else {
            return Ok(ExecOutcome::Continue);
        };
        for (step_index, step) in trace.steps.iter().enumerate() {
            self.ip = trace
                .step_ips
                .get(step_index)
                .copied()
                .unwrap_or(trace.root_ip);
            self.charge_interrupt_tick()?;
            match step {
                TraceStep::Nop => {}
                TraceStep::Ldc(index) => {
                    let value = self
                        .program
                        .constants
                        .get(*index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidConstant(*index))?;
                    self.stack.push(value);
                }
                TraceStep::Add => {
                    self.binary_add_op()?;
                }
                TraceStep::Sub => {
                    self.binary_numeric_op(
                        |lhs, rhs| Ok(lhs.wrapping_sub(rhs)),
                        |lhs, rhs| Ok(lhs - rhs),
                    )?;
                }
                TraceStep::Mul => {
                    self.binary_numeric_op(
                        |lhs, rhs| Ok(lhs.wrapping_mul(rhs)),
                        |lhs, rhs| Ok(lhs * rhs),
                    )?;
                }
                TraceStep::Div => {
                    self.binary_numeric_op(crate::vm::checked_int_div, |lhs, rhs| Ok(lhs / rhs))?;
                }
                TraceStep::Mod => {
                    self.binary_numeric_op(crate::vm::checked_int_rem, |lhs, rhs| Ok(lhs % rhs))?;
                }
                TraceStep::Shl => {
                    let rhs = self.pop_shift_amount()?;
                    let lhs = self.pop_int()?;
                    self.stack
                        .push(crate::bytecode::Value::Int(lhs.wrapping_shl(rhs)));
                }
                TraceStep::Shr => {
                    let rhs = self.pop_shift_amount()?;
                    let lhs = self.pop_int()?;
                    self.stack
                        .push(crate::bytecode::Value::Int(lhs.wrapping_shr(rhs)));
                }
                TraceStep::Lshr => {
                    let rhs = self.pop_shift_amount()?;
                    let lhs = self.pop_int()?;
                    self.stack
                        .push(crate::bytecode::Value::Int(super::super::logical_shr_i64(
                            lhs, rhs,
                        )));
                }
                TraceStep::And => {
                    let rhs = self.pop_bool()?;
                    let lhs = self.pop_bool()?;
                    self.stack.push(crate::bytecode::Value::Bool(lhs && rhs));
                }
                TraceStep::Or => {
                    let rhs = self.pop_bool()?;
                    let lhs = self.pop_bool()?;
                    self.stack.push(crate::bytecode::Value::Bool(lhs || rhs));
                }
                TraceStep::Not => {
                    self.unary_not_op()?;
                }
                TraceStep::Neg => {
                    let value = self.pop_numeric()?;
                    match value {
                        super::super::NumericValue::Int(value) => self
                            .stack
                            .push(crate::bytecode::Value::Int(value.wrapping_neg())),
                        super::super::NumericValue::Float(value) => {
                            self.stack.push(crate::bytecode::Value::Float(-value))
                        }
                    }
                }
                TraceStep::Ceq => {
                    let rhs = self.pop_value()?;
                    let lhs = self.pop_value()?;
                    self.stack.push(crate::bytecode::Value::Bool(lhs == rhs));
                }
                TraceStep::Clt => {
                    self.compare_numeric_op(|lhs, rhs| lhs < rhs, |lhs, rhs| lhs < rhs)?;
                }
                TraceStep::Cgt => {
                    self.compare_numeric_op(|lhs, rhs| lhs > rhs, |lhs, rhs| lhs > rhs)?;
                }
                TraceStep::Pop => {
                    self.pop_value()?;
                }
                TraceStep::Dup => {
                    let value = self.peek_value()?.clone();
                    self.stack.push(value);
                }
                TraceStep::Ldloc(index) => {
                    let slot = self
                        .locals
                        .get_mut(*index as usize)
                        .ok_or(VmError::InvalidLocal(*index))?;
                    let value = std::mem::replace(slot, crate::bytecode::Value::Null);
                    self.stack.push(value);
                }
                TraceStep::Stloc(index) => {
                    let value = self.pop_value()?;
                    self.store_local_with_drop_contract(*index, value)?;
                }
                TraceStep::BuiltinCall {
                    index,
                    argc,
                    call_ip,
                } => match self.execute_host_call(*index, *argc, *call_ip)? {
                    HostCallExecOutcome::Returned => {}
                    HostCallExecOutcome::Yielded => {
                        self.last_yield_reason = Some(super::super::VmYieldReason::Host);
                        return Ok(ExecOutcome::Yielded);
                    }
                    HostCallExecOutcome::Pending(op_id) => {
                        return Ok(ExecOutcome::Waiting(op_id));
                    }
                },
                TraceStep::Call {
                    index,
                    argc,
                    call_ip,
                } => match self.execute_host_call(*index, *argc, *call_ip)? {
                    HostCallExecOutcome::Returned => {}
                    HostCallExecOutcome::Yielded => {
                        self.last_yield_reason = Some(super::super::VmYieldReason::Host);
                        return Ok(ExecOutcome::Yielded);
                    }
                    HostCallExecOutcome::Pending(op_id) => return Ok(ExecOutcome::Waiting(op_id)),
                },
                TraceStep::GuardFalse { exit_ip } => {
                    let condition = self.pop_bool()?;
                    if !condition {
                        self.jump_to(*exit_ip)?;
                        self.jit.mark_trace_executed(trace_id);
                        return Ok(ExecOutcome::Continue);
                    }
                }
                TraceStep::JumpToIp { target_ip } => {
                    self.jump_to(*target_ip)?;
                    self.jit.mark_trace_executed(trace_id);
                    return Ok(ExecOutcome::Continue);
                }
                TraceStep::JumpToRoot => {
                    self.jump_to(trace.root_ip)?;
                    self.jit.mark_trace_executed(trace_id);
                    return Ok(ExecOutcome::Continue);
                }
                TraceStep::Ret => {
                    self.jit.mark_trace_executed(trace_id);
                    return Ok(ExecOutcome::Halted);
                }
            }
        }
        self.jit.mark_trace_executed(trace_id);
        Ok(ExecOutcome::Continue)
    }

    pub(in crate::vm) fn execute_jit_entry(&mut self, trace_id: usize) -> VmResult<ExecOutcome> {
        #[cfg(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ))]
        {
            if !self.builtin_overrides.is_empty() {
                return self.execute_jit_trace(trace_id);
            }
            self.execute_jit_native(trace_id)
        }
        #[cfg(not(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        )))]
        {
            self.execute_jit_trace(trace_id)
        }
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn execute_jit_native(&mut self, trace_id: usize) -> VmResult<ExecOutcome> {
        let mut current_trace_id = trace_id;
        self.ensure_native_trace(current_trace_id, native::NativeCompileProfile::Jit)?;
        let (mut entry, mut root_ip, mut terminal, mut has_yielding_call) =
            self.native_trace_state(current_trace_id)?;
        loop {
            native::clear_bridge_error();
            let status = unsafe { entry(self as *mut Vm) };
            self.native_trace_exec_count = self.native_trace_exec_count.saturating_add(1);
            self.jit.mark_trace_executed(current_trace_id);

            match status {
                native::STATUS_CONTINUE => {
                    if !has_yielding_call
                        && let Some(next_trace_id) = self.jit.compiled_trace_for_ip(self.ip)
                        && next_trace_id != current_trace_id
                    {
                        current_trace_id = next_trace_id;
                        self.ensure_native_trace(
                            current_trace_id,
                            native::NativeCompileProfile::Jit,
                        )?;
                        (entry, root_ip, terminal, has_yielding_call) =
                            self.native_trace_state(current_trace_id)?;
                        continue;
                    }
                    return Ok(ExecOutcome::Continue);
                }
                native::STATUS_TRACE_EXIT => {
                    // Fast path: if this trace looped back to its own root and cannot yield via host
                    // calls, keep executing in native mode without bouncing through the interpreter.
                    if !has_yielding_call
                        && terminal == JitTraceTerminal::LoopBack
                        && self.ip == root_ip
                    {
                        continue;
                    }
                    if !has_yielding_call {
                        let ip = self.ip;
                        let mut next_trace_id = self.jit.compiled_trace_for_ip(ip);
                        if next_trace_id.is_none() {
                            next_trace_id = {
                                let program = &self.program;
                                self.jit.observe_exit_ip(ip, program)
                            };
                        }
                        if let Some(next_trace_id) = next_trace_id
                            && next_trace_id != current_trace_id
                        {
                            current_trace_id = next_trace_id;
                            self.ensure_native_trace(
                                current_trace_id,
                                native::NativeCompileProfile::Jit,
                            )?;
                            (entry, root_ip, terminal, has_yielding_call) =
                                self.native_trace_state(current_trace_id)?;
                            continue;
                        }
                    }
                    return Ok(ExecOutcome::Continue);
                }
                native::STATUS_HALTED => return Ok(ExecOutcome::Halted),
                native::STATUS_YIELDED => {
                    self.last_yield_reason = Some(super::super::VmYieldReason::Host);
                    return Ok(ExecOutcome::Yielded);
                }
                native::STATUS_WAITING => {
                    let op_id = self.waiting_host_op.map(|op| op.op_id).ok_or_else(|| {
                        VmError::JitNative(
                            "native call bridge reported waiting without a pending op".to_string(),
                        )
                    })?;
                    return Ok(ExecOutcome::Waiting(op_id));
                }
                native::STATUS_OUT_OF_FUEL => {
                    return match self.interrupt_mode {
                        super::super::InterruptMode::Fuel => Err(VmError::OutOfFuel {
                            needed: u64::from(self.fuel_check_interval),
                            remaining: self.fuel_remaining,
                        }),
                        super::super::InterruptMode::Epoch => Err(
                            VmError::EpochDeadlineReached {
                                current: self.current_epoch(),
                                deadline: self.epoch_deadline,
                            },
                        ),
                        super::super::InterruptMode::None => Err(VmError::JitNative(
                            "native interruption checkpoint fired while interruption was disabled"
                                .to_string(),
                        )),
                    };
                }
                native::STATUS_ERROR => {
                    let err = native::take_bridge_error().unwrap_or_else(|| {
                        let trace_meta = self.jit.trace_clone(current_trace_id).map(|trace| {
                            format!(
                                "trace_id={} root_ip={} terminal={:?} steps={}",
                                trace.id,
                                trace.root_ip,
                                trace.terminal,
                                trace.steps.len()
                            )
                        });
                        VmError::JitNative(format!(
                            "jit bridge reported failure without VmError (ip={} stack_len={} {})",
                            self.ip,
                            self.stack.len(),
                            trace_meta.unwrap_or_else(|| "trace=<missing>".to_string())
                        ))
                    });
                    return Err(err);
                }
                other => {
                    return Err(VmError::JitNative(format!(
                        "unexpected native trace return status {}",
                        other
                    )));
                }
            }
        }
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn native_trace_state(
        &self,
        trace_id: usize,
    ) -> VmResult<(NativeTraceEntry, usize, JitTraceTerminal, bool)> {
        let native = self.native_traces.get(&trace_id).ok_or_else(|| {
            VmError::JitNative(format!("native trace entry for id {} missing", trace_id))
        })?;
        Ok((
            native.entry,
            native.root_ip,
            native.terminal.clone(),
            native.has_yielding_call,
        ))
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn ensure_native_trace(
        &mut self,
        trace_id: usize,
        compile_profile: native::NativeCompileProfile,
    ) -> VmResult<()> {
        let interrupt_settings = self.active_native_interrupt_settings();
        self.ensure_native_trace_with_settings(trace_id, compile_profile, interrupt_settings)
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    pub(super) fn ensure_native_trace_with_settings(
        &mut self,
        trace_id: usize,
        compile_profile: native::NativeCompileProfile,
        interrupt_settings: Option<native::NativeInterruptSettings>,
    ) -> VmResult<()> {
        if let Some(native) = self.native_traces.get(&trace_id)
            && native.interrupt_settings == interrupt_settings
            && compile_profile_satisfies(native.compile_profile, compile_profile)
        {
            return Ok(());
        }
        self.native_traces.remove(&trace_id);

        let program_cache_key = self.ensure_program_cache_key();
        let trace = self.jit.trace_clone(trace_id).ok_or_else(|| {
            VmError::JitNative(format!("trace {} missing for native compile", trace_id))
        })?;
        let key = native_trace_cache_key(&trace, interrupt_settings, compile_profile);
        let fallback_key = (compile_profile == native::NativeCompileProfile::Jit).then_some(
            native_trace_cache_key(
                &trace,
                interrupt_settings,
                native::NativeCompileProfile::Aot,
            ),
        );
        let cached = with_native_trace_cache(|cache| {
            if cache.active_program_key != Some(program_cache_key) {
                cache.entries.clear();
                cache.active_program_key = Some(program_cache_key);
            }
            if let Some(cached) = cache.entries.get(&key).cloned() {
                return Some(cached);
            }
            fallback_key.and_then(|fallback| cache.entries.get(&fallback).cloned())
        });
        if let Some(cached) = cached {
            self.native_traces.insert(
                trace_id,
                NativeTrace {
                    _keepalive: cached.keepalive,
                    entry: cached.entry,
                    code: cached.code,
                    root_ip: trace.root_ip,
                    terminal: trace.terminal,
                    has_yielding_call: trace.has_yielding_call,
                    interrupt_settings,
                    compile_profile: cached.compile_profile,
                },
            );
            return Ok(());
        }

        let compiled =
            native::compile_native_trace(&trace, interrupt_settings, compile_profile)?;
        let entry = unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(compiled.entry) };
        let code = Arc::<[u8]>::from(compiled.code.into_boxed_slice());
        let keepalive = Arc::new(Mutex::new(compiled.keepalive));
        let cached = NativeTraceCacheEntry {
            entry,
            keepalive: Arc::clone(&keepalive),
            code: Arc::clone(&code),
            compile_profile,
        };
        with_native_trace_cache(|cache| {
            if cache.active_program_key != Some(program_cache_key) {
                cache.entries.clear();
                cache.active_program_key = Some(program_cache_key);
            }
            cache.entries.insert(key, cached);
        });
        self.native_traces.insert(
            trace_id,
            NativeTrace {
                _keepalive: keepalive,
                entry,
                code,
                root_ip: trace.root_ip,
                terminal: trace.terminal,
                has_yielding_call: trace.has_yielding_call,
                interrupt_settings,
                compile_profile,
            },
        );
        Ok(())
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    pub(super) fn build_loaded_native_aot_trace(
        trace: &JitTrace,
        compiled: native::CompiledTrace,
        interrupt_settings: Option<native::NativeInterruptSettings>,
    ) -> NativeTrace {
        let entry = unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(compiled.entry) };
        let code = Arc::<[u8]>::from(compiled.code.into_boxed_slice());
        let keepalive = Arc::new(Mutex::new(compiled.keepalive));
        NativeTrace {
            _keepalive: keepalive,
            entry,
            code,
            root_ip: trace.root_ip,
            terminal: trace.terminal.clone(),
            has_yielding_call: trace.has_yielding_call,
            interrupt_settings,
            compile_profile: native::NativeCompileProfile::Aot,
        }
    }

    pub fn jit_native_trace_count(&self) -> usize {
        self.native_traces.len()
    }

    pub fn jit_native_exec_count(&self) -> u64 {
        self.native_trace_exec_count
    }
}

#[cfg(all(
    test,
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
))]
pub(crate) fn clear_native_trace_cache_for_tests() {
    with_native_trace_cache(|cache| {
        cache.entries.clear();
        cache.active_program_key = None;
    });
}

#[cfg(all(
    test,
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
))]
pub(crate) fn native_trace_cache_snapshot_for_tests() -> (Option<u64>, usize) {
    with_native_trace_cache(|cache| (cache.active_program_key, cache.entries.len()))
}
