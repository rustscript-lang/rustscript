#![allow(dead_code)]
use super::super::{ExecOutcome, Vm, VmError, VmResult};
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use super::JitTrace;
use super::ir::SsaExitId;
use super::trace::TraceExitKey;
use super::{JitMetrics, JitTraceTerminal, native};
use crate::vm::native::ROOT_FRAME_KEY;
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

type NativeTraceState = (
    NativeTraceEntry,
    usize,
    JitTraceTerminal,
    bool,
    bool,
    Option<Arc<HashMap<u32, TraceExitKey>>>,
    bool,
);

fn scalar_cycle_import(import: &super::deopt::SideTraceImport) -> bool {
    import.args.iter().all(|arg| {
        matches!(
            arg,
            super::ir::SsaMaterialization::BoxInt(_)
                | super::ir::SsaMaterialization::BoxFloat(_)
                | super::ir::SsaMaterialization::BoxBool(_)
        )
    })
}

fn elapsed_ns(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) struct NativeTrace {
    _keepalive: Arc<Mutex<native::TraceKeepAlive>>,
    _direct_keepalives: Vec<Arc<Mutex<native::TraceKeepAlive>>>,
    entry: NativeTraceEntry,
    tail_entry: NativeTraceEntry,
    direct_slots: Arc<HashMap<u32, Arc<native::NativeSideLinkSlot>>>,
    pub(super) code: Arc<[u8]>,
    root_ip: usize,
    terminal: JitTraceTerminal,
    has_call: bool,
    has_yielding_call: bool,
    lowering_kind: native::TraceLoweringKind,
    interrupt_settings: Option<native::NativeInterruptSettings>,
    compile_profile: native::NativeCompileProfile,
    drop_contract_events_enabled: bool,
    region: Option<NativeRegion>,
}

struct NativeRegion {
    _keepalive: Arc<Mutex<native::TraceKeepAlive>>,
    entry: NativeTraceEntry,
    code: Arc<[u8]>,
    terminal: JitTraceTerminal,
    has_call: bool,
    has_yielding_call: bool,
    lowering_kind: native::TraceLoweringKind,
    generation: u64,
    key: TraceExitKey,
    child_trace_id: usize,
    exit_keys: Arc<HashMap<u32, TraceExitKey>>,
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
    drop_contract_events_enabled: bool,
    frame_key: u64,
    root_ip: usize,
    entry_stack_depth: usize,
    terminal: JitTraceTerminal,
    ssa_text: String,
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
    tail_entry: NativeTraceEntry,
    keepalive: Arc<Mutex<native::TraceKeepAlive>>,
    code: Arc<[u8]>,
    lowering_kind: native::TraceLoweringKind,
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
    drop_contract_events_enabled: bool,
) -> NativeTraceCacheKey {
    NativeTraceCacheKey {
        interrupt_settings,
        compile_profile,
        drop_contract_events_enabled,
        frame_key: trace.frame_key,
        root_ip: trace.root_ip,
        entry_stack_depth: trace.entry_stack_depth,
        terminal: trace.terminal,
        ssa_text: trace.ssa_text(),
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
}

fn should_fallback_to_interpreter(err: &VmError) -> bool {
    matches!(err, VmError::JitNative(detail)
        if detail.contains("SSA native lowering does not support"))
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
pub(crate) fn resume_linked_trace_entry_address() -> usize {
    pd_vm_native_resume_linked_trace as *const () as usize
}

#[cfg(not(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
)))]
pub(crate) fn resume_linked_trace_entry_address() -> usize {
    0
}

#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
pub(crate) extern "C" fn pd_vm_native_resume_linked_trace(vm: *mut Vm) -> i32 {
    let Some(vm_ref) = (unsafe { vm.as_mut() }) else {
        native::store_bridge_error(VmError::JitNative(
            "native linked-trace helper received null vm pointer".to_string(),
        ));
        return native::STATUS_ERROR;
    };

    if vm_ref.jit_native_link_dispatch_depth > 0 {
        return native::STATUS_TRACE_EXIT;
    }

    vm_ref.jit_native_link_dispatch_depth = vm_ref.jit_native_link_dispatch_depth.saturating_add(1);
    match vm_ref.continue_linked_native_trace_from_exit() {
        Ok(status) => {
            vm_ref.jit_native_link_dispatch_depth =
                vm_ref.jit_native_link_dispatch_depth.saturating_sub(1);
            status
        }
        Err(err) => {
            vm_ref.jit_native_link_dispatch_depth =
                vm_ref.jit_native_link_dispatch_depth.saturating_sub(1);
            native::store_bridge_error(err);
            native::STATUS_ERROR
        }
    }
}

impl Vm {
    fn compiled_trace_for_active_entry(&self) -> Option<usize> {
        if self.active_frame_has_shared_capture_cells() {
            return None;
        }
        let entry_callable_prototypes = self.active_local_callable_prototypes();
        self.jit.compiled_trace_for_entry_with_callables(
            self.active_frame_key(),
            self.ip,
            self.active_operand_stack_len(),
            entry_callable_prototypes.as_deref(),
        )
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn continue_linked_native_trace_from_exit(&mut self) -> VmResult<i32> {
        self.jit_trace_exit_count = self.jit_trace_exit_count.saturating_add(1);
        let mut current_trace_id = {
            let ip = self.ip;
            let frame_key = self.active_frame_key();
            let stack_depth = self.active_operand_stack_len();
            let mut next_trace_id = self.compiled_trace_for_active_entry();
            if next_trace_id.is_none()
                && !self.active_frame_has_shared_capture_cells()
                && !self.jit.callable_frame_is_blocked(frame_key)
            {
                let entry_local_types =
                    (frame_key != ROOT_FRAME_KEY).then(|| self.active_local_types());
                let entry_callable_prototypes = self.active_local_callable_prototypes();
                let program = &self.program;
                next_trace_id = self.jit.observe_exit_entry_with_local_types(
                    frame_key,
                    ip,
                    stack_depth,
                    entry_local_types.as_deref(),
                    entry_callable_prototypes.as_deref(),
                    program,
                );
            }
            let Some(next_trace_id) = next_trace_id else {
                return Ok(native::STATUS_LINKED_CONTINUE);
            };
            next_trace_id
        };

        self.record_jit_link_handoff();
        if let Err(err) =
            self.ensure_native_trace(current_trace_id, native::NativeCompileProfile::Jit)
        {
            if should_fallback_to_interpreter(&err) {
                self.record_jit_helper_fallback();
                self.block_jit_trace(current_trace_id);
                return Ok(native::STATUS_LINKED_CONTINUE);
            }
            return Err(err);
        }

        let (
            mut entry,
            mut root_ip,
            mut terminal,
            _,
            mut has_yielding_call,
            mut exit_keys,
            mut is_region,
        ) = self.native_trace_state(current_trace_id)?;

        loop {
            native::clear_bridge_error();
            let region_edges_before = self.jit_native_region_edge_count;
            let direct_links_before = self.jit_native_direct_link_count;
            let status = unsafe { entry(self as *mut Vm) };
            self.native_trace_exec_count = self.native_trace_exec_count.saturating_add(1);
            if !is_region
                && self.jit_native_active_direct_trace_id != usize::MAX
                && self.jit_native_active_direct_trace_id != current_trace_id
            {
                current_trace_id = self.jit_native_active_direct_trace_id;
                let state = self.native_trace_state(current_trace_id)?;
                entry = state.0;
                root_ip = state.1;
                terminal = state.2;
                has_yielding_call = state.4;
                exit_keys = state.5;
                is_region = state.6;
            }
            self.record_native_direct_escape(status, direct_links_before);
            if is_region {
                self.jit_native_region_entry_count =
                    self.jit_native_region_entry_count.saturating_add(1);
                if self.jit_native_region_edge_count > region_edges_before {
                    self.jit.record_native_region_progress(current_trace_id);
                }
            }
            self.jit.mark_trace_executed(current_trace_id);
            let mut trace_exit_key = None;
            let mut instruction_failure_exit = false;
            let status = if let Some(exit_id) = native::decode_jit_trace_exit_status(status) {
                let key = if let Some(region_exit_keys) = &exit_keys {
                    *region_exit_keys.get(&exit_id).ok_or_else(|| {
                        VmError::JitNative(format!(
                            "fused native region returned impossible exit id {exit_id}"
                        ))
                    })?
                } else {
                    TraceExitKey {
                        parent_trace_id: current_trace_id,
                        exit_id: SsaExitId::new(exit_id),
                    }
                };
                instruction_failure_exit = self.jit.trace_exit_is_instruction_failure(key);
                self.jit
                    .record_trace_exit(key)
                    .map_err(|err| VmError::JitNative(err.message()))?;
                trace_exit_key = Some(key);
                native::STATUS_TRACE_EXIT
            } else {
                status
            };

            match status {
                native::STATUS_CONTINUE => {
                    let next_trace_id = if has_yielding_call {
                        None
                    } else {
                        self.compiled_trace_for_active_entry()
                    };
                    if let Some(next_trace_id) = next_trace_id
                        && next_trace_id != current_trace_id
                    {
                        self.publish_native_direct_slot(
                            current_trace_id,
                            native::CONTINUE_SLOT_ID,
                            next_trace_id,
                        )?;
                        self.record_jit_link_handoff();
                        current_trace_id = next_trace_id;
                        if let Err(err) = self.ensure_native_trace(
                            current_trace_id,
                            native::NativeCompileProfile::Jit,
                        ) {
                            if should_fallback_to_interpreter(&err) {
                                self.record_jit_helper_fallback();
                                self.block_jit_trace(current_trace_id);
                                return Ok(native::STATUS_LINKED_CONTINUE);
                            }
                            return Err(err);
                        }
                        (
                            entry,
                            root_ip,
                            terminal,
                            _,
                            has_yielding_call,
                            exit_keys,
                            is_region,
                        ) = self.native_trace_state(current_trace_id)?;
                        continue;
                    }
                    return Ok(native::STATUS_LINKED_CONTINUE);
                }
                native::STATUS_TRACE_EXIT => {
                    self.jit_trace_exit_count = self.jit_trace_exit_count.saturating_add(1);
                    if instruction_failure_exit {
                        return Ok(native::STATUS_LINKED_CONTINUE);
                    }
                    if !has_yielding_call
                        && terminal == JitTraceTerminal::LoopBack
                        && self.ip == root_ip
                    {
                        self.jit.record_native_loop_back(current_trace_id);
                        self.jit_native_loop_back_count =
                            self.jit_native_loop_back_count.saturating_add(1);
                        continue;
                    }
                    if self.jit.record_native_side_exit(current_trace_id)
                        && !self.jit_native_direct_links_enabled
                    {
                        self.block_jit_callable_frame(current_trace_id);
                        return Ok(native::STATUS_LINKED_CONTINUE);
                    }
                    if !has_yielding_call && !self.active_frame_has_shared_capture_cells() {
                        let ip = self.ip;
                        let frame_key = self.active_frame_key();
                        let stack_depth = self.active_operand_stack_len();
                        let mut next_trace_id = self.compiled_trace_for_active_entry();
                        if next_trace_id.is_none() && !self.jit.callable_frame_is_blocked(frame_key)
                        {
                            let entry_local_types =
                                (frame_key != ROOT_FRAME_KEY).then(|| self.active_local_types());
                            let entry_callable_prototypes = self.active_local_callable_prototypes();
                            let program = &self.program;
                            next_trace_id = self.jit.observe_exit_entry_with_local_types(
                                frame_key,
                                ip,
                                stack_depth,
                                entry_local_types.as_deref(),
                                entry_callable_prototypes.as_deref(),
                                program,
                            );
                        }
                        if let Some(next_trace_id) = next_trace_id
                            && next_trace_id != current_trace_id
                        {
                            if let Some(key) = trace_exit_key {
                                self.publish_native_direct_link(key, next_trace_id)?;
                                self.maybe_publish_native_region(key, next_trace_id);
                            }
                            self.record_jit_link_handoff();
                            current_trace_id = next_trace_id;
                            if let Err(err) = self.ensure_native_trace(
                                current_trace_id,
                                native::NativeCompileProfile::Jit,
                            ) {
                                if should_fallback_to_interpreter(&err) {
                                    self.record_jit_helper_fallback();
                                    self.block_jit_trace(current_trace_id);
                                    return Ok(native::STATUS_LINKED_CONTINUE);
                                }
                                return Err(err);
                            }
                            (
                                entry,
                                root_ip,
                                terminal,
                                _,
                                has_yielding_call,
                                exit_keys,
                                is_region,
                            ) = self.native_trace_state(current_trace_id)?;
                            continue;
                        }
                    }
                    return Ok(native::STATUS_LINKED_CONTINUE);
                }
                native::STATUS_HALTED
                | native::STATUS_YIELDED
                | native::STATUS_WAITING
                | native::STATUS_OUT_OF_FUEL
                | native::STATUS_ERROR => return Ok(status),
                other => {
                    return Err(VmError::JitNative(format!(
                        "unexpected linked native trace return status {}",
                        other
                    )));
                }
            }
        }
    }

    fn active_native_interrupt_settings(&self) -> Option<native::NativeInterruptSettings> {
        match self.interrupt_mode {
            super::super::InterruptMode::None => None,
            super::super::InterruptMode::Fuel => Some(native::NativeInterruptSettings::fuel(
                self.fuel_check_interval,
            )),
            super::super::InterruptMode::Epoch => Some(native::NativeInterruptSettings::epoch(
                self.fuel_check_interval,
            )),
        }
    }

    fn clear_native_direct_links(&self) {
        for native in self.native_traces.iter().flatten() {
            for slot in native.direct_slots.values() {
                slot.clear();
            }
        }
    }

    fn record_native_direct_escape(&mut self, _status: i32, direct_links_before: u64) {
        if !self.jit_native_direct_links_enabled
            || self.jit_native_direct_link_count == direct_links_before
        {
            return;
        }
        self.jit_native_direct_escape_streak = 0;
        if self.jit_native_active_direct_trace_id != usize::MAX {
            self.jit
                .record_native_loop_back(self.jit_native_active_direct_trace_id);
        }
    }

    fn publish_native_direct_link(
        &mut self,
        key: TraceExitKey,
        child_trace_id: usize,
    ) -> VmResult<()> {
        if !self.jit_native_direct_links_enabled || self.jit_native_direct_region_fallback {
            return Ok(());
        }
        self.publish_native_direct_slot(key.parent_trace_id, key.exit_id.raw(), child_trace_id)
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn publish_native_direct_slot(
        &mut self,
        parent_trace_id: usize,
        slot_id: u32,
        child_trace_id: usize,
    ) -> VmResult<()> {
        if self.jit.trace_has_entry_callable_guards(child_trace_id) {
            return Ok(());
        }
        if !self.jit_native_direct_cross_frame_enabled {
            let parent_frame_key = self
                .jit
                .trace_clone(parent_trace_id)
                .map(|trace| trace.frame_key);
            let child_frame_key = self
                .jit
                .trace_clone(child_trace_id)
                .map(|trace| trace.frame_key);
            if parent_frame_key != child_frame_key {
                return Ok(());
            }
        }
        self.ensure_native_trace(child_trace_id, native::NativeCompileProfile::Jit)?;
        let child_entry = self
            .native_traces
            .get(child_trace_id)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                VmError::JitNative("direct-link child native trace missing".to_string())
            })?
            .tail_entry as *const u8;
        let Some(slot) = self
            .native_traces
            .get(parent_trace_id)
            .and_then(Option::as_ref)
            .and_then(|native| native.direct_slots.get(&slot_id))
            .cloned()
        else {
            return Ok(());
        };
        slot.publish(child_entry);
        Ok(())
    }

    #[cfg(not(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    )))]
    fn publish_native_direct_slot(
        &mut self,
        _parent_trace_id: usize,
        _slot_id: u32,
        _child_trace_id: usize,
    ) -> VmResult<()> {
        Ok(())
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    fn maybe_publish_native_region(&mut self, key: TraceExitKey, child_trace_id: usize) {
        if self.jit_native_direct_links_enabled && !self.jit_native_direct_region_fallback {
            return;
        }
        if self
            .jit
            .trace_has_entry_callable_guards(key.parent_trace_id)
            || self.jit.trace_has_entry_callable_guards(child_trace_id)
        {
            return;
        }
        let Some(candidate) = self.jit.region_candidate(key, child_trace_id) else {
            return;
        };
        if candidate.generation != self.jit.region_generation() {
            return;
        }
        let Some(parent) = self.jit.trace_clone(key.parent_trace_id) else {
            self.jit.record_region_compile_failure(&candidate);
            return;
        };
        let Some(child) = self.jit.trace_clone(child_trace_id) else {
            self.jit.record_region_compile_failure(&candidate);
            return;
        };
        let back_import = scalar_cycle_import(&candidate.import)
            .then(|| {
                child
                    .ssa
                    .exits
                    .iter()
                    .filter(|exit| exit.exit_ip == parent.root_ip)
                    .find_map(|exit| {
                        self.jit
                            .side_trace_import(child.id, exit.id, parent.id)
                            .ok()
                    })
            })
            .flatten()
            .filter(scalar_cycle_import);
        let fused = match super::region::fuse_two_trace_region(
            &parent,
            &child,
            &candidate.import,
            back_import.as_ref(),
        ) {
            Ok(fused) => fused,
            Err(_) => {
                self.jit.record_region_compile_failure(&candidate);
                return;
            }
        };
        let interrupt_settings = self.active_native_interrupt_settings();
        let compile_profile = native::NativeCompileProfile::Jit;
        let drop_contract_events_enabled = self.drop_contract_events_enabled();
        let compile_started = std::time::Instant::now();
        let compile_result = native::compile_native_region(
            &fused,
            interrupt_settings,
            compile_profile,
            drop_contract_events_enabled,
        );
        self.jit_native_region_compile_time_ns = self
            .jit_native_region_compile_time_ns
            .saturating_add(elapsed_ns(compile_started));
        let compiled = match compile_result {
            Ok(compiled) => compiled,
            Err(_) => {
                self.jit.record_region_compile_failure(&candidate);
                return;
            }
        };
        let entry = unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(compiled.entry) };
        let lowering_kind = compiled.lowering_kind;
        let code = Arc::<[u8]>::from(compiled.code.into_boxed_slice());
        let keepalive = Arc::new(Mutex::new(compiled.keepalive));
        let region = NativeRegion {
            _keepalive: keepalive,
            entry,
            code,
            terminal: fused.trace.terminal,
            has_call: fused.trace.has_call,
            has_yielding_call: fused.trace.has_yielding_call,
            lowering_kind,
            generation: candidate.generation,
            key,
            child_trace_id,
            exit_keys: Arc::new(fused.exit_keys),
        };
        let Some(parent_native) = self
            .native_traces
            .get_mut(key.parent_trace_id)
            .and_then(Option::as_mut)
        else {
            self.jit.record_region_compile_failure(&candidate);
            return;
        };
        if !self.jit.publish_region(&candidate) {
            return;
        }
        parent_native.region = Some(region);
    }

    fn clear_native_region_owners(&mut self) {
        for native in self.native_traces.iter_mut().flatten() {
            native.region = None;
        }
    }

    pub(crate) fn disconnect_native_regions(&mut self) {
        self.jit.invalidate_regions();
        self.clear_native_region_owners();
    }

    fn block_jit_trace(&mut self, trace_id: usize) {
        self.jit.block_trace(trace_id);
        self.clear_native_region_owners();
    }

    fn block_jit_callable_frame(&mut self, trace_id: usize) {
        self.jit.block_callable_frame(trace_id);
        self.clear_native_region_owners();
    }

    pub fn set_jit_config(&mut self, config: super::JitConfig) {
        if config.enabled {
            self.ensure_program_cache_key();
        }
        self.clear_native_direct_links();
        self.native_traces.clear();
        self.native_trace_exec_count = 0;
        self.jit_native_region_entry_count = 0;
        self.jit_native_region_edge_count = 0;
        self.jit_native_direct_link_count = 0;
        self.jit_native_active_direct_trace_id = usize::MAX;
        self.jit_native_direct_escape_streak = 0;
        self.jit_native_direct_region_fallback = false;
        self.jit_native_compile_time_ns = 0;
        self.jit_native_region_compile_time_ns = 0;
        self.jit_trace_exit_count = 0;
        self.jit_native_loop_back_count = 0;
        self.jit_native_link_handoff_count = 0;
        self.jit_native_link_dispatch_depth = 0;
        self.jit_helper_fallback_count = 0;
        self.jit.set_config(config);
    }

    pub fn jit_config(&self) -> &super::JitConfig {
        self.jit.config()
    }

    pub fn jit_snapshot(&self) -> super::JitSnapshot {
        self.jit.snapshot(self.jit_runtime_metrics())
    }

    pub fn jit_exit_profiles(&self) -> Vec<super::JitExitProfile> {
        self.jit.exit_profiles()
    }

    pub fn jit_call_site_profiles(&self) -> Vec<super::JitCallSiteProfile> {
        self.jit.call_site_profiles()
    }

    pub fn jit_native_code_bytes(&self) -> usize {
        self.native_traces
            .iter()
            .flatten()
            .map(|native| native.code.len())
            .sum()
    }

    pub fn jit_native_region_code_bytes(&self) -> usize {
        self.native_traces
            .iter()
            .flatten()
            .filter_map(|native| native.region.as_ref())
            .map(|region| region.code.len())
            .sum()
    }

    pub fn jit_native_compile_time_ns(&self) -> u64 {
        self.jit_native_compile_time_ns
    }

    pub fn jit_native_region_compile_time_ns(&self) -> u64 {
        self.jit_native_region_compile_time_ns
    }

    pub fn dump_jit_info(&self) -> String {
        self.dump_jit_info_with_machine_code(true)
    }

    pub fn dump_jit_info_with_machine_code(&self, include_machine_code: bool) -> String {
        let mut out = self
            .jit
            .dump_text(self.program.debug.as_ref(), self.jit_runtime_metrics());
        out.push_str(&format!(
            "  native codegen backend: {}\n",
            native::selected_codegen_backend()
        ));
        out.push_str(&format!(
            "  native trace executions: {}\n",
            self.native_trace_exec_count
        ));
        out.push_str(&format!(
            "  native trace handoffs: {}\n",
            self.jit_native_link_handoff_count
        ));
        out.push_str(&format!(
            "  native region entries: {}\n",
            self.jit_native_region_entry_count
        ));
        out.push_str(&format!(
            "  native internal region edges: {}\n",
            self.jit_native_region_edge_count
        ));
        out.push_str(&format!(
            "  native direct side links: {}\n",
            self.jit_native_direct_link_count
        ));
        out.push_str(&format!(
            "  native compile time: {} ns (regions={} ns)\n",
            self.jit_native_compile_time_ns, self.jit_native_region_compile_time_ns
        ));
        out.push_str(&format!(
            "  native code bytes: {} (regions={})\n",
            self.jit_native_code_bytes(),
            self.jit_native_region_code_bytes()
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
        let native_trace_count = self.native_traces.iter().flatten().count();
        if native_trace_count == 0 {
            out.push_str("  native traces: 0\n");
            return out;
        }

        out.push_str(&format!("  native traces: {}\n", native_trace_count));
        for (id, native) in self.native_traces.iter().enumerate() {
            if let Some(native) = native {
                out.push_str(&format!(
                    "  native trace#{} entry=0x{:X} code_bytes={} lowering={}\n",
                    id,
                    native.entry as usize,
                    native.code.len(),
                    native.lowering_kind.as_str()
                ));
                if include_machine_code {
                    out.push_str("    code:");
                    for byte in native.code.iter() {
                        out.push_str(&format!(" {:02X}", byte));
                    }
                    out.push('\n');
                }
                if let Some(region) = &native.region {
                    out.push_str(&format!(
                        "    region entry=0x{:X} code_bytes={} lowering={}\n",
                        region.entry as usize,
                        region.code.len(),
                        region.lowering_kind.as_str()
                    ));
                    if include_machine_code {
                        out.push_str("      code:");
                        for byte in region.code.iter() {
                            out.push_str(&format!(" {:02X}", byte));
                        }
                        out.push('\n');
                    }
                }
            }
        }
        out
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
            match self.execute_jit_native(trace_id) {
                Ok(outcome) => Ok(outcome),
                Err(err) if should_fallback_to_interpreter(&err) => {
                    self.record_jit_helper_fallback();
                    self.block_jit_trace(trace_id);
                    Ok(ExecOutcome::Continue)
                }
                Err(err) => Err(err),
            }
        }
        #[cfg(not(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        )))]
        {
            let _ = trace_id;
            Ok(ExecOutcome::Continue)
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
        if let Err(err) =
            self.ensure_native_trace(current_trace_id, native::NativeCompileProfile::Jit)
        {
            if should_fallback_to_interpreter(&err) {
                self.record_jit_helper_fallback();
                self.block_jit_trace(current_trace_id);
                return Ok(ExecOutcome::Continue);
            }
            return Err(err);
        }
        let (
            mut entry,
            mut root_ip,
            mut terminal,
            _,
            mut has_yielding_call,
            mut exit_keys,
            mut is_region,
        ) = self.native_trace_state(current_trace_id)?;
        native::clear_bridge_error();
        loop {
            let region_edges_before = self.jit_native_region_edge_count;
            let direct_links_before = self.jit_native_direct_link_count;
            let status = unsafe { entry(self as *mut Vm) };
            self.native_trace_exec_count = self.native_trace_exec_count.saturating_add(1);
            if !is_region
                && self.jit_native_active_direct_trace_id != usize::MAX
                && self.jit_native_active_direct_trace_id != current_trace_id
            {
                current_trace_id = self.jit_native_active_direct_trace_id;
                let state = self.native_trace_state(current_trace_id)?;
                entry = state.0;
                root_ip = state.1;
                terminal = state.2;
                has_yielding_call = state.4;
                exit_keys = state.5;
                is_region = state.6;
            }
            self.record_native_direct_escape(status, direct_links_before);
            if is_region {
                self.jit_native_region_entry_count =
                    self.jit_native_region_entry_count.saturating_add(1);
                if self.jit_native_region_edge_count > region_edges_before {
                    self.jit.record_native_region_progress(current_trace_id);
                }
            }
            self.jit.mark_trace_executed(current_trace_id);
            let mut trace_exit_key = None;
            let mut instruction_failure_exit = false;
            let status = if let Some(exit_id) = native::decode_jit_trace_exit_status(status) {
                let key = if let Some(region_exit_keys) = &exit_keys {
                    *region_exit_keys.get(&exit_id).ok_or_else(|| {
                        VmError::JitNative(format!(
                            "fused native region returned impossible exit id {exit_id}"
                        ))
                    })?
                } else {
                    TraceExitKey {
                        parent_trace_id: current_trace_id,
                        exit_id: SsaExitId::new(exit_id),
                    }
                };
                instruction_failure_exit = self.jit.trace_exit_is_instruction_failure(key);
                self.jit
                    .record_trace_exit(key)
                    .map_err(|err| VmError::JitNative(err.message()))?;
                trace_exit_key = Some(key);
                native::STATUS_TRACE_EXIT
            } else {
                status
            };

            match status {
                native::STATUS_CONTINUE => {
                    let next_trace_id = if has_yielding_call {
                        None
                    } else {
                        self.compiled_trace_for_active_entry()
                    };
                    if let Some(next_trace_id) = next_trace_id
                        && next_trace_id != current_trace_id
                    {
                        self.publish_native_direct_slot(
                            current_trace_id,
                            native::CONTINUE_SLOT_ID,
                            next_trace_id,
                        )?;
                        self.record_jit_link_handoff();
                        current_trace_id = next_trace_id;
                        if let Some(state) = self.cached_native_trace_state(
                            current_trace_id,
                            native::NativeCompileProfile::Jit,
                        ) {
                            (
                                entry,
                                root_ip,
                                terminal,
                                _,
                                has_yielding_call,
                                exit_keys,
                                is_region,
                            ) = state;
                        } else {
                            if let Err(err) = self.ensure_native_trace(
                                current_trace_id,
                                native::NativeCompileProfile::Jit,
                            ) {
                                if should_fallback_to_interpreter(&err) {
                                    self.record_jit_helper_fallback();
                                    self.block_jit_trace(current_trace_id);
                                    return Ok(ExecOutcome::Continue);
                                }
                                return Err(err);
                            }
                            (
                                entry,
                                root_ip,
                                terminal,
                                _,
                                has_yielding_call,
                                exit_keys,
                                is_region,
                            ) = self.native_trace_state(current_trace_id)?;
                        }
                        continue;
                    }
                    return Ok(ExecOutcome::Continue);
                }
                native::STATUS_TRACE_EXIT => {
                    self.jit_trace_exit_count = self.jit_trace_exit_count.saturating_add(1);
                    if instruction_failure_exit {
                        return Ok(ExecOutcome::Continue);
                    }
                    if self.jit.trace_clone(current_trace_id).is_some_and(|trace| {
                        trace.op_names.last().map(String::as_str) == Some("callable_boundary")
                    }) {
                        self.block_jit_trace(current_trace_id);
                        return Ok(ExecOutcome::Continue);
                    }
                    // Fast path: if this trace looped back to its own root and cannot yield via host
                    // calls, keep executing in native mode without bouncing through the interpreter.
                    if !has_yielding_call
                        && terminal == JitTraceTerminal::LoopBack
                        && self.ip == root_ip
                    {
                        self.jit.record_native_loop_back(current_trace_id);
                        self.jit_native_loop_back_count =
                            self.jit_native_loop_back_count.saturating_add(1);
                        continue;
                    }
                    if self.jit.record_native_side_exit(current_trace_id)
                        && !self.jit_native_direct_links_enabled
                    {
                        self.block_jit_callable_frame(current_trace_id);
                        return Ok(ExecOutcome::Continue);
                    }
                    if !has_yielding_call && !self.active_frame_has_shared_capture_cells() {
                        let ip = self.ip;
                        let frame_key = self.active_frame_key();
                        let stack_depth = self.active_operand_stack_len();
                        let mut next_trace_id = self.compiled_trace_for_active_entry();
                        if next_trace_id.is_none() && !self.jit.callable_frame_is_blocked(frame_key)
                        {
                            next_trace_id = {
                                let entry_local_types = (frame_key != ROOT_FRAME_KEY)
                                    .then(|| self.active_local_types());
                                let entry_callable_prototypes =
                                    self.active_local_callable_prototypes();
                                let program = &self.program;
                                self.jit.observe_exit_entry_with_local_types(
                                    frame_key,
                                    ip,
                                    stack_depth,
                                    entry_local_types.as_deref(),
                                    entry_callable_prototypes.as_deref(),
                                    program,
                                )
                            };
                        }
                        if let Some(next_trace_id) = next_trace_id
                            && next_trace_id != current_trace_id
                        {
                            if let Some(key) = trace_exit_key {
                                self.publish_native_direct_link(key, next_trace_id)?;
                                self.maybe_publish_native_region(key, next_trace_id);
                            }
                            self.record_jit_link_handoff();
                            current_trace_id = next_trace_id;
                            if let Some(state) = self.cached_native_trace_state(
                                current_trace_id,
                                native::NativeCompileProfile::Jit,
                            ) {
                                (
                                    entry,
                                    root_ip,
                                    terminal,
                                    _,
                                    has_yielding_call,
                                    exit_keys,
                                    is_region,
                                ) = state;
                            } else {
                                if let Err(err) = self.ensure_native_trace(
                                    current_trace_id,
                                    native::NativeCompileProfile::Jit,
                                ) {
                                    if should_fallback_to_interpreter(&err) {
                                        self.record_jit_helper_fallback();
                                        self.block_jit_trace(current_trace_id);
                                        return Ok(ExecOutcome::Continue);
                                    }
                                    return Err(err);
                                }
                                (
                                    entry,
                                    root_ip,
                                    terminal,
                                    _,
                                    has_yielding_call,
                                    exit_keys,
                                    is_region,
                                ) = self.native_trace_state(current_trace_id)?;
                            }
                            continue;
                        }
                    }
                    return Ok(ExecOutcome::Continue);
                }
                native::STATUS_HALTED => return Ok(ExecOutcome::Halted),
                native::STATUS_LINKED_CONTINUE => {
                    if self.active_frame_has_shared_capture_cells() {
                        return Ok(ExecOutcome::Continue);
                    }
                    let ip = self.ip;
                    let frame_key = self.active_frame_key();
                    let stack_depth = self.active_operand_stack_len();
                    let mut next_trace_id = self.compiled_trace_for_active_entry();
                    if next_trace_id.is_none() && !self.jit.callable_frame_is_blocked(frame_key) {
                        next_trace_id = {
                            let entry_local_types =
                                (frame_key != ROOT_FRAME_KEY).then(|| self.active_local_types());
                            let entry_callable_prototypes = self.active_local_callable_prototypes();
                            let program = &self.program;
                            self.jit.observe_exit_entry_with_local_types(
                                frame_key,
                                ip,
                                stack_depth,
                                entry_local_types.as_deref(),
                                entry_callable_prototypes.as_deref(),
                                program,
                            )
                        };
                    }
                    if let Some(next_trace_id) = next_trace_id
                        && next_trace_id != current_trace_id
                    {
                        self.record_jit_link_handoff();
                        current_trace_id = next_trace_id;
                        if let Some(state) = self.cached_native_trace_state(
                            current_trace_id,
                            native::NativeCompileProfile::Jit,
                        ) {
                            (
                                entry,
                                root_ip,
                                terminal,
                                _,
                                has_yielding_call,
                                exit_keys,
                                is_region,
                            ) = state;
                        } else {
                            if let Err(err) = self.ensure_native_trace(
                                current_trace_id,
                                native::NativeCompileProfile::Jit,
                            ) {
                                if should_fallback_to_interpreter(&err) {
                                    self.record_jit_helper_fallback();
                                    self.block_jit_trace(current_trace_id);
                                    return Ok(ExecOutcome::Continue);
                                }
                                return Err(err);
                            }
                            (
                                entry,
                                root_ip,
                                terminal,
                                _,
                                has_yielding_call,
                                exit_keys,
                                is_region,
                            ) = self.native_trace_state(current_trace_id)?;
                        }
                        continue;
                    }
                    return Ok(ExecOutcome::Continue);
                }
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
                        super::super::InterruptMode::Epoch => Err(VmError::EpochDeadlineReached {
                            current: self.current_epoch(),
                            deadline: self.epoch_deadline,
                        }),
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
                                "trace_id={} root_ip={} terminal={:?} ops={}",
                                trace.id,
                                trace.root_ip,
                                trace.terminal,
                                trace.op_names.len()
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
    fn native_trace_state(&self, trace_id: usize) -> VmResult<NativeTraceState> {
        let native = self
            .native_traces
            .get(trace_id)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                VmError::JitNative(format!("native trace entry for id {} missing", trace_id))
            })?;
        if let Some(region) = native.region.as_ref().filter(|region| {
            self.jit.published_region().is_some_and(|published| {
                published.generation == region.generation
                    && published.key == region.key
                    && published.child_trace_id == region.child_trace_id
                    && published.key.parent_trace_id == trace_id
            })
        }) {
            return Ok((
                region.entry,
                native.root_ip,
                region.terminal,
                region.has_call,
                region.has_yielding_call,
                Some(Arc::clone(&region.exit_keys)),
                true,
            ));
        }
        Ok((
            native.entry,
            native.root_ip,
            native.terminal,
            native.has_call,
            native.has_yielding_call,
            None,
            false,
        ))
    }

    #[cfg(any(
        all(
            target_arch = "x86_64",
            any(target_os = "windows", all(unix, not(target_os = "macos")))
        ),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    #[inline(always)]
    fn cached_native_trace_state(
        &self,
        trace_id: usize,
        compile_profile: native::NativeCompileProfile,
    ) -> Option<NativeTraceState> {
        let native = self.native_traces.get(trace_id)?.as_ref()?;
        (native.interrupt_settings == self.active_native_interrupt_settings()
            && compile_profile_satisfies(native.compile_profile, compile_profile)
            && native.drop_contract_events_enabled == self.drop_contract_events_enabled)
            .then(|| self.native_trace_state(trace_id).ok())
            .flatten()
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
        if let Some(native) = self.native_traces.get(trace_id).and_then(Option::as_ref)
            && native.interrupt_settings == interrupt_settings
            && compile_profile_satisfies(native.compile_profile, compile_profile)
            && native.drop_contract_events_enabled == self.drop_contract_events_enabled()
        {
            return Ok(());
        }
        if self
            .native_traces
            .get(trace_id)
            .and_then(Option::as_ref)
            .is_some_and(|native| native.region.is_some())
        {
            self.disconnect_native_regions();
        }
        self.clear_native_direct_links();
        if let Some(slot) = self.native_traces.get_mut(trace_id) {
            *slot = None;
        }

        let program_cache_key = self.ensure_program_cache_key();
        let trace = self.jit.trace_clone(trace_id).ok_or_else(|| {
            VmError::JitNative(format!("trace {} missing for native compile", trace_id))
        })?;
        let drop_contract_events_enabled = self.drop_contract_events_enabled();
        let key = native_trace_cache_key(
            &trace,
            interrupt_settings,
            compile_profile,
            drop_contract_events_enabled,
        );
        let cached = with_native_trace_cache(|cache| {
            if cache.active_program_key != Some(program_cache_key) {
                cache.entries.clear();
                cache.active_program_key = Some(program_cache_key);
            }
            cache.entries.get(&key).cloned()
        });
        if let Some(cached) = cached {
            let dispatcher = native::compile_native_trace_dispatcher(
                trace_id,
                cached.tail_entry as *const u8,
                &trace,
            )?;
            let entry =
                unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(dispatcher.entry) };
            let tail_entry = unsafe {
                std::mem::transmute::<*const u8, NativeTraceEntry>(dispatcher.tail_entry)
            };
            let direct_keepalives = dispatcher
                .keepalives
                .into_iter()
                .map(|keepalive| Arc::new(Mutex::new(keepalive)))
                .collect();
            let mut code = cached.code.to_vec();
            code.extend_from_slice(&dispatcher.code);
            if self.native_traces.len() <= trace_id {
                self.native_traces.resize_with(trace_id + 1, || None);
            }
            self.native_traces[trace_id] = Some(NativeTrace {
                _keepalive: cached.keepalive,
                _direct_keepalives: direct_keepalives,
                entry,
                tail_entry,
                direct_slots: Arc::new(dispatcher.slots),
                code: Arc::from(code.into_boxed_slice()),
                root_ip: trace.root_ip,
                terminal: trace.terminal,
                has_call: trace.has_call,
                has_yielding_call: trace.has_yielding_call,
                lowering_kind: cached.lowering_kind,
                interrupt_settings,
                compile_profile: cached.compile_profile,
                drop_contract_events_enabled,
                region: None,
            });
            return Ok(());
        }

        let compile_started = std::time::Instant::now();
        let compile_result = native::compile_native_trace(
            &trace,
            interrupt_settings,
            compile_profile,
            drop_contract_events_enabled,
        );
        self.jit_native_compile_time_ns = self
            .jit_native_compile_time_ns
            .saturating_add(elapsed_ns(compile_started));
        let compiled = compile_result?;
        let lowering_kind = compiled.lowering_kind;
        let base_entry =
            unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(compiled.entry) };
        let base_tail_entry =
            unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(compiled.tail_entry) };
        let keepalive = Arc::new(Mutex::new(compiled.keepalive));
        let base_code = Arc::<[u8]>::from(compiled.code.clone().into_boxed_slice());
        let cached = NativeTraceCacheEntry {
            entry: base_entry,
            tail_entry: base_tail_entry,
            keepalive: Arc::clone(&keepalive),
            code: Arc::clone(&base_code),
            lowering_kind,
            compile_profile,
        };
        with_native_trace_cache(|cache| {
            if cache.active_program_key != Some(program_cache_key) {
                cache.entries.clear();
                cache.active_program_key = Some(program_cache_key);
            }
            cache.entries.insert(key, cached);
        });
        let dispatcher =
            native::compile_native_trace_dispatcher(trace_id, compiled.tail_entry, &trace)?;
        let entry = unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(dispatcher.entry) };
        let tail_entry =
            unsafe { std::mem::transmute::<*const u8, NativeTraceEntry>(dispatcher.tail_entry) };
        let direct_keepalives = dispatcher
            .keepalives
            .into_iter()
            .map(|keepalive| Arc::new(Mutex::new(keepalive)))
            .collect();
        let direct_slots = Arc::new(dispatcher.slots);
        let mut code = compiled.code;
        code.extend_from_slice(&dispatcher.code);
        let code = Arc::<[u8]>::from(code.into_boxed_slice());
        if self.native_traces.len() <= trace_id {
            self.native_traces.resize_with(trace_id + 1, || None);
        }
        self.native_traces[trace_id] = Some(NativeTrace {
            _keepalive: keepalive,
            _direct_keepalives: direct_keepalives,
            entry,
            tail_entry,
            direct_slots,
            code,
            root_ip: trace.root_ip,
            terminal: trace.terminal,
            has_call: trace.has_call,
            has_yielding_call: trace.has_yielding_call,
            lowering_kind,
            interrupt_settings,
            compile_profile,
            drop_contract_events_enabled,
            region: None,
        });
        Ok(())
    }

    pub fn jit_native_trace_count(&self) -> usize {
        self.native_traces.iter().flatten().count()
    }

    pub fn jit_native_exec_count(&self) -> u64 {
        self.native_trace_exec_count
    }

    pub(crate) fn jit_native_inherited_target(&self) -> usize {
        if !self.jit_native_direct_links_enabled || self.active_frame_has_shared_capture_cells() {
            return 0;
        }
        let Some(trace_id) = self.compiled_trace_for_active_entry() else {
            return 0;
        };
        self.native_traces
            .get(trace_id)
            .and_then(Option::as_ref)
            .map(|native| native.tail_entry as usize)
            .unwrap_or(0)
    }

    pub fn set_jit_native_direct_links_enabled(&mut self, enabled: bool) {
        let cross_frame_enabled = enabled;
        if self.jit_native_direct_links_enabled == enabled
            && self.jit_native_direct_cross_frame_enabled == cross_frame_enabled
        {
            return;
        }
        self.clear_native_direct_links();
        self.disconnect_native_regions();
        self.native_traces.clear();
        self.jit_native_direct_links_enabled = enabled;
        self.jit_native_direct_cross_frame_enabled = cross_frame_enabled;
        self.jit_native_direct_link_count = 0;
        self.jit_native_active_direct_trace_id = usize::MAX;
        self.jit_native_direct_escape_streak = 0;
        self.jit_native_direct_region_fallback = false;
    }

    pub fn jit_native_region_count(&self) -> usize {
        self.native_traces
            .iter()
            .flatten()
            .filter(|native| native.region.is_some())
            .count()
    }

    pub fn jit_native_region_entry_count(&self) -> u64 {
        self.jit_native_region_entry_count
    }

    pub fn jit_native_internal_region_edge_count(&self) -> u64 {
        self.jit_native_region_edge_count
    }

    pub fn jit_native_direct_link_count(&self) -> u64 {
        self.jit_native_direct_link_count
    }

    pub fn jit_native_active_direct_link_slot_count(&self) -> usize {
        self.native_traces
            .iter()
            .flatten()
            .flat_map(|native| native.direct_slots.values())
            .filter(|slot| !slot.target().is_null())
            .count()
    }

    pub fn jit_helper_fallback_count(&self) -> u64 {
        self.jit_helper_fallback_count
    }

    pub fn jit_native_link_handoff_count(&self) -> u64 {
        self.jit_native_link_handoff_count
    }

    fn jit_runtime_metrics(&self) -> JitMetrics {
        JitMetrics {
            boxed_load_site_count: 0,
            boxed_store_site_count: 0,
            trace_exit_count: self.jit_trace_exit_count,
            native_loop_back_count: self.jit_native_loop_back_count,
            helper_fallback_count: self.jit_helper_fallback_count,
            native_trace_exec_count: self.native_trace_exec_count,
            script_call_observations: 0,
            monomorphic_call_sites: 0,
            polymorphic_call_sites: 0,
            inline_attempts: 0,
            inline_successes: 0,
            inline_rejections: 0,
        }
    }

    fn record_jit_helper_fallback(&mut self) {
        self.jit_helper_fallback_count = self.jit_helper_fallback_count.saturating_add(1);
    }

    fn record_jit_link_handoff(&mut self) {
        self.jit_native_link_handoff_count = self.jit_native_link_handoff_count.saturating_add(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::jit::deopt::SideTraceImport;
    use crate::vm::jit::ir::{SsaMaterialization, SsaValueId};

    fn import_with(arg: SsaMaterialization) -> SideTraceImport {
        SideTraceImport {
            parent_exit: SsaExitId::new(0),
            stack_depth: 1,
            local_count: 0,
            dirty_locals: Vec::new(),
            args: vec![arg],
        }
    }

    #[test]
    fn scalar_cycle_import_rejects_tagged_and_heap_materializations() {
        assert!(scalar_cycle_import(&import_with(
            SsaMaterialization::BoxInt(SsaValueId::new(0))
        )));
        assert!(!scalar_cycle_import(&import_with(
            SsaMaterialization::Value(SsaValueId::new(0))
        )));
        assert!(!scalar_cycle_import(&import_with(
            SsaMaterialization::BoxHeapPtr {
                value: SsaValueId::new(0),
                tag: crate::ValueType::Array,
            }
        )));
    }
}
