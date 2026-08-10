//! Backend engine state.
//!
//! [`Engine`] owns the code-generation backends and their caches: the trace
//! JIT engine, native traces and their counters, the optional AOT program,
//! the regex cache, program-derived decode caches, and code-generation
//! telemetry. It holds no per-run interpreter state and no host bindings, so
//! it can be shared across runs (and, by construction, reused by any number of
//! instances that never share stacks or resources).
//!
//! Native ABI note: the JIT/AOT code generators read a handful of fields by
//! machine offset through `std::mem::offset_of!(Vm, engine.<field>)`. The
//! field set and the offsets are part of the native ABI; see
//! `crate::vm::native::layout`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::builtins::runtime::regex::RegexCache;
use crate::bytecode::{DecodedInstructionData, Program};
use crate::vm::aot;
use crate::vm::jit;
use crate::vm::native;

/// Engine-owned backend configuration, caches, and code-generation telemetry.
///
/// Thread safety: `Engine` is not shared between threads (`TraceJitEngine` is
/// not `Sync`); one VM facade owns one engine. Clone semantics: `Engine` is
/// intentionally not `Clone` — duplicating it would duplicate native traces
/// and JIT bookkeeping that are keyed to one execution identity.
pub(crate) struct Engine {
    pub(crate) jit: jit::TraceJitEngine,
    pub(crate) native_traces: Vec<Option<jit::NativeTrace>>,
    pub(crate) native_trace_exec_count: u64,
    pub(crate) aot_program: Option<aot::CompiledProgram>,
    pub(crate) aot_exec_count: u64,
    pub(crate) aot_interpreter_boundary_hit: bool,
    pub(crate) jit_native_region_entry_count: u64,
    pub(crate) jit_native_region_edge_count: u64,
    pub(crate) jit_native_direct_link_count: u64,
    pub(crate) jit_native_direct_links_enabled: bool,
    pub(crate) jit_native_direct_cross_frame_enabled: bool,
    pub(crate) jit_native_active_direct_trace_id: usize,
    pub(crate) jit_native_direct_escape_streak: u16,
    pub(crate) jit_native_direct_region_fallback: bool,
    pub(crate) jit_native_compile_time_ns: u64,
    pub(crate) jit_native_region_compile_time_ns: u64,
    pub(crate) jit_trace_exit_count: u64,
    pub(crate) jit_native_loop_back_count: u64,
    pub(crate) jit_native_link_handoff_count: u64,
    pub(crate) jit_native_link_dispatch_depth: u32,
    pub(crate) jit_helper_fallback_count: u64,
    pub(crate) jit_native_bridge_stats_enabled: bool,
    pub(crate) jit_native_bridge_counts: HashMap<&'static str, u64>,
    pub(crate) program_cache_key: u64,
    pub(crate) program_cache_key_ready: bool,
    pub(crate) regex_cache: RegexCache,
    pub(crate) decoded_instruction_data: Arc<DecodedInstructionData>,
    pub(crate) operand_type_hints: Option<Arc<[u8]>>,
    // Native ABI mirrors: the JIT/AOT code generators load these addresses by
    // field offset from the `Vm` facade. They are derived from the program and
    // from static helper entry points, and are documented as load-bearing for
    // `crate::vm::native`.
    pub(crate) program_constants_ptr: usize,
    #[allow(dead_code)]
    pub(crate) program_constants_len: usize,
    #[allow(dead_code)]
    pub(crate) native_helper_fn: usize,
    #[allow(dead_code)]
    pub(crate) native_interrupt_helper_fn: usize,
}

impl Engine {
    /// Builds an engine for one program and JIT configuration.
    pub(crate) fn new(jit_config: jit::JitConfig, program: &Program) -> Self {
        Self {
            jit: jit::TraceJitEngine::new(jit_config),
            native_traces: Vec::new(),
            native_trace_exec_count: 0,
            aot_program: None,
            aot_exec_count: 0,
            aot_interpreter_boundary_hit: false,
            jit_native_region_entry_count: 0,
            jit_native_region_edge_count: 0,
            jit_native_direct_link_count: 0,
            jit_native_direct_links_enabled: true,
            jit_native_direct_cross_frame_enabled: false,
            jit_native_active_direct_trace_id: usize::MAX,
            jit_native_direct_escape_streak: 0,
            jit_native_direct_region_fallback: false,
            jit_native_compile_time_ns: 0,
            jit_native_region_compile_time_ns: 0,
            jit_trace_exit_count: 0,
            jit_native_loop_back_count: 0,
            jit_native_link_handoff_count: 0,
            jit_native_link_dispatch_depth: 0,
            jit_helper_fallback_count: 0,
            jit_native_bridge_stats_enabled: false,
            jit_native_bridge_counts: HashMap::new(),
            program_cache_key: 0,
            program_cache_key_ready: false,
            regex_cache: RegexCache::default(),
            decoded_instruction_data: program.shared_decoded_instruction_data(),
            operand_type_hints: program.shared_operand_type_hints(),
            program_constants_ptr: program.constants.as_ptr() as usize,
            program_constants_len: program.constants.len(),
            native_helper_fn: native::helper_entry_address(),
            native_interrupt_helper_fn: native::interrupt_helper_entry_address(),
        }
    }

    /// Returns the program cache key, computing and caching it on first use.
    /// The key identifies the program for backend cache lookups; it is stable
    /// for the lifetime of the engine (the program is immutable).
    pub(crate) fn ensure_program_cache_key(&mut self, program: &Program) -> u64 {
        if !self.program_cache_key_ready {
            self.program_cache_key = super::compute_program_cache_key(program);
            self.program_cache_key_ready = true;
        }
        self.program_cache_key
    }

    /// Rewinds run-scoped backend state between runs while retaining compiled
    /// artifacts: hot-entry bookkeeping and call-site profiles are cleared,
    /// and the AOT boundary flag is recomputed from the compiled program.
    pub(crate) fn reset_runtime_state(&mut self, program: &Program) {
        self.aot_interpreter_boundary_hit = self
            .aot_program
            .as_ref()
            .is_some_and(|compiled| compiled.interpreter_boundary_only);
        self.jit.reset_runtime_backoff();
        self.jit.clear_call_site_profiles();
        let _ = program;
    }

    /// Invalidates code-generation caches that may reference run-scoped
    /// behavior (used when drop-contract event accounting is toggled).
    pub(crate) fn invalidate_codegen_caches(&mut self) {
        self.native_traces.clear();
        self.native_trace_exec_count = 0;
    }
}
