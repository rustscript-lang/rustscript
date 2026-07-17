use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::vm::{Program, Value, Vm, VmExecutionFrameSnapshot, VmFrameContinuation, VmStatus};

#[derive(Clone, Debug, PartialEq)]
pub struct VmRecordingFrame {
    pub ip: usize,
    pub call_depth: usize,
    pub execution_frames: Vec<VmExecutionFrameSnapshot>,
    pub stack: Vec<Value>,
    pub locals: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct VmRecording {
    pub program: Program,
    pub frames: Vec<VmRecordingFrame>,
    pub terminal_status: Option<VmStatus>,
}

#[derive(Clone, Debug, Default)]
pub struct VmRecordingReplayState {
    pub cursor: usize,
    pub offset_breakpoints: HashSet<usize>,
    pub line_breakpoints: HashSet<u32>,
}

#[derive(Clone, Debug)]
pub struct VmRecordingReplayResponse {
    pub output: String,
    pub current_line: Option<u32>,
    pub at_end: bool,
    pub exited: bool,
}

#[derive(Debug)]
pub enum VmRecordingError {
    Io(io::Error),
    Wire(crate::vmbc::WireError),
    InvalidFormat(&'static str),
    Message(String),
}

pub(super) struct VmRecordingBuilder {
    recording: VmRecording,
}

impl VmRecordingFrame {
    pub(super) fn from_vm(vm: &Vm) -> Self {
        Self {
            ip: vm.ip(),
            call_depth: vm.call_depth(),
            execution_frames: vm.execution_frames(),
            stack: vm.stack().to_vec(),
            locals: vm.locals().to_vec(),
        }
    }
}

impl VmRecordingBuilder {
    pub(super) fn new(program: Program) -> Self {
        Self {
            recording: VmRecording {
                program,
                frames: Vec::new(),
                terminal_status: None,
            },
        }
    }

    pub(super) fn record_state(&mut self, vm: &Vm) {
        let frame = VmRecordingFrame::from_vm(vm);
        if self.recording.frames.last() == Some(&frame) {
            return;
        }
        self.recording.frames.push(frame);
    }

    pub(super) fn on_terminal_status(&mut self, vm: &Vm, status: VmStatus) {
        self.record_state(vm);
        self.recording.terminal_status = Some(status);
    }

    pub(super) fn finish(self) -> VmRecording {
        self.recording
    }
}

impl VmRecording {
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), VmRecordingError> {
        let bytes = self.encode()?;
        std::fs::write(path, bytes).map_err(VmRecordingError::Io)
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, VmRecordingError> {
        let bytes = std::fs::read(path).map_err(VmRecordingError::Io)?;
        Self::decode(&bytes)
    }

    pub fn encode(&self) -> Result<Vec<u8>, VmRecordingError> {
        const MAGIC: [u8; 4] = *b"PDRC";
        const VERSION: u16 = 3;

        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());

        let program_bytes =
            crate::vmbc::encode_program(&self.program).map_err(VmRecordingError::Wire)?;
        write_u32_len(program_bytes.len(), &mut out)?;
        out.extend_from_slice(&program_bytes);

        let status_tag = match self.terminal_status {
            Some(VmStatus::Halted) => 1u8,
            Some(VmStatus::Yielded) => 2u8,
            Some(VmStatus::Waiting(_)) => 3u8,
            None => 0u8,
        };
        out.push(status_tag);
        if let Some(VmStatus::Waiting(op_id)) = self.terminal_status {
            out.extend_from_slice(&op_id.to_le_bytes());
        }

        write_u32_len(self.frames.len(), &mut out)?;
        let mut value_context = ValueEncodeContext::default();
        for frame in &self.frames {
            write_u32_from_usize(frame.ip, &mut out)?;
            write_u32_from_usize(frame.call_depth, &mut out)?;
            write_u32_len(frame.execution_frames.len(), &mut out)?;
            for execution_frame in &frame.execution_frames {
                match execution_frame.continuation {
                    VmFrameContinuation::Halt => out.push(0),
                    VmFrameContinuation::ResumeBytecode { return_ip } => {
                        out.push(1);
                        write_u32_from_usize(return_ip, &mut out)?;
                    }
                    VmFrameContinuation::ReturnToHost => out.push(2),
                }
                write_u32_from_usize(execution_frame.operand_stack_base, &mut out)?;
                write_u32_from_usize(execution_frame.local_base, &mut out)?;
                write_u32_from_usize(execution_frame.local_count, &mut out)?;
                match execution_frame.prototype_id {
                    Some(prototype_id) => {
                        out.push(1);
                        out.extend_from_slice(&prototype_id.to_le_bytes());
                    }
                    None => out.push(0),
                }
            }

            write_u32_len(frame.stack.len(), &mut out)?;
            for value in &frame.stack {
                encode_value(value, &mut out, &mut value_context)?;
            }

            write_u32_len(frame.locals.len(), &mut out)?;
            for value in &frame.locals {
                encode_value(value, &mut out, &mut value_context)?;
            }
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, VmRecordingError> {
        const MAGIC: [u8; 4] = *b"PDRC";
        const VERSION: u16 = 3;

        let mut cursor = RecordingCursor::new(bytes);

        let magic = cursor.read_exact(4)?;
        if magic != MAGIC {
            return Err(VmRecordingError::InvalidFormat("invalid recording magic"));
        }

        let version = cursor.read_u16()?;
        if version != VERSION {
            return Err(VmRecordingError::Message(format!(
                "unsupported recording version {version}"
            )));
        }

        let program_len = cursor.read_u32()? as usize;
        let program_bytes = cursor.read_exact(program_len)?;
        let program = crate::vmbc::decode_program(program_bytes).map_err(VmRecordingError::Wire)?;

        let terminal_status = match cursor.read_u8()? {
            0 => None,
            1 => Some(VmStatus::Halted),
            2 => Some(VmStatus::Yielded),
            3 if version >= VERSION => {
                let op_id = cursor.read_u64()?;
                Some(VmStatus::Waiting(op_id))
            }
            _ => {
                return Err(VmRecordingError::InvalidFormat(
                    "invalid terminal status tag",
                ));
            }
        };

        let frame_count = cursor.read_u32()? as usize;
        let mut frames = Vec::with_capacity(frame_count);
        let mut value_context = ValueDecodeContext::default();
        for _ in 0..frame_count {
            let ip = cursor.read_u32()? as usize;
            let call_depth = cursor.read_u32()? as usize;
            let execution_frame_count = cursor.read_u32()? as usize;
            let mut execution_frames = Vec::with_capacity(execution_frame_count);
            for _ in 0..execution_frame_count {
                let continuation = match cursor.read_u8()? {
                    0 => VmFrameContinuation::Halt,
                    1 => VmFrameContinuation::ResumeBytecode {
                        return_ip: cursor.read_u32()? as usize,
                    },
                    2 => VmFrameContinuation::ReturnToHost,
                    _ => {
                        return Err(VmRecordingError::InvalidFormat(
                            "invalid frame continuation tag",
                        ));
                    }
                };
                let operand_stack_base = cursor.read_u32()? as usize;
                let local_base = cursor.read_u32()? as usize;
                let local_count = cursor.read_u32()? as usize;
                let prototype_id = match cursor.read_u8()? {
                    0 => None,
                    1 => Some(cursor.read_u32()?),
                    _ => {
                        return Err(VmRecordingError::InvalidFormat(
                            "invalid frame prototype tag",
                        ));
                    }
                };
                execution_frames.push(VmExecutionFrameSnapshot {
                    continuation,
                    operand_stack_base,
                    local_base,
                    local_count,
                    prototype_id,
                });
            }

            let stack_len = cursor.read_u32()? as usize;
            let mut stack = Vec::with_capacity(stack_len);
            for _ in 0..stack_len {
                stack.push(decode_value(&mut cursor, &mut value_context)?);
            }

            let locals_len = cursor.read_u32()? as usize;
            let mut locals = Vec::with_capacity(locals_len);
            for _ in 0..locals_len {
                locals.push(decode_value(&mut cursor, &mut value_context)?);
            }

            frames.push(VmRecordingFrame {
                ip,
                call_depth,
                execution_frames,
                stack,
                locals,
            });
        }

        if !cursor.is_at_end() {
            return Err(VmRecordingError::InvalidFormat(
                "trailing bytes in recording payload",
            ));
        }

        Ok(Self {
            program,
            frames,
            terminal_status,
        })
    }
}

impl std::fmt::Display for VmRecordingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmRecordingError::Io(err) => write!(f, "{err}"),
            VmRecordingError::Wire(err) => write!(f, "{err}"),
            VmRecordingError::InvalidFormat(message) => write!(f, "{message}"),
            VmRecordingError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for VmRecordingError {}

fn write_u32_len(len: usize, out: &mut Vec<u8>) -> Result<(), VmRecordingError> {
    let value = u32::try_from(len)
        .map_err(|_| VmRecordingError::Message(format!("length too large: {len}")))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u32_from_usize(value: usize, out: &mut Vec<u8>) -> Result<(), VmRecordingError> {
    let value = u32::try_from(value)
        .map_err(|_| VmRecordingError::Message(format!("value too large: {value}")))?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

#[derive(Default)]
struct ValueEncodeContext {
    environment_ids: HashMap<usize, u32>,
    next_environment_id: u32,
}

#[derive(Default)]
struct ValueDecodeContext {
    environments: HashMap<u32, Arc<crate::CallableEnvironment>>,
}

fn encode_value(
    value: &Value,
    out: &mut Vec<u8>,
    context: &mut ValueEncodeContext,
) -> Result<(), VmRecordingError> {
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
        Value::Bool(false) => out.push(3),
        Value::Bool(true) => out.push(4),
        Value::String(value) => {
            out.push(5);
            write_u32_len(value.len(), out)?;
            out.extend_from_slice(value.as_bytes());
        }
        Value::Bytes(value) => {
            out.push(6);
            write_u32_len(value.len(), out)?;
            out.extend_from_slice(value.as_slice());
        }
        Value::Array(values) => {
            out.push(7);
            write_u32_len(values.len(), out)?;
            for value in values.iter() {
                encode_value(value, out, context)?;
            }
        }
        Value::Map(entries) => {
            out.push(8);
            write_u32_len(entries.len(), out)?;
            for (key, value) in entries.iter() {
                encode_value(key, out, context)?;
                encode_value(value, out, context)?;
            }
        }
        Value::Callable(callable) => {
            out.push(9);
            out.extend_from_slice(&callable.program_instance.to_le_bytes());
            out.extend_from_slice(&callable.prototype_id.to_le_bytes());
            out.push(match callable.kind {
                crate::CallableKind::FunctionItem => 0,
                crate::CallableKind::Closure => 1,
                crate::CallableKind::HostFunction => 2,
            });
            if let Some(env) = &callable.env {
                let environment_key = Arc::as_ptr(env) as usize;
                if let Some(environment_id) = context.environment_ids.get(&environment_key) {
                    out.push(2);
                    out.extend_from_slice(&environment_id.to_le_bytes());
                } else {
                    let environment_id = context.next_environment_id;
                    context.next_environment_id =
                        context.next_environment_id.checked_add(1).ok_or(
                            VmRecordingError::InvalidFormat("too many callable environments"),
                        )?;
                    context
                        .environment_ids
                        .insert(environment_key, environment_id);
                    out.push(1);
                    out.extend_from_slice(&environment_id.to_le_bytes());
                    let cells = env.cells.lock().map_err(|_| {
                        VmRecordingError::InvalidFormat("poisoned callable environment")
                    })?;
                    write_u32_len(cells.len(), out)?;
                    for cell in cells.iter() {
                        encode_value(cell, out, context)?;
                    }
                }
            } else {
                out.push(0);
            }
        }
    }
    Ok(())
}

fn decode_value(
    cursor: &mut RecordingCursor<'_>,
    context: &mut ValueDecodeContext,
) -> Result<Value, VmRecordingError> {
    match cursor.read_u8()? {
        0 => Ok(Value::Null),
        1 => Ok(Value::Int(cursor.read_i64()?)),
        2 => Ok(Value::Float(cursor.read_f64()?)),
        3 => Ok(Value::Bool(false)),
        4 => Ok(Value::Bool(true)),
        5 => {
            let len = cursor.read_u32()? as usize;
            let bytes = cursor.read_exact(len)?;
            let value = std::str::from_utf8(bytes)
                .map_err(|_| VmRecordingError::InvalidFormat("invalid utf-8 string"))?;
            Ok(Value::string(value))
        }
        6 => {
            let len = cursor.read_u32()? as usize;
            Ok(Value::bytes(cursor.read_exact(len)?.to_vec()))
        }
        7 => {
            let len = cursor.read_u32()? as usize;
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(decode_value(cursor, context)?);
            }
            Ok(Value::Array(values.into()))
        }
        8 => {
            let len = cursor.read_u32()? as usize;
            let mut entries = Vec::with_capacity(len);
            for _ in 0..len {
                let key = decode_value(cursor, context)?;
                let value = decode_value(cursor, context)?;
                entries.push((key, value));
            }
            Ok(Value::map(entries))
        }
        9 => {
            let program_instance = cursor.read_u64()?;
            let prototype_id = cursor.read_u32()?;
            let kind = match cursor.read_u8()? {
                0 => crate::CallableKind::FunctionItem,
                1 => crate::CallableKind::Closure,
                2 => crate::CallableKind::HostFunction,
                _ => return Err(VmRecordingError::InvalidFormat("invalid callable kind")),
            };
            let env = match cursor.read_u8()? {
                0 => None,
                1 => {
                    let environment_id = cursor.read_u32()?;
                    if context.environments.contains_key(&environment_id) {
                        return Err(VmRecordingError::InvalidFormat(
                            "duplicate callable environment id",
                        ));
                    }
                    let environment = Arc::new(crate::CallableEnvironment {
                        cells: Mutex::new(Vec::new()),
                    });
                    context
                        .environments
                        .insert(environment_id, environment.clone());
                    let len = cursor.read_u32()? as usize;
                    let mut cells = Vec::with_capacity(len);
                    for _ in 0..len {
                        cells.push(decode_value(cursor, context)?);
                    }
                    *environment.cells.lock().map_err(|_| {
                        VmRecordingError::InvalidFormat("poisoned callable environment")
                    })? = cells;
                    Some(environment)
                }
                2 => {
                    let environment_id = cursor.read_u32()?;
                    Some(context.environments.get(&environment_id).cloned().ok_or(
                        VmRecordingError::InvalidFormat("unknown callable environment id"),
                    )?)
                }
                _ => return Err(VmRecordingError::InvalidFormat("invalid callable env flag")),
            };
            Ok(Value::Callable(Arc::new(crate::CallableValue {
                program_instance,
                prototype_id,
                kind,
                env,
            })))
        }
        _ => Err(VmRecordingError::InvalidFormat("invalid value tag")),
    }
}

struct RecordingCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> RecordingCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_at_end(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], VmRecordingError> {
        if self.offset + len > self.bytes.len() {
            return Err(VmRecordingError::InvalidFormat(
                "unexpected end of recording payload",
            ));
        }
        let bytes = &self.bytes[self.offset..self.offset + len];
        self.offset += len;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, VmRecordingError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, VmRecordingError> {
        let mut bytes = [0u8; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32, VmRecordingError> {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64, VmRecordingError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_i64(&mut self) -> Result<i64, VmRecordingError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(i64::from_le_bytes(bytes))
    }

    fn read_f64(&mut self) -> Result<f64, VmRecordingError> {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(f64::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_preserves_callable_environment_aliases() {
        let environment = Arc::new(crate::CallableEnvironment {
            cells: Mutex::new(vec![Value::Int(7)]),
        });
        let callable = |prototype_id| {
            Value::Callable(Arc::new(crate::CallableValue {
                program_instance: 11,
                prototype_id,
                kind: crate::CallableKind::Closure,
                env: Some(environment.clone()),
            }))
        };
        let recording = VmRecording {
            program: Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]),
            frames: vec![VmRecordingFrame {
                ip: 0,
                call_depth: 0,
                execution_frames: Vec::new(),
                stack: vec![callable(1), callable(2)],
                locals: Vec::new(),
            }],
            terminal_status: None,
        };

        let decoded = VmRecording::decode(&recording.encode().expect("encode")).expect("decode");
        let Value::Callable(first) = &decoded.frames[0].stack[0] else {
            panic!("first value should be callable");
        };
        let Value::Callable(second) = &decoded.frames[0].stack[1] else {
            panic!("second value should be callable");
        };
        assert!(Arc::ptr_eq(
            first.env.as_ref().expect("first env"),
            second.env.as_ref().expect("second env")
        ));
    }

    #[test]
    fn recording_v3_rejects_legacy_versions() {
        let recording = VmRecording {
            program: Program::new(Vec::new(), vec![crate::OpCode::Ret as u8]),
            frames: Vec::new(),
            terminal_status: Some(VmStatus::Halted),
        };
        let bytes = recording.encode().expect("recording should encode");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 3);

        for legacy in [1u16, 2u16] {
            let mut legacy_bytes = bytes.clone();
            legacy_bytes[4..6].copy_from_slice(&legacy.to_le_bytes());
            assert!(matches!(
                VmRecording::decode(&legacy_bytes),
                Err(VmRecordingError::Message(message))
                    if message == format!("unsupported recording version {legacy}")
            ));
        }
    }
}
