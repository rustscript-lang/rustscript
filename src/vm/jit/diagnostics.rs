use super::super::Vm;
use super::{JitMetrics, JitSnapshot, native};

impl Vm {
    pub(super) fn jit_diagnostics_snapshot(&self) -> JitSnapshot {
        self.jit.snapshot(self.jit_diagnostics_metrics())
    }

    pub(super) fn jit_diagnostics_dump(&self, include_machine_code: bool) -> String {
        let mut out = self
            .jit
            .dump_text(self.program.debug.as_ref(), self.jit_diagnostics_metrics());
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

    fn jit_diagnostics_metrics(&self) -> JitMetrics {
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
}
