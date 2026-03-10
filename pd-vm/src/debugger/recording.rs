use std::collections::HashSet;
use std::io;
use std::path::Path;

use crate::vm::{Program, Value, Vm, VmStatus};

#[derive(Clone, Debug, PartialEq)]
pub struct VmRecordingFrame {
    pub ip: usize,
    pub call_depth: usize,
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
        const VERSION: u16 = 2;

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
        for frame in &self.frames {
            write_u32_from_usize(frame.ip, &mut out)?;
            write_u32_from_usize(frame.call_depth, &mut out)?;

            write_u32_len(frame.stack.len(), &mut out)?;
            for value in &frame.stack {
                encode_value(value, &mut out)?;
            }

            write_u32_len(frame.locals.len(), &mut out)?;
            for value in &frame.locals {
                encode_value(value, &mut out)?;
            }
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, VmRecordingError> {
        const MAGIC: [u8; 4] = *b"PDRC";
        const VERSION_LEGACY: u16 = 1;
        const VERSION: u16 = 2;

        let mut cursor = RecordingCursor::new(bytes);

        let magic = cursor.read_exact(4)?;
        if magic != MAGIC {
            return Err(VmRecordingError::InvalidFormat("invalid recording magic"));
        }

        let version = cursor.read_u16()?;
        if version != VERSION && version != VERSION_LEGACY {
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
        for _ in 0..frame_count {
            let ip = cursor.read_u32()? as usize;
            let call_depth = cursor.read_u32()? as usize;

            let stack_len = cursor.read_u32()? as usize;
            let mut stack = Vec::with_capacity(stack_len);
            for _ in 0..stack_len {
                stack.push(decode_value(&mut cursor)?);
            }

            let locals_len = cursor.read_u32()? as usize;
            let mut locals = Vec::with_capacity(locals_len);
            for _ in 0..locals_len {
                locals.push(decode_value(&mut cursor)?);
            }

            frames.push(VmRecordingFrame {
                ip,
                call_depth,
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

fn encode_value(value: &Value, out: &mut Vec<u8>) -> Result<(), VmRecordingError> {
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
        Value::Array(values) => {
            out.push(6);
            write_u32_len(values.len(), out)?;
            for value in values.iter() {
                encode_value(value, out)?;
            }
        }
        Value::Map(entries) => {
            out.push(7);
            write_u32_len(entries.len(), out)?;
            for (key, value) in entries.iter() {
                encode_value(key, out)?;
                encode_value(value, out)?;
            }
        }
    }
    Ok(())
}

fn decode_value(cursor: &mut RecordingCursor<'_>) -> Result<Value, VmRecordingError> {
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
            let mut values = Vec::with_capacity(len);
            for _ in 0..len {
                values.push(decode_value(cursor)?);
            }
            Ok(Value::Array(values.into()))
        }
        7 => {
            let len = cursor.read_u32()? as usize;
            let mut entries = Vec::with_capacity(len);
            for _ in 0..len {
                let key = decode_value(cursor)?;
                let value = decode_value(cursor)?;
                entries.push((key, value));
            }
            Ok(Value::Map(entries.into()))
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
