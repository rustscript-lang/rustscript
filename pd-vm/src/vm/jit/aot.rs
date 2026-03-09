use super::super::{Vm, VmError, VmResult};
use super::{JitTrace, JitTraceTerminal, TraceStep, native};
use crate::{HostImport, Value};
use std::collections::BTreeSet;

const AOT_MAGIC: [u8; 4] = *b"VMAO";
const AOT_VERSION: u16 = 5;
const AOT_FLAGS: u16 = 0;
const AOT_NATIVE_ABI_VERSION: u16 = 1;
const DEFAULT_AOT_BUNDLE_FUEL_CHECK_INTERVAL: u32 = 64;
const AOT_PLACEHOLDER_OPCODE: u8 = 0xFF;

struct EncodedAotTrace {
    trace: JitTrace,
    code: Vec<u8>,
}

struct DecodedAotBundle {
    local_count: usize,
    code_len: usize,
    interrupt_mode: Option<native::NativeInterruptMode>,
    fuel_check_interval: u32,
    constants: Vec<Value>,
    imports: Vec<HostImport>,
    traces: Vec<EncodedAotTrace>,
}

impl Vm {
    pub fn emit_aot_bundle(&mut self) -> VmResult<Vec<u8>> {
        self.emit_aot_bundle_with_fuel_check_interval(DEFAULT_AOT_BUNDLE_FUEL_CHECK_INTERVAL)
    }

    pub fn emit_aot_bundle_with_epoch_check_interval(
        &mut self,
        epoch_check_interval: u32,
    ) -> VmResult<Vec<u8>> {
        self.emit_aot_bundle_with_interrupt_settings(
            (epoch_check_interval != 0).then_some(native::NativeInterruptMode::Epoch),
            epoch_check_interval,
        )
    }

    pub fn emit_aot_bundle_with_fuel_check_interval(
        &mut self,
        fuel_check_interval: u32,
    ) -> VmResult<Vec<u8>> {
        self.emit_aot_bundle_with_interrupt_settings(
            (fuel_check_interval != 0).then_some(native::NativeInterruptMode::Fuel),
            fuel_check_interval,
        )
    }

    fn emit_aot_bundle_with_interrupt_settings(
        &mut self,
        interrupt_mode: Option<native::NativeInterruptMode>,
        fuel_check_interval: u32,
    ) -> VmResult<Vec<u8>> {
        #[cfg(not(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        )))]
        {
            return Err(VmError::JitNative(
                "native AOT bundles are unsupported on this target".to_string(),
            ));
        }

        #[cfg(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ))]
        {
            if !self.jit_config().enabled {
                return Err(VmError::JitNative(
                    "cannot emit native AOT bundle when JIT is disabled".to_string(),
                ));
            }

            self.ensure_program_cache_key();
            let required_max_trace_len = self.program.code.len().max(1);
            if self.jit_config().max_trace_len < required_max_trace_len {
                let mut config = self.jit_config().clone();
                config.max_trace_len = required_max_trace_len;
                self.set_jit_config(config);
            } else {
                self.native_traces.clear();
            }
            let trace_ids = {
                let program = &self.program;
                self.jit.prepare_aot(program)
            };
            if let Some(message) = summarize_aot_prepare_failures(&self.jit.snapshot()) {
                return Err(VmError::JitNative(message));
            }
            let mut trace_ids = trace_ids;
            if fuel_check_interval != 0 {
                let resume_roots =
                    collect_resumable_aot_roots(&self.jit, &trace_ids, fuel_check_interval);
                let mut compiled_ids = trace_ids.iter().copied().collect::<BTreeSet<_>>();
                for root_ip in resume_roots {
                    let program = &self.program;
                    if let Some(trace_id) = self.jit.ensure_aot_root(program, root_ip)
                        && compiled_ids.insert(trace_id)
                    {
                        trace_ids.push(trace_id);
                    }
                }
                if let Some(message) = summarize_aot_prepare_failures(&self.jit.snapshot()) {
                    return Err(VmError::JitNative(message));
                }
            }
            if trace_ids.is_empty() {
                return Err(VmError::JitNative(
                    "AOT compilation produced no native traces".to_string(),
                ));
            }

            let native_interrupt_settings =
                aot_native_interrupt_settings(interrupt_mode, fuel_check_interval);
            let mut traces = Vec::with_capacity(trace_ids.len());
            for trace_id in trace_ids {
                self.ensure_native_trace_with_settings(
                    trace_id,
                    native::NativeCompileProfile::Aot,
                    native_interrupt_settings,
                )?;
                let trace = self.jit.trace_clone(trace_id).ok_or_else(|| {
                    VmError::JitNative(format!("AOT trace {} missing after compilation", trace_id))
                })?;
                let native = self.native_traces.get(&trace_id).ok_or_else(|| {
                    VmError::JitNative(format!(
                        "native AOT trace {} missing after compilation",
                        trace_id
                    ))
                })?;
                traces.push(EncodedAotTrace {
                    trace,
                    code: native.code.as_ref().to_vec(),
                });
            }

            encode_aot_bundle(&DecodedAotBundle {
                local_count: self.locals.len(),
                code_len: self.program.code.len(),
                interrupt_mode,
                fuel_check_interval,
                constants: self.program.constants.clone(),
                imports: self.program.imports.clone(),
                traces,
            })
        }
    }

    pub fn from_aot_bundle_bytes(bytes: &[u8]) -> VmResult<Self> {
        #[cfg(not(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        )))]
        {
            let _ = bytes;
            return Err(VmError::JitNative(
                "native AOT bundles are unsupported on this target".to_string(),
            ));
        }

        #[cfg(any(
            all(
                target_arch = "x86_64",
                any(target_os = "windows", all(unix, not(target_os = "macos")))
            ),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ))]
        {
            let bundle = decode_aot_bundle(bytes)?;
            let fuel_check_interval = bundle.fuel_check_interval;
            let mut vm = Vm::new(crate::Program {
                constants: bundle.constants,
                code: vec![AOT_PLACEHOLDER_OPCODE; bundle.code_len],
                local_count: bundle.local_count,
                imports: bundle.imports,
                debug: None,
                type_map: None,
            });
            vm.native_only_aot = true;
            vm.native_aot_interrupt_check_interval = Some(fuel_check_interval);
            vm.native_aot_interrupt_mode = bundle.interrupt_mode.map(aot_vm_interrupt_mode);
            vm.fuel_check_interval = runtime_fuel_check_interval(fuel_check_interval);
            vm.fuel_ops_until_check = runtime_fuel_check_interval(fuel_check_interval);
            vm.jit
                .install_precompiled_traces(
                    bundle
                        .traces
                        .iter()
                        .map(|encoded| encoded.trace.clone())
                        .collect(),
                )
                .map_err(VmError::JitNative)?;

            for encoded in bundle.traces {
                let trace = encoded.trace;
                let compiled = native::load_compiled_trace(&encoded.code)?;
                let trace_id = trace.id;
                let native_trace = Vm::build_loaded_native_aot_trace(
                    &trace,
                    *compiled,
                    aot_native_interrupt_settings(bundle.interrupt_mode, fuel_check_interval),
                );
                vm.native_traces.insert(trace_id, native_trace);
            }

            Ok(vm)
        }
    }
}

fn summarize_aot_prepare_failures(snapshot: &super::JitSnapshot) -> Option<String> {
    let failures = snapshot
        .attempts
        .iter()
        .filter_map(|attempt| {
            let reason = attempt.result.as_ref().err()?;
            let mut detail = format!("root_ip={}", attempt.root_ip);
            if let Some(line) = attempt.line {
                detail.push_str(&format!(" line={line}"));
            }
            detail.push_str(": ");
            detail.push_str(&reason.message());
            Some(detail)
        })
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return None;
    }

    let preview_len = failures.len().min(6);
    let mut message = format!(
        "cannot emit native AOT bundle: {} AOT roots failed to compile",
        failures.len()
    );
    message.push_str(" (");
    message.push_str(&failures[..preview_len].join("; "));
    if failures.len() > preview_len {
        message.push_str(&format!("; and {} more", failures.len() - preview_len));
    }
    message.push(')');
    Some(message)
}

fn collect_resumable_aot_roots(
    jit: &super::TraceJitEngine,
    trace_ids: &[usize],
    fuel_check_interval: u32,
) -> Vec<usize> {
    let stride = fuel_check_interval as usize;
    if stride == 0 {
        return Vec::new();
    }

    let mut roots = BTreeSet::new();
    for &trace_id in trace_ids {
        let Some(trace) = jit.trace_clone(trace_id) else {
            continue;
        };
        for step_index in (stride..trace.step_ips.len()).step_by(stride) {
            roots.insert(trace.step_ips[step_index]);
        }
    }
    roots.into_iter().collect()
}

fn aot_native_interrupt_settings(
    interrupt_mode: Option<native::NativeInterruptMode>,
    fuel_check_interval: u32,
) -> Option<native::NativeInterruptSettings> {
    match interrupt_mode {
        Some(native::NativeInterruptMode::Fuel) if fuel_check_interval != 0 => {
            Some(native::NativeInterruptSettings::fuel(fuel_check_interval))
        }
        Some(native::NativeInterruptMode::Epoch) if fuel_check_interval != 0 => {
            Some(native::NativeInterruptSettings::epoch(fuel_check_interval))
        }
        _ => None,
    }
}

fn aot_vm_interrupt_mode(mode: native::NativeInterruptMode) -> crate::vm::InterruptMode {
    match mode {
        native::NativeInterruptMode::Fuel => crate::vm::InterruptMode::Fuel,
        native::NativeInterruptMode::Epoch => crate::vm::InterruptMode::Epoch,
    }
}

fn runtime_fuel_check_interval(fuel_check_interval: u32) -> u32 {
    fuel_check_interval.max(1)
}

fn encode_aot_bundle(bundle: &DecodedAotBundle) -> VmResult<Vec<u8>> {
    let layout_fingerprint = native::layout_fingerprint()?;
    let local_count = u32::try_from(bundle.local_count)
        .map_err(|_| VmError::JitNative("AOT local_count exceeds u32".to_string()))?;
    let code_len = u32::try_from(bundle.code_len)
        .map_err(|_| VmError::JitNative("AOT code_len exceeds u32".to_string()))?;

    let mut out = Vec::new();
    out.extend_from_slice(&AOT_MAGIC);
    out.extend_from_slice(&AOT_VERSION.to_le_bytes());
    out.extend_from_slice(&AOT_FLAGS.to_le_bytes());
    out.extend_from_slice(&AOT_NATIVE_ABI_VERSION.to_le_bytes());
    out.extend_from_slice(&layout_fingerprint.to_le_bytes());
    write_aot_string(std::env::consts::ARCH, &mut out)?;
    write_aot_string(std::env::consts::OS, &mut out)?;
    out.push((std::mem::size_of::<usize>() * 8) as u8);
    out.push(u8::from(cfg!(target_endian = "little")));
    out.push(encode_aot_interrupt_mode(bundle.interrupt_mode));
    out.extend_from_slice(&bundle.fuel_check_interval.to_le_bytes());
    out.extend_from_slice(&local_count.to_le_bytes());
    out.extend_from_slice(&code_len.to_le_bytes());
    write_aot_len(bundle.constants.len(), "AOT constants", &mut out)?;
    for value in &bundle.constants {
        encode_aot_value(value, &mut out)?;
    }
    write_aot_len(bundle.imports.len(), "AOT imports", &mut out)?;
    for import in &bundle.imports {
        write_aot_string(&import.name, &mut out)?;
        out.push(import.arity);
    }
    write_aot_len(bundle.traces.len(), "AOT traces", &mut out)?;
    for encoded in &bundle.traces {
        encode_aot_trace(encoded, &mut out)?;
    }
    Ok(out)
}

fn decode_aot_bundle(bytes: &[u8]) -> VmResult<DecodedAotBundle> {
    let mut cursor = AotCursor::new(bytes);
    let magic = cursor.read_array::<4>("magic")?;
    if magic != AOT_MAGIC {
        return Err(VmError::JitNative("invalid AOT magic".to_string()));
    }

    let version = cursor.read_u16("version")?;
    if version != AOT_VERSION {
        return Err(VmError::JitNative(format!(
            "unsupported AOT version {version}, expected {AOT_VERSION}",
        )));
    }

    let flags = cursor.read_u16("flags")?;
    if flags != AOT_FLAGS {
        return Err(VmError::JitNative(format!(
            "unsupported AOT flags {flags}, expected {AOT_FLAGS}",
        )));
    }

    let abi_version = cursor.read_u16("native ABI version")?;
    if abi_version != AOT_NATIVE_ABI_VERSION {
        return Err(VmError::JitNative(format!(
            "unsupported native AOT ABI version {abi_version}, expected {AOT_NATIVE_ABI_VERSION}",
        )));
    }

    let layout_fingerprint = cursor.read_u64("layout fingerprint")?;
    let expected_layout_fingerprint = native::layout_fingerprint()?;
    if layout_fingerprint != expected_layout_fingerprint {
        return Err(VmError::JitNative(
            "native AOT bundle was built for an incompatible VM layout".to_string(),
        ));
    }

    let arch = cursor.read_string("arch")?;
    if arch != std::env::consts::ARCH {
        return Err(VmError::JitNative(format!(
            "native AOT bundle targets arch {arch}, current arch is {}",
            std::env::consts::ARCH
        )));
    }

    let os = cursor.read_string("os")?;
    if os != std::env::consts::OS {
        return Err(VmError::JitNative(format!(
            "native AOT bundle targets os {os}, current os is {}",
            std::env::consts::OS
        )));
    }

    let pointer_width = cursor.read_u8("pointer width")?;
    let expected_pointer_width = (std::mem::size_of::<usize>() * 8) as u8;
    if pointer_width != expected_pointer_width {
        return Err(VmError::JitNative(format!(
            "native AOT bundle expects pointer width {pointer_width}, current width is {expected_pointer_width}",
        )));
    }

    let little_endian = cursor.read_u8("endianness")?;
    if little_endian != u8::from(cfg!(target_endian = "little")) {
        return Err(VmError::JitNative(
            "native AOT bundle endianness does not match current target".to_string(),
        ));
    }

    let interrupt_mode = decode_aot_interrupt_mode(cursor.read_u8("interrupt mode")?)?;
    let fuel_check_interval = cursor.read_u32("fuel check interval")?;

    let local_count = cursor.read_u32("local_count")? as usize;
    let code_len = cursor.read_u32("code_len")? as usize;
    if code_len == 0 {
        return Err(VmError::JitNative(
            "native AOT bundle code_len must be > 0".to_string(),
        ));
    }

    let constant_count = cursor.read_u32("constant count")? as usize;
    let mut constants = Vec::with_capacity(constant_count);
    for _ in 0..constant_count {
        constants.push(decode_aot_value(&mut cursor)?);
    }

    let import_count = cursor.read_u32("import count")? as usize;
    let mut imports = Vec::with_capacity(import_count);
    for _ in 0..import_count {
        imports.push(HostImport {
            name: cursor.read_string("import name")?,
            arity: cursor.read_u8("import arity")?,
        });
    }

    let trace_count = cursor.read_u32("trace count")? as usize;
    if trace_count == 0 {
        return Err(VmError::JitNative(
            "native AOT bundle contains no traces".to_string(),
        ));
    }
    let mut traces = Vec::with_capacity(trace_count);
    for _ in 0..trace_count {
        let encoded = decode_aot_trace(&mut cursor)?;
        validate_aot_trace(&encoded.trace, code_len)?;
        if encoded.code.is_empty() {
            return Err(VmError::JitNative(format!(
                "native AOT trace {} contains empty machine code",
                encoded.trace.id
            )));
        }
        traces.push(encoded);
    }
    cursor.finish()?;

    Ok(DecodedAotBundle {
        local_count,
        code_len,
        interrupt_mode,
        fuel_check_interval,
        constants,
        imports,
        traces,
    })
}

fn encode_aot_interrupt_mode(mode: Option<native::NativeInterruptMode>) -> u8 {
    match mode {
        None => 0,
        Some(native::NativeInterruptMode::Fuel) => 1,
        Some(native::NativeInterruptMode::Epoch) => 2,
    }
}

fn decode_aot_interrupt_mode(value: u8) -> VmResult<Option<native::NativeInterruptMode>> {
    match value {
        0 => Ok(None),
        1 => Ok(Some(native::NativeInterruptMode::Fuel)),
        2 => Ok(Some(native::NativeInterruptMode::Epoch)),
        other => Err(VmError::JitNative(format!(
            "invalid AOT interrupt mode tag {other}",
        ))),
    }
}

fn encode_aot_trace(encoded: &EncodedAotTrace, out: &mut Vec<u8>) -> VmResult<()> {
    let trace = &encoded.trace;
    debug_assert_eq!(trace.steps.len(), trace.step_ips.len());
    write_aot_u32(trace.id, "trace id", out)?;
    write_aot_u32(trace.root_ip, "trace root_ip", out)?;
    out.extend_from_slice(&trace.start_line.unwrap_or(u32::MAX).to_le_bytes());
    out.push(match trace.terminal {
        JitTraceTerminal::LoopBack => 0,
        JitTraceTerminal::BranchExit => 1,
        JitTraceTerminal::Halt => 2,
    });
    let mut flags = 0u8;
    if trace.has_call {
        flags |= 1;
    }
    if trace.has_yielding_call {
        flags |= 2;
    }
    out.push(flags);
    write_aot_len(trace.steps.len(), "trace steps", out)?;
    for (step_ip, step) in trace.step_ips.iter().copied().zip(trace.steps.iter()) {
        write_aot_u32(step_ip, "trace step_ip", out)?;
        encode_trace_step(step, out)?;
    }
    write_aot_len(encoded.code.len(), "trace machine code", out)?;
    out.extend_from_slice(&encoded.code);
    Ok(())
}

fn decode_aot_trace(cursor: &mut AotCursor<'_>) -> VmResult<EncodedAotTrace> {
    let id = cursor.read_u32("trace id")? as usize;
    let root_ip = cursor.read_u32("trace root_ip")? as usize;
    let start_line = match cursor.read_u32("trace start_line")? {
        u32::MAX => None,
        value => Some(value),
    };
    let terminal = match cursor.read_u8("trace terminal")? {
        0 => JitTraceTerminal::LoopBack,
        1 => JitTraceTerminal::BranchExit,
        2 => JitTraceTerminal::Halt,
        other => {
            return Err(VmError::JitNative(format!(
                "invalid AOT trace terminal tag {other}",
            )));
        }
    };
    let flags = cursor.read_u8("trace flags")?;
    let step_count = cursor.read_u32("trace step count")? as usize;
    let mut steps = Vec::with_capacity(step_count);
    let mut step_ips = Vec::with_capacity(step_count);
    for _ in 0..step_count {
        step_ips.push(cursor.read_u32("trace step_ip")? as usize);
        steps.push(decode_trace_step(cursor)?);
    }
    let code_len = cursor.read_u32("trace code len")? as usize;
    let code = cursor.read_bytes(code_len, "trace code")?.to_vec();

    Ok(EncodedAotTrace {
        trace: JitTrace {
            id,
            root_ip,
            start_line,
            has_call: (flags & 1) != 0,
            has_yielding_call: (flags & 2) != 0,
            steps,
            step_ips,
            terminal,
            executions: 0,
        },
        code,
    })
}

fn encode_trace_step(step: &TraceStep, out: &mut Vec<u8>) -> VmResult<()> {
    match step {
        TraceStep::Nop => out.push(0),
        TraceStep::Ldc(index) => {
            out.push(1);
            out.extend_from_slice(&index.to_le_bytes());
        }
        TraceStep::Add => out.push(2),
        TraceStep::IAdd => out.push(27),
        TraceStep::FAdd => out.push(28),
        TraceStep::SConcat => out.push(29),
        TraceStep::Sub => out.push(3),
        TraceStep::ISub => out.push(30),
        TraceStep::FSub => out.push(31),
        TraceStep::Mul => out.push(4),
        TraceStep::IMul => out.push(32),
        TraceStep::FMul => out.push(33),
        TraceStep::Div => out.push(5),
        TraceStep::IDiv => out.push(34),
        TraceStep::FDiv => out.push(35),
        TraceStep::Mod => out.push(6),
        TraceStep::IMod => out.push(36),
        TraceStep::FMod => out.push(37),
        TraceStep::Shl => out.push(7),
        TraceStep::Shr => out.push(8),
        TraceStep::Lshr => out.push(25),
        TraceStep::And => out.push(9),
        TraceStep::Or => out.push(10),
        TraceStep::Not => out.push(26),
        TraceStep::Neg => out.push(11),
        TraceStep::INeg => out.push(38),
        TraceStep::FNeg => out.push(39),
        TraceStep::Ceq => out.push(12),
        TraceStep::FCeq => out.push(40),
        TraceStep::Clt => out.push(13),
        TraceStep::FClt => out.push(41),
        TraceStep::Cgt => out.push(14),
        TraceStep::FCgt => out.push(42),
        TraceStep::Pop => out.push(15),
        TraceStep::Dup => out.push(16),
        TraceStep::Ldloc(index) => {
            out.push(17);
            out.push(*index);
        }
        TraceStep::Stloc(index) => {
            out.push(18);
            out.push(*index);
        }
        TraceStep::BuiltinCall {
            index,
            argc,
            call_ip,
        } => {
            out.push(19);
            out.extend_from_slice(&index.to_le_bytes());
            out.push(*argc);
            write_aot_u32(*call_ip, "builtin call_ip", out)?;
        }
        TraceStep::Call {
            index,
            argc,
            call_ip,
        } => {
            out.push(20);
            out.extend_from_slice(&index.to_le_bytes());
            out.push(*argc);
            write_aot_u32(*call_ip, "call_ip", out)?;
        }
        TraceStep::GuardFalse { exit_ip } => {
            out.push(21);
            write_aot_u32(*exit_ip, "guard exit_ip", out)?;
        }
        TraceStep::JumpToIp { target_ip } => {
            out.push(22);
            write_aot_u32(*target_ip, "jump target_ip", out)?;
        }
        TraceStep::JumpToRoot => out.push(23),
        TraceStep::Ret => out.push(24),
    }
    Ok(())
}

fn decode_trace_step(cursor: &mut AotCursor<'_>) -> VmResult<TraceStep> {
    let tag = cursor.read_u8("trace step tag")?;
    Ok(match tag {
        0 => TraceStep::Nop,
        1 => TraceStep::Ldc(cursor.read_u32("ldc index")?),
        2 => TraceStep::Add,
        27 => TraceStep::IAdd,
        28 => TraceStep::FAdd,
        29 => TraceStep::SConcat,
        3 => TraceStep::Sub,
        30 => TraceStep::ISub,
        31 => TraceStep::FSub,
        4 => TraceStep::Mul,
        32 => TraceStep::IMul,
        33 => TraceStep::FMul,
        5 => TraceStep::Div,
        34 => TraceStep::IDiv,
        35 => TraceStep::FDiv,
        6 => TraceStep::Mod,
        36 => TraceStep::IMod,
        37 => TraceStep::FMod,
        7 => TraceStep::Shl,
        8 => TraceStep::Shr,
        25 => TraceStep::Lshr,
        9 => TraceStep::And,
        10 => TraceStep::Or,
        26 => TraceStep::Not,
        11 => TraceStep::Neg,
        38 => TraceStep::INeg,
        39 => TraceStep::FNeg,
        12 => TraceStep::Ceq,
        40 => TraceStep::FCeq,
        13 => TraceStep::Clt,
        41 => TraceStep::FClt,
        14 => TraceStep::Cgt,
        42 => TraceStep::FCgt,
        15 => TraceStep::Pop,
        16 => TraceStep::Dup,
        17 => TraceStep::Ldloc(cursor.read_u8("ldloc index")?),
        18 => TraceStep::Stloc(cursor.read_u8("stloc index")?),
        19 => TraceStep::BuiltinCall {
            index: cursor.read_u16("builtin index")?,
            argc: cursor.read_u8("builtin argc")?,
            call_ip: cursor.read_u32("builtin call_ip")? as usize,
        },
        20 => TraceStep::Call {
            index: cursor.read_u16("call index")?,
            argc: cursor.read_u8("call argc")?,
            call_ip: cursor.read_u32("call_ip")? as usize,
        },
        21 => TraceStep::GuardFalse {
            exit_ip: cursor.read_u32("guard exit_ip")? as usize,
        },
        22 => TraceStep::JumpToIp {
            target_ip: cursor.read_u32("jump target_ip")? as usize,
        },
        23 => TraceStep::JumpToRoot,
        24 => TraceStep::Ret,
        other => {
            return Err(VmError::JitNative(format!(
                "invalid AOT trace step tag {other}",
            )));
        }
    })
}

fn encode_aot_value(value: &Value, out: &mut Vec<u8>) -> VmResult<()> {
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
            write_aot_string(value, out)?;
        }
        Value::Array(values) => {
            out.push(5);
            write_aot_len(values.len(), "array value", out)?;
            for value in values.iter() {
                encode_aot_value(value, out)?;
            }
        }
        Value::Map(entries) => {
            out.push(6);
            write_aot_len(entries.len(), "map value", out)?;
            for (key, value) in entries.iter() {
                encode_aot_value(key, out)?;
                encode_aot_value(value, out)?;
            }
        }
    }
    Ok(())
}

fn decode_aot_value(cursor: &mut AotCursor<'_>) -> VmResult<Value> {
    Ok(match cursor.read_u8("value tag")? {
        0 => Value::Null,
        1 => Value::Int(cursor.read_i64("int value")?),
        2 => Value::Float(cursor.read_f64("float value")?),
        3 => match cursor.read_u8("bool value")? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            other => {
                return Err(VmError::JitNative(format!("invalid AOT bool tag {other}",)));
            }
        },
        4 => Value::string(cursor.read_string("string value")?),
        5 => {
            let len = cursor.read_u32("array len")? as usize;
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(decode_aot_value(cursor)?);
            }
            Value::array(values)
        }
        6 => {
            let len = cursor.read_u32("map len")? as usize;
            let mut entries = Vec::with_capacity(len);
            for _ in 0..len {
                let key = decode_aot_value(cursor)?;
                let value = decode_aot_value(cursor)?;
                entries.push((key, value));
            }
            Value::map(entries)
        }
        other => {
            return Err(VmError::JitNative(
                format!("invalid AOT value tag {other}",),
            ));
        }
    })
}

fn validate_aot_trace(trace: &JitTrace, code_len: usize) -> VmResult<()> {
    if trace.root_ip >= code_len {
        return Err(VmError::JitNative(format!(
            "trace {} root_ip {} is out of range for code_len {}",
            trace.id, trace.root_ip, code_len
        )));
    }
    if trace.steps.len() != trace.step_ips.len() {
        return Err(VmError::JitNative(format!(
            "trace {} steps and step_ips length mismatch",
            trace.id
        )));
    }
    for &step_ip in &trace.step_ips {
        if step_ip >= code_len {
            return Err(VmError::JitNative(format!(
                "trace {} step_ip {} is out of range for code_len {}",
                trace.id, step_ip, code_len
            )));
        }
    }
    for step in &trace.steps {
        match step {
            TraceStep::BuiltinCall { call_ip, .. } | TraceStep::Call { call_ip, .. } => {
                let Some(resume_ip) = call_ip.checked_add(4) else {
                    return Err(VmError::JitNative(format!(
                        "trace {} call_ip {} overflows",
                        trace.id, call_ip
                    )));
                };
                if *call_ip >= code_len || resume_ip > code_len {
                    return Err(VmError::JitNative(format!(
                        "trace {} call_ip {} is out of range for code_len {}",
                        trace.id, call_ip, code_len
                    )));
                }
            }
            TraceStep::GuardFalse { exit_ip } => {
                if *exit_ip >= code_len {
                    return Err(VmError::JitNative(format!(
                        "trace {} guard exit_ip {} is out of range for code_len {}",
                        trace.id, exit_ip, code_len
                    )));
                }
            }
            TraceStep::JumpToIp { target_ip } => {
                if *target_ip >= code_len {
                    return Err(VmError::JitNative(format!(
                        "trace {} jump target_ip {} is out of range for code_len {}",
                        trace.id, target_ip, code_len
                    )));
                }
            }
            TraceStep::Nop
            | TraceStep::Ldc(_)
            | TraceStep::Add
            | TraceStep::IAdd
            | TraceStep::FAdd
            | TraceStep::SConcat
            | TraceStep::Sub
            | TraceStep::ISub
            | TraceStep::FSub
            | TraceStep::Mul
            | TraceStep::IMul
            | TraceStep::FMul
            | TraceStep::Div
            | TraceStep::IDiv
            | TraceStep::FDiv
            | TraceStep::Mod
            | TraceStep::IMod
            | TraceStep::FMod
            | TraceStep::Shl
            | TraceStep::Shr
            | TraceStep::Lshr
            | TraceStep::And
            | TraceStep::Or
            | TraceStep::Not
            | TraceStep::Neg
            | TraceStep::INeg
            | TraceStep::FNeg
            | TraceStep::Ceq
            | TraceStep::FCeq
            | TraceStep::Clt
            | TraceStep::FClt
            | TraceStep::Cgt
            | TraceStep::FCgt
            | TraceStep::Pop
            | TraceStep::Dup
            | TraceStep::Ldloc(_)
            | TraceStep::Stloc(_)
            | TraceStep::JumpToRoot
            | TraceStep::Ret => {}
        }
    }
    Ok(())
}

fn write_aot_len(len: usize, field: &str, out: &mut Vec<u8>) -> VmResult<()> {
    let len = u32::try_from(len)
        .map_err(|_| VmError::JitNative(format!("{field} length exceeds u32")))?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

fn write_aot_u32(value: usize, field: &str, out: &mut Vec<u8>) -> VmResult<()> {
    let value =
        u32::try_from(value).map_err(|_| VmError::JitNative(format!("{field} exceeds u32")))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_aot_string(value: &str, out: &mut Vec<u8>) -> VmResult<()> {
    write_aot_len(value.len(), "string", out)?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

struct AotCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> AotCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn finish(&self) -> VmResult<()> {
        if self.pos != self.bytes.len() {
            return Err(VmError::JitNative(
                "trailing bytes after AOT payload".to_string(),
            ));
        }
        Ok(())
    }

    fn read_array<const N: usize>(&mut self, field: &str) -> VmResult<[u8; N]> {
        let bytes = self.read_bytes(N, field)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn read_u8(&mut self, field: &str) -> VmResult<u8> {
        Ok(*self
            .read_bytes(1, field)?
            .first()
            .expect("read_bytes(1) must return one byte"))
    }

    fn read_u16(&mut self, field: &str) -> VmResult<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>(field)?))
    }

    fn read_u32(&mut self, field: &str) -> VmResult<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>(field)?))
    }

    fn read_u64(&mut self, field: &str) -> VmResult<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>(field)?))
    }

    fn read_i64(&mut self, field: &str) -> VmResult<i64> {
        Ok(i64::from_le_bytes(self.read_array::<8>(field)?))
    }

    fn read_f64(&mut self, field: &str) -> VmResult<f64> {
        Ok(f64::from_le_bytes(self.read_array::<8>(field)?))
    }

    fn read_string(&mut self, field: &str) -> VmResult<String> {
        let len = self.read_u32(field)? as usize;
        let bytes = self.read_bytes(len, field)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| VmError::JitNative(format!("invalid UTF-8 in AOT {field}")))
    }

    fn read_bytes(&mut self, len: usize, field: &str) -> VmResult<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| VmError::JitNative(format!("AOT {field} length overflow")))?;
        if end > self.bytes.len() {
            return Err(VmError::JitNative(format!(
                "truncated AOT bundle while reading {field}",
            )));
        }
        let bytes = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }
}
