use std::io;
use std::path::Path;

use crate::bytecode::Program;
use crate::vm::native::{
    helper_entry_offset, interrupt_helper_entry_offset, selected_codegen_backend,
};
use crate::vm::{Vm, VmError};

use super::super::jit::JitConfig;
use super::compile::CompiledProgram;

const MAGIC: [u8; 4] = *b"PAT\0";
const VERSION: u16 = 6;
const ABI_VERSION: u16 = 5;
const FLAGS: u16 = 0;

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
        if self.aot_program.is_none() {
            self.compile_aot()?;
        }
        let program_hash = self.ensure_program_cache_key();
        let aot_program = self
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
        let expected_program_hash = self.ensure_program_cache_key();
        let decoded = decode_artifact(bytes, Some(expected_program_hash))?;
        self.aot_program = Some(CompiledProgram::from_code(
            decoded.code,
            decoded.resume_ips,
        )?);
        self.aot_exec_count = 0;
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
        let mut vm = Vm::new_with_jit_config(decoded.program, jit_config);
        vm.aot_program = Some(CompiledProgram::from_code(
            decoded.code,
            decoded.resume_ips,
        )?);
        vm.aot_exec_count = 0;
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
    code: Vec<u8>,
    resume_ips: Vec<usize>,
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
    out.extend_from_slice(&FLAGS.to_le_bytes());
    out.push(u8::try_from(std::mem::size_of::<usize>()).expect("pointer width fits u8"));

    write_string("arch", std::env::consts::ARCH, &mut out)?;
    write_string("os", std::env::consts::OS, &mut out)?;
    write_string("backend", selected_codegen_backend(), &mut out)?;

    write_u32("vm ip offset", std::mem::offset_of!(Vm, ip), &mut out)?;
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
    if flags != FLAGS {
        return Err(AotArtifactError::UnsupportedFlags(flags));
    }

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
        std::mem::offset_of!(Vm, ip).to_string(),
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
        code,
        resume_ips,
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
    use crate::{BytecodeBuilder, Program, Value, ValueType, VmStatus};

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
        assert_eq!(standalone.program().type_map, program.type_map);
        assert!(
            standalone.has_aot_program(),
            "standalone vm should install aot"
        );
    }

    #[test]
    fn aot_artifact_v6_roundtrips_callable_metadata_and_rejects_old_revisions() {
        let compiled =
            crate::compile_source_for_repl("pub fn add_one(value: int) -> int { value + 1 }")
                .expect("callable program should compile");
        let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
        vm.compile_aot().expect("aot compile should succeed");
        let encoded = vm
            .encode_aot_artifact()
            .expect("artifact encode should succeed");
        assert_eq!(u16::from_le_bytes([encoded[4], encoded[5]]), 6);
        assert_eq!(u16::from_le_bytes([encoded[6], encoded[7]]), 5);

        let mut old_format = encoded.clone();
        old_format[4..6].copy_from_slice(&5u16.to_le_bytes());
        assert!(matches!(
            Vm::new_from_aot_artifact_with_jit_config(&old_format, JitConfig::default()),
            Err(AotArtifactError::UnsupportedVersion(5))
        ));
        let mut old_abi = encoded.clone();
        old_abi[6..8].copy_from_slice(&4u16.to_le_bytes());
        assert!(matches!(
            Vm::new_from_aot_artifact_with_jit_config(&old_abi, JitConfig::default()),
            Err(AotArtifactError::UnsupportedAbiVersion(4))
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
}
