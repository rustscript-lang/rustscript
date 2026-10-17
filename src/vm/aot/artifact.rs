use std::io;
use std::path::Path;

use crate::bytecode::Program;
use crate::vm::native::{
    helper_entry_offset, interrupt_helper_entry_offset, selected_codegen_backend,
};
use crate::vm::{Vm, VmError};

use super::super::jit::JitConfig;
use super::compile::{CompiledProgram, compile_artifact_interpreter_boundary, compile_program};

const MAGIC: [u8; 4] = *b"PAT\0";
const VERSION: u16 = 8;
const ABI_VERSION: u16 = 8;
const FLAG_INTERPRETER_BOUNDARY_ONLY: u16 = 1;
const SUPPORTED_FLAGS: u16 = FLAG_INTERPRETER_BOUNDARY_ONLY;

#[derive(Debug)]
pub enum AotArtifactError {
    Io(io::Error),
    Vm(VmError),
    Wire(crate::WireError),
    MissingAotProgram,
    UnexpectedEof,
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u16),
    UnsupportedAbiVersion(u16),
    UnsupportedFlags(u16),
    InvalidUtf8,
    TrailingBytes,
    LengthTooLarge(&'static str, usize),
    IncompatibleProgramHash {
        expected: u64,
        found: u64,
    },
    EmbeddedProgramHashMismatch {
        stored: u64,
        computed: u64,
    },
    InvalidValueTag(u8),
    InvalidBool(u8),
    InvalidValueType(u8),
    IncompatibleRuntime {
        field: &'static str,
        expected: String,
        found: String,
    },
}

impl std::fmt::Display for AotArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Vm(err) => write!(f, "{err}"),
            Self::Wire(err) => write!(f, "{err}"),
            Self::MissingAotProgram => write!(f, "whole-program aot is not compiled"),
            Self::UnexpectedEof => write!(f, "unexpected end of aot artifact"),
            Self::InvalidMagic(found) => write!(f, "invalid aot artifact magic: {found:?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported aot artifact version: {version}")
            }
            Self::UnsupportedAbiVersion(version) => {
                write!(f, "unsupported aot artifact abi version: {version}")
            }
            Self::UnsupportedFlags(flags) => write!(f, "unsupported aot artifact flags: {flags}"),
            Self::InvalidUtf8 => write!(f, "invalid utf-8 in aot artifact"),
            Self::TrailingBytes => write!(f, "trailing bytes after aot artifact payload"),
            Self::LengthTooLarge(field, len) => write!(f, "{field} length too large: {len}"),
            Self::IncompatibleProgramHash { expected, found } => write!(
                f,
                "aot artifact program hash mismatch: expected {expected}, found {found}"
            ),
            Self::EmbeddedProgramHashMismatch { stored, computed } => write!(
                f,
                "aot artifact embedded program hash mismatch: stored {stored}, computed {computed}"
            ),
            Self::InvalidValueTag(tag) => write!(f, "invalid aot artifact value tag: {tag}"),
            Self::InvalidBool(value) => write!(f, "invalid bool value in aot artifact: {value}"),
            Self::InvalidValueType(value) => {
                write!(f, "invalid value type in aot artifact: {value}")
            }
            Self::IncompatibleRuntime {
                field,
                expected,
                found,
            } => write!(
                f,
                "aot artifact runtime mismatch for {field}: expected {expected}, found {found}"
            ),
        }
    }
}

impl std::error::Error for AotArtifactError {}

impl From<VmError> for AotArtifactError {
    fn from(value: VmError) -> Self {
        Self::Vm(value)
    }
}

impl From<crate::WireError> for AotArtifactError {
    fn from(value: crate::WireError) -> Self {
        Self::Wire(value)
    }
}

impl Vm {
    pub fn encode_aot_artifact(&mut self) -> Result<Vec<u8>, AotArtifactError> {
        if self.engine.aot_program.is_none() {
            self.compile_aot()?;
        }
        let program_hash = self.ensure_program_cache_key();
        let aot_program = self
            .engine
            .aot_program
            .as_ref()
            .ok_or(AotArtifactError::MissingAotProgram)?;
        encode_artifact(self.program(), aot_program, program_hash)
    }

    pub fn save_aot_artifact_to_file<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> Result<(), AotArtifactError> {
        let bytes = self.encode_aot_artifact()?;
        std::fs::write(path, bytes).map_err(AotArtifactError::Io)
    }

    pub fn load_aot_artifact(&mut self, bytes: &[u8]) -> Result<(), AotArtifactError> {
        // Do not populate even the cache-key fields until materialization has
        // succeeded: a rejected artifact must leave the entire engine intact.
        let expected_program_hash = if self.engine.program_cache_key_ready {
            self.engine.program_cache_key
        } else {
            super::super::compute_program_cache_key(self.program())
        };
        let decoded = decode_artifact(bytes, Some(expected_program_hash))?;
        let compiled = materialize_native_program(&decoded)?;
        // Rebuilding native code from persisted metadata issues no source
        // certificate and cannot authenticate one from a cache key alone.
        self.revoke_capture_certification();
        self.ensure_program_cache_key();
        self.engine.aot_program = Some(compiled);
        self.engine.aot_exec_count = 0;
        Ok(())
    }

    pub fn load_aot_artifact_from_file<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> Result<(), AotArtifactError> {
        let bytes = std::fs::read(path).map_err(AotArtifactError::Io)?;
        self.load_aot_artifact(&bytes)
    }

    pub fn new_from_aot_artifact_with_jit_config(
        bytes: &[u8],
        jit_config: JitConfig,
    ) -> Result<Self, AotArtifactError> {
        let decoded = decode_artifact(bytes, None)?;
        let compiled = materialize_native_program(&decoded)?;
        let mut vm = Vm::new_with_jit_config(decoded.program, jit_config);
        vm.engine.aot_program = Some(compiled);
        vm.engine.aot_exec_count = 0;
        Ok(vm)
    }

    pub fn new_from_aot_artifact_file_with_jit_config<P: AsRef<Path>>(
        path: P,
        jit_config: JitConfig,
    ) -> Result<Self, AotArtifactError> {
        let bytes = std::fs::read(path).map_err(AotArtifactError::Io)?;
        Self::new_from_aot_artifact_with_jit_config(&bytes, jit_config)
    }
}

struct DecodedArtifact {
    program: Program,
    // Keep decoding the v8 payload in full for wire validation. These bytes
    // contain writer-process helper addresses and must never be executable.
    _code: Vec<u8>,
    _resume_ips: Vec<usize>,
    interpreter_boundary_only: bool,
}

fn materialize_native_program(
    decoded: &DecodedArtifact,
) -> Result<CompiledProgram, AotArtifactError> {
    // Preserve the executable-buffer length rejection without mapping any
    // persisted bytes. The full decoder validation has already completed.
    #[cfg(any(windows, unix))]
    if decoded._code.is_empty() {
        return Err(AotArtifactError::Vm(VmError::JitNative(
            "native trace produced empty machine code".to_string(),
        )));
    }
    if decoded.interpreter_boundary_only {
        return compile_artifact_interpreter_boundary().map_err(AotArtifactError::Vm);
    }
    let compiled = compile_program(&decoded.program)?;
    if compiled.interpreter_boundary_only {
        return Err(AotArtifactError::Vm(VmError::JitNative(
            "native AOT artifact rebuild produced an interpreter boundary".to_string(),
        )));
    }
    Ok(compiled)
}

fn encode_artifact(
    source_program: &Program,
    aot_program: &CompiledProgram,
    program_hash: u64,
) -> Result<Vec<u8>, AotArtifactError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&ABI_VERSION.to_le_bytes());
    let flags = if aot_program.interpreter_boundary_only {
        FLAG_INTERPRETER_BOUNDARY_ONLY
    } else {
        0
    };
    out.extend_from_slice(&flags.to_le_bytes());
    out.push(u8::try_from(std::mem::size_of::<usize>()).expect("pointer width fits u8"));

    write_string("arch", std::env::consts::ARCH, &mut out)?;
    write_string("os", std::env::consts::OS, &mut out)?;
    write_string("backend", selected_codegen_backend(), &mut out)?;

    write_u32(
        "vm ip offset",
        std::mem::offset_of!(Vm, instance.ip),
        &mut out,
    )?;
    write_u32(
        "native helper offset",
        helper_entry_offset() as usize,
        &mut out,
    )?;
    write_u32(
        "interrupt helper offset",
        interrupt_helper_entry_offset() as usize,
        &mut out,
    )?;
    out.extend_from_slice(&program_hash.to_le_bytes());

    write_u32("program local count", source_program.local_count, &mut out)?;
    let encoded_program = encode_program_payload(source_program)?;
    write_u32("embedded program length", encoded_program.len(), &mut out)?;
    out.extend_from_slice(&encoded_program);

    write_u32("resume ip count", aot_program.resume_ips.len(), &mut out)?;
    for &ip in aot_program.resume_ips.iter() {
        write_u32("resume ip", ip, &mut out)?;
    }

    write_u32(
        "native code length",
        aot_program.code_bytes().len(),
        &mut out,
    )?;
    out.extend_from_slice(aot_program.code_bytes());
    Ok(out)
}

fn decode_artifact(
    bytes: &[u8],
    expected_program_hash: Option<u64>,
) -> Result<DecodedArtifact, AotArtifactError> {
    let mut cursor = Cursor::new(bytes);

    let magic = cursor.read_exact_array::<4>()?;
    if magic != MAGIC {
        return Err(AotArtifactError::InvalidMagic(magic));
    }

    let version = cursor.read_u16()?;
    if version != VERSION {
        return Err(AotArtifactError::UnsupportedVersion(version));
    }

    let abi_version = cursor.read_u16()?;
    if abi_version != ABI_VERSION {
        return Err(AotArtifactError::UnsupportedAbiVersion(abi_version));
    }

    let flags = cursor.read_u16()?;
    if flags & !SUPPORTED_FLAGS != 0 {
        return Err(AotArtifactError::UnsupportedFlags(flags));
    }
    let interpreter_boundary_only = flags & FLAG_INTERPRETER_BOUNDARY_ONLY != 0;

    let pointer_width = cursor.read_u8()?;
    validate_runtime_field(
        "pointer width",
        std::mem::size_of::<usize>().to_string(),
        pointer_width.to_string(),
    )?;
    validate_runtime_field(
        "arch",
        std::env::consts::ARCH.to_string(),
        cursor.read_string()?,
    )?;
    validate_runtime_field(
        "os",
        std::env::consts::OS.to_string(),
        cursor.read_string()?,
    )?;
    validate_runtime_field(
        "backend",
        selected_codegen_backend().to_string(),
        cursor.read_string()?,
    )?;
    validate_runtime_field(
        "vm ip offset",
        std::mem::offset_of!(Vm, instance.ip).to_string(),
        cursor.read_u32()?.to_string(),
    )?;
    validate_runtime_field(
        "native helper offset",
        helper_entry_offset().to_string(),
        cursor.read_u32()?.to_string(),
    )?;
    validate_runtime_field(
        "interrupt helper offset",
        interrupt_helper_entry_offset().to_string(),
        cursor.read_u32()?.to_string(),
    )?;

    let found_program_hash = cursor.read_u64()?;
    let local_count = cursor.read_u32()? as usize;
    let embedded_program_len = cursor.read_u32()? as usize;
    let mut program = decode_program_payload(cursor.read_exact(embedded_program_len)?)?;
    program.local_count = local_count;
    let computed_hash = super::super::compute_program_cache_key(&program);
    if computed_hash != found_program_hash {
        return Err(AotArtifactError::EmbeddedProgramHashMismatch {
            stored: found_program_hash,
            computed: computed_hash,
        });
    }

    if let Some(expected_program_hash) = expected_program_hash
        && found_program_hash != expected_program_hash
    {
        return Err(AotArtifactError::IncompatibleProgramHash {
            expected: expected_program_hash,
            found: found_program_hash,
        });
    }

    let resume_count = cursor.read_u32()? as usize;
    let mut resume_ips = Vec::with_capacity(resume_count);
    for _ in 0..resume_count {
        resume_ips.push(cursor.read_u32()? as usize);
    }

    let code_len = cursor.read_u32()? as usize;
    let code = cursor.read_exact(code_len)?.to_vec();

    if !cursor.is_at_end() {
        return Err(AotArtifactError::TrailingBytes);
    }

    Ok(DecodedArtifact {
        program,
        _code: code,
        _resume_ips: resume_ips,
        interpreter_boundary_only,
    })
}

fn validate_runtime_field(
    field: &'static str,
    expected: String,
    found: String,
) -> Result<(), AotArtifactError> {
    if expected == found {
        return Ok(());
    }
    Err(AotArtifactError::IncompatibleRuntime {
        field,
        expected,
        found,
    })
}

fn write_u32(field: &'static str, value: usize, out: &mut Vec<u8>) -> Result<(), AotArtifactError> {
    let value = u32::try_from(value).map_err(|_| AotArtifactError::LengthTooLarge(field, value))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_string(
    field: &'static str,
    value: &str,
    out: &mut Vec<u8>,
) -> Result<(), AotArtifactError> {
    write_u32(field, value.len(), out)?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode_program_payload(program: &Program) -> Result<Vec<u8>, AotArtifactError> {
    crate::vmbc::encode_program(program).map_err(AotArtifactError::Wire)
}

fn decode_program_payload(bytes: &[u8]) -> Result<Program, AotArtifactError> {
    crate::vmbc::decode_program(bytes).map_err(AotArtifactError::Wire)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], AotArtifactError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(AotArtifactError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(AotArtifactError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    fn read_exact_array<const N: usize>(&mut self) -> Result<[u8; N], AotArtifactError> {
        let bytes = self.read_exact(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn read_u8(&mut self) -> Result<u8, AotArtifactError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, AotArtifactError> {
        Ok(u16::from_le_bytes(self.read_exact_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, AotArtifactError> {
        Ok(u32::from_le_bytes(self.read_exact_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, AotArtifactError> {
        Ok(u64::from_le_bytes(self.read_exact_array::<8>()?))
    }

    fn read_string(&mut self) -> Result<String, AotArtifactError> {
        let len = self.read_u32()? as usize;
        String::from_utf8(self.read_exact(len)?.to_vec()).map_err(|_| AotArtifactError::InvalidUtf8)
    }

    fn is_at_end(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_api::{
        HostApiBuilder, HostFunctionSchema, HostImportSchema, HostParamSchema, HostTypeSchema,
    };
    use crate::{
        BytecodeBuilder, CallOutcome, CallReturn, HostFunctionRegistry, Program, Value, ValueType,
        VmResult, VmStatus,
    };

    fn aot_overloaded_int(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(11))))
    }

    fn aot_overloaded_string(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::string(
            "string",
        ))))
    }

    #[cfg(not(feature = "cranelift-jit"))]
    #[test]
    fn artifact_disabled_backend_rejects_both_modes_without_installation() {
        let mut bc = BytecodeBuilder::new();
        bc.ret();
        let program = Program::new(Vec::new(), bc.finish());
        // Process-local storage is permitted; these fixture bytes are never run.
        let mut native = CompiledProgram::from_code(vec![0], vec![0]).unwrap();
        for boundary in [false, true] {
            native.interpreter_boundary_only = boundary;
            let bytes = encode_artifact(
                &program,
                &native,
                super::super::super::compute_program_cache_key(&program),
            )
            .unwrap();
            let mut vm = Vm::new(program.clone());
            assert!(
                matches!(vm.load_aot_artifact(&bytes), Err(AotArtifactError::Vm(VmError::JitNative(message))) if message.contains("disabled"))
            );
            assert!(!vm.has_aot_program());
            assert!(!vm.engine.program_cache_key_ready);
            assert_eq!(vm.aot_exec_count(), 0);
            assert!(
                matches!(Vm::new_from_aot_artifact_with_jit_config(&bytes, JitConfig::default()), Err(AotArtifactError::Vm(VmError::JitNative(message))) if message.contains("disabled"))
            );
        }
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_same_process_callable_and_fuel_match_source_native() {
        let compiled =
            crate::compile_source_for_repl("pub fn add_one(value: int) -> int { value + 1 }")
                .unwrap();
        let program = compiled.program.with_local_count(compiled.locals);
        for fuel in [0, 1, 100000] {
            let mut source = Vm::new(program.clone());
            source.compile_aot().unwrap();
            let bytes = source.encode_aot_artifact().unwrap();
            let mut companion = Vm::new(program.clone());
            companion.load_aot_artifact(&bytes).unwrap();
            let standalone = Vm::new_from_aot_artifact_with_jit_config(
                &bytes,
                JitConfig {
                    enabled: false,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut observations = Vec::new();
            for mut vm in [source, companion, standalone] {
                assert_eq!(vm.aot_exec_count(), 0);
                vm.set_fuel(fuel);
                let root = vm.run().unwrap();
                let callable = vm.resolve_exported_callable("add_one").unwrap();
                let result = vm.invoke_callable(callable, &[Value::Int(41)]);
                observations.push((
                    root,
                    format!("{result:?}"),
                    vm.get_fuel(),
                    vm.aot_exec_count(),
                ));
                assert!(
                    !vm.engine
                        .aot_program
                        .as_ref()
                        .unwrap()
                        .interpreter_boundary_only
                );
                assert_eq!(
                    std::sync::Arc::strong_count(&vm.engine.aot_program.as_ref().unwrap().code),
                    1
                );
            }
            assert_eq!(observations[0], observations[1], "companion fuel={fuel}");
            assert_eq!(observations[0], observations[2], "standalone fuel={fuel}");
            if fuel == 100000 {
                assert_eq!(observations[0].1, "Ok(Int(42))");
                assert!(observations[0].3 > 0);
            }
        }
    }

    #[cfg(feature = "cranelift-jit")]
    fn yield_once(vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let calls = vm
            .host_context()
            .module_state::<usize>()
            .copied()
            .unwrap_or(0);
        vm.host_context().set_module_state(calls + 1);
        if calls == 0 {
            Ok(CallOutcome::Yield)
        } else {
            Ok(CallOutcome::Return(CallReturn::one(Value::Int(42))))
        }
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_native_host_yield_resumes_with_existing_replay_semantics() {
        let compiled = crate::compile_source_for_repl(
            "fn pause() -> int; fn body() -> int { pause() } body();",
        )
        .unwrap();
        let program = compiled.program.with_local_count(compiled.locals);
        let mut source = Vm::new(program.clone());
        source.compile_aot().unwrap();
        let bytes = source.encode_aot_artifact().unwrap();
        let mut companion = Vm::new(program);
        companion.load_aot_artifact(&bytes).unwrap();
        let standalone = Vm::new_from_aot_artifact_with_jit_config(
            &bytes,
            JitConfig {
                enabled: false,
                ..Default::default()
            },
        )
        .unwrap();
        let mut counts = Vec::new();
        for mut vm in [source, companion, standalone] {
            vm.bind_static_stack_function("pause", yield_once);
            vm.set_fuel(100000);
            assert_eq!(vm.run().unwrap(), VmStatus::Yielded);
            assert_eq!(vm.last_yield_reason(), Some(crate::VmYieldReason::Host));
            assert_eq!(vm.resume().unwrap(), VmStatus::Halted);
            assert_eq!(vm.stack(), &[Value::Int(42)]);
            assert_eq!(vm.host_context().module_state::<usize>(), Some(&2));
            counts.push(vm.aot_exec_count());
        }
        assert!(counts[0] > 0);
        assert_eq!(counts[0], counts[1]);
        assert_eq!(counts[0], counts[2]);
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_runtime_error_matches_native_source_without_claiming_interpreter_parity() {
        let compiled = crate::compile_source_for_repl(
            "let numerator = 1; let denominator = 0; numerator / denominator;",
        )
        .unwrap();
        let program = compiled.program.with_local_count(compiled.locals);
        let mut source = Vm::new(program.clone());
        source.compile_aot().unwrap();
        let bytes = source.encode_aot_artifact().unwrap();
        let mut companion = Vm::new(program.clone());
        companion.load_aot_artifact(&bytes).unwrap();
        let standalone =
            Vm::new_from_aot_artifact_with_jit_config(&bytes, JitConfig::default()).unwrap();
        let mut errors = Vec::new();
        for mut vm in [source, companion, standalone] {
            vm.set_fuel(100000);
            errors.push(vm.run().unwrap_err().to_string());
            assert_eq!(
                vm.aot_exec_count(),
                1,
                "native error status returned once; no successful guest result"
            );
        }
        assert_eq!(errors[0], errors[1]);
        assert_eq!(errors[0], errors[2]);
        let mut interpreted = Vm::new_with_jit_config(
            program,
            JitConfig {
                enabled: false,
                ..Default::default()
            },
        );
        interpreted.set_fuel(100000);
        assert!(matches!(interpreted.run(), Err(VmError::DivisionByZero)));
        eprintln!(
            "KNOWN native error={}; interpreter error={}",
            errors[0],
            VmError::DivisionByZero
        );
    }

    #[cfg(feature = "cranelift-jit")]
    fn native_fixture() -> (Program, CompiledProgram) {
        let compiled = crate::compile_source_for_repl("let value = 1 + 2; value;").unwrap();
        let program = compiled.program.with_local_count(compiled.locals);
        let native = compile_program(&program).unwrap();
        assert!(!native.interpreter_boundary_only);
        (program, native)
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_empty_native_payload_keeps_legacy_rejection_in_both_modes() {
        let (program, mut native) = native_fixture();
        native.code = std::sync::Arc::from([]);
        for boundary in [false, true] {
            native.interpreter_boundary_only = boundary;
            let bytes = encode_artifact(
                &program,
                &native,
                super::super::super::compute_program_cache_key(&program),
            )
            .unwrap();
            assert!(decode_artifact(&bytes, None).is_ok());
            let mut vm = Vm::new(program.clone());
            assert!(matches!(vm.load_aot_artifact(&bytes),
                Err(AotArtifactError::Vm(VmError::JitNative(message))) if message == "native trace produced empty machine code"));
            assert!(
                matches!(Vm::new_from_aot_artifact_with_jit_config(&bytes, JitConfig::default()),
                Err(AotArtifactError::Vm(VmError::JitNative(message))) if message == "native trace produced empty machine code")
            );
            assert!(!vm.has_aot_program());
            assert!(!vm.engine.program_cache_key_ready);
            assert_eq!(vm.aot_exec_count(), 0);
        }
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_rebuild_failure_preserves_engine_state_and_source_authority() {
        // This is the remaining unsupported dynamic Number addition boundary,
        // not a synthetic failure marker. Its valid Program survives wire decoding.
        let issued = crate::compile_source_with_flavor_and_options_with_capture_certification(
            "fn successor(n: number) -> number { n + 1 } successor(6);",
            crate::SourceFlavor::RustScript,
            crate::CompileSourceFileOptions::new(),
        )
        .unwrap();
        let mut vm = issued.into_vm().unwrap();
        assert!(matches!(
            compile_program(vm.program()),
            Err(VmError::JitNative(_))
        ));
        vm.set_fuel(100000);
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        let (_, native) = native_fixture();
        let bytes = encode_artifact(
            vm.program(),
            &native,
            super::super::super::compute_program_cache_key(vm.program()),
        )
        .unwrap();
        assert!(decode_artifact(&bytes, None).is_ok());
        // A previously installed real boundary engine must remain owned on error.
        vm.engine.aot_program = Some(compile_artifact_interpreter_boundary().unwrap());
        let entry = vm.engine.aot_program.as_ref().unwrap().entry as usize;
        let program = vm.program.clone();
        let ip = vm.instance.ip;
        let stack = vm.stack().to_vec();
        let locals = vm.instance.locals.clone();
        let cert = vm.capture_certification_stats().unwrap();
        let cache = (
            vm.engine.program_cache_key,
            vm.engine.program_cache_key_ready,
        );
        let native_count = vm.aot_exec_count();
        let fuel = vm.fuel_checkpoint();
        let mut incompatible = bytes.clone();
        incompatible[10] ^= 0x40;
        assert!(matches!(
            vm.load_aot_artifact(&incompatible),
            Err(AotArtifactError::IncompatibleRuntime {
                field: "pointer width",
                ..
            })
        ));
        assert_eq!(vm.capture_certification_stats().unwrap(), cert);
        assert_eq!(
            vm.engine.aot_program.as_ref().unwrap().entry as usize,
            entry
        );
        let error = vm.load_aot_artifact(&bytes).unwrap_err();
        assert!(
            matches!(error, AotArtifactError::Vm(VmError::JitNative(_))),
            "{error}"
        );
        assert!(std::sync::Arc::ptr_eq(&vm.program, &program));
        assert_eq!(
            vm.engine.aot_program.as_ref().unwrap().entry as usize,
            entry
        );
        assert_eq!(vm.instance.ip, ip);
        assert_eq!(vm.stack(), stack);
        assert_eq!(vm.instance.locals, locals);
        assert_eq!(vm.capture_certification_stats().unwrap(), cert);
        assert_eq!(
            (
                vm.engine.program_cache_key,
                vm.engine.program_cache_key_ready
            ),
            cache
        );
        assert_eq!(vm.aot_exec_count(), native_count);
        assert_eq!(vm.fuel_checkpoint(), fuel);
        assert!(matches!(
            Vm::new_from_aot_artifact_with_jit_config(&bytes, JitConfig::default()),
            Err(AotArtifactError::Vm(VmError::JitNative(_)))
        ));
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_explicit_boundary_regenerates_ignoring_persisted_instructions() {
        let (program, mut boundary) = native_fixture();
        // Legacy flag1 can carry SSA bytes, but its declared boundary semantics
        // must be regenerated. These same-process bytes are safe as a control.
        let persisted_code = boundary.code_bytes().to_vec();
        boundary.interpreter_boundary_only = true;
        let bytes = encode_artifact(
            &program,
            &boundary,
            super::super::super::compute_program_cache_key(&program),
        )
        .unwrap();
        let mut companion = Vm::new(program);
        companion.load_aot_artifact(&bytes).unwrap();
        let standalone = Vm::new_from_aot_artifact_with_jit_config(
            &bytes,
            JitConfig {
                enabled: false,
                ..Default::default()
            },
        )
        .unwrap();
        for mut vm in [companion, standalone] {
            assert!(
                vm.engine
                    .aot_program
                    .as_ref()
                    .unwrap()
                    .interpreter_boundary_only
            );
            assert_ne!(
                vm.engine.aot_program.as_ref().unwrap().code_bytes(),
                persisted_code
            );
            vm.set_fuel(100000);
            assert_eq!(vm.run().unwrap(), VmStatus::Halted);
            assert_eq!(vm.stack(), &[Value::Int(3)]);
            assert_eq!(
                vm.aot_exec_count(),
                1,
                "only the regenerated boundary stub returns natively"
            );
        }
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_flag_zero_rejects_real_oversized_boundary_rebuild() {
        let (mut program, native) = native_fixture();
        // The real compiler budget includes frame locals in block parameters.
        // Exercise its actual boundary result without replacing compiler logic.
        program.local_count = 1_000_001;
        let rebuilt = compile_program(&program).unwrap();
        assert!(rebuilt.interpreter_boundary_only);
        let bytes = encode_artifact(
            &program,
            &native,
            super::super::super::compute_program_cache_key(&program),
        )
        .unwrap();
        assert!(
            matches!(Vm::new_from_aot_artifact_with_jit_config(&bytes, JitConfig::default()),
            Err(AotArtifactError::Vm(VmError::JitNative(message))) if message.contains("interpreter boundary"))
        );
        let mut vm = Vm::new(program);
        assert!(matches!(
            vm.load_aot_artifact(&bytes),
            Err(AotArtifactError::Vm(VmError::JitNative(_)))
        ));
        assert!(!vm.has_aot_program());
        assert!(!vm.engine.program_cache_key_ready);
        assert_eq!(vm.aot_exec_count(), 0);
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_validation_precedes_rebuild_and_preserves_uninitialized_cache() {
        let (program, native) = native_fixture();
        let bytes = encode_artifact(
            &program,
            &native,
            super::super::super::compute_program_cache_key(&program),
        )
        .unwrap();
        let mut cursor = Cursor::new(&bytes);
        cursor.read_exact(11).unwrap();
        let mut runtime_offsets = vec![(10, "pointer width")];
        for field in ["arch", "os", "backend"] {
            let len = cursor.read_u32().unwrap() as usize;
            runtime_offsets.push((cursor.offset, field));
            cursor.read_exact(len).unwrap();
        }
        for field in [
            "vm ip offset",
            "native helper offset",
            "interrupt helper offset",
        ] {
            runtime_offsets.push((cursor.offset, field));
            cursor.read_u32().unwrap();
        }
        let integrity_offset = cursor.offset;
        cursor.read_u64().unwrap();
        cursor.read_u32().unwrap();
        let program_length_offset = cursor.offset;
        let len = cursor.read_u32().unwrap() as usize;
        cursor.read_exact(len).unwrap();
        let resumes = cursor.read_u32().unwrap() as usize;
        cursor.read_exact(resumes * 4).unwrap();
        let code_length_offset = cursor.offset;
        let mut vm = Vm::new(program);
        for (offset, field) in runtime_offsets {
            let mut bad = bytes.clone();
            bad[offset] ^= 0x40;
            assert!(
                matches!(vm.load_aot_artifact(&bad), Err(AotArtifactError::IncompatibleRuntime { field: found, .. }) if found == field)
            );
            assert!(matches!(
                Vm::new_from_aot_artifact_with_jit_config(&bad, JitConfig::default()),
                Err(AotArtifactError::IncompatibleRuntime { .. })
            ));
        }
        let mut invalid_flags = bytes.clone();
        invalid_flags[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert!(matches!(
            vm.load_aot_artifact(&invalid_flags),
            Err(AotArtifactError::UnsupportedFlags(2))
        ));
        let mut integrity = bytes.clone();
        integrity[integrity_offset] ^= 1;
        assert!(matches!(
            vm.load_aot_artifact(&integrity),
            Err(AotArtifactError::EmbeddedProgramHashMismatch { .. })
        ));
        let mut missing = bytes.clone();
        missing[program_length_offset..program_length_offset + 4].fill(0);
        assert!(matches!(
            vm.load_aot_artifact(&missing),
            Err(AotArtifactError::Wire(_))
        ));
        let mut oversized = bytes.clone();
        oversized[code_length_offset..code_length_offset + 4]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            vm.load_aot_artifact(&oversized),
            Err(AotArtifactError::UnexpectedEof)
        ));
        for end in 0..bytes.len() {
            assert!(
                vm.load_aot_artifact(&bytes[..end]).is_err(),
                "truncation {end}"
            );
        }
        assert!(!vm.engine.program_cache_key_ready);
        assert!(!vm.has_aot_program());
        assert_eq!(vm.aot_exec_count(), 0);
        assert!(vm.stack().is_empty());
        assert_eq!(vm.instance.ip, 0);
    }

    #[test]
    fn aot_artifact_preserves_interpreter_boundary_mode() {
        let mut bc = BytecodeBuilder::new();
        bc.ret();
        let mut vm = Vm::new(Program::new(Vec::new(), bc.finish()));
        vm.compile_aot().expect("aot compile should succeed");
        vm.engine
            .aot_program
            .as_mut()
            .expect("compiled program")
            .interpreter_boundary_only = true;

        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");
        assert_eq!(
            u16::from_le_bytes([encoded[8], encoded[9]]),
            FLAG_INTERPRETER_BOUNDARY_ONLY
        );
        let standalone = Vm::new_from_aot_artifact_with_jit_config(
            &encoded,
            JitConfig {
                enabled: false,
                ..JitConfig::default()
            },
        )
        .expect("boundary artifact should load");
        assert!(
            standalone
                .engine
                .aot_program
                .as_ref()
                .expect("loaded aot program")
                .interpreter_boundary_only
        );
    }

    #[test]
    fn aot_artifact_decode_rejects_invalid_magic_and_trailing_bytes() {
        let mut bc = BytecodeBuilder::new();
        bc.ret();
        let mut vm = Vm::new(Program::new(vec![Value::Int(1)], bc.finish()));
        vm.compile_aot().expect("aot compile should succeed");

        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");

        let mut bad_magic = encoded.clone();
        bad_magic[0..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            vm.load_aot_artifact(&bad_magic),
            Err(AotArtifactError::InvalidMagic(_))
        ));

        let mut trailing = encoded;
        trailing.extend_from_slice(&[1, 2, 3]);
        assert!(matches!(
            vm.load_aot_artifact(&trailing),
            Err(AotArtifactError::TrailingBytes)
        ));
    }

    #[test]
    fn aot_artifact_decode_rejects_incompatible_program_hash() {
        let mut first_bc = BytecodeBuilder::new();
        first_bc.ldc(0);
        first_bc.ret();
        let mut first_vm = Vm::new(Program::new(vec![Value::Int(1)], first_bc.finish()));
        first_vm
            .compile_aot()
            .expect("first aot compile should succeed");
        let encoded = first_vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");

        let mut second_bc = BytecodeBuilder::new();
        second_bc.ldc(0);
        second_bc.ldc(1);
        second_bc.add();
        second_bc.ret();
        let mut second_vm = Vm::new(Program::new(
            vec![Value::Int(1), Value::Int(2)],
            second_bc.finish(),
        ));

        assert!(matches!(
            second_vm.load_aot_artifact(&encoded),
            Err(AotArtifactError::IncompatibleProgramHash { .. })
        ));
    }

    #[test]
    fn aot_artifact_roundtrips_into_standalone_vm() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(3);
        bc.ldloc(3);
        bc.ret();
        let program = Program::with_imports_and_debug(
            vec![Value::array(vec![
                Value::Int(1),
                Value::map(vec![(Value::string("k"), Value::Bool(true))]),
            ])],
            bc.finish(),
            vec![crate::bytecode::HostImport {
                name: "print".to_string(),
                arity: 1,
                return_type: ValueType::Unknown,
            }],
            None,
        )
        .with_local_count(8);
        let mut schema_builder = HostApiBuilder::new();
        let schema_function = HostFunctionSchema::with_return(
            "print",
            vec![HostParamSchema::value(
                "value",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Int)),
            )],
            HostTypeSchema::Unknown,
        );
        schema_builder.function(schema_function.clone());
        let schema_catalog = schema_builder.build().expect("schema catalog should build");
        let schema =
            crate::host_api::HostImportSchema::from_function(&schema_catalog, &schema_function);
        let program = program
            .with_host_import_schemas(vec![schema.clone()])
            .expect("schema metadata should align");
        let mut vm = Vm::new(program.clone());
        vm.compile_aot().expect("aot compile should succeed");

        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");
        let standalone = Vm::new_from_aot_artifact_with_jit_config(&encoded, JitConfig::default())
            .expect("standalone artifact should load");

        assert_eq!(standalone.program().local_count, 8);
        assert_eq!(standalone.program().constants, program.constants);
        assert_eq!(standalone.program().imports, program.imports);
        assert_eq!(
            standalone.program().host_import_schemas(),
            program.host_import_schemas()
        );
        assert_eq!(standalone.program().type_map, program.type_map);
        assert!(
            standalone.has_aot_program(),
            "standalone vm should install aot"
        );
    }

    #[test]
    fn aot_loaded_program_binds_full_schema_overloads() {
        let mut catalog_builder = HostApiBuilder::new();
        let int_function = HostFunctionSchema::with_return(
            "aot::overloaded",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        );
        let string_function = HostFunctionSchema::with_return(
            "aot::overloaded",
            vec![HostParamSchema::value("value", HostTypeSchema::String)],
            HostTypeSchema::String,
        );
        catalog_builder.function(int_function.clone());
        catalog_builder.function(string_function.clone());
        let catalog = catalog_builder.build().expect("overload catalog");
        let int_schema = HostImportSchema::from_function(&catalog, &int_function);
        let string_schema = HostImportSchema::from_function(&catalog, &string_function);

        let mut bytecode = BytecodeBuilder::new();
        bytecode.ldc(0);
        bytecode.call(0, 1);
        bytecode.ldc(1);
        bytecode.call(1, 1);
        bytecode.ret();
        let program = Program::with_imports_and_debug(
            vec![Value::Int(1), Value::string("x")],
            bytecode.finish(),
            vec![
                crate::bytecode::HostImport {
                    name: "aot::overloaded".to_string(),
                    arity: 1,
                    return_type: ValueType::Int,
                },
                crate::bytecode::HostImport {
                    name: "aot::overloaded".to_string(),
                    arity: 1,
                    return_type: ValueType::String,
                },
            ],
            None,
        )
        .with_host_import_schemas(vec![int_schema.clone(), string_schema.clone()])
        .expect("schema alignment");
        let mut compiler_vm = Vm::new(program);
        compiler_vm
            .compile_aot()
            .expect("aot compile should succeed");
        let artifact = compiler_vm
            .encode_aot_artifact()
            .expect("aot artifact should encode");

        let mut loaded = Vm::new_from_aot_artifact_with_jit_config(&artifact, JitConfig::default())
            .expect("aot artifact should load");
        let mut registry = HostFunctionRegistry::empty();
        registry
            .register_catalog_static(int_schema, aot_overloaded_int)
            .expect("integer overload registration");
        registry
            .register_catalog_static(string_schema, aot_overloaded_string)
            .expect("string overload registration");
        registry
            .bind_vm_cached(&mut loaded)
            .expect("loaded aot program should bind both overloads");
        assert_eq!(
            loaded.run().expect("aot overloads should execute"),
            VmStatus::Halted
        );
        assert_eq!(loaded.stack(), &[Value::Int(11), Value::string("string")]);

        let mut wrong_schema = HostImportSchema::from_function(&catalog, &int_function);
        wrong_schema.params[0].schema = HostTypeSchema::String;
        let mut wrong_registry = HostFunctionRegistry::empty();
        wrong_registry
            .register_catalog_static(wrong_schema, aot_overloaded_int)
            .expect("mismatched registration");
        let mut rejected =
            Vm::new_from_aot_artifact_with_jit_config(&artifact, JitConfig::default())
                .expect("aot artifact should load for mismatch check");
        assert!(wrong_registry.bind_vm_cached(&mut rejected).is_err());
    }

    #[test]
    fn aot_artifact_v8_roundtrips_callable_metadata_and_rejects_old_revisions() {
        let compiled =
            crate::compile_source_for_repl("pub fn add_one(value: int) -> int { value + 1 }")
                .expect("callable program should compile");
        let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
        vm.compile_aot().expect("aot compile should succeed");
        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");
        assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 8);
        assert_eq!(u16::from_le_bytes([encoded[6], encoded[7]]), 8);

        let mut old_format = encoded.clone();
        old_format[4..6].copy_from_slice(&7u16.to_le_bytes());
        assert!(matches!(
            Vm::new_from_aot_artifact_with_jit_config(&old_format, JitConfig::default()),
            Err(AotArtifactError::UnsupportedVersion(7))
        ));
        let mut old_abi = encoded.clone();
        old_abi[6..8].copy_from_slice(&7u16.to_le_bytes());
        assert!(matches!(
            Vm::new_from_aot_artifact_with_jit_config(&old_abi, JitConfig::default()),
            Err(AotArtifactError::UnsupportedAbiVersion(7))
        ));

        let mut standalone =
            Vm::new_from_aot_artifact_with_jit_config(&encoded, JitConfig::default())
                .expect("standalone callable artifact should load");
        assert_eq!(
            standalone.run().expect("root should halt"),
            VmStatus::Halted
        );
        let callable = standalone
            .resolve_exported_callable("add_one")
            .expect("export metadata should survive artifact roundtrip");
        assert_eq!(
            standalone
                .invoke_callable(callable, &[Value::Int(41)])
                .expect("artifact callable should execute"),
            Value::Int(42)
        );
    }

    #[test]
    fn aot_artifact_v8_roundtrips_direct_call_script_program() {
        // A real direct-only program: the root body calls a named function
        // through `CallScript` and the callee is a native AOT body, so the
        // artifact must embed both the callable metadata and the executable
        // AOT code for the direct path.
        let source = r#"
            fn bump(value: int) -> int { value + 1 }
            let mut i = 0;
            let mut total = 0;
            while i < 16 {
                total = bump(total);
                i = i + 1;
            }
            total;
        "#;
        let compiled =
            crate::compile_source_for_repl(source).expect("direct call program should compile");
        assert!(
            compiled
                .program
                .code
                .contains(&(crate::OpCode::CallScript as u8)),
            "expected the root body to embed CallScript bytecode"
        );

        let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
        vm.compile_aot().expect("aot compile should succeed");
        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");
        assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 8);
        assert_eq!(u16::from_le_bytes([encoded[6], encoded[7]]), 8);

        let mut old_format = encoded.clone();
        old_format[4..6].copy_from_slice(&7u16.to_le_bytes());
        assert!(matches!(
            Vm::new_from_aot_artifact_with_jit_config(&old_format, JitConfig::default()),
            Err(AotArtifactError::UnsupportedVersion(7))
        ));

        let mut standalone =
            Vm::new_from_aot_artifact_with_jit_config(&encoded, JitConfig::default())
                .expect("standalone direct artifact should load");
        assert!(
            standalone.has_aot_program(),
            "standalone vm should install aot"
        );
        assert_eq!(
            standalone.run().expect("direct call program should run"),
            VmStatus::Halted
        );
        assert_eq!(standalone.stack(), &[Value::Int(16)]);
        assert!(
            standalone.aot_exec_count() > 0,
            "standalone artifact should execute through the native AOT path: {}",
            standalone.dump_aot_info()
        );
        assert!(
            !standalone.dump_aot_info().contains("interpreter-boundary"),
            "standalone artifact should not fall back to the interpreter: {}",
            standalone.dump_aot_info()
        );
    }

    #[cfg(feature = "cranelift-jit")]
    #[test]
    fn artifact_load_rejects_cleanup_only_metadata_change() {
        let compiled = crate::compile_source("fn id(x: int) -> int { x }\nid(1);")
            .expect("cleanup binding fixture should compile");
        let program = compiled.program.with_local_count(compiled.locals);
        let site = script_ret_site(&program);
        let mut with_cleanup = program.clone();
        with_cleanup
            .ret_cleanups
            .push(crate::bytecode::RetCleanupPlan {
                site,
                locals: vec![crate::bytecode::RetCleanupLocal {
                    slot: 0,
                    schema: crate::compiler::TypeSchema::Resource(
                        crate::host_api::ResourceTypeKey::new("pr24.closure").unwrap(),
                    ),
                }],
            });
        let mut other_key = with_cleanup.clone();
        other_key.ret_cleanups[0].locals[0].schema = crate::compiler::TypeSchema::Resource(
            crate::host_api::ResourceTypeKey::new("pr24.blocked").unwrap(),
        );
        let original_program =
            crate::vmbc::encode_program(&with_cleanup).expect("cleanup program encodes");
        let other_program =
            crate::vmbc::encode_program(&other_key).expect("same-length key encodes");
        assert_eq!(original_program.len(), other_program.len());
        assert_ne!(original_program, other_program);

        let mut vm = Vm::new(with_cleanup.clone());
        vm.compile_aot()
            .expect("cleanup fixture should compile to aot");
        let bytes = vm.encode_aot_artifact().expect("artifact encodes");
        let path = std::env::temp_dir().join("r2-a02-cleanup-only-artifact.bin");
        vm.save_aot_artifact_to_file(&path).expect("artifact saves");

        let mut mismatched = Vm::new(other_key.clone());
        let incompatible = mismatched
            .load_aot_artifact_from_file(&path)
            .expect_err("cleanup-only program mismatch must reject");
        assert!(
            matches!(
                incompatible,
                AotArtifactError::IncompatibleProgramHash { .. }
            ),
            "{incompatible:?}"
        );
        assert!(!mismatched.has_aot_program());

        let pos = bytes
            .windows(original_program.len())
            .position(|window| window == original_program)
            .expect("artifact embeds the program bytes");
        let mut tampered = bytes.clone();
        tampered[pos..pos + other_program.len()].copy_from_slice(&other_program);
        let mut intact = Vm::new(with_cleanup.clone());
        let hash_error = intact
            .load_aot_artifact(&tampered)
            .expect_err("tampered cleanup bytes must fail the embedded hash");
        assert!(
            matches!(
                hash_error,
                AotArtifactError::EmbeddedProgramHashMismatch { .. }
            ),
            "{hash_error:?}"
        );
        assert!(!intact.has_aot_program());
        assert_eq!(intact.aot_exec_count(), 0);

        let mut positive = Vm::new(with_cleanup);
        positive
            .load_aot_artifact_from_file(&path)
            .expect("matching cleanup metadata must load");
        assert!(positive.has_aot_program());
        let _ = std::fs::remove_file(&path);
    }

    fn script_ret_site(program: &Program) -> u32 {
        let mut ip = 0usize;
        while ip < program.code.len() {
            let opcode = crate::vm::OpCode::try_from(program.code[ip]).expect("opcode");
            if opcode == crate::vm::OpCode::Ret {
                let site = u32::try_from(ip).unwrap();
                let in_frame = program.function_regions.iter().any(|region| {
                    site >= region.start_ip
                        && site < region.end_ip
                        && region.prototype_id.is_some_and(|id| {
                            program
                                .callable_prototypes
                                .get(id as usize)
                                .is_some_and(|prototype| prototype.frame_local_count > 0)
                        })
                });
                if in_frame {
                    return site;
                }
            }
            ip = ip
                .checked_add(1 + opcode.operand_len())
                .expect("instruction length");
        }
        panic!("fixture has no script Ret inside a nonempty frame");
    }
}
