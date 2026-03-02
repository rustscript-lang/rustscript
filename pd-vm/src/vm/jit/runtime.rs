use super::super::{ExecOutcome, HostCallExecOutcome, Vm, VmError, VmResult};
use super::{JitTrace, JitTraceTerminal, TraceStep, native};
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use std::sync::{Mutex, OnceLock};
#[cfg(all(
    feature = "cranelift-jit",
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
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

#[cfg(feature = "cranelift-jit")]
type MaybeCraneliftKeepalive = Option<Arc<native::CraneliftTraceKeepAlive>>;

pub(crate) struct NativeTrace {
    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    _memory: Option<Arc<native::ExecutableMemory>>,
    #[cfg(feature = "cranelift-jit")]
    _cranelift_keepalive: MaybeCraneliftKeepalive,
    entry: NativeTraceEntry,
    code: Arc<[u8]>,
    root_ip: usize,
    terminal: JitTraceTerminal,
    has_yielding_call: bool,
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
    backend: native::NativeCodegenBackend,
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
struct NativeTraceCacheEntry {
    memory: Arc<native::ExecutableMemory>,
    code: Arc<[u8]>,
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

#[cfg(all(
    feature = "cranelift-jit",
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
))]
#[derive(Clone)]
struct CraneliftNativeTraceCacheEntry {
    entry: NativeTraceEntry,
    keepalive: Arc<native::CraneliftTraceKeepAlive>,
    code: Arc<[u8]>,
}

#[cfg(all(
    feature = "cranelift-jit",
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
))]
struct CraneliftNativeTraceCache {
    active_program_key: Option<u64>,
    entries: HashMap<NativeTraceCacheKey, CraneliftNativeTraceCacheEntry>,
}

#[cfg(all(
    feature = "cranelift-jit",
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
))]
thread_local! {
    static CRANELIFT_NATIVE_TRACE_CACHE: RefCell<CraneliftNativeTraceCache> = RefCell::new(
        CraneliftNativeTraceCache {
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
static NATIVE_TRACE_CACHE: OnceLock<Mutex<NativeTraceCache>> = OnceLock::new();

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache() -> &'static Mutex<NativeTraceCache> {
    NATIVE_TRACE_CACHE.get_or_init(|| {
        Mutex::new(NativeTraceCache {
            active_program_key: None,
            entries: HashMap::new(),
        })
    })
}

#[cfg(all(
    feature = "cranelift-jit",
    any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )
))]
fn with_cranelift_native_trace_cache<R>(f: impl FnOnce(&mut CraneliftNativeTraceCache) -> R) -> R {
    CRANELIFT_NATIVE_TRACE_CACHE.with(|cell| {
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
    backend: native::NativeCodegenBackend,
) -> NativeTraceCacheKey {
    NativeTraceCacheKey {
        backend,
        root_ip: trace.root_ip,
        terminal: trace.terminal.clone(),
        steps: trace.steps.clone(),
    }
}

impl Vm {
    pub fn set_jit_config(&mut self, config: super::JitConfig) {
        self.jit.set_config(config);
    }

    pub fn jit_config(&self) -> &super::JitConfig {
        self.jit.config()
    }

    pub fn jit_snapshot(&self) -> super::JitSnapshot {
        self.jit.snapshot()
    }

    pub fn dump_jit_info(&self) -> String {
        let mut out = self.jit.dump_text(self.program.debug.as_ref());
        out.push_str(&format!(
            "  native codegen backend: {:?}\n",
            native::selected_codegen_backend()
        ));
        out.push_str(&format!(
            "  native trace executions: {}\n",
            self.native_trace_exec_count
        ));
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
                out.push_str("    code:");
                for byte in native.code.iter() {
                    out.push_str(&format!(" {:02X}", byte));
                }
                out.push('\n');
            }
        }
        out
    }

    #[cfg_attr(
        any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ),
        allow(dead_code)
    )]
    fn execute_jit_trace(&mut self, trace_id: usize) -> VmResult<ExecOutcome> {
        let Some(trace) = self.jit.trace_clone(trace_id) else {
            return Ok(ExecOutcome::Continue);
        };
        for step in &trace.steps {
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
                    self.binary_numeric_op(
                        |lhs, rhs| {
                            if rhs == 0 {
                                return Err(VmError::DivisionByZero);
                            }
                            Ok(lhs.wrapping_div(rhs))
                        },
                        |lhs, rhs| {
                            if rhs == 0.0 {
                                return Err(VmError::DivisionByZero);
                            }
                            Ok(lhs / rhs)
                        },
                    )?;
                }
                TraceStep::Mod => {
                    self.binary_numeric_op(
                        |lhs, rhs| {
                            if rhs == 0 {
                                return Err(VmError::DivisionByZero);
                            }
                            Ok(lhs.wrapping_rem(rhs))
                        },
                        |lhs, rhs| {
                            if rhs == 0.0 {
                                return Err(VmError::DivisionByZero);
                            }
                            Ok(lhs % rhs)
                        },
                    )?;
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
                    let value = self
                        .locals
                        .get(*index as usize)
                        .cloned()
                        .ok_or(VmError::InvalidLocal(*index))?;
                    self.stack.push(value);
                }
                TraceStep::Stloc(index) => {
                    let value = self.pop_value()?;
                    let slot = self
                        .locals
                        .get_mut(*index as usize)
                        .ok_or(VmError::InvalidLocal(*index))?;
                    *slot = value;
                }
                TraceStep::Call {
                    index,
                    argc,
                    call_ip,
                } => match self.execute_host_call(*index, *argc, *call_ip)? {
                    HostCallExecOutcome::Returned => {}
                    HostCallExecOutcome::Yielded => return Ok(ExecOutcome::Yielded),
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
        self.ensure_native_trace(trace_id)?;
        let (entry, root_ip, terminal, has_yielding_call) = {
            let native = self.native_traces.get(&trace_id).ok_or_else(|| {
                VmError::JitNative(format!("native trace entry for id {} missing", trace_id))
            })?;
            (
                native.entry,
                native.root_ip,
                native.terminal.clone(),
                native.has_yielding_call,
            )
        };

        loop {
            native::clear_bridge_error();
            let status = unsafe { entry(self as *mut Vm) };
            self.native_trace_exec_count = self.native_trace_exec_count.saturating_add(1);
            self.jit.mark_trace_executed(trace_id);

            match status {
                native::STATUS_CONTINUE => return Ok(ExecOutcome::Continue),
                native::STATUS_TRACE_EXIT => {
                    // Fast path: if this trace looped back to its own root and cannot yield via host
                    // calls, keep executing in native mode without bouncing through the interpreter.
                    if !has_yielding_call
                        && terminal == JitTraceTerminal::LoopBack
                        && self.ip == root_ip
                    {
                        continue;
                    }
                    return Ok(ExecOutcome::Continue);
                }
                native::STATUS_HALTED => return Ok(ExecOutcome::Halted),
                native::STATUS_YIELDED => return Ok(ExecOutcome::Yielded),
                native::STATUS_WAITING => {
                    let op_id = self.waiting_host_op.map(|op| op.op_id).ok_or_else(|| {
                        VmError::JitNative(
                            "native call bridge reported waiting without a pending op".to_string(),
                        )
                    })?;
                    return Ok(ExecOutcome::Waiting(op_id));
                }
                native::STATUS_ERROR => {
                    let err = native::take_bridge_error().unwrap_or_else(|| {
                        VmError::JitNative(
                            "jit bridge reported failure without VmError".to_string(),
                        )
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
    fn ensure_native_trace(&mut self, trace_id: usize) -> VmResult<()> {
        if self.native_traces.contains_key(&trace_id) {
            return Ok(());
        }

        let trace = self.jit.trace_clone(trace_id).ok_or_else(|| {
            VmError::JitNative(format!("trace {} missing for native compile", trace_id))
        })?;
        match native::selected_codegen_backend() {
            native::NativeCodegenBackend::Handwritten => {
                let key = native_trace_cache_key(&trace, native::NativeCodegenBackend::Handwritten);
                let cache = native_trace_cache();
                let (memory, code) = {
                    let mut guard = cache.lock().map_err(|_| {
                        VmError::JitNative("native trace cache lock poisoned".to_string())
                    })?;
                    if guard.active_program_key != Some(self.program_cache_key) {
                        guard.entries.clear();
                        guard.active_program_key = Some(self.program_cache_key);
                    }

                    if let Some(hit) = guard.entries.get(&key) {
                        (Arc::clone(&hit.memory), Arc::clone(&hit.code))
                    } else {
                        let code = Arc::<[u8]>::from(
                            native::emit_native_trace_bytes(&trace)?.into_boxed_slice(),
                        );
                        let memory = Arc::new(native::ExecutableMemory::from_code(code.as_ref())?);
                        guard.entries.insert(
                            key,
                            NativeTraceCacheEntry {
                                memory: Arc::clone(&memory),
                                code: Arc::clone(&code),
                            },
                        );
                        (memory, code)
                    }
                };
                let entry =
                    unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(memory.ptr) };
                self.native_traces.insert(
                    trace_id,
                    NativeTrace {
                        _memory: Some(memory),
                        #[cfg(feature = "cranelift-jit")]
                        _cranelift_keepalive: None,
                        entry,
                        code,
                        root_ip: trace.root_ip,
                        terminal: trace.terminal,
                        has_yielding_call: trace.has_yielding_call,
                    },
                );
            }
            native::NativeCodegenBackend::Cranelift => {
                #[cfg(feature = "cranelift-jit")]
                {
                    let key =
                        native_trace_cache_key(&trace, native::NativeCodegenBackend::Cranelift);
                    let cached = with_cranelift_native_trace_cache(|cache| {
                        if cache.active_program_key != Some(self.program_cache_key) {
                            cache.entries.clear();
                            cache.active_program_key = Some(self.program_cache_key);
                        }
                        cache.entries.get(&key).cloned()
                    });
                    if let Some(cached) = cached {
                        self.native_traces.insert(
                            trace_id,
                            NativeTrace {
                                _memory: None,
                                _cranelift_keepalive: Some(cached.keepalive),
                                entry: cached.entry,
                                code: cached.code,
                                root_ip: trace.root_ip,
                                terminal: trace.terminal,
                                has_yielding_call: trace.has_yielding_call,
                            },
                        );
                        return Ok(());
                    }
                }

                let compiled =
                    native::compile_native_trace(&trace, native::NativeCodegenBackend::Cranelift)?;
                match compiled {
                    #[cfg(feature = "cranelift-jit")]
                    native::CompiledNativeTrace::Cranelift(compiled) => {
                        let entry = unsafe {
                            std::mem::transmute::<*const u8, NativeTraceEntry>(compiled.entry)
                        };
                        let code = Arc::<[u8]>::from(compiled.code.into_boxed_slice());
                        let keepalive = Arc::new(compiled.keepalive);
                        #[cfg(feature = "cranelift-jit")]
                        {
                            let cached = CraneliftNativeTraceCacheEntry {
                                entry,
                                keepalive: Arc::clone(&keepalive),
                                code: Arc::clone(&code),
                            };
                            let key = native_trace_cache_key(
                                &trace,
                                native::NativeCodegenBackend::Cranelift,
                            );
                            with_cranelift_native_trace_cache(|cache| {
                                if cache.active_program_key != Some(self.program_cache_key) {
                                    cache.entries.clear();
                                    cache.active_program_key = Some(self.program_cache_key);
                                }
                                cache.entries.insert(key, cached);
                            });
                        }
                        self.native_traces.insert(
                            trace_id,
                            NativeTrace {
                                _memory: None,
                                #[cfg(feature = "cranelift-jit")]
                                _cranelift_keepalive: Some(keepalive),
                                entry,
                                code,
                                root_ip: trace.root_ip,
                                terminal: trace.terminal,
                                has_yielding_call: trace.has_yielding_call,
                            },
                        );
                    }
                    native::CompiledNativeTrace::Handwritten { code } => {
                        let code = Arc::<[u8]>::from(code.into_boxed_slice());
                        let memory = Arc::new(native::ExecutableMemory::from_code(code.as_ref())?);
                        let entry = unsafe {
                            std::mem::transmute::<*const u8, NativeTraceEntry>(memory.ptr)
                        };
                        self.native_traces.insert(
                            trace_id,
                            NativeTrace {
                                _memory: Some(memory),
                                #[cfg(feature = "cranelift-jit")]
                                _cranelift_keepalive: None,
                                entry,
                                code,
                                root_ip: trace.root_ip,
                                terminal: trace.terminal,
                                has_yielding_call: trace.has_yielding_call,
                            },
                        );
                    }
                }
            }
        }
        Ok(())
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
    if let Ok(mut guard) = native_trace_cache().lock() {
        guard.entries.clear();
        guard.active_program_key = None;
    }
    #[cfg(feature = "cranelift-jit")]
    with_cranelift_native_trace_cache(|cache| {
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
    let guard = native_trace_cache()
        .lock()
        .expect("native trace cache lock should succeed");
    (guard.active_program_key, guard.entries.len())
}
