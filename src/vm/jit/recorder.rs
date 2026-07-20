use std::fmt;

use crate::builtins::BuiltinFunction;
use crate::compiler::TypeSchema;
use crate::vm::{OpCode, Program, Value, ValueType, checked_int_div};

use super::JitTraceTerminal;
use super::deopt::materialize_ssa_values;
use super::inline::{InlineCandidate, InlineRejectReason, classify_static_inline_candidate};
use super::ir::{
    SsaBranchTarget, SsaInstKind, SsaMaterialization, SsaTerminator, SsaTrace, SsaTraceBuilder,
    SsaValue, SsaValueId, SsaValueRepr, VirtualFrameSnapshot,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecordedTrace {
    pub(crate) has_call: bool,
    pub(crate) has_yielding_call: bool,
    pub(crate) entry_callable_guards: Vec<(u8, u32)>,
    pub(crate) op_names: Vec<String>,
    pub(crate) ssa: SsaTrace,
    pub(crate) terminal: JitTraceTerminal,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TraceRecordError {
    UnsupportedOpcode(u8),
    UnsupportedTrace(String),
    InvalidJumpTarget {
        target: usize,
    },
    InvalidImmediate(&'static str),
    InvalidLocal(u8),
    StackUnderflow,
    TypeMismatch {
        expected: &'static str,
        actual: SsaValueRepr,
    },
    StackDepthMismatch {
        expected: usize,
        got: usize,
    },
    TraceTooLong {
        limit: usize,
    },
    MissingTerminal,
    InvalidIr(String),
}

impl fmt::Display for TraceRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOpcode(op) => write!(f, "unsupported opcode 0x{op:02X}"),
            Self::UnsupportedTrace(detail) => write!(f, "{detail}"),
            Self::InvalidJumpTarget { target } => {
                write!(
                    f,
                    "jump target {target} is invalid or out of bytecode bounds"
                )
            }
            Self::InvalidImmediate(kind) => {
                write!(f, "failed to decode immediate operand for {kind}")
            }
            Self::InvalidLocal(index) => write!(f, "invalid local {index}"),
            Self::StackUnderflow => write!(f, "symbolic stack underflow"),
            Self::TypeMismatch { expected, actual } => {
                write!(f, "expected {expected}, got {actual}")
            }
            Self::StackDepthMismatch { expected, got } => {
                write!(
                    f,
                    "backedge stack depth mismatch: expected {expected}, got {got}"
                )
            }
            Self::TraceTooLong { limit } => {
                write!(f, "trace length exceeded configured limit {limit}")
            }
            Self::MissingTerminal => write!(f, "trace recorder reached end without terminator"),
            Self::InvalidIr(detail) => write!(f, "invalid SSA trace: {detail}"),
        }
    }
}

impl std::error::Error for TraceRecordError {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ValueInfo {
    repr: SsaValueRepr,
    const_int: Option<i64>,
    const_float: Option<f64>,
    const_bool: Option<bool>,
    known_type: Option<ValueType>,
    force_value_eq: bool,
    source_local: Option<u8>,
}

impl ValueInfo {
    fn tagged() -> Self {
        Self {
            repr: SsaValueRepr::Tagged,
            const_int: None,
            const_float: None,
            const_bool: None,
            known_type: None,
            force_value_eq: false,
            source_local: None,
        }
    }

    fn tagged_typed(known_type: ValueType) -> Self {
        Self {
            repr: SsaValueRepr::Tagged,
            const_int: None,
            const_float: None,
            const_bool: None,
            known_type: Some(known_type),
            force_value_eq: false,
            source_local: None,
        }
    }

    fn int(value: Option<i64>) -> Self {
        Self {
            repr: SsaValueRepr::I64,
            const_int: value,
            const_float: None,
            const_bool: None,
            known_type: Some(ValueType::Int),
            force_value_eq: false,
            source_local: None,
        }
    }

    fn float(value: Option<f64>) -> Self {
        Self {
            repr: SsaValueRepr::F64,
            const_int: None,
            const_float: value,
            const_bool: None,
            known_type: Some(ValueType::Float),
            force_value_eq: false,
            source_local: None,
        }
    }

    fn bool(value: Option<bool>) -> Self {
        Self {
            repr: SsaValueRepr::Bool,
            const_int: None,
            const_float: None,
            const_bool: value,
            known_type: Some(ValueType::Bool),
            force_value_eq: false,
            source_local: None,
        }
    }

    fn heap(tag: ValueType) -> Self {
        Self {
            repr: SsaValueRepr::HeapPtr(tag),
            const_int: None,
            const_float: None,
            const_bool: None,
            known_type: Some(tag),
            force_value_eq: false,
            source_local: None,
        }
    }

    fn from_constant(value: &Value) -> Self {
        match value {
            Value::Int(value) => Self::int(Some(*value)),
            Value::Bool(value) => Self::bool(Some(*value)),
            Value::Float(value) => Self::float(Some(*value)),
            Value::Null => Self::tagged_typed(ValueType::Null),
            Value::String(_) => Self::tagged_typed(ValueType::String),
            Value::Bytes(_) => Self::tagged_typed(ValueType::Bytes),
            Value::Array(_) => Self::tagged_typed(ValueType::Array),
            Value::Map(_) => Self::tagged_typed(ValueType::Map),
            Value::Callable(_) => Self::tagged_typed(ValueType::Callable),
        }
    }

    fn type_name() -> Self {
        let mut info = Self::tagged_typed(ValueType::String);
        info.force_value_eq = true;
        info
    }

    fn sourced_from(mut self, local: u8) -> Self {
        if self.source_local.is_none() {
            self.source_local = Some(local);
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AnalysisFrame {
    stack: Vec<ValueInfo>,
    locals: Vec<ValueInfo>,
}

impl AnalysisFrame {
    fn new(
        program: &Program,
        entry_stack_depth: usize,
        local_count: usize,
        entry_local_types: Option<&[ValueType]>,
    ) -> Self {
        Self {
            stack: vec![ValueInfo::tagged(); entry_stack_depth],
            locals: (0..local_count)
                .map(|local| entry_local_info(program, local, entry_local_types))
                .collect(),
        }
    }

    fn pop(&mut self) -> Result<ValueInfo, TraceRecordError> {
        self.stack.pop().ok_or(TraceRecordError::StackUnderflow)
    }

    fn push(&mut self, value: ValueInfo) {
        self.stack.push(value);
    }

    fn local(&self, index: u8) -> Result<ValueInfo, TraceRecordError> {
        self.locals
            .get(index as usize)
            .copied()
            .ok_or(TraceRecordError::InvalidLocal(index))
    }

    fn store_local(&mut self, index: u8, value: ValueInfo) -> Result<(), TraceRecordError> {
        let slot = self
            .locals
            .get_mut(index as usize)
            .ok_or(TraceRecordError::InvalidLocal(index))?;
        *slot = value;
        Ok(())
    }
}

fn entry_local_info(
    program: &Program,
    local: usize,
    entry_local_types: Option<&[ValueType]>,
) -> ValueInfo {
    let known_type = entry_local_types
        .and_then(|types| types.get(local))
        .copied()
        .or_else(|| {
            program
                .type_map
                .as_ref()
                .and_then(|type_map| type_map.local_types.get(local))
                .copied()
        })
        .filter(|ty| *ty != ValueType::Unknown);
    known_type.map_or_else(ValueInfo::tagged, ValueInfo::tagged_typed)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SymbolicValue {
    value: SsaValue,
    info: ValueInfo,
}

fn inline_schema_guard_type(schema: &TypeSchema) -> Option<Option<ValueType>> {
    match schema {
        TypeSchema::Unknown | TypeSchema::GenericParam(_) => Some(None),
        TypeSchema::Int => Some(Some(ValueType::Int)),
        TypeSchema::Float => Some(Some(ValueType::Float)),
        TypeSchema::Bool => Some(Some(ValueType::Bool)),
        TypeSchema::String => Some(Some(ValueType::String)),
        TypeSchema::Bytes => Some(Some(ValueType::Bytes)),
        TypeSchema::Named(_, _) | TypeSchema::Map(_) | TypeSchema::Object(_) => {
            Some(Some(ValueType::Map))
        }
        TypeSchema::Array(_) | TypeSchema::ArrayTuple(_) | TypeSchema::ArrayTupleRest { .. } => {
            Some(Some(ValueType::Array))
        }
        TypeSchema::Null => Some(Some(ValueType::Null)),
        TypeSchema::Number | TypeSchema::Optional(_) | TypeSchema::Callable { .. } => None,
    }
}

fn inline_argument_schemas_supported(
    arguments: &[SymbolicValue],
    schema: Option<&TypeSchema>,
) -> bool {
    let Some(TypeSchema::Callable { params, result }) = schema else {
        return schema.is_none();
    };
    inline_schema_guard_type(result).is_some()
        && params.len() == arguments.len()
        && params.iter().zip(arguments).all(|(schema, argument)| {
            let Some(guard_type) = inline_schema_guard_type(schema) else {
                return false;
            };
            match (guard_type, argument.info.repr) {
                (None, _) | (Some(_), SsaValueRepr::Tagged) => true,
                (Some(ValueType::Int), SsaValueRepr::I64)
                | (Some(ValueType::Float), SsaValueRepr::F64)
                | (Some(ValueType::Bool), SsaValueRepr::Bool) => true,
                (Some(expected), SsaValueRepr::HeapPtr(actual)) => expected == actual,
                _ => false,
            }
        })
}

fn append_inline_argument_schema_guards(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    arguments: &[SymbolicValue],
    schema: Option<&TypeSchema>,
) -> Result<Option<SsaValueId>, TraceRecordError> {
    let Some(TypeSchema::Callable { params, .. }) = schema else {
        return Ok(None);
    };
    let mut guard = None;
    for (schema, argument) in params.iter().zip(arguments) {
        let Some(Some(expected)) = inline_schema_guard_type(schema) else {
            continue;
        };
        if argument.info.repr != SsaValueRepr::Tagged {
            continue;
        }
        let predicate = builder
            .append_value_inst(
                block,
                ip,
                SsaValueRepr::Bool,
                SsaInstKind::ValueIsType {
                    input: argument.value.id,
                    tag: expected,
                },
            )
            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
        guard = Some(if let Some(previous) = guard {
            builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::BoolAnd {
                        lhs: previous,
                        rhs: predicate.id,
                    },
                )
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?
                .id
        } else {
            predicate.id
        });
    }
    Ok(guard)
}

fn append_inline_result_schema_guard(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    result: SymbolicValue,
    schema: Option<&TypeSchema>,
) -> Result<Option<SsaValueId>, TraceRecordError> {
    let Some(TypeSchema::Callable { result: schema, .. }) = schema else {
        return Ok(None);
    };
    let Some(guard_type) = inline_schema_guard_type(schema) else {
        return Err(TraceRecordError::UnsupportedTrace(
            "inline callable return schema is not guardable".to_string(),
        ));
    };
    let Some(expected) = guard_type else {
        return Ok(None);
    };
    let statically_matches = match result.info.repr {
        SsaValueRepr::I64 => expected == ValueType::Int,
        SsaValueRepr::F64 => expected == ValueType::Float,
        SsaValueRepr::Bool => expected == ValueType::Bool,
        SsaValueRepr::HeapPtr(actual) => expected == actual,
        SsaValueRepr::Tagged => {
            let predicate = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::ValueIsType {
                        input: result.value.id,
                        tag: expected,
                    },
                )
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            return Ok(Some(predicate.id));
        }
    };
    if statically_matches {
        return Ok(None);
    }
    let predicate = builder
        .append_value_inst(
            block,
            ip,
            SsaValueRepr::Bool,
            SsaInstKind::Constant(Value::Bool(false)),
        )
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok(Some(predicate.id))
}

#[derive(Clone, Debug, PartialEq)]
struct SymbolicFrame {
    stack: Vec<SymbolicValue>,
    locals: Vec<SymbolicValue>,
    dirty_locals: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq)]
struct InlineRecorderFrame {
    candidate: InlineCandidate,
    call_ip: usize,
    return_ip: usize,
    caller: SymbolicFrame,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoopHeaderPlan {
    stack_reprs: Vec<SsaValueRepr>,
    stack_known_types: Vec<Option<ValueType>>,
    local_reprs: Vec<SsaValueRepr>,
    local_known_types: Vec<Option<ValueType>>,
    entry_seed: Vec<LoopSeed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryUseState {
    Untouched,
    ReadBeforeWrite,
    WrittenBeforeRead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopSeed {
    Entry,
    ZeroI64,
    ZeroF64,
    FalseBool,
}

impl SymbolicFrame {
    fn new(stack: Vec<SymbolicValue>, locals: Vec<SymbolicValue>) -> Self {
        let dirty_locals = vec![false; locals.len()];
        Self {
            stack,
            locals,
            dirty_locals,
        }
    }

    fn pop(&mut self) -> Result<SymbolicValue, TraceRecordError> {
        self.stack.pop().ok_or(TraceRecordError::StackUnderflow)
    }

    fn push(&mut self, value: SymbolicValue) {
        self.stack.push(value);
    }

    fn local(&self, index: u8) -> Result<SymbolicValue, TraceRecordError> {
        self.locals
            .get(index as usize)
            .copied()
            .ok_or(TraceRecordError::InvalidLocal(index))
    }

    fn store_local(&mut self, index: u8, value: SymbolicValue) -> Result<(), TraceRecordError> {
        let local = usize::from(index);
        let slot = self
            .locals
            .get_mut(local)
            .ok_or(TraceRecordError::InvalidLocal(index))?;
        *slot = value;
        self.dirty_locals[local] = true;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodedOp {
    Nop {
        ip: usize,
    },
    Ret {
        ip: usize,
    },
    Ldc {
        ip: usize,
        index: u32,
    },
    Ldloc {
        ip: usize,
        index: u8,
    },
    Stloc {
        ip: usize,
        index: u8,
    },
    Pop {
        ip: usize,
    },
    Dup {
        ip: usize,
    },
    Neg {
        ip: usize,
    },
    Not {
        ip: usize,
    },
    BinOp {
        ip: usize,
        opcode: u8,
    },
    Compare {
        ip: usize,
        opcode: u8,
    },
    Brfalse {
        ip: usize,
        target: usize,
        fallthrough_ip: usize,
        prefer_join_path: bool,
    },
    Br {
        ip: usize,
        target: usize,
    },
    Call {
        ip: usize,
        index: u16,
        builtin: Option<BuiltinFunction>,
        argc: u8,
        yields: bool,
    },
    CallValue {
        ip: usize,
        argc: u8,
        resume_ip: usize,
    },
}

impl DecodedOp {
    fn ip(self) -> usize {
        match self {
            Self::Nop { ip }
            | Self::Ret { ip }
            | Self::Ldc { ip, .. }
            | Self::Ldloc { ip, .. }
            | Self::Stloc { ip, .. }
            | Self::Pop { ip }
            | Self::Dup { ip }
            | Self::Neg { ip }
            | Self::Not { ip }
            | Self::BinOp { ip, .. }
            | Self::Compare { ip, .. }
            | Self::Brfalse { ip, .. }
            | Self::Br { ip, .. }
            | Self::Call { ip, .. }
            | Self::CallValue { ip, .. } => ip,
        }
    }

    fn may_need_inline_failure_exit(self) -> bool {
        matches!(
            self,
            Self::Ret { .. }
                | Self::Neg { .. }
                | Self::Not { .. }
                | Self::BinOp { .. }
                | Self::Compare { .. }
                | Self::Brfalse { .. }
                | Self::Call { .. }
        )
    }

    fn is_useful_native_computation(self) -> bool {
        match self {
            Self::Nop { .. }
            | Self::Ret { .. }
            | Self::Ldc { .. }
            | Self::Ldloc { .. }
            | Self::Pop { .. }
            | Self::Dup { .. }
            | Self::Br { .. }
            | Self::Call { .. }
            | Self::CallValue { .. } => false,
            Self::Stloc { .. }
            | Self::Neg { .. }
            | Self::Not { .. }
            | Self::BinOp { .. }
            | Self::Compare { .. }
            | Self::Brfalse { .. } => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntBinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    Lshr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoolBinOpKind {
    And,
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatBinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumericBinOpKind {
    Int(IntBinOpKind),
    Float(FloatBinOpKind),
    Concat(ConcatBinOpKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntCompareKind {
    Eq,
    Lt,
    Gt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatCompareKind {
    Eq,
    Lt,
    Gt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumericCompareKind {
    Int(IntCompareKind),
    Float(FloatCompareKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumericUnaryKind {
    Int,
    Float,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConcatBinOpKind {
    String,
    Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeapContainerKind {
    String,
    Bytes,
    Array,
    Map,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpecializedBuiltinKind {
    ValueLen,
    StringLen,
    BytesLen,
    StringSlice,
    BytesSlice,
    StringGet,
    BytesGet,
    BytesHas,
    StringContains,
    RegexMatch,
    RegexReplace,
    StringReplaceLiteral,
    StringLowerAscii,
    TypeOf,
    TypeOfKnown(ValueType),
    ToString,
    ToStringIdentity,
    StringSplitLiteral,
    StringConcat,
    BytesConcat,
    BytesFromArrayU8,
    BytesToUtf8Ascii,
    BytesToArrayU8,
    ArrayNew,
    ArrayLen,
    ArrayGet,
    ArrayHas,
    ArraySet,
    ArrayPush,
    MapLen,
    MapGet,
    MapHas,
    MapSet,
    MapIterNext,
    MapIterTakeKey,
    MapIterTakeValue,
}

struct TraceCursor<'a> {
    program: &'a Program,
    ip: usize,
    max_trace_len: usize,
    recorded_ops: usize,
}

impl<'a> TraceCursor<'a> {
    fn new(program: &'a Program, root_ip: usize, max_trace_len: usize) -> Self {
        Self {
            program,
            ip: root_ip,
            max_trace_len,
            recorded_ops: 0,
        }
    }

    fn ip(&self) -> usize {
        self.ip
    }

    fn jump_to(&mut self, target: usize) -> Result<(), TraceRecordError> {
        if target >= self.program.code.len() {
            return Err(TraceRecordError::InvalidJumpTarget { target });
        }
        self.ip = target;
        Ok(())
    }

    fn next(&mut self) -> Result<Option<DecodedOp>, TraceRecordError> {
        if self.recorded_ops >= self.max_trace_len {
            return Err(TraceRecordError::TraceTooLong {
                limit: self.max_trace_len,
            });
        }
        let Some(&opcode) = self.program.code.get(self.ip) else {
            return Ok(None);
        };
        let instr_ip = self.ip;
        self.ip = self.ip.saturating_add(1);

        let decoded = if opcode == OpCode::Nop as u8 {
            self.recorded_ops += 1;
            DecodedOp::Nop { ip: instr_ip }
        } else if opcode == OpCode::Ret as u8 {
            self.recorded_ops += 1;
            DecodedOp::Ret { ip: instr_ip }
        } else if opcode == OpCode::Ldc as u8 {
            self.recorded_ops += 1;
            let index = read_u32(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("ldc"))?;
            DecodedOp::Ldc {
                ip: instr_ip,
                index,
            }
        } else if opcode == OpCode::Ldloc as u8 {
            self.recorded_ops += 1;
            let index = read_u8(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("ldloc"))?;
            DecodedOp::Ldloc {
                ip: instr_ip,
                index,
            }
        } else if opcode == OpCode::Stloc as u8 {
            self.recorded_ops += 1;
            let index = read_u8(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("stloc"))?;
            DecodedOp::Stloc {
                ip: instr_ip,
                index,
            }
        } else if opcode == OpCode::Pop as u8 {
            self.recorded_ops += 1;
            DecodedOp::Pop { ip: instr_ip }
        } else if opcode == OpCode::Dup as u8 {
            self.recorded_ops += 1;
            DecodedOp::Dup { ip: instr_ip }
        } else if opcode == OpCode::Neg as u8 {
            self.recorded_ops += 1;
            DecodedOp::Neg { ip: instr_ip }
        } else if opcode == OpCode::Not as u8 {
            self.recorded_ops += 1;
            DecodedOp::Not { ip: instr_ip }
        } else if opcode == OpCode::Add as u8
            || opcode == OpCode::Sub as u8
            || opcode == OpCode::Mul as u8
            || opcode == OpCode::Div as u8
            || opcode == OpCode::Mod as u8
            || opcode == OpCode::Shl as u8
            || opcode == OpCode::Shr as u8
            || opcode == OpCode::Lshr as u8
            || opcode == OpCode::And as u8
            || opcode == OpCode::Or as u8
        {
            self.recorded_ops += 1;
            DecodedOp::BinOp {
                ip: instr_ip,
                opcode,
            }
        } else if opcode == OpCode::Ceq as u8
            || opcode == OpCode::Clt as u8
            || opcode == OpCode::Cgt as u8
        {
            self.recorded_ops += 1;
            DecodedOp::Compare {
                ip: instr_ip,
                opcode,
            }
        } else if opcode == OpCode::Brfalse as u8 {
            self.recorded_ops += 1;
            let target = read_u32(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("brfalse"))?
                as usize;
            if target >= self.program.code.len() {
                return Err(TraceRecordError::InvalidJumpTarget { target });
            }
            let fallthrough_ip = self.ip;
            let prefer_join_path = target > fallthrough_ip
                && straight_line_if_join_side_entry(&self.program.code, fallthrough_ip, target)
                    .is_some();
            DecodedOp::Brfalse {
                ip: instr_ip,
                target,
                fallthrough_ip,
                prefer_join_path,
            }
        } else if opcode == OpCode::Br as u8 {
            let target = read_u32(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("br"))? as usize;
            if target >= self.program.code.len() {
                return Err(TraceRecordError::InvalidJumpTarget { target });
            }
            if target <= instr_ip {
                self.recorded_ops += 1;
            }
            DecodedOp::Br {
                ip: instr_ip,
                target,
            }
        } else if opcode == OpCode::Call as u8 {
            self.recorded_ops += 1;
            let index = read_u16(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("call index"))?;
            let argc = read_u8(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("call argc"))?;
            if let Some(builtin) = BuiltinFunction::from_call_index(index) {
                if !builtin.accepts_arity(argc) {
                    return Err(TraceRecordError::InvalidImmediate("builtin call arity"));
                }
                DecodedOp::Call {
                    ip: instr_ip,
                    index,
                    builtin: Some(builtin),
                    argc,
                    yields: false,
                }
            } else {
                DecodedOp::Call {
                    ip: instr_ip,
                    index,
                    builtin: None,
                    argc,
                    yields: true,
                }
            }
        } else if opcode == OpCode::CallValue as u8 {
            self.recorded_ops += 1;
            let argc = read_u8(&self.program.code, &mut self.ip)
                .ok_or(TraceRecordError::InvalidImmediate("callvalue argc"))?;
            DecodedOp::CallValue {
                ip: instr_ip,
                argc,
                resume_ip: self.ip,
            }
        } else {
            return Err(TraceRecordError::UnsupportedOpcode(opcode));
        };

        Ok(Some(decoded))
    }
}

#[cfg(test)]
pub(crate) fn record_trace(
    program: &Program,
    root_ip: usize,
    entry_stack_depth: usize,
    max_trace_len: usize,
    non_yielding_host_imports: &[bool],
) -> Result<RecordedTrace, TraceRecordError> {
    record_trace_with_local_count(
        program,
        crate::vm::native::ROOT_FRAME_KEY,
        root_ip,
        entry_stack_depth,
        program.local_count,
        None,
        None,
        max_trace_len,
        non_yielding_host_imports,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_trace_with_local_count(
    program: &Program,
    caller_frame_key: u64,
    root_ip: usize,
    entry_stack_depth: usize,
    local_count: usize,
    entry_local_types: Option<&[ValueType]>,
    entry_callable_prototypes: Option<&[Option<u32>]>,
    max_trace_len: usize,
    non_yielding_host_imports: &[bool],
) -> Result<RecordedTrace, TraceRecordError> {
    let loop_header_plan = infer_loop_header_plan(
        program,
        caller_frame_key,
        root_ip,
        entry_stack_depth,
        local_count,
        entry_local_types,
        max_trace_len,
        non_yielding_host_imports,
    )?;
    let mut builder = SsaTraceBuilder::new(root_ip, entry_stack_depth);
    let entry = builder.entry();

    let entry_stack = (0..entry_stack_depth)
        .map(|index| {
            builder
                .append_param(entry, SsaValueRepr::Tagged, format!("stack{index}"))
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let entry_locals = (0..local_count)
        .map(|local| {
            builder
                .append_param(entry, SsaValueRepr::Tagged, format!("local{local}"))
                .map(|value| SymbolicValue {
                    value,
                    info: entry_local_info(program, local, entry_local_types),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let (loop_body_block, mut current_block, mut frame) = if let Some(loop_plan) = &loop_header_plan
    {
        for (index, repr) in loop_plan.stack_reprs.iter().copied().enumerate() {
            validate_loop_carrier_repr(index, repr)?;
        }
        for (local, repr) in loop_plan.local_reprs.iter().copied().enumerate() {
            validate_loop_carrier_repr(local, repr)?;
        }
        let body = builder.create_block();
        let mut body_stack = Vec::with_capacity(loop_plan.stack_reprs.len());
        let mut body_locals = Vec::with_capacity(loop_plan.local_reprs.len());
        let mut entry_args =
            Vec::with_capacity(loop_plan.stack_reprs.len() + loop_plan.local_reprs.len());
        for (index, repr) in loop_plan.stack_reprs.iter().copied().enumerate() {
            let entry_arg =
                ensure_entry_repr(&mut builder, entry, root_ip, entry_stack[index], repr)?
                    .value
                    .id;
            let value = builder
                .append_param(body, repr, format!("loop_stack{index}"))
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            body_stack.push(SymbolicValue {
                value,
                info: ValueInfo {
                    repr,
                    const_int: None,
                    const_float: None,
                    const_bool: None,
                    known_type: loop_plan.stack_known_types[index],
                    force_value_eq: false,
                    source_local: None,
                },
            });
            entry_args.push(entry_arg);
        }
        for (local, repr) in loop_plan.local_reprs.iter().copied().enumerate() {
            let entry_arg = match loop_plan.entry_seed[local] {
                LoopSeed::Entry => {
                    ensure_entry_repr(&mut builder, entry, root_ip, entry_locals[local], repr)?
                        .value
                        .id
                }
                LoopSeed::ZeroI64 => {
                    builder
                        .append_value_inst(
                            entry,
                            root_ip,
                            SsaValueRepr::I64,
                            SsaInstKind::Constant(Value::Int(0)),
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?
                        .id
                }
                LoopSeed::ZeroF64 => {
                    builder
                        .append_value_inst(
                            entry,
                            root_ip,
                            SsaValueRepr::F64,
                            SsaInstKind::Constant(Value::Float(0.0)),
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?
                        .id
                }
                LoopSeed::FalseBool => {
                    builder
                        .append_value_inst(
                            entry,
                            root_ip,
                            SsaValueRepr::Bool,
                            SsaInstKind::Constant(Value::Bool(false)),
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?
                        .id
                }
            };
            let value = builder
                .append_param(body, repr, format!("loop_local{local}"))
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            body_locals.push(SymbolicValue {
                value,
                info: ValueInfo {
                    repr,
                    const_int: None,
                    const_float: None,
                    const_bool: None,
                    known_type: loop_plan.local_known_types[local],
                    force_value_eq: false,
                    source_local: None,
                },
            });
            entry_args.push(entry_arg);
        }
        builder
            .set_terminator(
                entry,
                SsaTerminator::Jump {
                    target: body,
                    args: entry_args,
                },
            )
            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
        (body, body, SymbolicFrame::new(body_stack, body_locals))
    } else {
        (
            entry,
            entry,
            SymbolicFrame::new(entry_stack.clone(), entry_locals.clone()),
        )
    };

    let mut cursor = TraceCursor::new(program, root_ip, max_trace_len);
    let mut terminal = None;
    let mut op_names = Vec::new();
    let mut has_call = false;
    let mut has_yielding_call = false;
    let mut has_useful_native_computation = false;
    let mut entry_callable_guards = Vec::new();
    let mut inline_frame: Option<InlineRecorderFrame> = None;

    loop {
        let Some(decoded) = cursor.next()? else {
            break;
        };
        let is_useful_native_computation = decoded.is_useful_native_computation();
        let instruction_failure_exit = if decoded.may_need_inline_failure_exit() {
            inline_frame
                .as_ref()
                .map(|inlined| add_symbolic_exit(&mut builder, decoded.ip(), &frame, Some(inlined)))
        } else {
            None
        };
        builder.set_current_failure_exit(instruction_failure_exit);

        match decoded {
            DecodedOp::Nop { .. } => op_names.push("nop".to_string()),
            DecodedOp::Ret { ip } => {
                if inline_frame.is_some() {
                    if frame.stack.is_empty() {
                        let value = builder
                            .append_value_inst(
                                current_block,
                                ip,
                                SsaValueRepr::Tagged,
                                SsaInstKind::Constant(Value::Null),
                            )
                            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                        frame.push(SymbolicValue {
                            value,
                            info: ValueInfo::tagged_typed(ValueType::Null),
                        });
                    }
                    let result = *frame.stack.last().expect("inline result ensured above");
                    let prototype_id = inline_frame
                        .as_ref()
                        .expect("inline frame checked above")
                        .candidate
                        .prototype_id;
                    let return_guard = append_inline_result_schema_guard(
                        &mut builder,
                        current_block,
                        ip,
                        result,
                        program.callable_prototypes[prototype_id as usize]
                            .schema
                            .as_ref(),
                    )?;
                    if let Some(condition) = return_guard {
                        let failure_exit = instruction_failure_exit.ok_or_else(|| {
                            TraceRecordError::InvalidIr(
                                "inline return schema guard is missing its virtual-frame exit"
                                    .to_string(),
                            )
                        })?;
                        let (guarded_block, guarded_frame, guarded_args) =
                            continue_with_inline_frame(
                                &mut builder,
                                &frame,
                                &mut inline_frame,
                                "inline_return_schema",
                            )?;
                        builder
                            .set_terminator(
                                current_block,
                                SsaTerminator::BranchBool {
                                    condition,
                                    if_true: super::ir::SsaBranchTarget::Block {
                                        target: guarded_block,
                                        args: guarded_args,
                                    },
                                    if_false: super::ir::SsaBranchTarget::Exit(failure_exit),
                                },
                            )
                            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                        current_block = guarded_block;
                        frame = guarded_frame;
                        op_names.push("inline_return_schema_guard".to_string());
                    }
                    let inlined = inline_frame.take().expect("inline frame checked above");
                    let result = frame.pop()?;
                    op_names.push("inline_ret".to_string());
                    frame = inlined.caller;
                    frame.push(result);
                    cursor.jump_to(inlined.return_ip)?;
                    has_useful_native_computation = true;
                    continue;
                }
                op_names.push("ret".to_string());
                let exit = add_symbolic_exit(&mut builder, ip, &frame, inline_frame.as_ref());
                builder
                    .set_terminator(current_block, SsaTerminator::Return { exit })
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                terminal = Some(JitTraceTerminal::Halt);
                break;
            }
            DecodedOp::Ldc { ip, index } => {
                op_names.push("ldc".to_string());
                let value = load_constant(&mut builder, current_block, ip, program, index)?;
                frame.push(value);
            }
            DecodedOp::Ldloc { index, .. } => {
                op_names.push("ldloc".to_string());
                let mut value = frame.local(index)?;
                value.info = value.info.sourced_from(index);
                frame.push(value);
            }
            DecodedOp::Stloc { index, .. } => {
                op_names.push("stloc".to_string());
                let value = frame.pop()?;
                frame.store_local(index, value)?;
            }
            DecodedOp::Pop { .. } => {
                op_names.push("pop".to_string());
                let _ = frame.pop()?;
            }
            DecodedOp::Dup { .. } => {
                op_names.push("dup".to_string());
                let value = *frame.stack.last().ok_or(TraceRecordError::StackUnderflow)?;
                frame.push(value);
            }
            DecodedOp::Neg { ip } => {
                let value = frame.pop()?;
                let (name, out) = match select_numeric_neg(value.info)? {
                    NumericUnaryKind::Int => {
                        let value = ensure_int(&mut builder, current_block, ip, value)?;
                        emit_int_neg(&mut builder, current_block, ip, value)?
                    }
                    NumericUnaryKind::Float => {
                        let value = ensure_float(&mut builder, current_block, ip, value)?;
                        emit_float_neg(&mut builder, current_block, ip, value)?
                    }
                };
                op_names.push(name.to_string());
                frame.push(out);
            }
            DecodedOp::Not { ip } => {
                let value = ensure_bool(&mut builder, current_block, ip, frame.pop()?)?;
                let (name, out) = emit_bool_not(&mut builder, current_block, ip, value)?;
                op_names.push(name.to_string());
                frame.push(out);
            }
            DecodedOp::BinOp { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let (name, out) = if opcode == OpCode::And as u8 || opcode == OpCode::Or as u8 {
                    let kind = select_bool_binop(program, ip, opcode, lhs.info, rhs.info)?;
                    let lhs = ensure_bool(&mut builder, current_block, ip, lhs)?;
                    let rhs = ensure_bool(&mut builder, current_block, ip, rhs)?;
                    emit_bool_binop(&mut builder, current_block, ip, kind, lhs, rhs)?
                } else {
                    match select_numeric_binop(program, ip, opcode, lhs.info, rhs.info)? {
                        NumericBinOpKind::Int(kind) => {
                            let lhs = ensure_int(&mut builder, current_block, ip, lhs)?;
                            let rhs = ensure_int(&mut builder, current_block, ip, rhs)?;
                            emit_int_binop(&mut builder, current_block, ip, kind, lhs, rhs)?
                        }
                        NumericBinOpKind::Float(kind) => {
                            let lhs = ensure_float(&mut builder, current_block, ip, lhs)?;
                            let rhs = ensure_float(&mut builder, current_block, ip, rhs)?;
                            emit_float_binop(&mut builder, current_block, ip, kind, lhs, rhs)?
                        }
                        NumericBinOpKind::Concat(kind) => {
                            emit_concat_binop(&mut builder, current_block, ip, kind, lhs, rhs)?
                        }
                    }
                };
                op_names.push(name.to_string());
                frame.push(out);
            }
            DecodedOp::Compare { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let numeric = select_numeric_compare(program, ip, opcode, lhs.info, rhs.info);
                let (name, out) = match numeric {
                    Ok(NumericCompareKind::Int(kind)) => {
                        let lhs = ensure_int(&mut builder, current_block, ip, lhs)?;
                        let rhs = ensure_int(&mut builder, current_block, ip, rhs)?;
                        emit_int_compare(&mut builder, current_block, ip, kind, lhs, rhs)?
                    }
                    Ok(NumericCompareKind::Float(kind)) => {
                        let lhs = ensure_float(&mut builder, current_block, ip, lhs)?;
                        let rhs = ensure_float(&mut builder, current_block, ip, rhs)?;
                        emit_float_compare(&mut builder, current_block, ip, kind, lhs, rhs)?
                    }
                    Err(_)
                        if opcode == OpCode::Ceq as u8
                            && lhs.info.repr == SsaValueRepr::Tagged
                            && rhs.info.repr == SsaValueRepr::Tagged =>
                    {
                        emit_value_eq(&mut builder, current_block, ip, lhs, rhs)?
                    }
                    Err(err) => return Err(err),
                };
                op_names.push(name.to_string());
                frame.push(out);
            }
            DecodedOp::Brfalse {
                ip,
                target,
                fallthrough_ip,
                prefer_join_path,
            } => {
                let condition = ensure_bool(&mut builder, current_block, ip, frame.pop()?)?;
                let prefer_join_path = prefer_join_path && condition.info.const_bool.is_none();
                if target < fallthrough_ip {
                    if target != root_ip {
                        return Err(TraceRecordError::UnsupportedTrace(format!(
                            "unsupported backward brfalse target {target} at ip {ip}"
                        )));
                    }
                    op_names.push("loop_if_false".to_string());
                    if frame.stack.len() != entry_stack_depth {
                        return Err(TraceRecordError::StackDepthMismatch {
                            expected: entry_stack_depth,
                            got: frame.stack.len(),
                        });
                    }
                    let exit = add_symbolic_exit(
                        &mut builder,
                        fallthrough_ip,
                        &frame,
                        inline_frame.as_ref(),
                    );
                    builder
                        .set_terminator(
                            current_block,
                            SsaTerminator::BranchBool {
                                condition: condition.value.id,
                                if_true: SsaBranchTarget::Exit(exit),
                                if_false: SsaBranchTarget::Block {
                                    target: loop_body_block,
                                    args: loop_backedge_args(&frame.stack, &frame.locals),
                                },
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    builder
                        .merge_exit_dirty_locals(&frame.dirty_locals)
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    terminal = Some(JitTraceTerminal::BranchExit);
                    break;
                }

                if condition.info.const_bool == Some(false) {
                    op_names.push("guard_true".to_string());
                    let exit = add_symbolic_exit(
                        &mut builder,
                        fallthrough_ip,
                        &frame,
                        inline_frame.as_ref(),
                    );
                    let (next_block, next_frame, args) = continue_with_inline_frame(
                        &mut builder,
                        &frame,
                        &mut inline_frame,
                        "guard",
                    )?;
                    builder
                        .set_terminator(
                            current_block,
                            SsaTerminator::BranchBool {
                                condition: condition.value.id,
                                if_true: SsaBranchTarget::Exit(exit),
                                if_false: SsaBranchTarget::Block {
                                    target: next_block,
                                    args,
                                },
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    current_block = next_block;
                    frame = next_frame;
                    cursor.jump_to(target)?;
                    has_useful_native_computation |= is_useful_native_computation;
                    continue;
                }

                if prefer_join_path {
                    op_names.push("guard_true".to_string());
                    let exit = add_symbolic_exit(
                        &mut builder,
                        fallthrough_ip,
                        &frame,
                        inline_frame.as_ref(),
                    );
                    let (next_block, next_frame, args) = continue_with_inline_frame(
                        &mut builder,
                        &frame,
                        &mut inline_frame,
                        "guard",
                    )?;
                    builder
                        .set_terminator(
                            current_block,
                            SsaTerminator::BranchBool {
                                condition: condition.value.id,
                                if_true: SsaBranchTarget::Exit(exit),
                                if_false: SsaBranchTarget::Block {
                                    target: next_block,
                                    args,
                                },
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    current_block = next_block;
                    frame = next_frame;
                    cursor.jump_to(target)?;
                    has_useful_native_computation |= is_useful_native_computation;
                    continue;
                }

                op_names.push("guard_false".to_string());
                let exit = add_symbolic_exit(&mut builder, target, &frame, inline_frame.as_ref());
                let (next_block, next_frame, args) =
                    continue_with_inline_frame(&mut builder, &frame, &mut inline_frame, "guard")?;
                builder
                    .set_terminator(
                        current_block,
                        SsaTerminator::BranchBool {
                            condition: condition.value.id,
                            if_true: SsaBranchTarget::Block {
                                target: next_block,
                                args,
                            },
                            if_false: SsaBranchTarget::Exit(exit),
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                current_block = next_block;
                frame = next_frame;
            }
            DecodedOp::Br { target, .. } => {
                if target == root_ip {
                    op_names.push("jump_root".to_string());
                    if frame.stack.len() != entry_stack_depth {
                        return Err(TraceRecordError::StackDepthMismatch {
                            expected: entry_stack_depth,
                            got: frame.stack.len(),
                        });
                    }
                    builder
                        .set_terminator(
                            current_block,
                            SsaTerminator::Jump {
                                target: loop_body_block,
                                args: loop_backedge_args(&frame.stack, &frame.locals),
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    builder
                        .merge_exit_dirty_locals(&frame.dirty_locals)
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    terminal = Some(JitTraceTerminal::LoopBack);
                    break;
                }
                if target < cursor.ip() {
                    op_names.push("jump_ip".to_string());
                    let exit =
                        add_symbolic_exit(&mut builder, target, &frame, inline_frame.as_ref());
                    builder
                        .set_terminator(current_block, SsaTerminator::Exit { exit })
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    terminal = Some(JitTraceTerminal::BranchExit);
                    break;
                }
                cursor.jump_to(target)?;
            }

            DecodedOp::CallValue {
                ip,
                argc,
                resume_ip,
            } => {
                if frame.stack.len() < usize::from(argc) + 1 {
                    return Err(TraceRecordError::StackUnderflow);
                }
                let callable = frame.stack[frame.stack.len() - usize::from(argc) - 1];
                let caller_prototype_id = (caller_frame_key != crate::vm::native::ROOT_FRAME_KEY)
                    .then_some(caller_frame_key as u32);
                let candidate = classify_static_inline_candidate(
                    program,
                    caller_frame_key,
                    caller_prototype_id,
                    callable.info.source_local,
                    argc,
                    max_trace_len.saturating_sub(cursor.recorded_ops),
                )
                .and_then(|candidate| {
                    let prototypes =
                        entry_callable_prototypes.ok_or(InlineRejectReason::UnknownTarget)?;
                    let source_local = callable
                        .info
                        .source_local
                        .ok_or(InlineRejectReason::UnknownTarget)?;
                    if prototypes.get(usize::from(source_local)).copied().flatten()
                        != Some(candidate.prototype_id)
                    {
                        return Err(InlineRejectReason::PolymorphicTarget);
                    }
                    let prototype = &program.callable_prototypes[candidate.prototype_id as usize];
                    let argument_start = frame.stack.len() - usize::from(argc);
                    inline_argument_schemas_supported(
                        &frame.stack[argument_start..],
                        prototype.schema.as_ref(),
                    )
                    .then_some(candidate)
                    .ok_or(InlineRejectReason::SchemaUnproven)
                });
                let inline_reject_reason = candidate.as_ref().err().copied();
                if inline_frame.is_none()
                    && let Ok(candidate) = candidate
                {
                    let source_local =
                        callable
                            .info
                            .source_local
                            .ok_or(TraceRecordError::UnsupportedTrace(
                                "inline candidate lost callable source local".to_string(),
                            ))?;
                    let entry_guard = (source_local, candidate.prototype_id);
                    if !entry_callable_guards.contains(&entry_guard) {
                        entry_callable_guards.push(entry_guard);
                    }
                    let prototype = &program.callable_prototypes[candidate.prototype_id as usize];
                    let argument_start = frame.stack.len() - usize::from(argc);
                    let schema_guard = append_inline_argument_schema_guards(
                        &mut builder,
                        current_block,
                        ip,
                        &frame.stack[argument_start..],
                        prototype.schema.as_ref(),
                    )?;
                    if let Some(schema_guard) = schema_guard {
                        let schema_exit =
                            add_symbolic_exit(&mut builder, ip, &frame, inline_frame.as_ref());
                        let (guarded_block, guarded_frame, guard_args) =
                            continue_with_inline_frame(
                                &mut builder,
                                &frame,
                                &mut inline_frame,
                                "inline_callable_schema",
                            )?;
                        builder
                            .set_terminator(
                                current_block,
                                SsaTerminator::BranchBool {
                                    condition: schema_guard,
                                    if_true: SsaBranchTarget::Block {
                                        target: guarded_block,
                                        args: guard_args,
                                    },
                                    if_false: SsaBranchTarget::Exit(schema_exit),
                                },
                            )
                            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                        current_block = guarded_block;
                        frame = guarded_frame;
                        op_names.push("inline_callable_schema_guard".to_string());
                    }

                    let operand_base = frame.stack.len() - usize::from(argc) - 1;
                    let mut operands = frame.stack.split_off(operand_base);
                    let _callable = operands.remove(0);
                    let prototype = &program.callable_prototypes[candidate.prototype_id as usize];
                    let null = builder
                        .append_value_inst(
                            current_block,
                            ip,
                            SsaValueRepr::Tagged,
                            SsaInstKind::Constant(Value::Null),
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    let null = SymbolicValue {
                        value: null,
                        info: ValueInfo::tagged_typed(ValueType::Null),
                    };
                    let mut callee_locals = vec![null; prototype.frame_local_count];
                    for binding in &program.root_callable_bindings {
                        let slot = usize::from(binding.local_slot);
                        if slot < callee_locals.len() && slot < frame.locals.len() {
                            callee_locals[slot] = frame.locals[slot];
                        }
                    }
                    for (slot, mut argument) in candidate.parameter_slots.iter().zip(operands) {
                        if argument.info.repr == SsaValueRepr::Tagged {
                            let cloned = builder
                                .append_value_inst(
                                    current_block,
                                    ip,
                                    SsaValueRepr::Tagged,
                                    SsaInstKind::CloneTagged {
                                        input: argument.value.id,
                                    },
                                )
                                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                            argument.value = cloned;
                        }
                        callee_locals[usize::from(*slot)] = argument;
                    }
                    op_names.push(format!("inline_call:{}", candidate.prototype_id));
                    let caller = std::mem::replace(
                        &mut frame,
                        SymbolicFrame::new(Vec::new(), callee_locals),
                    );
                    inline_frame = Some(InlineRecorderFrame {
                        candidate: candidate.clone(),
                        call_ip: ip,
                        return_ip: resume_ip,
                        caller,
                    });
                    cursor.jump_to(candidate.entry_ip)?;
                    has_call = true;
                    continue;
                }
                if let Some(reason) = inline_reject_reason {
                    op_names.push(format!("inline_reject:{reason:?}"));
                } else if inline_frame.is_some() {
                    op_names.push("inline_reject:NestedCallable".to_string());
                }
                op_names.push("call_value".to_string());
                let exit = add_symbolic_exit(&mut builder, ip, &frame, inline_frame.as_ref());
                builder
                    .set_terminator(
                        current_block,
                        SsaTerminator::CallValue {
                            argc,
                            call_ip: ip,
                            resume_ip,
                            exit,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                has_call = true;
                has_yielding_call = true;
                terminal = Some(JitTraceTerminal::CallValue);
                break;
            }
            DecodedOp::Call {
                ip,
                index,
                builtin,
                argc,
                yields,
            } => {
                if builtin == Some(BuiltinFunction::ArrayNew) && argc == 0 {
                    let (name, out) = emit_specialized_builtin_call(
                        &mut builder,
                        current_block,
                        ip,
                        &mut frame,
                        SpecializedBuiltinKind::ArrayNew,
                    )?;
                    op_names.push(name.to_string());
                    frame.push(out);
                    has_useful_native_computation = true;
                    continue;
                }
                if let Some(builtin) = builtin
                    && argc > 0
                {
                    let args = call_arg_slice(&frame.stack, usize::from(argc))?;
                    let container_source = args[0].info.source_local;
                    let container_was_moved = container_source.is_some_and(|local| {
                        frame.dirty_locals[usize::from(local)]
                            && frame.locals[usize::from(local)].info.known_type
                                == Some(ValueType::Null)
                            && args[1..]
                                .iter()
                                .all(|arg| arg.info.source_local != Some(local))
                    });
                    let specialized_kind = select_specialized_builtin_kind(
                        program,
                        ip,
                        builtin,
                        args[0].info,
                        container_was_moved,
                    );
                    if let Some(kind) = specialized_kind {
                        let (name, out) = emit_specialized_builtin_call(
                            &mut builder,
                            current_block,
                            ip,
                            &mut frame,
                            kind,
                        )?;
                        op_names.push(name.to_string());
                        frame.push(out);
                        has_useful_native_computation = true;
                        continue;
                    }
                }
                if builtin.is_none()
                    && non_yielding_host_imports
                        .get(usize::from(index))
                        .copied()
                        .unwrap_or(false)
                {
                    let args = call_arg_slice(&frame.stack, usize::from(argc))?
                        .iter()
                        .map(|arg| arg.value.id)
                        .collect::<Vec<_>>();
                    for _ in 0..argc {
                        let _ = frame.pop()?;
                    }
                    let return_type = program
                        .imports
                        .get(usize::from(index))
                        .map(|import| import.return_type)
                        .unwrap_or(ValueType::Unknown);
                    let (return_repr, return_info) = match return_type {
                        ValueType::Int => (SsaValueRepr::I64, ValueInfo::int(None)),
                        ValueType::Float => (SsaValueRepr::F64, ValueInfo::float(None)),
                        ValueType::Bool => (SsaValueRepr::Bool, ValueInfo::bool(None)),
                        _ => (SsaValueRepr::Tagged, ValueInfo::tagged_typed(return_type)),
                    };
                    let value = builder
                        .append_value_inst(
                            current_block,
                            ip,
                            return_repr,
                            SsaInstKind::HostCall {
                                import: index,
                                args,
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    frame.push(SymbolicValue {
                        value,
                        info: return_info,
                    });
                    has_call = true;
                    op_names.push("host_call".to_string());
                    has_useful_native_computation = true;
                    continue;
                }
                if !has_useful_native_computation {
                    return Err(TraceRecordError::UnsupportedTrace(
                        "zero-benefit call-boundary trace".to_string(),
                    ));
                }
                has_call = true;
                has_yielding_call |= yields;
                op_names.push("call".to_string());
                let exit = add_symbolic_exit(&mut builder, ip, &frame, inline_frame.as_ref());
                builder
                    .set_terminator(current_block, SsaTerminator::Exit { exit })
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                terminal = Some(JitTraceTerminal::BranchExit);
                break;
            }
        }
        has_useful_native_computation |= is_useful_native_computation;
    }

    let terminal = terminal.ok_or(TraceRecordError::MissingTerminal)?;
    if loop_header_plan.is_some()
        && entry_callable_guards.iter().any(|(local, _)| {
            let local = usize::from(*local);
            frame.dirty_locals.get(local).copied().unwrap_or(false)
                || inline_frame
                    .as_ref()
                    .and_then(|inlined| inlined.caller.dirty_locals.get(local))
                    .copied()
                    .unwrap_or(false)
        })
    {
        return Err(TraceRecordError::UnsupportedTrace(
            "inline callable source local is mutated by the native loop".to_string(),
        ));
    }
    let ssa = builder.finish();
    ssa.verify()
        .map_err(|err| TraceRecordError::InvalidIr(format!("{err:?}")))?;

    Ok(RecordedTrace {
        has_call,
        has_yielding_call,
        entry_callable_guards,
        op_names,
        ssa,
        terminal,
    })
}

#[allow(clippy::too_many_arguments)]
fn infer_loop_header_plan(
    program: &Program,
    caller_frame_key: u64,
    root_ip: usize,
    entry_stack_depth: usize,
    local_count: usize,
    entry_local_types: Option<&[ValueType]>,
    max_trace_len: usize,
    non_yielding_host_imports: &[bool],
) -> Result<Option<LoopHeaderPlan>, TraceRecordError> {
    let mut cursor = TraceCursor::new(program, root_ip, max_trace_len);
    let mut frame = AnalysisFrame::new(program, entry_stack_depth, local_count, entry_local_types);
    let mut entry_use = vec![EntryUseState::Untouched; local_count];
    let mut local_written = vec![false; local_count];
    let mut inline_frame: Option<(AnalysisFrame, usize)> = None;

    loop {
        let Some(decoded) = cursor.next()? else {
            return Ok(None);
        };

        match decoded {
            DecodedOp::Nop { .. } => {}
            DecodedOp::Ret { .. } => {
                let Some((mut caller, return_ip)) = inline_frame.take() else {
                    return Ok(None);
                };
                let result = frame
                    .stack
                    .pop()
                    .unwrap_or_else(|| ValueInfo::tagged_typed(ValueType::Null));
                caller.push(result);
                frame = caller;
                cursor.jump_to(return_ip)?;
            }
            DecodedOp::Ldc { index, .. } => {
                let constant = program.constants.get(index as usize).ok_or_else(|| {
                    TraceRecordError::UnsupportedTrace(format!(
                        "unsupported constant #{index}: missing constant"
                    ))
                })?;
                frame.push(ValueInfo::from_constant(constant));
            }
            DecodedOp::Ldloc { index, .. } => {
                if inline_frame.is_none()
                    && let Some(state) = entry_use.get_mut(index as usize)
                    && matches!(*state, EntryUseState::Untouched)
                {
                    *state = EntryUseState::ReadBeforeWrite;
                }
                frame.push(frame.local(index)?.sourced_from(index))
            }
            DecodedOp::Stloc { index, .. } => {
                if inline_frame.is_none()
                    && let Some(written) = local_written.get_mut(index as usize)
                {
                    *written = true;
                }
                if inline_frame.is_none()
                    && let Some(state) = entry_use.get_mut(index as usize)
                    && matches!(*state, EntryUseState::Untouched)
                {
                    *state = EntryUseState::WrittenBeforeRead;
                }
                let value = frame.pop()?;
                frame.store_local(index, value)?;
            }
            DecodedOp::Pop { .. } => {
                let _ = frame.pop()?;
            }
            DecodedOp::Dup { .. } => {
                let value = *frame.stack.last().ok_or(TraceRecordError::StackUnderflow)?;
                frame.push(value);
            }
            DecodedOp::Neg { .. } => {
                let value = frame.pop()?;
                let result = match select_numeric_neg(value)? {
                    NumericUnaryKind::Int => {
                        let value = expect_int_info(value)?;
                        ValueInfo::int(value.const_int.map(i64::wrapping_neg))
                    }
                    NumericUnaryKind::Float => {
                        let value = expect_float_info(value)?;
                        ValueInfo::float(value.const_float.map(std::ops::Neg::neg))
                    }
                };
                frame.push(result);
            }
            DecodedOp::Not { .. } => {
                let value = expect_bool_info(frame.pop()?)?;
                frame.push(ValueInfo::bool(value.const_bool.map(|value| !value)));
            }
            DecodedOp::BinOp { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                if opcode == OpCode::And as u8 || opcode == OpCode::Or as u8 {
                    let kind = select_bool_binop(program, ip, opcode, lhs, rhs)?;
                    let lhs = expect_bool_info(lhs)?;
                    let rhs = expect_bool_info(rhs)?;
                    frame.push(result_info_for_bool_binop(kind, lhs, rhs));
                } else {
                    match select_numeric_binop(program, ip, opcode, lhs, rhs)? {
                        NumericBinOpKind::Int(kind) => {
                            let lhs = expect_int_info(lhs)?;
                            let rhs = expect_int_info(rhs)?;
                            validate_int_operands(program, ip, kind, lhs, rhs)?;
                            frame.push(result_info_for_int_binop(kind, lhs, rhs)?);
                        }
                        NumericBinOpKind::Float(kind) => {
                            let lhs = expect_float_info(lhs)?;
                            let rhs = expect_float_info(rhs)?;
                            validate_float_operands(program, ip, kind, lhs, rhs)?;
                            frame.push(result_info_for_float_binop(kind, lhs, rhs));
                        }
                        NumericBinOpKind::Concat(kind) => {
                            frame.push(result_info_for_concat_binop(kind));
                        }
                    }
                }
            }
            DecodedOp::Compare { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                match select_numeric_compare(program, ip, opcode, lhs, rhs) {
                    Ok(NumericCompareKind::Int(kind)) => {
                        let lhs = expect_int_info(lhs)?;
                        let rhs = expect_int_info(rhs)?;
                        validate_int_compare_operands(program, ip, kind, lhs, rhs)?;
                        frame.push(result_info_for_int_compare(kind, lhs, rhs));
                    }
                    Ok(NumericCompareKind::Float(kind)) => {
                        let lhs = expect_float_info(lhs)?;
                        let rhs = expect_float_info(rhs)?;
                        validate_float_compare_operands(program, ip, kind, lhs, rhs)?;
                        frame.push(result_info_for_float_compare(kind, lhs, rhs));
                    }
                    Err(_)
                        if opcode == OpCode::Ceq as u8
                            && lhs.repr == SsaValueRepr::Tagged
                            && rhs.repr == SsaValueRepr::Tagged =>
                    {
                        frame.push(ValueInfo::bool(None));
                    }
                    Err(err) => return Err(err),
                }
            }
            DecodedOp::Call {
                ip,
                builtin: Some(builtin),
                argc,
                ..
            } => {
                if argc == 0 {
                    if builtin == BuiltinFunction::ArrayNew {
                        let _ = analyze_specialized_builtin_call(
                            &mut frame,
                            SpecializedBuiltinKind::ArrayNew,
                        )?;
                        continue;
                    }
                    return Ok(None);
                }
                let args = call_arg_slice(&frame.stack, usize::from(argc))?;
                let container_source = args[0].source_local;
                let container_was_moved = container_source.is_some_and(|local| {
                    frame.locals[usize::from(local)].known_type == Some(ValueType::Null)
                        && args[1..].iter().all(|arg| arg.source_local != Some(local))
                });
                let Some(kind) = select_specialized_builtin_kind(
                    program,
                    ip,
                    builtin,
                    args[0],
                    container_was_moved,
                ) else {
                    return Ok(None);
                };
                let _ = analyze_specialized_builtin_call(&mut frame, kind)?;
            }
            DecodedOp::Call {
                index,
                builtin: None,
                argc,
                ..
            } if non_yielding_host_imports
                .get(usize::from(index))
                .copied()
                .unwrap_or(false) =>
            {
                for _ in 0..argc {
                    let _ = frame.pop()?;
                }
                let return_type = program
                    .imports
                    .get(usize::from(index))
                    .map(|import| import.return_type)
                    .unwrap_or(ValueType::Unknown);
                frame.push(match return_type {
                    ValueType::Int => ValueInfo::int(None),
                    ValueType::Float => ValueInfo::float(None),
                    ValueType::Bool => ValueInfo::bool(None),
                    _ => ValueInfo::tagged_typed(return_type),
                });
            }
            DecodedOp::CallValue {
                argc, resume_ip, ..
            } => {
                if inline_frame.is_some() || frame.stack.len() < usize::from(argc) + 1 {
                    return Ok(None);
                }
                let callable = frame.stack[frame.stack.len() - usize::from(argc) - 1];
                let caller_prototype_id = (caller_frame_key != crate::vm::native::ROOT_FRAME_KEY)
                    .then_some(caller_frame_key as u32);
                let Ok(candidate) = classify_static_inline_candidate(
                    program,
                    caller_frame_key,
                    caller_prototype_id,
                    callable.source_local,
                    argc,
                    max_trace_len.saturating_sub(cursor.recorded_ops),
                ) else {
                    return Ok(None);
                };
                let prototype = &program.callable_prototypes[candidate.prototype_id as usize];
                let operand_base = frame.stack.len() - usize::from(argc) - 1;
                let mut operands = frame.stack.split_off(operand_base);
                let _callable = operands.remove(0);
                let null = ValueInfo::tagged_typed(ValueType::Null);
                let mut callee_locals = vec![null; prototype.frame_local_count];
                for binding in &program.root_callable_bindings {
                    let slot = usize::from(binding.local_slot);
                    if slot < callee_locals.len() && slot < frame.locals.len() {
                        callee_locals[slot] = frame.locals[slot];
                    }
                }
                for (slot, argument) in candidate.parameter_slots.iter().zip(operands) {
                    callee_locals[usize::from(*slot)] = argument;
                }
                let caller = std::mem::replace(
                    &mut frame,
                    AnalysisFrame {
                        stack: Vec::new(),
                        locals: callee_locals,
                    },
                );
                inline_frame = Some((caller, resume_ip));
                cursor.jump_to(candidate.entry_ip)?;
            }
            DecodedOp::Call { .. } => return Ok(None),
            DecodedOp::Brfalse {
                target,
                fallthrough_ip,
                ..
            } => {
                let condition = expect_bool_info(frame.pop()?)?;
                if target < fallthrough_ip {
                    if target != root_ip {
                        return Err(TraceRecordError::UnsupportedTrace(format!(
                            "unsupported backward brfalse target {target}"
                        )));
                    }
                    if frame.stack.len() != entry_stack_depth {
                        return Err(TraceRecordError::StackDepthMismatch {
                            expected: entry_stack_depth,
                            got: frame.stack.len(),
                        });
                    }
                    return Ok(Some(build_loop_header_plan(
                        &frame.stack,
                        &frame.locals,
                        &entry_use,
                        &local_written,
                    )));
                }
                if condition.const_bool == Some(false) {
                    cursor.jump_to(target)?;
                }
            }
            DecodedOp::Br { target, .. } => {
                if target == root_ip {
                    if frame.stack.len() != entry_stack_depth {
                        return Err(TraceRecordError::StackDepthMismatch {
                            expected: entry_stack_depth,
                            got: frame.stack.len(),
                        });
                    }
                    return Ok(Some(build_loop_header_plan(
                        &frame.stack,
                        &frame.locals,
                        &entry_use,
                        &local_written,
                    )));
                }
                if target < cursor.ip() {
                    return Ok(None);
                }
                cursor.jump_to(target)?;
            }
        }
    }
}

fn select_numeric_neg(value: ValueInfo) -> Result<NumericUnaryKind, TraceRecordError> {
    match value.repr {
        SsaValueRepr::I64 => Ok(NumericUnaryKind::Int),
        SsaValueRepr::F64 => Ok(NumericUnaryKind::Float),
        SsaValueRepr::Tagged if value.const_int.is_some() => Ok(NumericUnaryKind::Int),
        SsaValueRepr::Tagged if value.const_float.is_some() => Ok(NumericUnaryKind::Float),
        _ => Err(TraceRecordError::UnsupportedTrace(
            "SSA recorder requires numeric specialization for neg traces".to_string(),
        )),
    }
}

fn select_bool_binop(
    program: &Program,
    ip: usize,
    opcode: u8,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<BoolBinOpKind, TraceRecordError> {
    let operand_types = operand_types(program, ip);
    let bool_like = matches!(lhs.repr, SsaValueRepr::Bool | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::Bool | SsaValueRepr::Tagged);
    let has_bool_evidence = lhs.repr == SsaValueRepr::Bool
        || rhs.repr == SsaValueRepr::Bool
        || lhs.const_bool.is_some()
        || rhs.const_bool.is_some()
        || lhs.known_type == Some(ValueType::Bool)
        || rhs.known_type == Some(ValueType::Bool);

    let kind = match opcode {
        x if x == OpCode::And as u8 => BoolBinOpKind::And,
        x if x == OpCode::Or as u8 => BoolBinOpKind::Or,
        _ => {
            return Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder expected a boolean binop opcode".to_string(),
            ));
        }
    };

    match operand_types {
        (ValueType::Bool, ValueType::Bool) => Ok(kind),
        (ValueType::Unknown, ValueType::Unknown)
        | (ValueType::Bool, ValueType::Unknown)
        | (ValueType::Unknown, ValueType::Bool)
            if bool_like && has_bool_evidence =>
        {
            Ok(kind)
        }
        _ => Err(TraceRecordError::UnsupportedTrace(
            "SSA recorder requires bool-specializable eager boolean operands".to_string(),
        )),
    }
}

fn observed_concat_binop_kind(lhs: ValueInfo, rhs: ValueInfo) -> Option<ConcatBinOpKind> {
    match (
        observed_heap_container_kind(lhs),
        observed_heap_container_kind(rhs),
    ) {
        (Some(HeapContainerKind::String), Some(HeapContainerKind::String)) => {
            Some(ConcatBinOpKind::String)
        }
        (Some(HeapContainerKind::Bytes), Some(HeapContainerKind::Bytes)) => {
            Some(ConcatBinOpKind::Bytes)
        }
        _ => None,
    }
}

fn select_numeric_binop(
    program: &Program,
    ip: usize,
    opcode: u8,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<NumericBinOpKind, TraceRecordError> {
    if lhs.repr == SsaValueRepr::I64 && rhs.repr == SsaValueRepr::I64 {
        let kind = match opcode {
            x if x == OpCode::Add as u8 => IntBinOpKind::Add,
            x if x == OpCode::Sub as u8 => IntBinOpKind::Sub,
            x if x == OpCode::Mul as u8 => IntBinOpKind::Mul,
            x if x == OpCode::Div as u8 => IntBinOpKind::Div,
            x if x == OpCode::Mod as u8 => IntBinOpKind::Mod,
            x if x == OpCode::Shl as u8 => IntBinOpKind::Shl,
            x if x == OpCode::Shr as u8 => IntBinOpKind::Shr,
            x if x == OpCode::Lshr as u8 => IntBinOpKind::Lshr,
            _ => {
                return Err(TraceRecordError::UnsupportedTrace(
                    "SSA recorder expected a numeric binop opcode".to_string(),
                ));
            }
        };
        return Ok(NumericBinOpKind::Int(kind));
    }
    if lhs.repr == SsaValueRepr::F64 && rhs.repr == SsaValueRepr::F64 {
        let kind = match opcode {
            x if x == OpCode::Add as u8 => FloatBinOpKind::Add,
            x if x == OpCode::Sub as u8 => FloatBinOpKind::Sub,
            x if x == OpCode::Mul as u8 => FloatBinOpKind::Mul,
            x if x == OpCode::Div as u8 => FloatBinOpKind::Div,
            x if x == OpCode::Mod as u8 => FloatBinOpKind::Mod,
            _ => {
                return Err(TraceRecordError::UnsupportedTrace(
                    "SSA recorder expected a numeric binop opcode".to_string(),
                ));
            }
        };
        return Ok(NumericBinOpKind::Float(kind));
    }
    let static_operand_types = operand_types(program, ip);
    let operand_types = (
        lhs.known_type.unwrap_or(static_operand_types.0),
        rhs.known_type.unwrap_or(static_operand_types.1),
    );
    let observed_concat = observed_concat_binop_kind(lhs, rhs);
    let int_like = matches!(lhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged);
    let float_like = matches!(lhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged);
    let has_int_evidence = lhs.repr == SsaValueRepr::I64
        || rhs.repr == SsaValueRepr::I64
        || lhs.const_int.is_some()
        || rhs.const_int.is_some();
    let has_float_evidence = lhs.repr == SsaValueRepr::F64
        || rhs.repr == SsaValueRepr::F64
        || lhs.const_float.is_some()
        || rhs.const_float.is_some();

    match opcode {
        x if x == OpCode::Add as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericBinOpKind::Int(IntBinOpKind::Add)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Add))
            }
            (ValueType::String, ValueType::String) => {
                Ok(NumericBinOpKind::Concat(ConcatBinOpKind::String))
            }
            (ValueType::Bytes, ValueType::Bytes) => {
                Ok(NumericBinOpKind::Concat(ConcatBinOpKind::Bytes))
            }
            (ValueType::String, ValueType::Unknown) | (ValueType::Unknown, ValueType::String)
                if observed_concat == Some(ConcatBinOpKind::String) =>
            {
                Ok(NumericBinOpKind::Concat(ConcatBinOpKind::String))
            }
            (ValueType::Bytes, ValueType::Unknown) | (ValueType::Unknown, ValueType::Bytes)
                if observed_concat == Some(ConcatBinOpKind::Bytes) =>
            {
                Ok(NumericBinOpKind::Concat(ConcatBinOpKind::Bytes))
            }
            (ValueType::Unknown, ValueType::Unknown) if observed_concat.is_some() => Ok(
                NumericBinOpKind::Concat(observed_concat.expect("concat kind checked above")),
            ),
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Add))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Add))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(format!(
                "SSA recorder requires int- or float-specializable add operands (types={operand_types:?}, lhs={:?}, rhs={:?})",
                lhs.repr, rhs.repr
            ))),
        },
        x if x == OpCode::Sub as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericBinOpKind::Int(IntBinOpKind::Sub)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Sub))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Sub))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Sub))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable sub operands".to_string(),
            )),
        },
        x if x == OpCode::Mul as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericBinOpKind::Int(IntBinOpKind::Mul)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Mul))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Mul))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Mul))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable mul operands".to_string(),
            )),
        },
        x if x == OpCode::Div as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericBinOpKind::Int(IntBinOpKind::Div)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Div))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Div))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Div))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable div operands".to_string(),
            )),
        },
        x if x == OpCode::Mod as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericBinOpKind::Int(IntBinOpKind::Mod)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Mod))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Mod))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Mod))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable mod operands".to_string(),
            )),
        },
        x if x == OpCode::Shl as u8 => match operand_types {
            (ValueType::Int, ValueType::Int)
            | (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Shl))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int-specializable shift operands".to_string(),
            )),
        },
        x if x == OpCode::Shr as u8 => match operand_types {
            (ValueType::Int, ValueType::Int)
            | (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Shr))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int-specializable shift operands".to_string(),
            )),
        },
        x if x == OpCode::Lshr as u8 => match operand_types {
            (ValueType::Int, ValueType::Int)
            | (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericBinOpKind::Int(IntBinOpKind::Lshr))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int-specializable shift operands".to_string(),
            )),
        },
        _ => Err(TraceRecordError::UnsupportedTrace(
            "SSA recorder expected a numeric binop opcode".to_string(),
        )),
    }
}

fn select_numeric_compare(
    program: &Program,
    ip: usize,
    opcode: u8,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<NumericCompareKind, TraceRecordError> {
    if lhs.force_value_eq || rhs.force_value_eq {
        return Err(TraceRecordError::UnsupportedTrace(
            "SSA recorder requires value equality for known non-numeric operands".to_string(),
        ));
    }
    if lhs.repr == SsaValueRepr::I64 && rhs.repr == SsaValueRepr::I64 {
        return match opcode {
            x if x == OpCode::Ceq as u8 => Ok(NumericCompareKind::Int(IntCompareKind::Eq)),
            x if x == OpCode::Clt as u8 => Ok(NumericCompareKind::Int(IntCompareKind::Lt)),
            x if x == OpCode::Cgt as u8 => Ok(NumericCompareKind::Int(IntCompareKind::Gt)),
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder expected a numeric comparison opcode".to_string(),
            )),
        };
    }
    if lhs.repr == SsaValueRepr::F64 && rhs.repr == SsaValueRepr::F64 {
        return match opcode {
            x if x == OpCode::Ceq as u8 => Ok(NumericCompareKind::Float(FloatCompareKind::Eq)),
            x if x == OpCode::Clt as u8 => Ok(NumericCompareKind::Float(FloatCompareKind::Lt)),
            x if x == OpCode::Cgt as u8 => Ok(NumericCompareKind::Float(FloatCompareKind::Gt)),
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder expected a numeric comparison opcode".to_string(),
            )),
        };
    }
    let operand_types = operand_types(program, ip);
    let int_like = matches!(lhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged);
    let float_like = matches!(lhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged);
    let has_int_evidence = lhs.repr == SsaValueRepr::I64
        || rhs.repr == SsaValueRepr::I64
        || lhs.const_int.is_some()
        || rhs.const_int.is_some();
    let has_float_evidence = lhs.repr == SsaValueRepr::F64
        || rhs.repr == SsaValueRepr::F64
        || lhs.const_float.is_some()
        || rhs.const_float.is_some();

    match opcode {
        x if x == OpCode::Ceq as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericCompareKind::Int(IntCompareKind::Eq)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericCompareKind::Float(FloatCompareKind::Eq))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericCompareKind::Int(IntCompareKind::Eq))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericCompareKind::Float(FloatCompareKind::Eq))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable equality operands".to_string(),
            )),
        },
        x if x == OpCode::Clt as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericCompareKind::Int(IntCompareKind::Lt)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericCompareKind::Float(FloatCompareKind::Lt))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericCompareKind::Int(IntCompareKind::Lt))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericCompareKind::Float(FloatCompareKind::Lt))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable less-than operands".to_string(),
            )),
        },
        x if x == OpCode::Cgt as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericCompareKind::Int(IntCompareKind::Gt)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericCompareKind::Float(FloatCompareKind::Gt))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Int, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Int)
                if int_like && (has_int_evidence || !has_float_evidence) =>
            {
                Ok(NumericCompareKind::Int(IntCompareKind::Gt))
            }
            (ValueType::Unknown, ValueType::Unknown)
            | (ValueType::Float, ValueType::Unknown)
            | (ValueType::Unknown, ValueType::Float)
                if float_like && has_float_evidence =>
            {
                Ok(NumericCompareKind::Float(FloatCompareKind::Gt))
            }
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable greater-than operands"
                    .to_string(),
            )),
        },
        _ => Err(TraceRecordError::UnsupportedTrace(
            "SSA recorder expected a numeric comparison opcode".to_string(),
        )),
    }
}

fn emit_int_neg(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let const_value = value.info.const_int;
    let value = builder
        .append_value_inst(
            block,
            ip,
            SsaValueRepr::I64,
            SsaInstKind::IntNeg {
                input: value.value.id,
            },
        )
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((
        "ineg",
        SymbolicValue {
            value,
            info: ValueInfo::int(const_value.map(i64::wrapping_neg)),
        },
    ))
}

fn emit_float_neg(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let const_value = value.info.const_float;
    let value = builder
        .append_value_inst(
            block,
            ip,
            SsaValueRepr::F64,
            SsaInstKind::FloatNeg {
                input: value.value.id,
            },
        )
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((
        "fneg",
        SymbolicValue {
            value,
            info: ValueInfo::float(const_value.map(std::ops::Neg::neg)),
        },
    ))
}

fn emit_int_binop(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    kind: IntBinOpKind,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    match kind {
        IntBinOpKind::Add => {
            if let Some(imm) = rhs.info.const_int {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntAddImm {
                            lhs: lhs.value.id,
                            imm,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "iadd_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntAdd {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "iadd",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Sub => {
            if let Some(imm) = rhs.info.const_int {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntSubImm {
                            lhs: lhs.value.id,
                            imm,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "isub_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntSub {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "isub",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Mul => {
            if let Some(imm) = rhs.info.const_int {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntMulImm {
                            lhs: lhs.value.id,
                            imm,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "imul_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntMul {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "imul",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Div => {
            if let Some(imm) = rhs.info.const_int {
                if imm == 0 {
                    return Err(TraceRecordError::UnsupportedTrace(
                        "SSA recorder does not record division-by-zero integer traces".to_string(),
                    ));
                }
                if imm == -1 {
                    return Err(TraceRecordError::UnsupportedTrace(
                        "SSA recorder does not support integer div traces with rhs -1 yet"
                            .to_string(),
                    ));
                }
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntDivImm {
                            lhs: lhs.value.id,
                            imm,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "idiv_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntDiv {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "idiv",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Mod => {
            if let Some(imm) = rhs.info.const_int {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntModImm {
                            lhs: lhs.value.id,
                            imm,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "imod_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntMod {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "imod",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Shl => {
            if let Some(amount) = rhs
                .info
                .const_int
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= 63)
            {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntShlImm {
                            lhs: lhs.value.id,
                            amount,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "ilocal_shl_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntShl {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "ishl",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Shr => {
            if let Some(amount) = rhs
                .info
                .const_int
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= 63)
            {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntShrImm {
                            lhs: lhs.value.id,
                            amount,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "ishr_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntShr {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "ishr",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
        IntBinOpKind::Lshr => {
            if let Some(amount) = rhs
                .info
                .const_int
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= 63)
            {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntLshrImm {
                            lhs: lhs.value.id,
                            amount,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "ilshr_imm",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            } else {
                let value = builder
                    .append_value_inst(
                        block,
                        ip,
                        SsaValueRepr::I64,
                        SsaInstKind::IntLshr {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    )
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                Ok((
                    "ilshr",
                    SymbolicValue {
                        value,
                        info: ValueInfo::int(None),
                    },
                ))
            }
        }
    }
}

fn emit_bool_binop(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    kind: BoolBinOpKind,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let (name, inst_kind, const_bool) = match kind {
        BoolBinOpKind::And => (
            "bool_and",
            SsaInstKind::BoolAnd {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
            lhs.info
                .const_bool
                .zip(rhs.info.const_bool)
                .map(|(lhs, rhs)| lhs && rhs),
        ),
        BoolBinOpKind::Or => (
            "bool_or",
            SsaInstKind::BoolOr {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
            lhs.info
                .const_bool
                .zip(rhs.info.const_bool)
                .map(|(lhs, rhs)| lhs || rhs),
        ),
    };
    let value = builder
        .append_value_inst(block, ip, SsaValueRepr::Bool, inst_kind)
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((
        name,
        SymbolicValue {
            value,
            info: ValueInfo::bool(const_bool),
        },
    ))
}

fn emit_bool_not(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let result = builder
        .append_value_inst(
            block,
            ip,
            SsaValueRepr::Bool,
            SsaInstKind::BoolNot {
                input: value.value.id,
            },
        )
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((
        "bool_not",
        SymbolicValue {
            value: result,
            info: ValueInfo::bool(value.info.const_bool.map(|value| !value)),
        },
    ))
}

fn emit_value_eq(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let result = builder
        .append_value_inst(
            block,
            ip,
            SsaValueRepr::Bool,
            SsaInstKind::ValueCmpEq {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        )
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((
        "value_eq",
        SymbolicValue {
            value: result,
            info: ValueInfo::bool(None),
        },
    ))
}

fn emit_int_compare(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    kind: IntCompareKind,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let (name, inst_kind) = match kind {
        IntCompareKind::Eq => (
            "ceq",
            SsaInstKind::IntCmpEq {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        IntCompareKind::Lt => {
            if let Some(imm) = rhs.info.const_int {
                (
                    "ilocal_clt_imm",
                    SsaInstKind::IntCmpLtImm {
                        lhs: lhs.value.id,
                        imm,
                    },
                )
            } else {
                (
                    "clt",
                    SsaInstKind::IntCmpLt {
                        lhs: lhs.value.id,
                        rhs: rhs.value.id,
                    },
                )
            }
        }
        IntCompareKind::Gt => {
            if let Some(imm) = rhs.info.const_int {
                (
                    "ilocal_cgt_imm",
                    SsaInstKind::IntCmpGtImm {
                        lhs: lhs.value.id,
                        imm,
                    },
                )
            } else {
                (
                    "cgt",
                    SsaInstKind::IntCmpGt {
                        lhs: lhs.value.id,
                        rhs: rhs.value.id,
                    },
                )
            }
        }
    };
    let value = builder
        .append_value_inst(block, ip, SsaValueRepr::Bool, inst_kind)
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    let info = result_info_for_int_compare(kind, lhs.info, rhs.info);
    Ok((name, SymbolicValue { value, info }))
}

fn emit_float_binop(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    kind: FloatBinOpKind,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let (name, inst_kind) = match kind {
        FloatBinOpKind::Add => (
            "fadd",
            SsaInstKind::FloatAdd {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        FloatBinOpKind::Sub => (
            "fsub",
            SsaInstKind::FloatSub {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        FloatBinOpKind::Mul => (
            "fmul",
            SsaInstKind::FloatMul {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        FloatBinOpKind::Div => (
            "fdiv",
            SsaInstKind::FloatDiv {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        FloatBinOpKind::Mod => (
            "fmod",
            SsaInstKind::FloatMod {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
    };
    let value = builder
        .append_value_inst(block, ip, SsaValueRepr::F64, inst_kind)
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((
        name,
        SymbolicValue {
            value,
            info: ValueInfo::float(None),
        },
    ))
}

fn emit_concat_binop(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    kind: ConcatBinOpKind,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let (name, inst_kind, info) = match kind {
        ConcatBinOpKind::String => {
            let lhs = ensure_heap_ptr(builder, block, ip, lhs, ValueType::String)?;
            let rhs = ensure_heap_ptr(builder, block, ip, rhs, ValueType::String)?;
            (
                "string_concat",
                SsaInstKind::StringConcat {
                    lhs: lhs.value.id,
                    rhs: rhs.value.id,
                },
                ValueInfo::tagged_typed(ValueType::String),
            )
        }
        ConcatBinOpKind::Bytes => {
            let lhs = ensure_heap_ptr(builder, block, ip, lhs, ValueType::Bytes)?;
            let rhs = ensure_heap_ptr(builder, block, ip, rhs, ValueType::Bytes)?;
            (
                "bytes_concat",
                SsaInstKind::BytesConcat {
                    lhs: lhs.value.id,
                    rhs: rhs.value.id,
                },
                ValueInfo::tagged_typed(ValueType::Bytes),
            )
        }
    };
    let value = builder
        .append_value_inst(block, ip, SsaValueRepr::Tagged, inst_kind)
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok((name, SymbolicValue { value, info }))
}

fn emit_float_compare(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    kind: FloatCompareKind,
    lhs: SymbolicValue,
    rhs: SymbolicValue,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    let (name, inst_kind) = match kind {
        FloatCompareKind::Eq => (
            "fceq",
            SsaInstKind::FloatCmpEq {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        FloatCompareKind::Lt => (
            "fclt",
            SsaInstKind::FloatCmpLt {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
        FloatCompareKind::Gt => (
            "fcgt",
            SsaInstKind::FloatCmpGt {
                lhs: lhs.value.id,
                rhs: rhs.value.id,
            },
        ),
    };
    let value = builder
        .append_value_inst(block, ip, SsaValueRepr::Bool, inst_kind)
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    let info = result_info_for_float_compare(kind, lhs.info, rhs.info);
    Ok((name, SymbolicValue { value, info }))
}

impl HeapContainerKind {
    fn value_type(self) -> ValueType {
        match self {
            Self::String => ValueType::String,
            Self::Bytes => ValueType::Bytes,
            Self::Array => ValueType::Array,
            Self::Map => ValueType::Map,
        }
    }
}

fn call_arg_slice<T>(stack: &[T], argc: usize) -> Result<&[T], TraceRecordError> {
    if stack.len() < argc {
        return Err(TraceRecordError::StackUnderflow);
    }
    Ok(&stack[stack.len() - argc..])
}

fn observed_heap_container_kind(info: ValueInfo) -> Option<HeapContainerKind> {
    match info.repr {
        SsaValueRepr::HeapPtr(ValueType::String) => Some(HeapContainerKind::String),
        SsaValueRepr::HeapPtr(ValueType::Bytes) => Some(HeapContainerKind::Bytes),
        SsaValueRepr::HeapPtr(ValueType::Array) => Some(HeapContainerKind::Array),
        SsaValueRepr::HeapPtr(ValueType::Map) => Some(HeapContainerKind::Map),
        _ => match info.known_type {
            Some(ValueType::String) => Some(HeapContainerKind::String),
            Some(ValueType::Bytes) => Some(HeapContainerKind::Bytes),
            Some(ValueType::Array) => Some(HeapContainerKind::Array),
            Some(ValueType::Map) => Some(HeapContainerKind::Map),
            _ => None,
        },
    }
}

fn select_specialized_builtin_kind(
    program: &Program,
    ip: usize,
    builtin: BuiltinFunction,
    container: ValueInfo,
    container_was_moved: bool,
) -> Option<SpecializedBuiltinKind> {
    match builtin {
        BuiltinFunction::ReMatch => return Some(SpecializedBuiltinKind::RegexMatch),
        BuiltinFunction::ReReplace => return Some(SpecializedBuiltinKind::RegexReplace),
        BuiltinFunction::StringContains => return Some(SpecializedBuiltinKind::StringContains),
        BuiltinFunction::StringLowerAscii => {
            return Some(SpecializedBuiltinKind::StringLowerAscii);
        }
        BuiltinFunction::TypeOf => {
            return Some(match container.known_type {
                Some(value_type) => SpecializedBuiltinKind::TypeOfKnown(value_type),
                None => SpecializedBuiltinKind::TypeOf,
            });
        }
        BuiltinFunction::ToString => {
            return Some(if container.known_type == Some(ValueType::String) {
                SpecializedBuiltinKind::ToStringIdentity
            } else {
                SpecializedBuiltinKind::ToString
            });
        }
        BuiltinFunction::StringSplitLiteral => {
            return Some(SpecializedBuiltinKind::StringSplitLiteral);
        }
        BuiltinFunction::BytesToUtf8 => {
            return Some(SpecializedBuiltinKind::BytesToUtf8Ascii);
        }
        BuiltinFunction::MapIterNext => return Some(SpecializedBuiltinKind::MapIterNext),
        BuiltinFunction::MapIterTakeKey => return Some(SpecializedBuiltinKind::MapIterTakeKey),
        BuiltinFunction::MapIterTakeValue => {
            return Some(SpecializedBuiltinKind::MapIterTakeValue);
        }
        _ => {}
    }
    let observed_kind = observed_heap_container_kind(container);
    let container_kind = if matches!(
        builtin,
        BuiltinFunction::Len
            | BuiltinFunction::Slice
            | BuiltinFunction::Get
            | BuiltinFunction::Has
            | BuiltinFunction::Concat
            | BuiltinFunction::StringReplaceLiteral
            | BuiltinFunction::Set
            | BuiltinFunction::ArrayPush
    ) {
        match operand_types(program, ip).0 {
            ValueType::String => Some(HeapContainerKind::String),
            ValueType::Bytes => Some(HeapContainerKind::Bytes),
            ValueType::Array => Some(HeapContainerKind::Array),
            ValueType::Map => Some(HeapContainerKind::Map),
            _ => observed_kind,
        }
    } else {
        observed_kind
    };
    let Some(container_kind) = container_kind else {
        return (builtin == BuiltinFunction::Len).then_some(SpecializedBuiltinKind::ValueLen);
    };

    match (builtin, container_kind) {
        (BuiltinFunction::Len, HeapContainerKind::String) => {
            Some(SpecializedBuiltinKind::StringLen)
        }
        (BuiltinFunction::Len, HeapContainerKind::Bytes) => Some(SpecializedBuiltinKind::BytesLen),
        (BuiltinFunction::Slice, HeapContainerKind::String) => {
            Some(SpecializedBuiltinKind::StringSlice)
        }
        (BuiltinFunction::Slice, HeapContainerKind::Bytes) => {
            Some(SpecializedBuiltinKind::BytesSlice)
        }
        (BuiltinFunction::Get, HeapContainerKind::String) => {
            Some(SpecializedBuiltinKind::StringGet)
        }
        (BuiltinFunction::Get, HeapContainerKind::Bytes) => Some(SpecializedBuiltinKind::BytesGet),
        (BuiltinFunction::Has, HeapContainerKind::Bytes) => Some(SpecializedBuiltinKind::BytesHas),
        (BuiltinFunction::StringReplaceLiteral, HeapContainerKind::String) => {
            Some(SpecializedBuiltinKind::StringReplaceLiteral)
        }
        (BuiltinFunction::Concat, HeapContainerKind::String) => {
            Some(SpecializedBuiltinKind::StringConcat)
        }
        (BuiltinFunction::Concat, HeapContainerKind::Bytes) => {
            Some(SpecializedBuiltinKind::BytesConcat)
        }
        (BuiltinFunction::BytesFromArrayU8, HeapContainerKind::Array) => {
            Some(SpecializedBuiltinKind::BytesFromArrayU8)
        }
        (BuiltinFunction::BytesToArrayU8, HeapContainerKind::Bytes) => {
            Some(SpecializedBuiltinKind::BytesToArrayU8)
        }
        (BuiltinFunction::Len, HeapContainerKind::Array) => Some(SpecializedBuiltinKind::ArrayLen),
        (BuiltinFunction::Get, HeapContainerKind::Array) => Some(SpecializedBuiltinKind::ArrayGet),
        (BuiltinFunction::Has, HeapContainerKind::Array) => Some(SpecializedBuiltinKind::ArrayHas),
        (BuiltinFunction::Set, HeapContainerKind::Array) if container_was_moved => {
            Some(SpecializedBuiltinKind::ArraySet)
        }
        (BuiltinFunction::ArrayPush, HeapContainerKind::Array) if container_was_moved => {
            Some(SpecializedBuiltinKind::ArrayPush)
        }
        (BuiltinFunction::Len, HeapContainerKind::Map) => Some(SpecializedBuiltinKind::MapLen),
        (BuiltinFunction::Get, HeapContainerKind::Map) => Some(SpecializedBuiltinKind::MapGet),
        (BuiltinFunction::Has, HeapContainerKind::Map) => Some(SpecializedBuiltinKind::MapHas),
        (BuiltinFunction::Set, HeapContainerKind::Map) if container_was_moved => {
            Some(SpecializedBuiltinKind::MapSet)
        }
        _ => None,
    }
}

fn analyze_specialized_builtin_call(
    frame: &mut AnalysisFrame,
    kind: SpecializedBuiltinKind,
) -> Result<&'static str, TraceRecordError> {
    match kind {
        SpecializedBuiltinKind::ValueLen => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::int(None));
            Ok("value_len")
        }
        SpecializedBuiltinKind::StringLen => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::int(None));
            Ok("string_len")
        }
        SpecializedBuiltinKind::BytesLen => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::int(None));
            Ok("bytes_len")
        }
        SpecializedBuiltinKind::StringSlice => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("string_slice")
        }
        SpecializedBuiltinKind::BytesSlice => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Bytes));
            Ok("bytes_slice")
        }
        SpecializedBuiltinKind::StringGet => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("string_get")
        }
        SpecializedBuiltinKind::BytesGet => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::int(None));
            Ok("bytes_get")
        }
        SpecializedBuiltinKind::BytesHas => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::bool(None));
            Ok("bytes_has")
        }
        SpecializedBuiltinKind::StringContains => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::bool(None));
            Ok("string_contains")
        }
        SpecializedBuiltinKind::RegexMatch => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::bool(None));
            Ok("regex_match")
        }
        SpecializedBuiltinKind::RegexReplace => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("regex_replace")
        }
        SpecializedBuiltinKind::StringReplaceLiteral => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("string_replace_literal")
        }
        SpecializedBuiltinKind::StringLowerAscii => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("string_lower_ascii")
        }
        SpecializedBuiltinKind::TypeOf | SpecializedBuiltinKind::TypeOfKnown(_) => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::type_name());
            Ok("type_of")
        }
        SpecializedBuiltinKind::ToString | SpecializedBuiltinKind::ToStringIdentity => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok(
                if matches!(kind, SpecializedBuiltinKind::ToStringIdentity) {
                    "to_string_identity"
                } else {
                    "to_string"
                },
            )
        }
        SpecializedBuiltinKind::StringSplitLiteral => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Array));
            Ok("string_split_literal")
        }
        SpecializedBuiltinKind::StringConcat => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("string_concat")
        }
        SpecializedBuiltinKind::BytesConcat => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Bytes));
            Ok("bytes_concat")
        }
        SpecializedBuiltinKind::BytesFromArrayU8 => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Bytes));
            Ok("bytes_from_array_u8")
        }
        SpecializedBuiltinKind::BytesToUtf8Ascii => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::String));
            Ok("bytes_to_utf8_ascii")
        }
        SpecializedBuiltinKind::BytesToArrayU8 => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Array));
            Ok("bytes_to_array_u8")
        }
        SpecializedBuiltinKind::ArrayNew => {
            frame.push(ValueInfo::tagged_typed(ValueType::Array));
            Ok("array_new")
        }
        SpecializedBuiltinKind::ArrayLen => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::int(None));
            Ok("array_len")
        }
        SpecializedBuiltinKind::ArrayGet => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged());
            Ok("array_get")
        }
        SpecializedBuiltinKind::ArrayHas => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::bool(None));
            Ok("array_has")
        }
        SpecializedBuiltinKind::ArraySet => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Array));
            Ok("array_set")
        }
        SpecializedBuiltinKind::ArrayPush => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Array));
            Ok("array_push")
        }
        SpecializedBuiltinKind::MapLen => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::int(None));
            Ok("map_len")
        }
        SpecializedBuiltinKind::MapGet => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged());
            Ok("map_get")
        }
        SpecializedBuiltinKind::MapHas => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::bool(None));
            Ok("map_has")
        }
        SpecializedBuiltinKind::MapSet => {
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged_typed(ValueType::Map));
            Ok("map_set")
        }
        SpecializedBuiltinKind::MapIterNext => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::bool(None));
            Ok("map_iter_next")
        }
        SpecializedBuiltinKind::MapIterTakeKey => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged());
            Ok("map_iter_take_key")
        }
        SpecializedBuiltinKind::MapIterTakeValue => {
            let _ = frame.pop()?;
            frame.push(ValueInfo::tagged());
            Ok("map_iter_take_value")
        }
    }
}

fn emit_specialized_builtin_call(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    frame: &mut SymbolicFrame,
    kind: SpecializedBuiltinKind,
) -> Result<(&'static str, SymbolicValue), TraceRecordError> {
    match kind {
        SpecializedBuiltinKind::ValueLen => {
            let value = frame.pop()?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::ValueLen {
                        value: value.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::int(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("value_len", out))
        }
        SpecializedBuiltinKind::StringLen => {
            let text = frame.pop()?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                text,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::StringLen {
                        text: text.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::int(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_len", out))
        }
        SpecializedBuiltinKind::BytesLen => {
            let bytes = frame.pop()?;
            let bytes = ensure_heap_ptr(
                builder,
                block,
                ip,
                bytes,
                HeapContainerKind::Bytes.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::BytesLen {
                        bytes: bytes.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::int(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_len", out))
        }
        SpecializedBuiltinKind::StringSlice => {
            let length = ensure_int(builder, block, ip, frame.pop()?)?;
            let start = ensure_int(builder, block, ip, frame.pop()?)?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::StringSlice {
                        text: text.value.id,
                        start: start.value.id,
                        length: length.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_slice", out))
        }
        SpecializedBuiltinKind::BytesSlice => {
            let length = ensure_int(builder, block, ip, frame.pop()?)?;
            let start = ensure_int(builder, block, ip, frame.pop()?)?;
            let bytes = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::Bytes.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::BytesSlice {
                        bytes: bytes.value.id,
                        start: start.value.id,
                        length: length.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Bytes),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_slice", out))
        }
        SpecializedBuiltinKind::StringGet => {
            let index = ensure_int(builder, block, ip, frame.pop()?)?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::StringGet {
                        text: text.value.id,
                        index: index.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_get", out))
        }
        SpecializedBuiltinKind::BytesGet => {
            let index = ensure_int(builder, block, ip, frame.pop()?)?;
            let bytes = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::Bytes.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::BytesGet {
                        bytes: bytes.value.id,
                        index: index.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::int(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_get", out))
        }
        SpecializedBuiltinKind::BytesHas => {
            let index = ensure_int(builder, block, ip, frame.pop()?)?;
            let bytes = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::Bytes.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::BytesHas {
                        bytes: bytes.value.id,
                        index: index.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::bool(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_has", out))
        }
        SpecializedBuiltinKind::StringContains => {
            let needle = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::StringContains {
                        text: text.value.id,
                        needle: needle.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::bool(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_contains", out))
        }
        SpecializedBuiltinKind::RegexMatch => {
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let pattern = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::RegexMatch {
                        pattern: pattern.value.id,
                        text: text.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::bool(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("regex_match", out))
        }
        SpecializedBuiltinKind::RegexReplace => {
            let replacement = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let pattern = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::RegexReplace {
                        pattern: pattern.value.id,
                        text: text.value.id,
                        replacement: replacement.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("regex_replace", out))
        }
        SpecializedBuiltinKind::StringReplaceLiteral => {
            let replacement = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let needle = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::StringReplaceLiteral {
                        text: text.value.id,
                        needle: needle.value.id,
                        replacement: replacement.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_replace_literal", out))
        }
        SpecializedBuiltinKind::StringLowerAscii => {
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::StringLowerAscii {
                        text: text.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_lower_ascii", out))
        }
        SpecializedBuiltinKind::TypeOfKnown(value_type) => {
            let _ = frame.pop()?;
            let type_name = match value_type {
                ValueType::Null => "null",
                ValueType::Int => "int",
                ValueType::Float => "float",
                ValueType::Bool => "bool",
                ValueType::String => "string",
                ValueType::Bytes => "bytes",
                ValueType::Array => "array",
                ValueType::Map => "map",
                ValueType::Callable => "callable",
                ValueType::Unknown => {
                    return Err(TraceRecordError::UnsupportedTrace(
                        "type_of known specialization received unknown type".to_string(),
                    ));
                }
            };
            let constant = Value::string(type_name);
            let info = ValueInfo::type_name();
            let value = builder
                .append_value_inst(block, ip, info.repr, SsaInstKind::Constant(constant))
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("type_of", SymbolicValue { value, info }))
        }
        SpecializedBuiltinKind::ToStringIdentity => {
            let value = frame.pop()?;
            Ok(("to_string_identity", value))
        }
        SpecializedBuiltinKind::ToString => {
            let value = frame.pop()?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::ToString {
                        value: value.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("to_string", out))
        }
        SpecializedBuiltinKind::TypeOf => {
            let value = frame.pop()?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::TypeOf {
                        value: value.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::type_name(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("type_of", out))
        }
        SpecializedBuiltinKind::StringSplitLiteral => {
            let delimiter = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let text = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::String.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::StringSplitLiteral {
                        text: text.value.id,
                        delimiter: delimiter.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Array),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("string_split_literal", out))
        }
        SpecializedBuiltinKind::StringConcat => {
            let rhs = frame.pop()?;
            let lhs = frame.pop()?;
            emit_concat_binop(builder, block, ip, ConcatBinOpKind::String, lhs, rhs)
        }
        SpecializedBuiltinKind::BytesConcat => {
            let rhs = frame.pop()?;
            let lhs = frame.pop()?;
            emit_concat_binop(builder, block, ip, ConcatBinOpKind::Bytes, lhs, rhs)
        }
        SpecializedBuiltinKind::BytesFromArrayU8 => {
            let array = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::Array.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::BytesFromArrayU8 {
                        array: array.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Bytes),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_from_array_u8", out))
        }
        SpecializedBuiltinKind::BytesToUtf8Ascii => {
            let bytes = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::Bytes.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::BytesToUtf8Ascii {
                        bytes: bytes.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::String),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_to_utf8_ascii", out))
        }
        SpecializedBuiltinKind::BytesToArrayU8 => {
            let bytes = ensure_heap_ptr(
                builder,
                block,
                ip,
                frame.pop()?,
                HeapContainerKind::Bytes.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::BytesToArrayU8 {
                        bytes: bytes.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Array),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("bytes_to_array_u8", out))
        }
        SpecializedBuiltinKind::ArrayNew => {
            let out = builder
                .append_value_inst(block, ip, SsaValueRepr::Tagged, SsaInstKind::ArrayNew)
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Array),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("array_new", out))
        }
        SpecializedBuiltinKind::ArrayLen => {
            let array = frame.pop()?;
            let array = ensure_heap_ptr(
                builder,
                block,
                ip,
                array,
                HeapContainerKind::Array.value_type(),
            )?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::ArrayLen {
                        array: array.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::int(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("array_len", out))
        }
        SpecializedBuiltinKind::ArrayGet => {
            let index = frame.pop()?;
            let array = frame.pop()?;
            let array = ensure_heap_ptr(
                builder,
                block,
                ip,
                array,
                HeapContainerKind::Array.value_type(),
            )?;
            let index = ensure_int(builder, block, ip, index)?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::ArrayGet {
                        array: array.value.id,
                        index: index.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("array_get", out))
        }
        SpecializedBuiltinKind::ArrayHas => {
            let index = frame.pop()?;
            let array = frame.pop()?;
            let array = ensure_heap_ptr(
                builder,
                block,
                ip,
                array,
                HeapContainerKind::Array.value_type(),
            )?;
            let index = ensure_int(builder, block, ip, index)?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::ArrayHas {
                        array: array.value.id,
                        index: index.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::bool(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("array_has", out))
        }
        SpecializedBuiltinKind::ArraySet => {
            let value = frame.pop()?;
            let index = ensure_int(builder, block, ip, frame.pop()?)?;
            let array = frame.pop()?;
            if array.info.repr != SsaValueRepr::Tagged {
                return Err(TraceRecordError::TypeMismatch {
                    expected: "owned tagged array",
                    actual: array.info.repr,
                });
            }
            let is_append = matches!(
                builder.defining_inst(index.value.id).map(|inst| &inst.kind),
                Some(SsaInstKind::ArrayLen { array: array_ptr })
                    if matches!(
                        builder.defining_inst(*array_ptr).map(|inst| &inst.kind),
                        Some(SsaInstKind::UnboxHeapPtr {
                            input,
                            tag: ValueType::Array,
                        }) if *input == array.value.id
                    )
            );
            let kind = if is_append {
                SsaInstKind::ArrayPush {
                    array: array.value.id,
                    value: value.value.id,
                }
            } else {
                SsaInstKind::ArraySet {
                    array: array.value.id,
                    index: index.value.id,
                    value: value.value.id,
                }
            };
            let out = builder
                .append_value_inst(block, ip, SsaValueRepr::Tagged, kind)
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Array),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok((if is_append { "array_push" } else { "array_set" }, out))
        }
        SpecializedBuiltinKind::ArrayPush => {
            let value = frame.pop()?;
            let array = frame.pop()?;
            if array.info.repr != SsaValueRepr::Tagged {
                return Err(TraceRecordError::TypeMismatch {
                    expected: "owned tagged array",
                    actual: array.info.repr,
                });
            }
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::ArrayPush {
                        array: array.value.id,
                        value: value.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Array),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("array_push", out))
        }
        SpecializedBuiltinKind::MapLen => {
            let map = frame.pop()?;
            let map =
                ensure_heap_ptr(builder, block, ip, map, HeapContainerKind::Map.value_type())?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::MapLen { map: map.value.id },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::int(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_len", out))
        }
        SpecializedBuiltinKind::MapGet => {
            let key = frame.pop()?;
            let map = frame.pop()?;
            let map =
                ensure_heap_ptr(builder, block, ip, map, HeapContainerKind::Map.value_type())?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::MapGet {
                        map: map.value.id,
                        key: key.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_get", out))
        }
        SpecializedBuiltinKind::MapHas => {
            let key = frame.pop()?;
            let map = frame.pop()?;
            let map =
                ensure_heap_ptr(builder, block, ip, map, HeapContainerKind::Map.value_type())?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::MapHas {
                        map: map.value.id,
                        key: key.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::bool(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_has", out))
        }
        SpecializedBuiltinKind::MapSet => {
            let value = frame.pop()?;
            let key = frame.pop()?;
            let map = frame.pop()?;
            if map.info.repr != SsaValueRepr::Tagged {
                return Err(TraceRecordError::TypeMismatch {
                    expected: "owned tagged map",
                    actual: map.info.repr,
                });
            }
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::MapSet {
                        map: map.value.id,
                        key: key.value.id,
                        value: value.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged_typed(ValueType::Map),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_set", out))
        }
        SpecializedBuiltinKind::MapIterNext => {
            let slot = ensure_int(builder, block, ip, frame.pop()?)?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::MapIterNext {
                        slot: slot.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::bool(None),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_iter_next", out))
        }
        SpecializedBuiltinKind::MapIterTakeKey => {
            let slot = ensure_int(builder, block, ip, frame.pop()?)?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::MapIterTakeKey {
                        slot: slot.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_iter_take_key", out))
        }
        SpecializedBuiltinKind::MapIterTakeValue => {
            let slot = ensure_int(builder, block, ip, frame.pop()?)?;
            let out = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Tagged,
                    SsaInstKind::MapIterTakeValue {
                        slot: slot.value.id,
                    },
                )
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(("map_iter_take_value", out))
        }
    }
}

fn ensure_entry_repr(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
    target: SsaValueRepr,
) -> Result<SymbolicValue, TraceRecordError> {
    if value.info.repr == target {
        return Ok(value);
    }
    match (value.info.repr, target) {
        (SsaValueRepr::Tagged, SsaValueRepr::I64) => ensure_int(builder, block, ip, value),
        (SsaValueRepr::Tagged, SsaValueRepr::F64) => ensure_float(builder, block, ip, value),
        (SsaValueRepr::Tagged, SsaValueRepr::Bool) => ensure_bool(builder, block, ip, value),
        (SsaValueRepr::Tagged, SsaValueRepr::HeapPtr(tag)) => {
            ensure_heap_ptr(builder, block, ip, value, tag)
        }
        (_, repr) => Err(TraceRecordError::TypeMismatch {
            expected: match repr {
                SsaValueRepr::Tagged => "tagged",
                SsaValueRepr::I64 => "int",
                SsaValueRepr::F64 => "float",
                SsaValueRepr::Bool => "bool",
                SsaValueRepr::HeapPtr(_) => "heap-ptr",
            },
            actual: value.info.repr,
        }),
    }
}

fn ensure_int(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
) -> Result<SymbolicValue, TraceRecordError> {
    match value.info.repr {
        SsaValueRepr::I64 => Ok(value),
        SsaValueRepr::Tagged => {
            let unboxed = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::I64,
                    SsaInstKind::UnboxInt {
                        input: value.value.id,
                    },
                )
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(SymbolicValue {
                value: unboxed,
                info: ValueInfo::int(value.info.const_int),
            })
        }
        other => Err(TraceRecordError::TypeMismatch {
            expected: "int",
            actual: other,
        }),
    }
}

fn ensure_float(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
) -> Result<SymbolicValue, TraceRecordError> {
    match value.info.repr {
        SsaValueRepr::F64 => Ok(value),
        SsaValueRepr::Tagged => {
            let unboxed = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::F64,
                    SsaInstKind::UnboxFloat {
                        input: value.value.id,
                    },
                )
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(SymbolicValue {
                value: unboxed,
                info: ValueInfo::float(value.info.const_float),
            })
        }
        other => Err(TraceRecordError::TypeMismatch {
            expected: "float",
            actual: other,
        }),
    }
}

fn ensure_bool(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
) -> Result<SymbolicValue, TraceRecordError> {
    match value.info.repr {
        SsaValueRepr::Bool => Ok(value),
        SsaValueRepr::Tagged => {
            let unboxed = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::Bool,
                    SsaInstKind::UnboxBool {
                        input: value.value.id,
                    },
                )
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(SymbolicValue {
                value: unboxed,
                info: ValueInfo::bool(value.info.const_bool),
            })
        }
        other => Err(TraceRecordError::TypeMismatch {
            expected: "bool",
            actual: other,
        }),
    }
}

fn ensure_heap_ptr(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    value: SymbolicValue,
    tag: ValueType,
) -> Result<SymbolicValue, TraceRecordError> {
    match value.info.repr {
        SsaValueRepr::HeapPtr(actual) if actual == tag => Ok(value),
        SsaValueRepr::Tagged => {
            let unboxed = builder
                .append_value_inst(
                    block,
                    ip,
                    SsaValueRepr::HeapPtr(tag),
                    SsaInstKind::UnboxHeapPtr {
                        input: value.value.id,
                        tag,
                    },
                )
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
            Ok(SymbolicValue {
                value: unboxed,
                info: ValueInfo::heap(tag),
            })
        }
        other => Err(TraceRecordError::TypeMismatch {
            expected: "heap-ptr",
            actual: other,
        }),
    }
}

fn expect_int_info(value: ValueInfo) -> Result<ValueInfo, TraceRecordError> {
    match value.repr {
        SsaValueRepr::I64 | SsaValueRepr::Tagged => Ok(ValueInfo::int(value.const_int)),
        _ => Err(TraceRecordError::TypeMismatch {
            expected: "int",
            actual: value.repr,
        }),
    }
}

fn expect_float_info(value: ValueInfo) -> Result<ValueInfo, TraceRecordError> {
    match value.repr {
        SsaValueRepr::F64 | SsaValueRepr::Tagged => Ok(ValueInfo::float(value.const_float)),
        _ => Err(TraceRecordError::TypeMismatch {
            expected: "float",
            actual: value.repr,
        }),
    }
}

fn expect_bool_info(value: ValueInfo) -> Result<ValueInfo, TraceRecordError> {
    match value.repr {
        SsaValueRepr::Bool | SsaValueRepr::Tagged => Ok(ValueInfo::bool(value.const_bool)),
        _ => Err(TraceRecordError::TypeMismatch {
            expected: "bool",
            actual: value.repr,
        }),
    }
}

fn validate_int_operands(
    program: &Program,
    ip: usize,
    kind: IntBinOpKind,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<(), TraceRecordError> {
    if lhs.repr == SsaValueRepr::I64 && rhs.repr == SsaValueRepr::I64 {
        return Ok(());
    }
    let explicit = operand_types(program, ip);
    let has_evidence = lhs.repr == SsaValueRepr::I64
        || rhs.repr == SsaValueRepr::I64
        || lhs.const_int.is_some()
        || rhs.const_int.is_some();
    let has_float_evidence = lhs.repr == SsaValueRepr::F64
        || rhs.repr == SsaValueRepr::F64
        || lhs.const_float.is_some()
        || rhs.const_float.is_some();
    let int_like = matches!(lhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged);

    match explicit {
        (ValueType::Int, ValueType::Int) => Ok(()),
        (ValueType::Unknown, ValueType::Unknown)
        | (ValueType::Int, ValueType::Unknown)
        | (ValueType::Unknown, ValueType::Int)
            if int_like && (has_evidence || !has_float_evidence) =>
        {
            Ok(())
        }
        _ => Err(TraceRecordError::UnsupportedTrace(format!(
            "SSA recorder cannot prove integer operands for {:?} at ip {ip}",
            kind
        ))),
    }
}

fn validate_int_compare_operands(
    program: &Program,
    ip: usize,
    kind: IntCompareKind,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<(), TraceRecordError> {
    if lhs.repr == SsaValueRepr::I64 && rhs.repr == SsaValueRepr::I64 {
        return Ok(());
    }
    let explicit = operand_types(program, ip);
    let has_evidence = lhs.repr == SsaValueRepr::I64
        || rhs.repr == SsaValueRepr::I64
        || lhs.const_int.is_some()
        || rhs.const_int.is_some();
    let has_float_evidence = lhs.repr == SsaValueRepr::F64
        || rhs.repr == SsaValueRepr::F64
        || lhs.const_float.is_some()
        || rhs.const_float.is_some();
    let int_like = matches!(lhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::I64 | SsaValueRepr::Tagged);

    match explicit {
        (ValueType::Int, ValueType::Int) => Ok(()),
        (ValueType::Unknown, ValueType::Unknown)
        | (ValueType::Int, ValueType::Unknown)
        | (ValueType::Unknown, ValueType::Int)
            if int_like && (has_evidence || !has_float_evidence) =>
        {
            Ok(())
        }
        _ => Err(TraceRecordError::UnsupportedTrace(format!(
            "SSA recorder cannot prove integer operands for {:?} at ip {ip}",
            kind
        ))),
    }
}

fn validate_float_operands(
    program: &Program,
    ip: usize,
    kind: FloatBinOpKind,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<(), TraceRecordError> {
    if lhs.repr == SsaValueRepr::F64 && rhs.repr == SsaValueRepr::F64 {
        return Ok(());
    }
    let explicit = operand_types(program, ip);
    let has_evidence = lhs.repr == SsaValueRepr::F64
        || rhs.repr == SsaValueRepr::F64
        || lhs.const_float.is_some()
        || rhs.const_float.is_some();
    let float_like = matches!(lhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged);

    match explicit {
        (ValueType::Float, ValueType::Float) => Ok(()),
        (ValueType::Unknown, ValueType::Unknown)
        | (ValueType::Float, ValueType::Unknown)
        | (ValueType::Unknown, ValueType::Float)
            if float_like && has_evidence =>
        {
            Ok(())
        }
        _ => Err(TraceRecordError::UnsupportedTrace(format!(
            "SSA recorder cannot prove float operands for {:?} at ip {ip}",
            kind
        ))),
    }
}

fn validate_float_compare_operands(
    program: &Program,
    ip: usize,
    kind: FloatCompareKind,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<(), TraceRecordError> {
    let explicit = operand_types(program, ip);
    let has_evidence = lhs.repr == SsaValueRepr::F64
        || rhs.repr == SsaValueRepr::F64
        || lhs.const_float.is_some()
        || rhs.const_float.is_some();
    let float_like = matches!(lhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged)
        && matches!(rhs.repr, SsaValueRepr::F64 | SsaValueRepr::Tagged);

    match explicit {
        (ValueType::Float, ValueType::Float) => Ok(()),
        (ValueType::Unknown, ValueType::Unknown)
        | (ValueType::Float, ValueType::Unknown)
        | (ValueType::Unknown, ValueType::Float)
            if float_like && has_evidence =>
        {
            Ok(())
        }
        _ => Err(TraceRecordError::UnsupportedTrace(format!(
            "SSA recorder cannot prove float operands for {:?} at ip {ip}",
            kind
        ))),
    }
}

fn result_info_for_int_binop(
    kind: IntBinOpKind,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<ValueInfo, TraceRecordError> {
    match kind {
        IntBinOpKind::Add => Ok(ValueInfo::int(
            lhs.const_int.zip(rhs.const_int).map(|(lhs, rhs)| lhs + rhs),
        )),
        IntBinOpKind::Sub => Ok(ValueInfo::int(
            lhs.const_int.zip(rhs.const_int).map(|(lhs, rhs)| lhs - rhs),
        )),
        IntBinOpKind::Mul => Ok(ValueInfo::int(
            lhs.const_int.zip(rhs.const_int).map(|(lhs, rhs)| lhs * rhs),
        )),
        IntBinOpKind::Div => {
            if let Some(rhs_const) = rhs.const_int {
                if rhs_const == 0 {
                    return Err(TraceRecordError::UnsupportedTrace(
                        "SSA recorder does not record division-by-zero integer traces".to_string(),
                    ));
                }
                if rhs_const == -1 {
                    return Err(TraceRecordError::UnsupportedTrace(
                        "SSA recorder does not support integer div traces with rhs -1 yet"
                            .to_string(),
                    ));
                }
                Ok(ValueInfo::int(lhs.const_int.and_then(|lhs_const| {
                    checked_int_div(lhs_const, rhs_const).ok()
                })))
            } else {
                Ok(ValueInfo::int(None))
            }
        }
        IntBinOpKind::Mod => Ok(ValueInfo::int(None)),
        IntBinOpKind::Shl => {
            if let Some(amount) = rhs
                .const_int
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= 63)
            {
                Ok(ValueInfo::int(lhs.const_int.map(|value| value << amount)))
            } else {
                Ok(ValueInfo::int(None))
            }
        }
        IntBinOpKind::Shr => {
            if let Some(amount) = rhs
                .const_int
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= 63)
            {
                Ok(ValueInfo::int(lhs.const_int.map(|value| value >> amount)))
            } else {
                Ok(ValueInfo::int(None))
            }
        }
        IntBinOpKind::Lshr => {
            if let Some(amount) = rhs
                .const_int
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value <= 63)
            {
                Ok(ValueInfo::int(
                    lhs.const_int.map(|value| ((value as u64) >> amount) as i64),
                ))
            } else {
                Ok(ValueInfo::int(None))
            }
        }
    }
}

fn result_info_for_float_binop(kind: FloatBinOpKind, lhs: ValueInfo, rhs: ValueInfo) -> ValueInfo {
    match kind {
        FloatBinOpKind::Add => ValueInfo::float(
            lhs.const_float
                .zip(rhs.const_float)
                .map(|(lhs, rhs)| lhs + rhs),
        ),
        FloatBinOpKind::Sub => ValueInfo::float(
            lhs.const_float
                .zip(rhs.const_float)
                .map(|(lhs, rhs)| lhs - rhs),
        ),
        FloatBinOpKind::Mul => ValueInfo::float(
            lhs.const_float
                .zip(rhs.const_float)
                .map(|(lhs, rhs)| lhs * rhs),
        ),
        FloatBinOpKind::Div => ValueInfo::float(
            lhs.const_float
                .zip(rhs.const_float)
                .map(|(lhs, rhs)| lhs / rhs),
        ),
        FloatBinOpKind::Mod => ValueInfo::float(
            lhs.const_float
                .zip(rhs.const_float)
                .map(|(lhs, rhs)| lhs % rhs),
        ),
    }
}

fn result_info_for_int_compare(kind: IntCompareKind, lhs: ValueInfo, rhs: ValueInfo) -> ValueInfo {
    let const_bool = lhs
        .const_int
        .zip(rhs.const_int)
        .map(|(lhs, rhs)| match kind {
            IntCompareKind::Eq => lhs == rhs,
            IntCompareKind::Lt => lhs < rhs,
            IntCompareKind::Gt => lhs > rhs,
        });
    ValueInfo::bool(const_bool)
}

fn result_info_for_float_compare(
    kind: FloatCompareKind,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> ValueInfo {
    let const_bool = lhs
        .const_float
        .zip(rhs.const_float)
        .map(|(lhs, rhs)| match kind {
            FloatCompareKind::Eq => lhs == rhs,
            FloatCompareKind::Lt => lhs < rhs,
            FloatCompareKind::Gt => lhs > rhs,
        });
    ValueInfo::bool(const_bool)
}

fn result_info_for_concat_binop(kind: ConcatBinOpKind) -> ValueInfo {
    match kind {
        ConcatBinOpKind::String => ValueInfo::tagged_typed(ValueType::String),
        ConcatBinOpKind::Bytes => ValueInfo::tagged_typed(ValueType::Bytes),
    }
}

fn result_info_for_bool_binop(kind: BoolBinOpKind, lhs: ValueInfo, rhs: ValueInfo) -> ValueInfo {
    let const_bool = lhs
        .const_bool
        .zip(rhs.const_bool)
        .map(|(lhs, rhs)| match kind {
            BoolBinOpKind::And => lhs && rhs,
            BoolBinOpKind::Or => lhs || rhs,
        });
    ValueInfo::bool(const_bool)
}

fn load_constant(
    builder: &mut SsaTraceBuilder,
    block: super::ir::SsaBlockId,
    ip: usize,
    program: &Program,
    index: u32,
) -> Result<SymbolicValue, TraceRecordError> {
    let constant = program.constants.get(index as usize).ok_or_else(|| {
        TraceRecordError::UnsupportedTrace(format!(
            "unsupported constant #{index}: missing constant"
        ))
    })?;
    let constant = constant.clone();
    let info = ValueInfo::from_constant(&constant);
    let value = builder
        .append_value_inst(block, ip, info.repr, SsaInstKind::Constant(constant))
        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
    Ok(SymbolicValue { value, info })
}

fn continue_with_inline_frame(
    builder: &mut SsaTraceBuilder,
    frame: &SymbolicFrame,
    inline_frame: &mut Option<InlineRecorderFrame>,
    label_prefix: &str,
) -> Result<
    (
        super::ir::SsaBlockId,
        SymbolicFrame,
        Vec<super::ir::SsaValueId>,
    ),
    TraceRecordError,
> {
    let Some(inlined) = inline_frame.as_mut() else {
        return continue_with_frame(builder, frame, label_prefix);
    };
    let callee_stack_len = frame.stack.len();
    let callee_local_len = frame.locals.len();
    let combined = SymbolicFrame {
        stack: frame
            .stack
            .iter()
            .chain(&inlined.caller.stack)
            .copied()
            .collect(),
        locals: frame
            .locals
            .iter()
            .chain(&inlined.caller.locals)
            .copied()
            .collect(),
        dirty_locals: Vec::new(),
    };
    let (block, mut next, args) = continue_with_frame(builder, &combined, label_prefix)?;
    let caller_stack = next.stack.split_off(callee_stack_len);
    let caller_locals = next.locals.split_off(callee_local_len);
    inlined.caller.stack = caller_stack;
    inlined.caller.locals = caller_locals;
    next.dirty_locals = frame.dirty_locals.clone();
    Ok((block, next, args))
}

fn continue_with_frame(
    builder: &mut SsaTraceBuilder,
    frame: &SymbolicFrame,
    label_prefix: &str,
) -> Result<
    (
        super::ir::SsaBlockId,
        SymbolicFrame,
        Vec<super::ir::SsaValueId>,
    ),
    TraceRecordError,
> {
    let block = builder.create_block();
    let args = frame
        .stack
        .iter()
        .chain(frame.locals.iter())
        .map(|value| value.value.id)
        .collect::<Vec<_>>();

    let mut next_stack = Vec::with_capacity(frame.stack.len());
    for (index, value) in frame.stack.iter().copied().enumerate() {
        let param = builder
            .append_param(
                block,
                value.info.repr,
                format!("{label_prefix}_stack{index}"),
            )
            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
        next_stack.push(SymbolicValue {
            value: param,
            info: ValueInfo {
                repr: value.info.repr,
                const_int: None,
                const_float: None,
                const_bool: None,
                known_type: value.info.known_type,
                force_value_eq: value.info.force_value_eq,
                source_local: None,
            },
        });
    }

    let mut next_locals = Vec::with_capacity(frame.locals.len());
    for (index, value) in frame.locals.iter().copied().enumerate() {
        let param = builder
            .append_param(
                block,
                value.info.repr,
                format!("{label_prefix}_local{index}"),
            )
            .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
        next_locals.push(SymbolicValue {
            value: param,
            info: ValueInfo {
                repr: value.info.repr,
                const_int: None,
                const_float: None,
                const_bool: None,
                known_type: value.info.known_type,
                force_value_eq: value.info.force_value_eq,
                source_local: None,
            },
        });
    }

    Ok((
        block,
        SymbolicFrame {
            stack: next_stack,
            locals: next_locals,
            dirty_locals: frame.dirty_locals.clone(),
        },
        args,
    ))
}

fn materialize_stack(stack: &[SymbolicValue]) -> Vec<SsaMaterialization> {
    materialize_ssa_values(stack.iter().map(|value| value.value))
}

fn materialize_locals(locals: &[SymbolicValue]) -> Vec<SsaMaterialization> {
    materialize_ssa_values(locals.iter().map(|value| value.value))
}

fn add_symbolic_exit(
    builder: &mut SsaTraceBuilder,
    exit_ip: usize,
    frame: &SymbolicFrame,
    inline_frame: Option<&InlineRecorderFrame>,
) -> super::ir::SsaExitId {
    let Some(inline_frame) = inline_frame else {
        return builder.add_exit(
            exit_ip,
            materialize_stack(&frame.stack),
            materialize_locals(&frame.locals),
            frame.dirty_locals.clone(),
        );
    };
    builder.add_exit_with_virtual_frames(
        inline_frame.call_ip,
        materialize_stack(&inline_frame.caller.stack),
        materialize_locals(&inline_frame.caller.locals),
        inline_frame.caller.dirty_locals.clone(),
        vec![VirtualFrameSnapshot {
            prototype_id: inline_frame.candidate.prototype_id,
            call_ip: inline_frame.call_ip,
            return_ip: inline_frame.return_ip,
            resume_ip: exit_ip,
            operand_stack: materialize_stack(&frame.stack),
            locals: materialize_locals(&frame.locals),
            dirty_locals: frame.dirty_locals.clone(),
        }],
    )
}

fn build_loop_header_plan(
    stack: &[ValueInfo],
    locals: &[ValueInfo],
    entry_use: &[EntryUseState],
    local_written: &[bool],
) -> LoopHeaderPlan {
    let stack_reprs = stack.iter().map(|value| value.repr).collect();
    let stack_known_types = stack.iter().map(|value| value.known_type).collect();
    let mut local_reprs = Vec::with_capacity(locals.len());
    let mut local_known_types = Vec::with_capacity(locals.len());
    let mut entry_seed = Vec::with_capacity(locals.len());
    for ((local, state), written) in locals
        .iter()
        .zip(entry_use.iter())
        .zip(local_written.iter().copied())
    {
        let repr = planned_loop_local_repr(*local, *state, written);
        if matches!(state, EntryUseState::WrittenBeforeRead) {
            local_reprs.push(repr);
            local_known_types.push(local.known_type);
            entry_seed.push(match repr {
                SsaValueRepr::I64 => LoopSeed::ZeroI64,
                SsaValueRepr::F64 => LoopSeed::ZeroF64,
                SsaValueRepr::Bool => LoopSeed::FalseBool,
                _ => LoopSeed::Entry,
            });
        } else {
            local_reprs.push(repr);
            local_known_types.push(local.known_type);
            entry_seed.push(LoopSeed::Entry);
        }
    }
    LoopHeaderPlan {
        stack_reprs,
        stack_known_types,
        local_reprs,
        local_known_types,
        entry_seed,
    }
}

fn loop_backedge_args(
    stack: &[SymbolicValue],
    locals: &[SymbolicValue],
) -> Vec<super::ir::SsaValueId> {
    stack
        .iter()
        .chain(locals.iter())
        .map(|value| value.value.id)
        .collect()
}

fn validate_loop_carrier_repr(local: usize, repr: SsaValueRepr) -> Result<(), TraceRecordError> {
    let _ = (local, repr);
    Ok(())
}

fn planned_loop_local_repr(
    local: ValueInfo,
    state: EntryUseState,
    written_on_trace: bool,
) -> SsaValueRepr {
    if matches!(state, EntryUseState::ReadBeforeWrite) && !written_on_trace {
        return match local.known_type {
            Some(ValueType::String) => SsaValueRepr::HeapPtr(ValueType::String),
            Some(ValueType::Bytes) => SsaValueRepr::HeapPtr(ValueType::Bytes),
            _ => local.repr,
        };
    }
    local.repr
}

fn operand_types(program: &Program, ip: usize) -> (ValueType, ValueType) {
    program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.operand_types.get(&ip))
        .copied()
        .unwrap_or((ValueType::Unknown, ValueType::Unknown))
}

fn straight_line_if_join_side_entry(
    code: &[u8],
    side_start: usize,
    join_ip: usize,
) -> Option<usize> {
    let mut cursor = side_start;
    while cursor < join_ip {
        let opcode = OpCode::try_from(*code.get(cursor)?).ok()?;
        cursor = cursor.saturating_add(1);
        match opcode {
            OpCode::Br => {
                let target = read_u32(code, &mut cursor)? as usize;
                return (target == join_ip && cursor == join_ip).then_some(side_start);
            }
            OpCode::Brfalse | OpCode::Ret => return None,
            _ => {
                let operand_len = opcode.operand_len();
                if cursor.saturating_add(operand_len) > join_ip {
                    return None;
                }
                cursor = cursor.saturating_add(operand_len);
            }
        }
    }
    None
}

fn read_u8(code: &[u8], ip: &mut usize) -> Option<u8> {
    let value = *code.get(*ip)?;
    *ip = ip.saturating_add(1);
    Some(value)
}

fn read_u16(code: &[u8], ip: &mut usize) -> Option<u16> {
    if ip.saturating_add(2) > code.len() {
        return None;
    }
    let bytes = [code[*ip], code[*ip + 1]];
    *ip = ip.saturating_add(2);
    Some(u16::from_le_bytes(bytes))
}

fn read_u32(code: &[u8], ip: &mut usize) -> Option<u32> {
    if ip.saturating_add(4) > code.len() {
        return None;
    }
    let bytes = [code[*ip], code[*ip + 1], code[*ip + 2], code[*ip + 3]];
    *ip = ip.saturating_add(4);
    Some(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BytecodeBuilder, CallableKind, CallablePrototype, CallableTarget, FunctionRegion,
        RootCallableBinding, ScriptFunction, Value,
    };

    fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
        let start = instr_ip as usize + 1;
        code[start..start + 4].copy_from_slice(&target.to_le_bytes());
    }

    #[test]
    fn callable_opcodes_decode_with_explicit_trace_semantics() {
        let mut bytecode = BytecodeBuilder::new();
        bytecode.call_value(2);
        let program = Program::new(Vec::new(), bytecode.finish());
        let mut cursor = TraceCursor::new(&program, 0, 8);
        assert!(matches!(
            cursor.next().expect("callvalue decode"),
            Some(DecodedOp::CallValue {
                ip: 0,
                argc: 2,
                resume_ip: 2
            })
        ));
        assert!(cursor.next().expect("trace end").is_none());
    }

    #[test]
    fn unknown_len_container_uses_checked_value_len_specialization() {
        let program = Program::new(Vec::new(), Vec::new());
        assert_eq!(
            select_specialized_builtin_kind(
                &program,
                0,
                BuiltinFunction::Len,
                ValueInfo::tagged(),
                false,
            ),
            Some(SpecializedBuiltinKind::ValueLen),
        );
    }

    #[test]
    fn unboxed_numeric_compare_ignores_stale_operand_type_hint() {
        let program = Program::new(Vec::new(), Vec::new()).with_type_map(crate::TypeMap {
            operand_types: std::collections::HashMap::from([(
                7,
                (ValueType::Null, ValueType::Int),
            )]),
            ..crate::TypeMap::default()
        });

        let lhs = ValueInfo::int(None);
        let rhs = ValueInfo::int(None);
        assert_eq!(
            select_numeric_compare(&program, 7, OpCode::Clt as u8, lhs, rhs)
                .expect("unboxed integer compare should override stale hint"),
            NumericCompareKind::Int(IntCompareKind::Lt),
        );
        validate_int_compare_operands(&program, 7, IntCompareKind::Lt, lhs, rhs)
            .expect("unboxed integer validator should override stale hint");
        assert_eq!(
            select_numeric_binop(&program, 7, OpCode::Add as u8, lhs, rhs)
                .expect("unboxed integer binop should override stale hint"),
            NumericBinOpKind::Int(IntBinOpKind::Add),
        );
        validate_int_operands(&program, 7, IntBinOpKind::Add, lhs, rhs)
            .expect("unboxed integer binop validator should override stale hint");
    }

    #[test]
    fn records_linear_local_increment_trace_directly() {
        let mut bc = BytecodeBuilder::new();
        bc.ldloc(0);
        bc.ldc(0);
        bc.add();
        bc.stloc(0);
        let ret_ip = bc.position();
        bc.ret();
        let program = Program::new(vec![Value::Int(1)], bc.finish()).with_local_count(1);

        let recorded = record_trace(&program, 0, 0, 32, &[]).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::Halt);
        assert!(recorded.op_names.iter().any(|name| name == "iadd_imm"));
        assert!(matches!(
            recorded.ssa.blocks[0].terminator,
            Some(SsaTerminator::Return { .. })
        ));
        let exit = &recorded.ssa.exits[0];
        assert_eq!(exit.exit_ip, ret_ip as usize);
        assert!(matches!(exit.locals[0], SsaMaterialization::BoxInt(_)));
        assert_eq!(exit.dirty_locals, vec![true]);
    }

    #[test]
    fn records_no_dirty_locals_before_any_store() {
        let mut bc = BytecodeBuilder::new();
        bc.ldloc(0);
        bc.ret();
        let program = Program::new(Vec::new(), bc.finish()).with_local_count(2);

        let recorded = record_trace(&program, 0, 0, 16, &[]).expect("recorded trace");

        assert_eq!(recorded.ssa.exits[0].dirty_locals, vec![false, false]);
        assert!(recorded.ssa.render_text().contains("dirty_locals=[]"));
    }

    #[test]
    fn records_exact_dirty_local_after_store() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(3);
        bc.ret();
        let program = Program::new(vec![Value::Int(7)], bc.finish()).with_local_count(4);

        let recorded = record_trace(&program, 0, 0, 16, &[]).expect("recorded trace");

        assert_eq!(
            recorded.ssa.exits[0].dirty_locals,
            vec![false, false, false, true]
        );
        assert!(recorded.ssa.render_text().contains("dirty_locals=[3]"));
    }

    #[test]
    fn propagates_dirty_locals_through_guard_continuation() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(1);
        bc.ldloc(0);
        bc.ldc(1);
        bc.clt();
        let guard_ip = bc.position();
        bc.brfalse(0);
        bc.ret();
        let exit_ip = bc.position();
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, guard_ip, exit_ip);
        let program = Program::new(vec![Value::Int(5), Value::Int(10)], code).with_local_count(2);

        let recorded = record_trace(&program, 0, 0, 32, &[]).expect("recorded trace");

        assert_eq!(recorded.ssa.exits.len(), 2);
        assert_eq!(recorded.ssa.exits[0].dirty_locals, vec![false, true]);
        assert_eq!(recorded.ssa.exits[1].dirty_locals, vec![false, true]);
    }

    #[test]
    fn records_loop_if_false_root_trace_directly() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(0);
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(1);
        bc.add();
        bc.stloc(0);
        bc.ldloc(0);
        bc.ldc(2);
        bc.ceq();
        let branch_ip = bc.position();
        bc.brfalse(0);
        bc.ldloc(0);
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, branch_ip, root_ip);
        let program = Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(4)], code)
            .with_local_count(1);

        let recorded =
            record_trace(&program, root_ip as usize, 0, 64, &[]).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::BranchExit);
        assert!(recorded.op_names.iter().any(|name| name == "loop_if_false"));
        assert_eq!(recorded.ssa.blocks.len(), 2);
        assert!(matches!(
            recorded.ssa.blocks[1].terminator,
            Some(SsaTerminator::BranchBool { .. })
        ));
    }

    #[test]
    fn conditional_loop_backedge_merges_dirty_locals_into_earlier_guard_exits() {
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(1);
        bc.clt();
        let guard_ip = bc.position();
        bc.brfalse(0);
        bc.ldloc(1);
        bc.ldc(0);
        bc.add();
        bc.stloc(1);
        bc.ldloc(0);
        bc.ldc(0);
        bc.add();
        bc.stloc(0);
        bc.ldloc(0);
        bc.ldc(2);
        bc.ceq();
        bc.brfalse(root_ip);
        bc.ret();
        let exit_ip = bc.position();
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, guard_ip, exit_ip);
        let program = Program::new(vec![Value::Int(1), Value::Int(10), Value::Int(3)], code)
            .with_local_count(2);

        let recorded =
            record_trace(&program, root_ip as usize, 0, 128, &[]).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::BranchExit);
        assert_eq!(recorded.ssa.exits.len(), 2);
        for exit in &recorded.ssa.exits {
            assert_eq!(exit.dirty_locals, vec![true, true]);
        }
    }

    #[test]
    fn records_guarded_loop_backedge_trace_directly() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(0);
        bc.ldc(0);
        bc.stloc(1);
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(1);
        bc.clt();
        let guard_ip = bc.position();
        bc.brfalse(0);
        bc.ldloc(1);
        bc.ldloc(0);
        bc.add();
        bc.stloc(1);
        bc.ldloc(0);
        bc.ldc(2);
        bc.add();
        bc.stloc(0);
        bc.br(root_ip);
        let exit_ip = bc.position();
        bc.ldloc(1);
        bc.ret();

        let mut code = bc.finish();
        patch_branch_target(&mut code, guard_ip, exit_ip);
        let program = Program::new(vec![Value::Int(0), Value::Int(6), Value::Int(1)], code)
            .with_local_count(2);

        let recorded =
            record_trace(&program, root_ip as usize, 0, 128, &[]).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::LoopBack);
        assert!(recorded.op_names.iter().any(|name| name == "guard_false"));
        assert!(recorded.op_names.iter().any(|name| name == "jump_root"));
        assert_eq!(recorded.ssa.blocks.len(), 3);
        assert_eq!(recorded.ssa.exits.len(), 1);
        assert_eq!(recorded.ssa.exits[0].dirty_locals, vec![true, true]);
        assert!(recorded.ssa.render_text().contains("dirty_locals=[0, 1]"));
        assert!(matches!(
            recorded.ssa.blocks[2].terminator,
            Some(SsaTerminator::Jump { .. })
        ));
    }

    #[test]
    fn inline_static_leaf_rejects_loop_that_mutates_guarded_callable_source() {
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.call_value(0);
        bc.pop();
        bc.ldloc(1);
        bc.stloc(0);
        bc.br(root_ip);
        let first_entry = bc.position();
        bc.ldc(0);
        bc.ret();
        let first_end = bc.position();
        let second_entry = bc.position();
        bc.ldc(1);
        bc.ret();
        let second_end = bc.position();
        let program = Program::new(vec![Value::Int(1), Value::Int(2)], bc.finish())
            .with_local_count(2)
            .with_callable_metadata(
                vec![
                    ScriptFunction {
                        entry_ip: first_entry,
                        end_ip: first_end,
                    },
                    ScriptFunction {
                        entry_ip: second_entry,
                        end_ip: second_end,
                    },
                ],
                vec![
                    CallablePrototype {
                        kind: CallableKind::FunctionItem,
                        target: CallableTarget::ScriptFunction(0),
                        arity: 0,
                        frame_local_count: 1,
                        parameter_slots: Vec::new(),
                        capture_source_slots: Vec::new(),
                        capture_slots: Vec::new(),
                        capture_modes: Vec::new(),
                        self_slot: None,
                        schema: None,
                    },
                    CallablePrototype {
                        kind: CallableKind::FunctionItem,
                        target: CallableTarget::ScriptFunction(1),
                        arity: 0,
                        frame_local_count: 1,
                        parameter_slots: Vec::new(),
                        capture_source_slots: Vec::new(),
                        capture_slots: Vec::new(),
                        capture_modes: Vec::new(),
                        self_slot: None,
                        schema: None,
                    },
                ],
                vec![
                    FunctionRegion {
                        start_ip: first_entry,
                        end_ip: first_end,
                        prototype_id: Some(0),
                    },
                    FunctionRegion {
                        start_ip: second_entry,
                        end_ip: second_end,
                        prototype_id: Some(1),
                    },
                ],
                vec![
                    RootCallableBinding {
                        local_slot: 0,
                        prototype_id: 0,
                    },
                    RootCallableBinding {
                        local_slot: 1,
                        prototype_id: 1,
                    },
                ],
            );

        let error = record_trace_with_local_count(
            &program,
            crate::vm::native::ROOT_FRAME_KEY,
            root_ip as usize,
            0,
            2,
            None,
            Some(&[Some(0), Some(1)]),
            64,
            &[],
        )
        .expect_err("mutated callable source must reject native loop");
        assert!(
            matches!(error, TraceRecordError::UnsupportedTrace(ref reason) if reason.contains("callable source local")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn inline_static_leaf_tracks_callable_source_across_local_copy() {
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(1);
        bc.stloc(0);
        bc.ldloc(0);
        bc.call_value(0);
        bc.pop();
        bc.br(root_ip);
        let first_entry = bc.position();
        bc.ldc(0);
        bc.ret();
        let first_end = bc.position();
        let second_entry = bc.position();
        bc.ldc(1);
        bc.ret();
        let second_end = bc.position();

        let program = Program::new(vec![Value::Int(1), Value::Int(2)], bc.finish())
            .with_local_count(2)
            .with_callable_metadata(
                vec![
                    ScriptFunction {
                        entry_ip: first_entry,
                        end_ip: first_end,
                    },
                    ScriptFunction {
                        entry_ip: second_entry,
                        end_ip: second_end,
                    },
                ],
                vec![
                    CallablePrototype {
                        kind: CallableKind::FunctionItem,
                        target: CallableTarget::ScriptFunction(0),
                        arity: 0,
                        frame_local_count: 2,
                        parameter_slots: Vec::new(),
                        capture_source_slots: Vec::new(),
                        capture_slots: Vec::new(),
                        capture_modes: Vec::new(),
                        self_slot: None,
                        schema: None,
                    },
                    CallablePrototype {
                        kind: CallableKind::FunctionItem,
                        target: CallableTarget::ScriptFunction(1),
                        arity: 0,
                        frame_local_count: 2,
                        parameter_slots: Vec::new(),
                        capture_source_slots: Vec::new(),
                        capture_slots: Vec::new(),
                        capture_modes: Vec::new(),
                        self_slot: None,
                        schema: None,
                    },
                ],
                vec![
                    FunctionRegion {
                        start_ip: first_entry,
                        end_ip: first_end,
                        prototype_id: Some(0),
                    },
                    FunctionRegion {
                        start_ip: second_entry,
                        end_ip: second_end,
                        prototype_id: Some(1),
                    },
                ],
                vec![
                    RootCallableBinding {
                        local_slot: 0,
                        prototype_id: 0,
                    },
                    RootCallableBinding {
                        local_slot: 1,
                        prototype_id: 1,
                    },
                ],
            );
        let callable_prototypes = [Some(0), Some(1)];

        let recorded = record_trace_with_local_count(
            &program,
            crate::vm::native::ROOT_FRAME_KEY,
            root_ip as usize,
            0,
            program.local_count,
            None,
            Some(&callable_prototypes),
            64,
            &[],
        )
        .expect("copied callable trace");

        assert!(recorded.op_names.iter().any(|name| name == "inline_call:1"));
        assert!(!recorded.op_names.iter().any(|name| name == "inline_call:0"));
    }

    #[test]
    fn inline_static_leaf_records_call_and_return_inside_root_loop() {
        let mut bc = BytecodeBuilder::new();
        let root_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(0);
        bc.call_value(1);
        bc.pop();
        bc.br(root_ip);
        let function_entry = bc.position();
        bc.ldloc(1);
        bc.ldc(0);
        bc.add();
        bc.ret();
        let function_end = bc.position();

        let program = Program::new(vec![Value::Int(1)], bc.finish())
            .with_local_count(2)
            .with_callable_metadata(
                vec![ScriptFunction {
                    entry_ip: function_entry,
                    end_ip: function_end,
                }],
                vec![CallablePrototype {
                    kind: CallableKind::FunctionItem,
                    target: CallableTarget::ScriptFunction(0),
                    arity: 1,
                    frame_local_count: 2,
                    parameter_slots: vec![1],
                    capture_source_slots: Vec::new(),
                    capture_slots: Vec::new(),
                    capture_modes: Vec::new(),
                    self_slot: None,
                    schema: None,
                }],
                vec![FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                }],
                vec![RootCallableBinding {
                    local_slot: 0,
                    prototype_id: 0,
                }],
            );

        let unproven = record_trace(&program, root_ip as usize, 0, 64, &[]).unwrap();
        assert_eq!(unproven.terminal, JitTraceTerminal::CallValue);
        assert!(!unproven.op_names.iter().any(|name| name == "inline_call:0"));

        let callable_prototypes = [Some(0), None];
        let recorded = record_trace_with_local_count(
            &program,
            crate::vm::native::ROOT_FRAME_KEY,
            root_ip as usize,
            0,
            program.local_count,
            None,
            Some(&callable_prototypes),
            64,
            &[],
        )
        .unwrap();
        assert_eq!(recorded.terminal, JitTraceTerminal::LoopBack);
        assert!(recorded.op_names.iter().any(|name| name == "inline_call:0"));
        assert!(recorded.op_names.iter().any(|name| name == "inline_ret"));
        assert!(!recorded.op_names.iter().any(|name| name == "call_value"));
        assert!(
            recorded
                .ssa
                .blocks
                .iter()
                .all(|block| !matches!(block.terminator, Some(SsaTerminator::CallValue { .. })))
        );
    }
}
