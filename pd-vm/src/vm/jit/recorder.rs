use std::fmt;

use crate::builtins::BuiltinFunction;
use crate::vm::{OpCode, Program, Value, ValueType, checked_int_div};

use super::JitTraceTerminal;
use super::deopt::materialize_ssa_values;
use super::ir::{
    SsaBranchTarget, SsaInstKind, SsaMaterialization, SsaTerminator, SsaTrace, SsaTraceBuilder,
    SsaValue, SsaValueRepr,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecordedTrace {
    pub(crate) has_call: bool,
    pub(crate) has_yielding_call: bool,
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
    LiveStackOnBackedge {
        depth: usize,
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
            Self::LiveStackOnBackedge { depth } => {
                write!(
                    f,
                    "backedge requires empty symbolic stack, got depth {depth}"
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
}

impl ValueInfo {
    fn tagged() -> Self {
        Self {
            repr: SsaValueRepr::Tagged,
            const_int: None,
            const_float: None,
            const_bool: None,
        }
    }

    fn int(value: Option<i64>) -> Self {
        Self {
            repr: SsaValueRepr::I64,
            const_int: value,
            const_float: None,
            const_bool: None,
        }
    }

    fn float(value: Option<f64>) -> Self {
        Self {
            repr: SsaValueRepr::F64,
            const_int: None,
            const_float: value,
            const_bool: None,
        }
    }

    fn bool(value: Option<bool>) -> Self {
        Self {
            repr: SsaValueRepr::Bool,
            const_int: None,
            const_float: None,
            const_bool: value,
        }
    }

    fn from_constant(value: &Value) -> Self {
        match value {
            Value::Int(value) => Self::int(Some(*value)),
            Value::Bool(value) => Self::bool(Some(*value)),
            Value::Float(value) => Self::float(Some(*value)),
            Value::Null | Value::String(_) | Value::Bytes(_) | Value::Array(_) | Value::Map(_) => {
                Self::tagged()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AnalysisFrame {
    stack: Vec<ValueInfo>,
    locals: Vec<ValueInfo>,
}

impl AnalysisFrame {
    fn new(local_count: usize) -> Self {
        Self {
            stack: Vec::new(),
            locals: vec![ValueInfo::tagged(); local_count],
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct SymbolicValue {
    value: SsaValue,
    info: ValueInfo,
}

#[derive(Clone, Debug, PartialEq)]
struct SymbolicFrame {
    stack: Vec<SymbolicValue>,
    locals: Vec<SymbolicValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoopHeaderPlan {
    local_reprs: Vec<SsaValueRepr>,
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
    fn new(locals: Vec<SymbolicValue>) -> Self {
        Self {
            stack: Vec::new(),
            locals,
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
        let slot = self
            .locals
            .get_mut(index as usize)
            .ok_or(TraceRecordError::InvalidLocal(index))?;
        *slot = value;
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
        yields: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntBinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
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
        } else if opcode == OpCode::Add as u8
            || opcode == OpCode::Sub as u8
            || opcode == OpCode::Mul as u8
            || opcode == OpCode::Div as u8
            || opcode == OpCode::Mod as u8
            || opcode == OpCode::Shl as u8
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
                    yields: false,
                }
            } else {
                DecodedOp::Call {
                    ip: instr_ip,
                    yields: true,
                }
            }
        } else {
            return Err(TraceRecordError::UnsupportedOpcode(opcode));
        };

        Ok(Some(decoded))
    }
}

pub(crate) fn record_trace(
    program: &Program,
    root_ip: usize,
    max_trace_len: usize,
) -> Result<RecordedTrace, TraceRecordError> {
    let loop_header_plan = infer_loop_header_plan(program, root_ip, max_trace_len)?;
    let mut builder = SsaTraceBuilder::new(root_ip);
    let entry = builder.entry();

    let entry_locals = (0..program.local_count)
        .map(|local| {
            builder
                .append_param(entry, SsaValueRepr::Tagged, format!("local{local}"))
                .map(|value| SymbolicValue {
                    value,
                    info: ValueInfo::tagged(),
                })
                .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let (loop_body_block, mut current_block, mut frame) = if let Some(loop_plan) = &loop_header_plan
    {
        for (local, repr) in loop_plan.local_reprs.iter().copied().enumerate() {
            validate_loop_carrier_repr(local, repr)?;
        }
        let body = builder.create_block();
        let mut body_locals = Vec::with_capacity(loop_plan.local_reprs.len());
        let mut entry_args = Vec::with_capacity(loop_plan.local_reprs.len());
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
        (body, body, SymbolicFrame::new(body_locals))
    } else {
        (entry, entry, SymbolicFrame::new(entry_locals.clone()))
    };

    let mut cursor = TraceCursor::new(program, root_ip, max_trace_len);
    let mut terminal = None;
    let mut op_names = Vec::new();
    let mut has_call = false;
    let mut has_yielding_call = false;

    loop {
        let Some(decoded) = cursor.next()? else {
            break;
        };

        match decoded {
            DecodedOp::Nop { .. } => op_names.push("nop".to_string()),
            DecodedOp::Ret { ip } => {
                op_names.push("ret".to_string());
                let exit = builder.add_exit(
                    ip,
                    materialize_stack(&frame.stack),
                    materialize_locals(&frame.locals),
                );
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
                frame.push(frame.local(index)?);
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
            DecodedOp::BinOp { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let (name, out) =
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
                    };
                op_names.push(name.to_string());
                frame.push(out);
            }
            DecodedOp::Compare { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                let (name, out) =
                    match select_numeric_compare(program, ip, opcode, lhs.info, rhs.info)? {
                        NumericCompareKind::Int(kind) => {
                            let lhs = ensure_int(&mut builder, current_block, ip, lhs)?;
                            let rhs = ensure_int(&mut builder, current_block, ip, rhs)?;
                            emit_int_compare(&mut builder, current_block, ip, kind, lhs, rhs)?
                        }
                        NumericCompareKind::Float(kind) => {
                            let lhs = ensure_float(&mut builder, current_block, ip, lhs)?;
                            let rhs = ensure_float(&mut builder, current_block, ip, rhs)?;
                            emit_float_compare(&mut builder, current_block, ip, kind, lhs, rhs)?
                        }
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
                    if !frame.stack.is_empty() {
                        return Err(TraceRecordError::LiveStackOnBackedge {
                            depth: frame.stack.len(),
                        });
                    }
                    let exit = builder.add_exit(
                        fallthrough_ip,
                        materialize_stack(&frame.stack),
                        materialize_locals(&frame.locals),
                    );
                    builder
                        .set_terminator(
                            current_block,
                            SsaTerminator::BranchBool {
                                condition: condition.value.id,
                                if_true: SsaBranchTarget::Exit(exit),
                                if_false: SsaBranchTarget::Block {
                                    target: loop_body_block,
                                    args: loop_backedge_args(&frame.locals),
                                },
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    terminal = Some(JitTraceTerminal::BranchExit);
                    break;
                }

                if condition.info.const_bool == Some(false) {
                    op_names.push("guard_true".to_string());
                    let exit = builder.add_exit(
                        fallthrough_ip,
                        materialize_stack(&frame.stack),
                        materialize_locals(&frame.locals),
                    );
                    let (next_block, next_frame, args) =
                        continue_with_frame(&mut builder, &frame, "guard")?;
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
                    continue;
                }

                if prefer_join_path {
                    op_names.push("guard_true".to_string());
                    let exit = builder.add_exit(
                        fallthrough_ip,
                        materialize_stack(&frame.stack),
                        materialize_locals(&frame.locals),
                    );
                    let (next_block, next_frame, args) =
                        continue_with_frame(&mut builder, &frame, "guard")?;
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
                    continue;
                }

                op_names.push("guard_false".to_string());
                let exit = builder.add_exit(
                    target,
                    materialize_stack(&frame.stack),
                    materialize_locals(&frame.locals),
                );
                let (next_block, next_frame, args) =
                    continue_with_frame(&mut builder, &frame, "guard")?;
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
                    if !frame.stack.is_empty() {
                        return Err(TraceRecordError::LiveStackOnBackedge {
                            depth: frame.stack.len(),
                        });
                    }
                    builder
                        .set_terminator(
                            current_block,
                            SsaTerminator::Jump {
                                target: loop_body_block,
                                args: loop_backedge_args(&frame.locals),
                            },
                        )
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    terminal = Some(JitTraceTerminal::LoopBack);
                    break;
                }
                if target < cursor.ip() {
                    op_names.push("jump_ip".to_string());
                    let exit = builder.add_exit(
                        target,
                        materialize_stack(&frame.stack),
                        materialize_locals(&frame.locals),
                    );
                    builder
                        .set_terminator(current_block, SsaTerminator::Exit { exit })
                        .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                    terminal = Some(JitTraceTerminal::BranchExit);
                    break;
                }
                cursor.jump_to(target)?;
            }
            DecodedOp::Call { ip, yields } => {
                has_call = true;
                has_yielding_call |= yields;
                op_names.push("call".to_string());
                let exit = builder.add_exit(
                    ip,
                    materialize_stack(&frame.stack),
                    materialize_locals(&frame.locals),
                );
                builder
                    .set_terminator(current_block, SsaTerminator::Exit { exit })
                    .map_err(|err| TraceRecordError::InvalidIr(err.to_string()))?;
                terminal = Some(JitTraceTerminal::BranchExit);
                break;
            }
        }
    }

    let terminal = terminal.ok_or(TraceRecordError::MissingTerminal)?;
    let ssa = builder.finish();
    ssa.verify()
        .map_err(|err| TraceRecordError::InvalidIr(format!("{err:?}")))?;

    Ok(RecordedTrace {
        has_call,
        has_yielding_call,
        op_names,
        ssa,
        terminal,
    })
}

fn infer_loop_header_plan(
    program: &Program,
    root_ip: usize,
    max_trace_len: usize,
) -> Result<Option<LoopHeaderPlan>, TraceRecordError> {
    let mut cursor = TraceCursor::new(program, root_ip, max_trace_len);
    let mut frame = AnalysisFrame::new(program.local_count);
    let mut entry_use = vec![EntryUseState::Untouched; program.local_count];

    loop {
        let Some(decoded) = cursor.next()? else {
            return Ok(None);
        };

        match decoded {
            DecodedOp::Nop { .. } => {}
            DecodedOp::Ret { .. } | DecodedOp::Call { .. } => return Ok(None),
            DecodedOp::Ldc { index, .. } => {
                let constant = program.constants.get(index as usize).ok_or_else(|| {
                    TraceRecordError::UnsupportedTrace(format!(
                        "unsupported constant #{index}: missing constant"
                    ))
                })?;
                frame.push(ValueInfo::from_constant(constant));
            }
            DecodedOp::Ldloc { index, .. } => {
                if let Some(state) = entry_use.get_mut(index as usize)
                    && matches!(*state, EntryUseState::Untouched)
                {
                    *state = EntryUseState::ReadBeforeWrite;
                }
                frame.push(frame.local(index)?)
            }
            DecodedOp::Stloc { index, .. } => {
                if let Some(state) = entry_use.get_mut(index as usize)
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
            DecodedOp::BinOp { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
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
                }
            }
            DecodedOp::Compare { ip, opcode } => {
                let rhs = frame.pop()?;
                let lhs = frame.pop()?;
                match select_numeric_compare(program, ip, opcode, lhs, rhs)? {
                    NumericCompareKind::Int(kind) => {
                        let lhs = expect_int_info(lhs)?;
                        let rhs = expect_int_info(rhs)?;
                        validate_int_compare_operands(program, ip, kind, lhs, rhs)?;
                    }
                    NumericCompareKind::Float(kind) => {
                        let lhs = expect_float_info(lhs)?;
                        let rhs = expect_float_info(rhs)?;
                        validate_float_compare_operands(program, ip, kind, lhs, rhs)?;
                    }
                }
                frame.push(ValueInfo::bool(None));
            }
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
                    if !frame.stack.is_empty() {
                        return Err(TraceRecordError::LiveStackOnBackedge {
                            depth: frame.stack.len(),
                        });
                    }
                    return Ok(Some(build_loop_header_plan(&frame.locals, &entry_use)));
                }
                if condition.const_bool == Some(false) {
                    cursor.jump_to(target)?;
                }
            }
            DecodedOp::Br { target, .. } => {
                if target == root_ip {
                    if !frame.stack.is_empty() {
                        return Err(TraceRecordError::LiveStackOnBackedge {
                            depth: frame.stack.len(),
                        });
                    }
                    return Ok(Some(build_loop_header_plan(&frame.locals, &entry_use)));
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

fn select_numeric_binop(
    program: &Program,
    ip: usize,
    opcode: u8,
    lhs: ValueInfo,
    rhs: ValueInfo,
) -> Result<NumericBinOpKind, TraceRecordError> {
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
        x if x == OpCode::Add as u8 => match operand_types {
            (ValueType::Int, ValueType::Int) => Ok(NumericBinOpKind::Int(IntBinOpKind::Add)),
            (ValueType::Float, ValueType::Float) => {
                Ok(NumericBinOpKind::Float(FloatBinOpKind::Add))
            }
            (ValueType::String, ValueType::String) | (ValueType::Bytes, ValueType::Bytes) => {
                Err(TraceRecordError::UnsupportedTrace(
                    "SSA recorder does not support string/bytes concat traces yet".to_string(),
                ))
            }
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
            _ => Err(TraceRecordError::UnsupportedTrace(
                "SSA recorder requires int- or float-specializable add operands".to_string(),
            )),
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
    }
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
    Ok((
        name,
        SymbolicValue {
            value,
            info: ValueInfo::bool(None),
        },
    ))
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
    Ok((
        name,
        SymbolicValue {
            value,
            info: ValueInfo::bool(None),
        },
    ))
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
            },
        });
    }

    Ok((
        block,
        SymbolicFrame {
            stack: next_stack,
            locals: next_locals,
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

fn build_loop_header_plan(locals: &[ValueInfo], entry_use: &[EntryUseState]) -> LoopHeaderPlan {
    let mut local_reprs = Vec::with_capacity(locals.len());
    let mut entry_seed = Vec::with_capacity(locals.len());
    for (local, state) in locals.iter().zip(entry_use.iter()) {
        if matches!(state, EntryUseState::WrittenBeforeRead) {
            local_reprs.push(local.repr);
            entry_seed.push(match local.repr {
                SsaValueRepr::I64 => LoopSeed::ZeroI64,
                SsaValueRepr::F64 => LoopSeed::ZeroF64,
                SsaValueRepr::Bool => LoopSeed::FalseBool,
                _ => LoopSeed::Entry,
            });
        } else {
            local_reprs.push(local.repr);
            entry_seed.push(LoopSeed::Entry);
        }
    }
    LoopHeaderPlan {
        local_reprs,
        entry_seed,
    }
}

fn loop_backedge_args(locals: &[SymbolicValue]) -> Vec<super::ir::SsaValueId> {
    locals.iter().map(|value| value.value.id).collect()
}

fn validate_loop_carrier_repr(local: usize, repr: SsaValueRepr) -> Result<(), TraceRecordError> {
    match repr {
        SsaValueRepr::Tagged | SsaValueRepr::I64 | SsaValueRepr::F64 | SsaValueRepr::Bool => Ok(()),
        other => Err(TraceRecordError::UnsupportedTrace(format!(
            "unsupported loop-carried local {local} representation {other}"
        ))),
    }
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
    use crate::{BytecodeBuilder, Value};

    fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
        let start = instr_ip as usize + 1;
        code[start..start + 4].copy_from_slice(&target.to_le_bytes());
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

        let recorded = record_trace(&program, 0, 32).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::Halt);
        assert!(recorded.op_names.iter().any(|name| name == "iadd_imm"));
        assert!(matches!(
            recorded.ssa.blocks[0].terminator,
            Some(SsaTerminator::Return { .. })
        ));
        let exit = &recorded.ssa.exits[0];
        assert_eq!(exit.exit_ip, ret_ip as usize);
        assert!(matches!(exit.locals[0], SsaMaterialization::BoxInt(_)));
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

        let recorded = record_trace(&program, root_ip as usize, 64).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::BranchExit);
        assert!(recorded.op_names.iter().any(|name| name == "loop_if_false"));
        assert_eq!(recorded.ssa.blocks.len(), 2);
        assert!(matches!(
            recorded.ssa.blocks[1].terminator,
            Some(SsaTerminator::BranchBool { .. })
        ));
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

        let recorded = record_trace(&program, root_ip as usize, 128).expect("recorded trace");
        assert_eq!(recorded.terminal, JitTraceTerminal::LoopBack);
        assert!(recorded.op_names.iter().any(|name| name == "guard_false"));
        assert!(recorded.op_names.iter().any(|name| name == "jump_root"));
        assert_eq!(recorded.ssa.blocks.len(), 3);
        assert!(matches!(
            recorded.ssa.blocks[2].terminator,
            Some(SsaTerminator::Jump { .. })
        ));
    }
}
