use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::{self, Write};

use crate::builtins::BuiltinFunction;
use crate::vm::{Program, Value, ValueType};

use super::cfg::AotBlockTerminal;
use super::ir::{
    AotBytesCodecKind, AotCall, AotCallDispatch, AotConcatKind, AotInstruction, AotLowerError,
    AotProgram, AotTextBytesKind, lower_program,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct AotSsaValueId(u32);

impl AotSsaValueId {
    fn new(raw: u32) -> Self {
        Self(raw)
    }
}

impl fmt::Display for AotSsaValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct AotSsaBlockId(u32);

impl AotSsaBlockId {
    fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for AotSsaBlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct AotCheckpointId(u32);

impl AotCheckpointId {
    fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for AotCheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cp{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AotSsaValueRepr {
    Tagged,
    I64,
    F64,
    Bool,
    HeapPtr(ValueType),
}

impl fmt::Display for AotSsaValueRepr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tagged => f.write_str("tagged"),
            Self::I64 => f.write_str("i64"),
            Self::F64 => f.write_str("f64"),
            Self::Bool => f.write_str("bool"),
            Self::HeapPtr(tag) => write!(f, "ptr<{tag:?}>"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AotSsaValue {
    pub(crate) id: AotSsaValueId,
    pub(crate) repr: AotSsaValueRepr,
}

impl AotSsaValue {
    fn new(id: AotSsaValueId, repr: AotSsaValueRepr) -> Self {
        Self { id, repr }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotSsaBlockParam {
    pub(crate) value: AotSsaValue,
    pub(crate) label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AotSsaInstKind {
    IntConst(i64),
    FloatConst(f64),
    BoolConst(bool),
    ConstSlot {
        index: u32,
    },
    StringLen {
        text: AotSsaValueId,
    },
    BytesLen {
        bytes: AotSsaValueId,
    },
    StringSlice {
        text: AotSsaValueId,
        start: AotSsaValueId,
        length: AotSsaValueId,
    },
    BytesSlice {
        bytes: AotSsaValueId,
        start: AotSsaValueId,
        length: AotSsaValueId,
    },
    StringGet {
        text: AotSsaValueId,
        index: AotSsaValueId,
    },
    BytesGet {
        bytes: AotSsaValueId,
        index: AotSsaValueId,
    },
    BytesHas {
        bytes: AotSsaValueId,
        index: AotSsaValueId,
    },
    StringConcat {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BytesConcat {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BytesFromArrayU8 {
        array: AotSsaValueId,
    },
    BytesToArrayU8 {
        bytes: AotSsaValueId,
    },
    IntAdd {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntSub {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntMul {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntDiv {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntMod {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntShl {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntShr {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntLshr {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatAdd {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatSub {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatMul {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatDiv {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatMod {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BoolAnd {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BoolOr {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BoolNot {
        input: AotSsaValueId,
    },
    TaggedToInt {
        input: AotSsaValueId,
    },
    TaggedNumberToFloat {
        input: AotSsaValueId,
    },
    IntToFloat {
        input: AotSsaValueId,
    },
    IntNeg {
        input: AotSsaValueId,
    },
    FloatNeg {
        input: AotSsaValueId,
    },
    IntCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntCmpLt {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    IntCmpGt {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BoolCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    TaggedCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    StringCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    BytesCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    NullCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatCmpEq {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatCmpLt {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
    FloatCmpGt {
        lhs: AotSsaValueId,
        rhs: AotSsaValueId,
    },
}

impl AotSsaInstKind {
    fn inputs(&self) -> Vec<AotSsaValueId> {
        match self {
            Self::IntConst(_)
            | Self::FloatConst(_)
            | Self::BoolConst(_)
            | Self::ConstSlot { .. } => Vec::new(),
            Self::StringLen { text } => vec![*text],
            Self::BytesLen { bytes } => vec![*bytes],
            Self::StringSlice {
                text,
                start,
                length,
            } => vec![*text, *start, *length],
            Self::BytesSlice {
                bytes,
                start,
                length,
            } => vec![*bytes, *start, *length],
            Self::StringGet { text, index } => vec![*text, *index],
            Self::BytesGet { bytes, index } => vec![*bytes, *index],
            Self::BytesHas { bytes, index } => vec![*bytes, *index],
            Self::StringConcat { lhs, rhs } | Self::BytesConcat { lhs, rhs } => vec![*lhs, *rhs],
            Self::BytesFromArrayU8 { array } => vec![*array],
            Self::BytesToArrayU8 { bytes } => vec![*bytes],
            Self::IntAdd { lhs, rhs }
            | Self::IntSub { lhs, rhs }
            | Self::IntMul { lhs, rhs }
            | Self::IntDiv { lhs, rhs }
            | Self::IntMod { lhs, rhs }
            | Self::IntShl { lhs, rhs }
            | Self::IntShr { lhs, rhs }
            | Self::IntLshr { lhs, rhs }
            | Self::FloatAdd { lhs, rhs }
            | Self::FloatSub { lhs, rhs }
            | Self::FloatMul { lhs, rhs }
            | Self::FloatDiv { lhs, rhs }
            | Self::FloatMod { lhs, rhs }
            | Self::BoolAnd { lhs, rhs }
            | Self::BoolOr { lhs, rhs }
            | Self::IntCmpEq { lhs, rhs }
            | Self::IntCmpLt { lhs, rhs }
            | Self::IntCmpGt { lhs, rhs }
            | Self::BoolCmpEq { lhs, rhs }
            | Self::TaggedCmpEq { lhs, rhs }
            | Self::StringCmpEq { lhs, rhs }
            | Self::BytesCmpEq { lhs, rhs }
            | Self::NullCmpEq { lhs, rhs }
            | Self::FloatCmpEq { lhs, rhs }
            | Self::FloatCmpLt { lhs, rhs }
            | Self::FloatCmpGt { lhs, rhs } => vec![*lhs, *rhs],
            Self::BoolNot { input }
            | Self::TaggedToInt { input }
            | Self::TaggedNumberToFloat { input }
            | Self::IntToFloat { input }
            | Self::IntNeg { input }
            | Self::FloatNeg { input } => {
                vec![*input]
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AotSsaInst {
    pub(crate) ip: usize,
    pub(crate) output: AotSsaValue,
    pub(crate) kind: AotSsaInstKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AotSsaMaterialization {
    Value(AotSsaValueId),
    BoxInt(AotSsaValueId),
    BoxFloat(AotSsaValueId),
    BoxBool(AotSsaValueId),
    BoxHeapPtr {
        value: AotSsaValueId,
        tag: ValueType,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotCheckpoint {
    pub(crate) id: AotCheckpointId,
    pub(crate) ip: usize,
    pub(crate) target: AotSsaBlockId,
    pub(crate) stack: Vec<AotSsaValueRepr>,
    pub(crate) locals: Vec<AotSsaValueRepr>,
    pub(crate) external: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AotSsaJumpTarget {
    pub(crate) target: AotSsaBlockId,
    pub(crate) args: Vec<AotSsaValueId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AotSsaTerminator {
    Jump(AotSsaJumpTarget),
    BranchBool {
        condition: AotSsaValueId,
        if_true: AotSsaJumpTarget,
        if_false: AotSsaJumpTarget,
    },
    CallBoundary {
        call: AotCall,
        stack: Vec<AotSsaMaterialization>,
        locals: Vec<AotSsaMaterialization>,
    },
    Return {
        ip: usize,
        stack: Vec<AotSsaMaterialization>,
        locals: Vec<AotSsaMaterialization>,
    },
    Stop {
        ip: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AotSsaBlock {
    pub(crate) id: AotSsaBlockId,
    pub(crate) params: Vec<AotSsaBlockParam>,
    pub(crate) insts: Vec<AotSsaInst>,
    pub(crate) terminator: Option<AotSsaTerminator>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AotSsaProgram {
    pub(crate) entry_ip: usize,
    pub(crate) entry_block: AotSsaBlockId,
    pub(crate) blocks: Vec<AotSsaBlock>,
    pub(crate) checkpoints: Vec<AotCheckpoint>,
    pub(crate) resume_ips: Vec<usize>,
}

impl AotSsaProgram {
    pub(crate) fn checkpoint_for_ip(&self, ip: usize) -> Option<&AotCheckpoint> {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.ip == ip)
    }

    pub(crate) fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(&mut out, "entry={} resume=[{}]", self.entry_block, {
            self.resume_ips
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        });
        for checkpoint in &self.checkpoints {
            let _ = writeln!(
                &mut out,
                "{} ip={} external={} target={} stack=[{}] locals=[{}]",
                checkpoint.id,
                checkpoint.ip,
                checkpoint.external,
                checkpoint.target,
                checkpoint
                    .stack
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                checkpoint
                    .locals
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        for block in &self.blocks {
            let params = block
                .params
                .iter()
                .map(|param| format!("{}:{}={}", param.label, param.value.id, param.value.repr))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(&mut out, "{}({params})", block.id);
            for inst in &block.insts {
                let _ = writeln!(
                    &mut out,
                    "  {}:{} = {:?}",
                    inst.output.id, inst.output.repr, inst.kind
                );
            }
            let _ = writeln!(
                &mut out,
                "  {:?}",
                block.terminator.as_ref().expect("verified terminator")
            );
        }
        out
    }

    pub(crate) fn verify(&self) -> Result<(), AotSsaVerifyError> {
        let mut block_ids = BTreeSet::new();
        let mut checkpoint_ids = BTreeSet::new();
        let mut block_params = HashMap::new();
        let mut value_reprs = HashMap::new();

        for block in &self.blocks {
            if !block_ids.insert(block.id) {
                return Err(AotSsaVerifyError::DuplicateBlock(block.id));
            }
            let reprs = block
                .params
                .iter()
                .map(|param| {
                    value_reprs.insert(param.value.id, param.value.repr);
                    param.value.repr
                })
                .collect::<Vec<_>>();
            block_params.insert(block.id, reprs);
        }

        for block in &self.blocks {
            let Some(terminator) = block.terminator.as_ref() else {
                return Err(AotSsaVerifyError::MissingTerminator(block.id));
            };
            let mut available_values = HashMap::new();
            for param in &block.params {
                available_values.insert(param.value.id, param.value.repr);
            }
            for inst in &block.insts {
                for input in inst.kind.inputs() {
                    if !available_values.contains_key(&input) {
                        return Err(AotSsaVerifyError::UseBeforeDef {
                            block: block.id,
                            value: input,
                        });
                    }
                }
                available_values.insert(inst.output.id, inst.output.repr);
                value_reprs.insert(inst.output.id, inst.output.repr);
            }
            verify_terminator(terminator, &block_params, &available_values)?;
        }

        let Some(entry_block) = self.blocks.get(self.entry_block.index()) else {
            return Err(AotSsaVerifyError::UnknownEntry(self.entry_block));
        };
        if entry_block.id != self.entry_block {
            return Err(AotSsaVerifyError::UnknownEntry(self.entry_block));
        }

        for checkpoint in &self.checkpoints {
            if !checkpoint_ids.insert(checkpoint.id) {
                return Err(AotSsaVerifyError::DuplicateCheckpoint(checkpoint.id));
            }
            let params = block_params
                .get(&checkpoint.target)
                .ok_or(AotSsaVerifyError::UnknownBlock(checkpoint.target))?;
            let expected = checkpoint
                .stack
                .iter()
                .chain(checkpoint.locals.iter())
                .copied()
                .collect::<Vec<_>>();
            if *params != expected {
                return Err(AotSsaVerifyError::CheckpointArityMismatch {
                    checkpoint: checkpoint.id,
                    target: checkpoint.target,
                });
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum AotSsaVerifyError {
    DuplicateBlock(AotSsaBlockId),
    DuplicateCheckpoint(AotCheckpointId),
    MissingTerminator(AotSsaBlockId),
    UnknownEntry(AotSsaBlockId),
    UnknownBlock(AotSsaBlockId),
    UnknownValue(AotSsaValueId),
    UseBeforeDef {
        block: AotSsaBlockId,
        value: AotSsaValueId,
    },
    JumpArityMismatch {
        target: AotSsaBlockId,
    },
    JumpReprMismatch {
        target: AotSsaBlockId,
    },
    NonBoolBranchCondition(AotSsaValueId),
    CheckpointArityMismatch {
        checkpoint: AotCheckpointId,
        target: AotSsaBlockId,
    },
    InvalidMaterialization {
        value: AotSsaValueId,
        expected: AotSsaValueRepr,
        actual: AotSsaValueRepr,
    },
}

impl fmt::Display for AotSsaVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug)]
pub(crate) enum AotSsaBuildError {
    Lower(AotLowerError),
    Verify(AotSsaVerifyError),
    InvalidCheckpointIp(usize),
    InvalidLocal(u8),
    InvalidConstant(u32),
    UnsupportedInstruction {
        ip: usize,
        instruction: String,
    },
    StackUnderflow {
        ip: usize,
        instruction: &'static str,
    },
    FrameMismatch {
        ip: usize,
        expected: FrameShape,
        actual: FrameShape,
    },
    NonBoolBranchCondition {
        ip: usize,
        repr: AotSsaValueRepr,
    },
}

impl From<AotLowerError> for AotSsaBuildError {
    fn from(value: AotLowerError) -> Self {
        Self::Lower(value)
    }
}

impl From<AotSsaVerifyError> for AotSsaBuildError {
    fn from(value: AotSsaVerifyError) -> Self {
        Self::Verify(value)
    }
}

impl fmt::Display for AotSsaBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Clone, Copy)]
struct FrameValue {
    value: AotSsaValue,
}

#[derive(Clone)]
struct Frame {
    stack: Vec<FrameValue>,
    locals: Vec<FrameValue>,
}

impl Frame {
    fn shape(&self) -> FrameShape {
        FrameShape {
            stack: self.stack.iter().map(|value| value.value.repr).collect(),
            locals: self.locals.iter().map(|value| value.value.repr).collect(),
        }
    }

    fn pop(
        &mut self,
        ip: usize,
        instruction: &'static str,
    ) -> Result<FrameValue, AotSsaBuildError> {
        self.stack
            .pop()
            .ok_or(AotSsaBuildError::StackUnderflow { ip, instruction })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameShape {
    stack: Vec<AotSsaValueRepr>,
    locals: Vec<AotSsaValueRepr>,
}

#[derive(Clone)]
struct DecodedStep {
    ip: usize,
    next_ip: usize,
    instruction: AotInstruction,
}

#[derive(Clone)]
struct DecodedBlock {
    start_ip: usize,
    end_ip: usize,
    steps: Vec<DecodedStep>,
    terminal: AotBlockTerminal,
    terminal_ip: Option<usize>,
}

fn verify_terminator(
    terminator: &AotSsaTerminator,
    block_params: &HashMap<AotSsaBlockId, Vec<AotSsaValueRepr>>,
    available_values: &HashMap<AotSsaValueId, AotSsaValueRepr>,
) -> Result<(), AotSsaVerifyError> {
    match terminator {
        AotSsaTerminator::Jump(target) => {
            verify_jump_target(target, block_params, available_values)
        }
        AotSsaTerminator::BranchBool {
            condition,
            if_true,
            if_false,
        } => {
            if available_values.get(condition) != Some(&AotSsaValueRepr::Bool) {
                return Err(AotSsaVerifyError::NonBoolBranchCondition(*condition));
            }
            verify_jump_target(if_true, block_params, available_values)?;
            verify_jump_target(if_false, block_params, available_values)
        }
        AotSsaTerminator::CallBoundary { stack, locals, .. }
        | AotSsaTerminator::Return { stack, locals, .. } => {
            for materialization in stack.iter().chain(locals.iter()) {
                verify_materialization(materialization, available_values)?;
            }
            Ok(())
        }
        AotSsaTerminator::Stop { .. } => Ok(()),
    }
}

fn verify_jump_target(
    target: &AotSsaJumpTarget,
    block_params: &HashMap<AotSsaBlockId, Vec<AotSsaValueRepr>>,
    available_values: &HashMap<AotSsaValueId, AotSsaValueRepr>,
) -> Result<(), AotSsaVerifyError> {
    let expected = block_params
        .get(&target.target)
        .ok_or(AotSsaVerifyError::UnknownBlock(target.target))?;
    if expected.len() != target.args.len() {
        return Err(AotSsaVerifyError::JumpArityMismatch {
            target: target.target,
        });
    }
    for (arg, repr) in target.args.iter().zip(expected.iter()) {
        let actual = available_values
            .get(arg)
            .copied()
            .ok_or(AotSsaVerifyError::UnknownValue(*arg))?;
        if !jump_arg_repr_compatible(actual, *repr) {
            return Err(AotSsaVerifyError::JumpReprMismatch {
                target: target.target,
            });
        }
    }
    Ok(())
}

fn jump_arg_repr_compatible(src: AotSsaValueRepr, dst: AotSsaValueRepr) -> bool {
    src == dst
        || matches!(
            (src, dst),
            (AotSsaValueRepr::I64, AotSsaValueRepr::Tagged)
                | (AotSsaValueRepr::F64, AotSsaValueRepr::Tagged)
                | (AotSsaValueRepr::Bool, AotSsaValueRepr::Tagged)
                | (AotSsaValueRepr::Tagged, AotSsaValueRepr::I64)
                | (AotSsaValueRepr::Tagged, AotSsaValueRepr::F64)
                | (AotSsaValueRepr::Tagged, AotSsaValueRepr::Bool)
        )
}

fn verify_materialization(
    materialization: &AotSsaMaterialization,
    value_reprs: &HashMap<AotSsaValueId, AotSsaValueRepr>,
) -> Result<(), AotSsaVerifyError> {
    let (value, expected) = match materialization {
        AotSsaMaterialization::Value(value) => (*value, AotSsaValueRepr::Tagged),
        AotSsaMaterialization::BoxInt(value) => (*value, AotSsaValueRepr::I64),
        AotSsaMaterialization::BoxFloat(value) => (*value, AotSsaValueRepr::F64),
        AotSsaMaterialization::BoxBool(value) => (*value, AotSsaValueRepr::Bool),
        AotSsaMaterialization::BoxHeapPtr { value, tag } => {
            (*value, AotSsaValueRepr::HeapPtr(*tag))
        }
    };
    let actual = value_reprs
        .get(&value)
        .copied()
        .ok_or(AotSsaVerifyError::UnknownValue(value))?;
    if actual != expected {
        return Err(AotSsaVerifyError::InvalidMaterialization {
            value,
            expected,
            actual,
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct StepLoc {
    block_index: usize,
    step_index: usize,
}

#[derive(Clone)]
enum ProcessResult {
    Jump {
        target_ip: usize,
        frame: Frame,
    },
    Branch {
        condition: FrameValue,
        if_true_ip: usize,
        if_false_ip: usize,
        frame_after_pop: Frame,
    },
    CallBoundary {
        call: AotCall,
        frame: Frame,
        resume_frame: Frame,
    },
    Return {
        ip: usize,
        frame: Frame,
    },
    Stop {
        ip: usize,
        frame: Frame,
    },
}

trait InstEmitter {
    fn emit(&mut self, ip: usize, kind: AotSsaInstKind, repr: AotSsaValueRepr) -> AotSsaValue;
}

struct ShapeEmitter {
    next_value: u32,
}

impl InstEmitter for ShapeEmitter {
    fn emit(&mut self, _ip: usize, _kind: AotSsaInstKind, repr: AotSsaValueRepr) -> AotSsaValue {
        let value = AotSsaValue::new(AotSsaValueId::new(self.next_value), repr);
        self.next_value = self.next_value.saturating_add(1);
        value
    }
}

struct BlockEmitter<'a> {
    block: &'a mut AotSsaBlock,
    next_value: &'a mut u32,
}

impl<'a> InstEmitter for BlockEmitter<'a> {
    fn emit(&mut self, ip: usize, kind: AotSsaInstKind, repr: AotSsaValueRepr) -> AotSsaValue {
        let value = AotSsaValue::new(AotSsaValueId::new(*self.next_value), repr);
        *self.next_value = self.next_value.saturating_add(1);
        self.block.insts.push(AotSsaInst {
            ip,
            output: value,
            kind,
        });
        value
    }
}

struct Builder<'a> {
    program: &'a Program,
    lowered: AotProgram,
    decoded_blocks: Vec<DecodedBlock>,
    block_lookup: HashMap<usize, usize>,
    step_lookup: HashMap<usize, StepLoc>,
    terminal_lookup: HashMap<usize, usize>,
    checkpoint_ips: BTreeSet<usize>,
    external_resume_ips: BTreeSet<usize>,
    incoming_shapes: HashMap<usize, FrameShape>,
    direct_host_result_counts: HashMap<usize, usize>,
    next_fake_value: u32,
}

impl<'a> Builder<'a> {
    fn new(
        program: &'a Program,
        direct_host_result_counts: HashMap<usize, usize>,
    ) -> Result<Self, AotSsaBuildError> {
        let lowered = lower_program(program)?;
        let decoded_blocks = decode_blocks(program, &lowered)?;
        let mut block_lookup = HashMap::new();
        let mut step_lookup = HashMap::new();
        let mut terminal_lookup = HashMap::new();
        for (block_index, block) in decoded_blocks.iter().enumerate() {
            block_lookup.insert(block.start_ip, block_index);
            for (step_index, step) in block.steps.iter().enumerate() {
                step_lookup.insert(
                    step.ip,
                    StepLoc {
                        block_index,
                        step_index,
                    },
                );
            }
            if let Some(terminal_ip) = block.terminal_ip {
                terminal_lookup.insert(terminal_ip, block_index);
            }
        }
        let mut checkpoint_ips = BTreeSet::new();
        for block in &decoded_blocks {
            checkpoint_ips.insert(block.start_ip);
        }
        let mut external_resume_ips = BTreeSet::from([lowered.entry_ip]);
        for block in &decoded_blocks {
            for step in &block.steps {
                if let AotInstruction::Call(call) = &step.instruction {
                    checkpoint_ips.insert(call.call_ip);
                    checkpoint_ips.insert(call.resume_ip);
                    external_resume_ips.insert(call.call_ip);
                    external_resume_ips.insert(call.resume_ip);
                }
            }
        }
        Ok(Self {
            program,
            lowered,
            decoded_blocks,
            block_lookup,
            step_lookup,
            terminal_lookup,
            checkpoint_ips,
            external_resume_ips,
            incoming_shapes: HashMap::new(),
            direct_host_result_counts,
            next_fake_value: 1,
        })
    }

    fn build(mut self) -> Result<AotSsaProgram, AotSsaBuildError> {
        self.propagate_shapes()?;

        let mut checkpoint_ips = self.incoming_shapes.keys().copied().collect::<Vec<_>>();
        checkpoint_ips.sort_unstable();

        let mut block_ids = HashMap::new();
        let mut checkpoint_ids = HashMap::new();
        let mut blocks = Vec::with_capacity(checkpoint_ips.len());
        let mut checkpoints = Vec::with_capacity(checkpoint_ips.len());
        let mut next_value = 1u32;

        for (index, ip) in checkpoint_ips.iter().copied().enumerate() {
            let block_id = AotSsaBlockId::new(index as u32);
            block_ids.insert(ip, block_id);

            let shape = self
                .incoming_shapes
                .get(&ip)
                .cloned()
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            let mut params = Vec::with_capacity(shape.stack.len() + shape.locals.len());
            for (stack_index, repr) in shape.stack.iter().copied().enumerate() {
                let value = AotSsaValue::new(AotSsaValueId::new(next_value), repr);
                next_value = next_value.saturating_add(1);
                params.push(AotSsaBlockParam {
                    value,
                    label: format!("s{stack_index}"),
                });
            }
            for (local_index, repr) in shape.locals.iter().copied().enumerate() {
                let value = AotSsaValue::new(AotSsaValueId::new(next_value), repr);
                next_value = next_value.saturating_add(1);
                params.push(AotSsaBlockParam {
                    value,
                    label: format!("l{local_index}"),
                });
            }
            blocks.push(AotSsaBlock {
                id: block_id,
                params,
                insts: Vec::new(),
                terminator: None,
            });
        }

        for (index, ip) in checkpoint_ips.iter().copied().enumerate() {
            let checkpoint_id = AotCheckpointId::new(index as u32);
            checkpoint_ids.insert(ip, checkpoint_id);
            let shape = self
                .incoming_shapes
                .get(&ip)
                .cloned()
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            checkpoints.push(AotCheckpoint {
                id: checkpoint_id,
                ip,
                target: *block_ids
                    .get(&ip)
                    .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?,
                stack: shape.stack,
                locals: shape.locals,
                external: self.external_resume_ips.contains(&ip),
            });
        }

        for ip in checkpoint_ips.iter().copied() {
            let block_id = *block_ids
                .get(&ip)
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            let block = blocks
                .get_mut(block_id.index())
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            let mut emitter = BlockEmitter {
                block,
                next_value: &mut next_value,
            };
            let mut frame = frame_from_params(&emitter.block.params);
            let result = self.process_from_ip(ip, &mut frame, &mut emitter)?;
            emitter.block.terminator = Some(match result {
                ProcessResult::Jump { target_ip, frame } => {
                    AotSsaTerminator::Jump(AotSsaJumpTarget {
                        target: *block_ids
                            .get(&target_ip)
                            .ok_or(AotSsaBuildError::InvalidCheckpointIp(target_ip))?,
                        args: block_args(&frame),
                    })
                }
                ProcessResult::Branch {
                    condition,
                    if_true_ip,
                    if_false_ip,
                    frame_after_pop,
                } => AotSsaTerminator::BranchBool {
                    condition: condition.value.id,
                    if_true: AotSsaJumpTarget {
                        target: *block_ids
                            .get(&if_true_ip)
                            .ok_or(AotSsaBuildError::InvalidCheckpointIp(if_true_ip))?,
                        args: block_args(&frame_after_pop),
                    },
                    if_false: AotSsaJumpTarget {
                        target: *block_ids
                            .get(&if_false_ip)
                            .ok_or(AotSsaBuildError::InvalidCheckpointIp(if_false_ip))?,
                        args: block_args(&frame_after_pop),
                    },
                },
                ProcessResult::CallBoundary {
                    call,
                    frame,
                    resume_frame: _,
                } => AotSsaTerminator::CallBoundary {
                    call,
                    stack: materialize_values(&frame.stack),
                    locals: materialize_values(&frame.locals),
                },
                ProcessResult::Return { ip, frame } => AotSsaTerminator::Return {
                    ip,
                    stack: materialize_values(&frame.stack),
                    locals: materialize_values(&frame.locals),
                },
                ProcessResult::Stop { ip, .. } => AotSsaTerminator::Stop { ip },
            });
        }

        let entry_block = *block_ids
            .get(&self.lowered.entry_ip)
            .ok_or(AotSsaBuildError::InvalidCheckpointIp(self.lowered.entry_ip))?;
        let resume_ips = checkpoint_ips
            .iter()
            .copied()
            .filter(|ip| self.external_resume_ips.contains(ip))
            .collect::<Vec<_>>();

        let program = AotSsaProgram {
            entry_ip: self.lowered.entry_ip,
            entry_block,
            blocks,
            checkpoints,
            resume_ips,
        };
        program.verify()?;
        Ok(program)
    }

    fn propagate_shapes(&mut self) -> Result<(), AotSsaBuildError> {
        let entry_shape = Frame {
            stack: Vec::new(),
            locals: (0..self.program.local_count)
                .map(|local| FrameValue {
                    value: AotSsaValue::new(
                        AotSsaValueId::new(self.fresh_fake_value()),
                        entry_local_repr(self.program, local),
                    ),
                })
                .collect(),
        }
        .shape();

        self.incoming_shapes
            .insert(self.lowered.entry_ip, entry_shape.clone());
        let mut queue = VecDeque::from([self.lowered.entry_ip]);

        while let Some(ip) = queue.pop_front() {
            let shape = self
                .incoming_shapes
                .get(&ip)
                .cloned()
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            let mut frame = frame_from_shape(&shape, &mut self.next_fake_value);
            let mut emitter = ShapeEmitter {
                next_value: self.next_fake_value,
            };
            let result = self.process_from_ip(ip, &mut frame, &mut emitter)?;
            self.next_fake_value = emitter.next_value;
            match result {
                ProcessResult::Jump { target_ip, frame } => {
                    self.merge_shape(target_ip, frame.shape(), &mut queue)?;
                }
                ProcessResult::Branch {
                    if_true_ip,
                    if_false_ip,
                    frame_after_pop,
                    ..
                } => {
                    let shape = frame_after_pop.shape();
                    self.merge_shape(if_true_ip, shape.clone(), &mut queue)?;
                    self.merge_shape(if_false_ip, shape, &mut queue)?;
                }
                ProcessResult::CallBoundary {
                    call, resume_frame, ..
                } => {
                    self.merge_shape(call.resume_ip, resume_frame.shape(), &mut queue)?;
                }
                ProcessResult::Return { .. } | ProcessResult::Stop { .. } => {}
            }
        }

        Ok(())
    }

    fn merge_shape(
        &mut self,
        ip: usize,
        shape: FrameShape,
        queue: &mut VecDeque<usize>,
    ) -> Result<(), AotSsaBuildError> {
        match self.incoming_shapes.get(&ip) {
            Some(existing) if existing == &shape => Ok(()),
            Some(existing) => {
                let Some(merged) = merge_frame_shapes(existing, &shape) else {
                    return Err(AotSsaBuildError::FrameMismatch {
                        ip,
                        expected: existing.clone(),
                        actual: shape,
                    });
                };
                if &merged != existing {
                    self.incoming_shapes.insert(ip, merged);
                    queue.push_back(ip);
                }
                Ok(())
            }
            None => {
                self.checkpoint_ips.insert(ip);
                self.incoming_shapes.insert(ip, shape);
                queue.push_back(ip);
                Ok(())
            }
        }
    }

    fn lookup_block_and_step(&self, ip: usize) -> Result<(&DecodedBlock, usize), AotSsaBuildError> {
        if let Some(loc) = self.step_lookup.get(&ip).copied() {
            let block = self
                .decoded_blocks
                .get(loc.block_index)
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            return Ok((block, loc.step_index));
        }
        if let Some(terminal_block_index) = self.terminal_lookup.get(&ip).copied() {
            let block = self
                .decoded_blocks
                .get(terminal_block_index)
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            return Ok((block, block.steps.len()));
        }
        let block_index = *self
            .block_lookup
            .get(&ip)
            .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
        let block = self
            .decoded_blocks
            .get(block_index)
            .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
        if block.start_ip == ip {
            return Ok((block, 0));
        }
        Err(AotSsaBuildError::InvalidCheckpointIp(ip))
    }

    fn fresh_fake_value(&mut self) -> u32 {
        let next = self.next_fake_value;
        self.next_fake_value = self.next_fake_value.saturating_add(1);
        next
    }
}

fn merge_frame_shapes(lhs: &FrameShape, rhs: &FrameShape) -> Option<FrameShape> {
    if lhs.stack.len() != rhs.stack.len() || lhs.locals.len() != rhs.locals.len() {
        return None;
    }
    Some(FrameShape {
        stack: lhs
            .stack
            .iter()
            .copied()
            .zip(rhs.stack.iter().copied())
            .map(|(lhs, rhs)| merge_value_repr(lhs, rhs))
            .collect(),
        locals: lhs
            .locals
            .iter()
            .copied()
            .zip(rhs.locals.iter().copied())
            .map(|(lhs, rhs)| merge_value_repr(lhs, rhs))
            .collect(),
    })
}

fn merge_value_repr(lhs: AotSsaValueRepr, rhs: AotSsaValueRepr) -> AotSsaValueRepr {
    if lhs == rhs {
        lhs
    } else {
        AotSsaValueRepr::Tagged
    }
}

pub(crate) fn build_aot_ssa(program: &Program) -> Result<AotSsaProgram, AotSsaBuildError> {
    let ambiguous_calls = collect_ambiguous_direct_host_calls(program)?;
    if ambiguous_calls.is_empty() {
        return Builder::new(program, HashMap::new())?.build();
    }

    let mut first_error = None;
    let max_variants = if ambiguous_calls.len() > 16 {
        1
    } else {
        1usize << ambiguous_calls.len()
    };
    for mask in 0..max_variants {
        let mut direct_host_result_counts = HashMap::new();
        for (index, call_ip) in ambiguous_calls.iter().copied().enumerate() {
            let result_count = if (mask >> index) & 1 == 0 { 1 } else { 0 };
            direct_host_result_counts.insert(call_ip, result_count);
        }
        match Builder::new(program, direct_host_result_counts)?.build() {
            Ok(ssa) => return Ok(ssa),
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }

    Err(first_error.expect("ambiguous call search must try at least one build"))
}

fn collect_ambiguous_direct_host_calls(program: &Program) -> Result<Vec<usize>, AotSsaBuildError> {
    if !program.imports.is_empty() {
        return Ok(Vec::new());
    }

    let lowered = lower_program(program)?;
    let mut call_ips = Vec::new();
    for block in &lowered.blocks {
        for instruction in &block.instructions {
            if let AotInstruction::Call(AotCall {
                call_ip,
                dispatch: AotCallDispatch::HostImport,
                ..
            }) = instruction
            {
                call_ips.push(*call_ip);
            }
        }
    }
    Ok(call_ips)
}

impl<'a> Builder<'a> {
    fn process_from_ip<E: InstEmitter>(
        &self,
        start_ip: usize,
        frame: &mut Frame,
        emitter: &mut E,
    ) -> Result<ProcessResult, AotSsaBuildError> {
        let (block, mut step_index) = self.lookup_block_and_step(start_ip)?;
        while let Some(step) = block.steps.get(step_index) {
            if step.ip != start_ip && self.checkpoint_ips.contains(&step.ip) {
                return Ok(ProcessResult::Jump {
                    target_ip: step.ip,
                    frame: frame.clone(),
                });
            }
            if !apply_direct_instruction(
                self.program,
                &self.direct_host_result_counts,
                frame,
                emitter,
                step,
            )? {
                if let AotInstruction::Call(call) = &step.instruction {
                    let mut resume_frame = frame.clone();
                    apply_call_effect(
                        self.program,
                        &self.direct_host_result_counts,
                        &mut resume_frame,
                        call,
                        step.ip,
                    )?;
                    return Ok(ProcessResult::CallBoundary {
                        call: call.clone(),
                        frame: frame.clone(),
                        resume_frame,
                    });
                }
                return Err(AotSsaBuildError::UnsupportedInstruction {
                    ip: step.ip,
                    instruction: format!("{:?}", step.instruction),
                });
            }
            step_index += 1;
        }

        if let Some(terminal_ip) = block.terminal_ip
            && terminal_ip != start_ip
            && self.checkpoint_ips.contains(&terminal_ip)
        {
            return Ok(ProcessResult::Jump {
                target_ip: terminal_ip,
                frame: frame.clone(),
            });
        }

        match &block.terminal {
            AotBlockTerminal::Jump { target_ip } => Ok(ProcessResult::Jump {
                target_ip: *target_ip,
                frame: frame.clone(),
            }),
            AotBlockTerminal::Fallthrough { next_ip } => Ok(ProcessResult::Jump {
                target_ip: *next_ip,
                frame: frame.clone(),
            }),
            AotBlockTerminal::ConditionalJump {
                target_ip,
                fallthrough_ip,
            } => {
                let condition = frame.pop(block.terminal_ip.unwrap_or(block.end_ip), "brfalse")?;
                if condition.value.repr != AotSsaValueRepr::Bool {
                    return Err(AotSsaBuildError::NonBoolBranchCondition {
                        ip: block.terminal_ip.unwrap_or(block.end_ip),
                        repr: condition.value.repr,
                    });
                }
                Ok(ProcessResult::Branch {
                    condition,
                    if_true_ip: *fallthrough_ip,
                    if_false_ip: *target_ip,
                    frame_after_pop: frame.clone(),
                })
            }
            AotBlockTerminal::Return => Ok(ProcessResult::Return {
                ip: block
                    .terminal_ip
                    .ok_or(AotSsaBuildError::InvalidCheckpointIp(block.start_ip))?,
                frame: frame.clone(),
            }),
            AotBlockTerminal::Stop => Ok(ProcessResult::Stop {
                ip: self.program.code.len(),
                frame: frame.clone(),
            }),
        }
    }
}

fn decode_blocks(
    program: &Program,
    lowered: &AotProgram,
) -> Result<Vec<DecodedBlock>, AotSsaBuildError> {
    let mut blocks = Vec::with_capacity(lowered.blocks.len());
    for block in &lowered.blocks {
        let mut steps = Vec::with_capacity(block.instructions.len());
        let mut ip = block.start_ip;
        for instruction in &block.instructions {
            let opcode_byte = *program
                .code
                .get(ip)
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            let opcode = crate::vm::OpCode::try_from(opcode_byte)
                .map_err(|_| AotSsaBuildError::InvalidCheckpointIp(ip))?;
            let next_ip = ip
                .checked_add(1 + opcode.operand_len())
                .ok_or(AotSsaBuildError::InvalidCheckpointIp(ip))?;
            steps.push(DecodedStep {
                ip,
                next_ip,
                instruction: instruction.clone(),
            });
            ip = next_ip;
        }
        blocks.push(DecodedBlock {
            start_ip: block.start_ip,
            end_ip: block.end_ip,
            steps,
            terminal: block.terminal.clone(),
            terminal_ip: terminal_ip(block),
        });
    }
    Ok(blocks)
}

fn terminal_ip(block: &super::ir::AotIrBlock) -> Option<usize> {
    match block.terminal {
        AotBlockTerminal::Return => block.end_ip.checked_sub(1),
        AotBlockTerminal::Jump { .. } | AotBlockTerminal::ConditionalJump { .. } => {
            block.end_ip.checked_sub(5)
        }
        AotBlockTerminal::Fallthrough { .. } | AotBlockTerminal::Stop => None,
    }
}

fn frame_from_shape(shape: &FrameShape, next_value: &mut u32) -> Frame {
    let mut fresh = || {
        let id = *next_value;
        *next_value = next_value.saturating_add(1);
        AotSsaValueId::new(id)
    };
    Frame {
        stack: shape
            .stack
            .iter()
            .copied()
            .map(|repr| FrameValue {
                value: AotSsaValue::new(fresh(), repr),
            })
            .collect(),
        locals: shape
            .locals
            .iter()
            .copied()
            .map(|repr| FrameValue {
                value: AotSsaValue::new(fresh(), repr),
            })
            .collect(),
    }
}

fn frame_from_params(params: &[AotSsaBlockParam]) -> Frame {
    let mut stack = Vec::new();
    let mut locals = Vec::new();
    for param in params {
        if param.label.starts_with('s') {
            stack.push(FrameValue { value: param.value });
        } else {
            locals.push(FrameValue { value: param.value });
        }
    }
    Frame { stack, locals }
}

fn block_args(frame: &Frame) -> Vec<AotSsaValueId> {
    frame
        .stack
        .iter()
        .chain(frame.locals.iter())
        .map(|value| value.value.id)
        .collect()
}

fn materialize_values(values: &[FrameValue]) -> Vec<AotSsaMaterialization> {
    values
        .iter()
        .map(|value| match value.value.repr {
            AotSsaValueRepr::Tagged => AotSsaMaterialization::Value(value.value.id),
            AotSsaValueRepr::I64 => AotSsaMaterialization::BoxInt(value.value.id),
            AotSsaValueRepr::F64 => AotSsaMaterialization::BoxFloat(value.value.id),
            AotSsaValueRepr::Bool => AotSsaMaterialization::BoxBool(value.value.id),
            AotSsaValueRepr::HeapPtr(tag) => AotSsaMaterialization::BoxHeapPtr {
                value: value.value.id,
                tag,
            },
        })
        .collect()
}

fn entry_local_repr(program: &Program, local: usize) -> AotSsaValueRepr {
    value_type_repr(entry_local_type(program, local))
}

fn entry_local_type(program: &Program, local: usize) -> ValueType {
    program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.local_types.get(local))
        .copied()
        .unwrap_or(ValueType::Unknown)
}

fn operand_types_at(program: &Program, ip: usize) -> (ValueType, ValueType) {
    program
        .type_map
        .as_ref()
        .and_then(|type_map| type_map.operand_types.get(&ip))
        .copied()
        .unwrap_or((ValueType::Unknown, ValueType::Unknown))
}

fn value_type_repr(ty: ValueType) -> AotSsaValueRepr {
    match ty {
        ValueType::Int => AotSsaValueRepr::I64,
        ValueType::Float => AotSsaValueRepr::F64,
        ValueType::Bool => AotSsaValueRepr::Bool,
        ValueType::Unknown
        | ValueType::Null
        | ValueType::String
        | ValueType::Bytes
        | ValueType::Array
        | ValueType::Map => AotSsaValueRepr::Tagged,
    }
}

fn value_repr_for_constant(value: &Value) -> AotSsaValueRepr {
    match value {
        Value::Int(_) => AotSsaValueRepr::I64,
        Value::Float(_) => AotSsaValueRepr::F64,
        Value::Bool(_) => AotSsaValueRepr::Bool,
        Value::Null | Value::String(_) | Value::Bytes(_) | Value::Array(_) | Value::Map(_) => {
            AotSsaValueRepr::Tagged
        }
    }
}

fn apply_direct_instruction<E: InstEmitter>(
    program: &Program,
    _direct_host_result_counts: &HashMap<usize, usize>,
    frame: &mut Frame,
    emitter: &mut E,
    step: &DecodedStep,
) -> Result<bool, AotSsaBuildError> {
    let ip = step.ip;
    match &step.instruction {
        AotInstruction::Nop => Ok(true),
        AotInstruction::Ldc { const_index } => {
            let constant = program
                .constants
                .get(*const_index as usize)
                .ok_or(AotSsaBuildError::InvalidConstant(*const_index))?;
            let repr = value_repr_for_constant(constant);
            let kind = match constant {
                Value::Int(value) => AotSsaInstKind::IntConst(*value),
                Value::Float(value) => AotSsaInstKind::FloatConst(*value),
                Value::Bool(value) => AotSsaInstKind::BoolConst(*value),
                Value::Null
                | Value::String(_)
                | Value::Bytes(_)
                | Value::Array(_)
                | Value::Map(_) => AotSsaInstKind::ConstSlot {
                    index: *const_index,
                },
            };
            frame.stack.push(FrameValue {
                value: emitter.emit(ip, kind, repr),
            });
            Ok(true)
        }
        AotInstruction::Ldloc { index } => {
            let value = frame
                .locals
                .get(*index as usize)
                .copied()
                .ok_or(AotSsaBuildError::InvalidLocal(*index))?;
            frame.stack.push(value);
            Ok(true)
        }
        AotInstruction::Stloc { index } => {
            let value = frame.pop(ip, "stloc")?;
            let local = frame
                .locals
                .get_mut(*index as usize)
                .ok_or(AotSsaBuildError::InvalidLocal(*index))?;
            *local = value;
            Ok(true)
        }
        AotInstruction::Pop => {
            frame.pop(ip, "pop")?;
            Ok(true)
        }
        AotInstruction::Dup => {
            let value = *frame.stack.last().ok_or(AotSsaBuildError::StackUnderflow {
                ip,
                instruction: "dup",
            })?;
            frame.stack.push(value);
            Ok(true)
        }
        AotInstruction::IAdd => emit_typed_int_binary(frame, emitter, ip, "iadd", |lhs, rhs| {
            AotSsaInstKind::IntAdd { lhs, rhs }
        }),
        AotInstruction::ISub => emit_typed_int_binary(frame, emitter, ip, "isub", |lhs, rhs| {
            AotSsaInstKind::IntSub { lhs, rhs }
        }),
        AotInstruction::IMul => emit_typed_int_binary(frame, emitter, ip, "imul", |lhs, rhs| {
            AotSsaInstKind::IntMul { lhs, rhs }
        }),
        AotInstruction::Shl => emit_binary(
            frame,
            emitter,
            ip,
            "shl",
            AotSsaValueRepr::I64,
            |lhs, rhs| AotSsaInstKind::IntShl { lhs, rhs },
        ),
        AotInstruction::Shr => emit_binary(
            frame,
            emitter,
            ip,
            "shr",
            AotSsaValueRepr::I64,
            |lhs, rhs| AotSsaInstKind::IntShr { lhs, rhs },
        ),
        AotInstruction::Lshr => emit_binary(
            frame,
            emitter,
            ip,
            "lshr",
            AotSsaValueRepr::I64,
            |lhs, rhs| AotSsaInstKind::IntLshr { lhs, rhs },
        ),
        AotInstruction::FAdd => emit_typed_float_binary(frame, emitter, ip, "fadd", |lhs, rhs| {
            AotSsaInstKind::FloatAdd { lhs, rhs }
        }),
        AotInstruction::FSub => emit_typed_float_binary(frame, emitter, ip, "fsub", |lhs, rhs| {
            AotSsaInstKind::FloatSub { lhs, rhs }
        }),
        AotInstruction::FMul => emit_typed_float_binary(frame, emitter, ip, "fmul", |lhs, rhs| {
            AotSsaInstKind::FloatMul { lhs, rhs }
        }),
        AotInstruction::FDiv => emit_typed_float_binary(frame, emitter, ip, "fdiv", |lhs, rhs| {
            AotSsaInstKind::FloatDiv { lhs, rhs }
        }),
        AotInstruction::Len(AotTextBytesKind::String) => emit_tagged_unary(
            frame,
            emitter,
            ip,
            "string_len",
            AotSsaValueRepr::I64,
            |text| AotSsaInstKind::StringLen { text },
        ),
        AotInstruction::Len(AotTextBytesKind::Bytes) => emit_tagged_unary(
            frame,
            emitter,
            ip,
            "bytes_len",
            AotSsaValueRepr::I64,
            |bytes| AotSsaInstKind::BytesLen { bytes },
        ),
        AotInstruction::Concat(AotConcatKind::String) => {
            emit_tagged_binary(frame, emitter, ip, "string_concat", |lhs, rhs| {
                AotSsaInstKind::StringConcat { lhs, rhs }
            })
        }
        AotInstruction::Concat(AotConcatKind::Bytes) => {
            emit_tagged_binary(frame, emitter, ip, "bytes_concat", |lhs, rhs| {
                AotSsaInstKind::BytesConcat { lhs, rhs }
            })
        }
        AotInstruction::Slice(AotTextBytesKind::String) => emit_tagged_ternary_with_ints(
            frame,
            emitter,
            ip,
            "string_slice",
            |text, start, length| AotSsaInstKind::StringSlice {
                text,
                start,
                length,
            },
        ),
        AotInstruction::Slice(AotTextBytesKind::Bytes) => emit_tagged_ternary_with_ints(
            frame,
            emitter,
            ip,
            "bytes_slice",
            |bytes, start, length| AotSsaInstKind::BytesSlice {
                bytes,
                start,
                length,
            },
        ),
        AotInstruction::Get(AotTextBytesKind::String) => emit_tagged_binary_with_int_rhs(
            frame,
            emitter,
            ip,
            "string_get",
            AotSsaValueRepr::Tagged,
            |text, index| AotSsaInstKind::StringGet { text, index },
        ),
        AotInstruction::Get(AotTextBytesKind::Bytes) => emit_tagged_binary_with_int_rhs(
            frame,
            emitter,
            ip,
            "bytes_get",
            AotSsaValueRepr::I64,
            |bytes, index| AotSsaInstKind::BytesGet { bytes, index },
        ),
        AotInstruction::HasBytes => emit_tagged_binary_with_int_rhs(
            frame,
            emitter,
            ip,
            "bytes_has",
            AotSsaValueRepr::Bool,
            |bytes, index| AotSsaInstKind::BytesHas { bytes, index },
        ),
        AotInstruction::BytesCodec(AotBytesCodecKind::FromArrayU8) => emit_tagged_unary(
            frame,
            emitter,
            ip,
            "bytes_from_array_u8",
            AotSsaValueRepr::Tagged,
            |array| AotSsaInstKind::BytesFromArrayU8 { array },
        ),
        AotInstruction::BytesCodec(AotBytesCodecKind::ToArrayU8) => emit_tagged_unary(
            frame,
            emitter,
            ip,
            "bytes_to_array_u8",
            AotSsaValueRepr::Tagged,
            |bytes| AotSsaInstKind::BytesToArrayU8 { bytes },
        ),
        AotInstruction::And => emit_binary(
            frame,
            emitter,
            ip,
            "and",
            AotSsaValueRepr::Bool,
            |lhs, rhs| AotSsaInstKind::BoolAnd { lhs, rhs },
        ),
        AotInstruction::Or => emit_binary(
            frame,
            emitter,
            ip,
            "or",
            AotSsaValueRepr::Bool,
            |lhs, rhs| AotSsaInstKind::BoolOr { lhs, rhs },
        ),
        AotInstruction::Not => {
            emit_unary(frame, emitter, ip, "not", AotSsaValueRepr::Bool, |input| {
                AotSsaInstKind::BoolNot { input }
            })
        }
        AotInstruction::INeg => emit_typed_int_unary(frame, emitter, ip, "ineg", |input| {
            AotSsaInstKind::IntNeg { input }
        }),
        AotInstruction::FNeg => emit_typed_float_unary(frame, emitter, ip, "fneg", |input| {
            AotSsaInstKind::FloatNeg { input }
        }),
        AotInstruction::FCeq => emit_typed_float_compare(frame, emitter, ip, "fceq", |lhs, rhs| {
            AotSsaInstKind::FloatCmpEq { lhs, rhs }
        }),
        AotInstruction::FClt => emit_typed_float_compare(frame, emitter, ip, "fclt", |lhs, rhs| {
            AotSsaInstKind::FloatCmpLt { lhs, rhs }
        }),
        AotInstruction::FCgt => emit_typed_float_compare(frame, emitter, ip, "fcgt", |lhs, rhs| {
            AotSsaInstKind::FloatCmpGt { lhs, rhs }
        }),
        AotInstruction::Add => emit_numeric_binary(
            program,
            frame,
            emitter,
            ip,
            "add",
            |lhs, rhs| AotSsaInstKind::IntAdd { lhs, rhs },
            |lhs, rhs| AotSsaInstKind::FloatAdd { lhs, rhs },
        ),
        AotInstruction::Sub => emit_numeric_binary(
            program,
            frame,
            emitter,
            ip,
            "sub",
            |lhs, rhs| AotSsaInstKind::IntSub { lhs, rhs },
            |lhs, rhs| AotSsaInstKind::FloatSub { lhs, rhs },
        ),
        AotInstruction::Mul => emit_numeric_binary(
            program,
            frame,
            emitter,
            ip,
            "mul",
            |lhs, rhs| AotSsaInstKind::IntMul { lhs, rhs },
            |lhs, rhs| AotSsaInstKind::FloatMul { lhs, rhs },
        ),
        AotInstruction::Div => emit_numeric_binary(
            program,
            frame,
            emitter,
            ip,
            "div",
            |lhs, rhs| AotSsaInstKind::IntDiv { lhs, rhs },
            |lhs, rhs| AotSsaInstKind::FloatDiv { lhs, rhs },
        ),
        AotInstruction::Mod => emit_numeric_binary(
            program,
            frame,
            emitter,
            ip,
            "mod",
            |lhs, rhs| AotSsaInstKind::IntMod { lhs, rhs },
            |lhs, rhs| AotSsaInstKind::FloatMod { lhs, rhs },
        ),
        AotInstruction::IDiv => emit_typed_int_binary(frame, emitter, ip, "idiv", |lhs, rhs| {
            AotSsaInstKind::IntDiv { lhs, rhs }
        }),
        AotInstruction::IMod => emit_typed_int_binary(frame, emitter, ip, "imod", |lhs, rhs| {
            AotSsaInstKind::IntMod { lhs, rhs }
        }),
        AotInstruction::FMod => emit_typed_float_binary(frame, emitter, ip, "fmod", |lhs, rhs| {
            AotSsaInstKind::FloatMod { lhs, rhs }
        }),
        AotInstruction::Ceq => {
            let rhs = frame.pop(ip, "ceq")?;
            let lhs = frame.pop(ip, "ceq")?;
            let kind = match (lhs.value.repr, rhs.value.repr) {
                (AotSsaValueRepr::I64, AotSsaValueRepr::I64) => Some(AotSsaInstKind::IntCmpEq {
                    lhs: lhs.value.id,
                    rhs: rhs.value.id,
                }),
                (AotSsaValueRepr::Bool, AotSsaValueRepr::Bool) => Some(AotSsaInstKind::BoolCmpEq {
                    lhs: lhs.value.id,
                    rhs: rhs.value.id,
                }),
                (AotSsaValueRepr::F64, AotSsaValueRepr::F64) => Some(AotSsaInstKind::FloatCmpEq {
                    lhs: lhs.value.id,
                    rhs: rhs.value.id,
                }),
                (AotSsaValueRepr::Tagged, AotSsaValueRepr::Tagged) => {
                    let tagged_kind = match operand_types_at(program, ip) {
                        (ValueType::String, ValueType::String) => AotSsaInstKind::StringCmpEq {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                        (ValueType::Bytes, ValueType::Bytes) => AotSsaInstKind::BytesCmpEq {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                        (ValueType::Null, ValueType::Null) => AotSsaInstKind::NullCmpEq {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                        _ => AotSsaInstKind::TaggedCmpEq {
                            lhs: lhs.value.id,
                            rhs: rhs.value.id,
                        },
                    };
                    Some(tagged_kind)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                frame.stack.push(FrameValue {
                    value: emitter.emit(ip, kind, AotSsaValueRepr::Bool),
                });
                Ok(true)
            } else {
                frame.stack.push(lhs);
                frame.stack.push(rhs);
                Ok(false)
            }
        }
        AotInstruction::Clt => emit_generic_compare(program, frame, emitter, ip, "clt", true),
        AotInstruction::Cgt => emit_generic_compare(program, frame, emitter, ip, "cgt", false),
        AotInstruction::Neg => {
            emit_generic_unary(frame, emitter, ip, "neg", |repr, input| match repr {
                AotSsaValueRepr::I64 => {
                    Some((AotSsaValueRepr::I64, AotSsaInstKind::IntNeg { input }))
                }
                AotSsaValueRepr::F64 => {
                    Some((AotSsaValueRepr::F64, AotSsaInstKind::FloatNeg { input }))
                }
                _ => None,
            })
        }
        AotInstruction::Call(_) => Ok(false),
    }
}

fn emit_binary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    repr: AotSsaValueRepr,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if lhs.value.repr != repr || rhs.value.repr != repr {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(lhs.value.id, rhs.value.id), repr),
    });
    Ok(true)
}

fn emit_unary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    repr: AotSsaValueRepr,
    build: impl FnOnce(AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let input = frame.pop(ip, instruction)?;
    if input.value.repr != repr {
        frame.stack.push(input);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(input.value.id), repr),
    });
    Ok(true)
}

fn emit_typed_int_binary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if !matches!(
        lhs.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::Tagged
    ) || !matches!(
        rhs.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::Tagged
    ) {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    let lhs = coerce_numeric_to_int(emitter, ip, lhs.value);
    let rhs = coerce_numeric_to_int(emitter, ip, rhs.value);
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(lhs.id, rhs.id), AotSsaValueRepr::I64),
    });
    Ok(true)
}

fn emit_typed_float_binary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if !matches!(
        lhs.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 | AotSsaValueRepr::Tagged
    ) || !matches!(
        rhs.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 | AotSsaValueRepr::Tagged
    ) {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    let lhs = coerce_numeric_to_float(emitter, ip, lhs.value);
    let rhs = coerce_numeric_to_float(emitter, ip, rhs.value);
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(lhs.id, rhs.id), AotSsaValueRepr::F64),
    });
    Ok(true)
}

fn emit_typed_int_unary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let input = frame.pop(ip, instruction)?;
    if !matches!(
        input.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::Tagged
    ) {
        frame.stack.push(input);
        return Ok(false);
    }
    let input = coerce_numeric_to_int(emitter, ip, input.value);
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(input.id), AotSsaValueRepr::I64),
    });
    Ok(true)
}

fn emit_typed_float_unary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let input = frame.pop(ip, instruction)?;
    if !matches!(
        input.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 | AotSsaValueRepr::Tagged
    ) {
        frame.stack.push(input);
        return Ok(false);
    }
    let input = coerce_numeric_to_float(emitter, ip, input.value);
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(input.id), AotSsaValueRepr::F64),
    });
    Ok(true)
}

fn emit_typed_float_compare<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if !matches!(
        lhs.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 | AotSsaValueRepr::Tagged
    ) || !matches!(
        rhs.value.repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 | AotSsaValueRepr::Tagged
    ) {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    let lhs = coerce_numeric_to_float(emitter, ip, lhs.value);
    let rhs = coerce_numeric_to_float(emitter, ip, rhs.value);
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(lhs.id, rhs.id), AotSsaValueRepr::Bool),
    });
    Ok(true)
}

fn emit_generic_unary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueRepr, AotSsaValueId) -> Option<(AotSsaValueRepr, AotSsaInstKind)>,
) -> Result<bool, AotSsaBuildError> {
    let input = frame.pop(ip, instruction)?;
    let Some((repr, kind)) = build(input.value.repr, input.value.id) else {
        frame.stack.push(input);
        return Ok(false);
    };
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, kind, repr),
    });
    Ok(true)
}

fn emit_tagged_unary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    output_repr: AotSsaValueRepr,
    build: impl FnOnce(AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let input = frame.pop(ip, instruction)?;
    if input.value.repr != AotSsaValueRepr::Tagged {
        frame.stack.push(input);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(input.value.id), output_repr),
    });
    Ok(true)
}

fn emit_tagged_binary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if lhs.value.repr != AotSsaValueRepr::Tagged || rhs.value.repr != AotSsaValueRepr::Tagged {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(
            ip,
            build(lhs.value.id, rhs.value.id),
            AotSsaValueRepr::Tagged,
        ),
    });
    Ok(true)
}

fn emit_tagged_binary_with_int_rhs<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    output_repr: AotSsaValueRepr,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if lhs.value.repr != AotSsaValueRepr::Tagged || rhs.value.repr != AotSsaValueRepr::I64 {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(lhs.value.id, rhs.value.id), output_repr),
    });
    Ok(true)
}

fn emit_tagged_ternary_with_ints<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let length = frame.pop(ip, instruction)?;
    let start = frame.pop(ip, instruction)?;
    let source = frame.pop(ip, instruction)?;
    if source.value.repr != AotSsaValueRepr::Tagged
        || start.value.repr != AotSsaValueRepr::I64
        || length.value.repr != AotSsaValueRepr::I64
    {
        frame.stack.push(source);
        frame.stack.push(start);
        frame.stack.push(length);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(
            ip,
            build(source.value.id, start.value.id, length.value.id),
            AotSsaValueRepr::Tagged,
        ),
    });
    Ok(true)
}

fn emit_compare<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    repr: AotSsaValueRepr,
    build: impl FnOnce(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if lhs.value.repr != repr || rhs.value.repr != repr {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, build(lhs.value.id, rhs.value.id), AotSsaValueRepr::Bool),
    });
    Ok(true)
}

fn emit_generic_binary<E: InstEmitter>(
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    build: impl FnOnce(
        AotSsaValueRepr,
        AotSsaValueId,
        AotSsaValueId,
    ) -> Option<(AotSsaValueRepr, AotSsaInstKind)>,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    if lhs.value.repr != rhs.value.repr {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    }
    let Some((repr, kind)) = build(lhs.value.repr, lhs.value.id, rhs.value.id) else {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        return Ok(false);
    };
    frame.stack.push(FrameValue {
        value: emitter.emit(ip, kind, repr),
    });
    Ok(true)
}

fn emit_numeric_binary<E: InstEmitter, I, F>(
    program: &Program,
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    int_build: I,
    float_build: F,
) -> Result<bool, AotSsaBuildError>
where
    I: Fn(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind + Copy,
    F: Fn(AotSsaValueId, AotSsaValueId) -> AotSsaInstKind + Copy,
{
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    match select_numeric_mode(program, ip, lhs.value.repr, rhs.value.repr) {
        Some(AotNumericMode::Int) => {
            let lhs = coerce_numeric_to_int(emitter, ip, lhs.value);
            let rhs = coerce_numeric_to_int(emitter, ip, rhs.value);
            frame.stack.push(FrameValue {
                value: emitter.emit(ip, int_build(lhs.id, rhs.id), AotSsaValueRepr::I64),
            });
            Ok(true)
        }
        Some(AotNumericMode::Float) => {
            let lhs = coerce_numeric_to_float(emitter, ip, lhs.value);
            let rhs = coerce_numeric_to_float(emitter, ip, rhs.value);
            frame.stack.push(FrameValue {
                value: emitter.emit(ip, float_build(lhs.id, rhs.id), AotSsaValueRepr::F64),
            });
            Ok(true)
        }
        _ => {
            frame.stack.push(lhs);
            frame.stack.push(rhs);
            Ok(false)
        }
    }
}

fn emit_generic_compare<E: InstEmitter>(
    program: &Program,
    frame: &mut Frame,
    emitter: &mut E,
    ip: usize,
    instruction: &'static str,
    is_lt: bool,
) -> Result<bool, AotSsaBuildError> {
    let rhs = frame.pop(ip, instruction)?;
    let lhs = frame.pop(ip, instruction)?;
    let kind = match select_numeric_mode(program, ip, lhs.value.repr, rhs.value.repr) {
        Some(AotNumericMode::Int) => {
            let lhs = coerce_numeric_to_int(emitter, ip, lhs.value);
            let rhs = coerce_numeric_to_int(emitter, ip, rhs.value);
            Some(if is_lt {
                AotSsaInstKind::IntCmpLt {
                    lhs: lhs.id,
                    rhs: rhs.id,
                }
            } else {
                AotSsaInstKind::IntCmpGt {
                    lhs: lhs.id,
                    rhs: rhs.id,
                }
            })
        }
        Some(AotNumericMode::Float) => {
            let lhs = coerce_numeric_to_float(emitter, ip, lhs.value);
            let rhs = coerce_numeric_to_float(emitter, ip, rhs.value);
            Some(if is_lt {
                AotSsaInstKind::FloatCmpLt {
                    lhs: lhs.id,
                    rhs: rhs.id,
                }
            } else {
                AotSsaInstKind::FloatCmpGt {
                    lhs: lhs.id,
                    rhs: rhs.id,
                }
            })
        }
        _ => None,
    };
    if let Some(kind) = kind {
        frame.stack.push(FrameValue {
            value: emitter.emit(ip, kind, AotSsaValueRepr::Bool),
        });
        Ok(true)
    } else {
        frame.stack.push(lhs);
        frame.stack.push(rhs);
        Ok(false)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AotNumericMode {
    Int,
    Float,
}

fn select_numeric_mode(
    program: &Program,
    ip: usize,
    lhs: AotSsaValueRepr,
    rhs: AotSsaValueRepr,
) -> Option<AotNumericMode> {
    match (lhs, rhs) {
        (AotSsaValueRepr::I64, AotSsaValueRepr::I64) => Some(AotNumericMode::Int),
        (AotSsaValueRepr::F64, AotSsaValueRepr::F64)
        | (AotSsaValueRepr::I64, AotSsaValueRepr::F64)
        | (AotSsaValueRepr::F64, AotSsaValueRepr::I64)
        | (AotSsaValueRepr::Tagged, AotSsaValueRepr::F64)
        | (AotSsaValueRepr::F64, AotSsaValueRepr::Tagged) => Some(AotNumericMode::Float),
        (AotSsaValueRepr::Tagged, AotSsaValueRepr::I64)
        | (AotSsaValueRepr::I64, AotSsaValueRepr::Tagged)
        | (AotSsaValueRepr::Tagged, AotSsaValueRepr::Tagged) => {
            match operand_types_at(program, ip) {
                (ValueType::Int, ValueType::Int) => Some(AotNumericMode::Int),
                (ValueType::Float, ValueType::Float)
                | (ValueType::Float, ValueType::Unknown)
                | (ValueType::Unknown, ValueType::Float) => Some(AotNumericMode::Float),
                _ => None,
            }
        }
        _ => None,
    }
}

fn is_numeric_repr(repr: AotSsaValueRepr) -> bool {
    matches!(
        repr,
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 | AotSsaValueRepr::Tagged
    )
}

fn coerce_numeric_to_int<E: InstEmitter>(
    emitter: &mut E,
    ip: usize,
    value: AotSsaValue,
) -> AotSsaValue {
    match value.repr {
        AotSsaValueRepr::I64 => value,
        AotSsaValueRepr::Tagged => emitter.emit(
            ip,
            AotSsaInstKind::TaggedToInt { input: value.id },
            AotSsaValueRepr::I64,
        ),
        _ => unreachable!("int coercion only accepts i64/tagged values"),
    }
}

fn coerce_numeric_to_float<E: InstEmitter>(
    emitter: &mut E,
    ip: usize,
    value: AotSsaValue,
) -> AotSsaValue {
    match value.repr {
        AotSsaValueRepr::I64 => emitter.emit(
            ip,
            AotSsaInstKind::IntToFloat { input: value.id },
            AotSsaValueRepr::F64,
        ),
        AotSsaValueRepr::F64 => value,
        AotSsaValueRepr::Tagged => emitter.emit(
            ip,
            AotSsaInstKind::TaggedNumberToFloat { input: value.id },
            AotSsaValueRepr::F64,
        ),
        _ => unreachable!("numeric coercion only accepts i64/f64/tagged values"),
    }
}

fn apply_call_effect(
    program: &Program,
    direct_host_result_counts: &HashMap<usize, usize>,
    frame: &mut Frame,
    call: &AotCall,
    ip: usize,
) -> Result<(), AotSsaBuildError> {
    let mut arg_reprs = Vec::with_capacity(call.argc as usize);
    for _ in 0..call.argc {
        let arg = frame.pop(ip, "call")?;
        arg_reprs.push(arg.value.repr);
    }
    arg_reprs.reverse();
    for repr in call_result_reprs(program, direct_host_result_counts, call, &arg_reprs) {
        push_effect_value(frame, repr);
    }
    Ok(())
}

fn call_result_reprs(
    program: &Program,
    direct_host_result_counts: &HashMap<usize, usize>,
    call: &AotCall,
    arg_reprs: &[AotSsaValueRepr],
) -> Vec<AotSsaValueRepr> {
    let result_count = match call.dispatch {
        AotCallDispatch::Builtin => builtin_runtime_result_count(call.index).unwrap_or(1),
        AotCallDispatch::HostImport if program.imports.is_empty() => direct_host_result_counts
            .get(&call.call_ip)
            .copied()
            .unwrap_or(1),
        AotCallDispatch::HostImport => 1,
    };
    if result_count == 0 {
        return Vec::new();
    }
    let repr = match call.dispatch {
        AotCallDispatch::Builtin => infer_builtin_result_repr(call.index, arg_reprs)
            .or_else(|| {
                BuiltinFunction::from_call_index(call.index)
                    .map(BuiltinFunction::static_return_type)
                    .map(value_type_repr)
            })
            .unwrap_or(AotSsaValueRepr::Tagged),
        AotCallDispatch::HostImport => program
            .imports
            .get(call.index as usize)
            .map(|import| value_type_repr(import.return_type))
            .unwrap_or(AotSsaValueRepr::Tagged),
    };
    vec![repr; result_count]
}

fn builtin_runtime_result_count(call_index: u16) -> Option<usize> {
    let builtin = BuiltinFunction::from_call_index(call_index)?;
    Some(match builtin {
        // `assert` is typed like a `null`-producing expression in the frontend, but the runtime
        // consumes its arguments and pushes no value on success.
        BuiltinFunction::Assert => 0,
        _ => 1,
    })
}

fn push_effect_value(frame: &mut Frame, repr: AotSsaValueRepr) {
    frame.stack.push(FrameValue {
        value: AotSsaValue::new(AotSsaValueId::new(0), repr),
    });
}

fn infer_builtin_result_repr(
    call_index: u16,
    arg_reprs: &[AotSsaValueRepr],
) -> Option<AotSsaValueRepr> {
    let builtin = BuiltinFunction::from_call_index(call_index)?;
    match builtin {
        BuiltinFunction::MathAbs
        | BuiltinFunction::MathFloor
        | BuiltinFunction::MathCeil
        | BuiltinFunction::MathRound
        | BuiltinFunction::MathTrunc
        | BuiltinFunction::MathSignum => same_numeric_result_repr(arg_reprs),
        BuiltinFunction::MathMin | BuiltinFunction::MathMax | BuiltinFunction::MathClamp => {
            promoted_numeric_result_repr(arg_reprs)
        }
        _ => None,
    }
}

fn same_numeric_result_repr(arg_reprs: &[AotSsaValueRepr]) -> Option<AotSsaValueRepr> {
    let [repr] = arg_reprs else {
        return None;
    };
    match repr {
        AotSsaValueRepr::I64 | AotSsaValueRepr::F64 => Some(*repr),
        _ => None,
    }
}

fn promoted_numeric_result_repr(arg_reprs: &[AotSsaValueRepr]) -> Option<AotSsaValueRepr> {
    if arg_reprs.is_empty() {
        return None;
    }
    let mut saw_float = false;
    for repr in arg_reprs {
        match repr {
            AotSsaValueRepr::I64 => {}
            AotSsaValueRepr::F64 => saw_float = true,
            _ => return None,
        }
    }
    Some(if saw_float {
        AotSsaValueRepr::F64
    } else {
        AotSsaValueRepr::I64
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BytecodeBuilder, HostImport, Value};

    #[test]
    fn aot_ssa_exposes_call_and_resume_checkpoints() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        let call_ip = bc.position();
        bc.call(0, 1);
        let resume_ip = bc.position();
        bc.pop();
        bc.ret();

        let program = Program::with_imports_and_debug(
            vec![Value::Int(7)],
            bc.finish(),
            vec![HostImport {
                name: "host".to_string(),
                arity: 1,
                return_type: ValueType::Int,
            }],
            None,
        );

        let ssa = build_aot_ssa(&program).expect("ssa should build");
        assert!(ssa.resume_ips.contains(&(call_ip as usize)));
        assert!(ssa.resume_ips.contains(&(resume_ip as usize)));
    }

    #[test]
    fn aot_ssa_limits_resume_ips_to_entry_without_host_calls() {
        let mut bc = BytecodeBuilder::new();
        bc.ldc(0);
        bc.stloc(0);
        let loop_ip = bc.position();
        bc.ldloc(0);
        bc.ldc(2);
        bc.clt();
        let exit_branch_ip = bc.position();
        bc.brfalse(0);
        bc.ldloc(0);
        bc.ldc(1);
        bc.add();
        bc.stloc(0);
        bc.br(loop_ip);
        let exit_ip = bc.position();
        bc.ldloc(0);
        bc.ret();

        let mut code = bc.finish();
        let exit_branch_ip = exit_branch_ip as usize;
        code[exit_branch_ip + 1..exit_branch_ip + 5].copy_from_slice(&exit_ip.to_le_bytes());

        let program = Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(3)], code)
            .with_local_count(1);

        let ssa = build_aot_ssa(&program).expect("ssa should build");
        assert_eq!(ssa.resume_ips, vec![0]);
    }
}
