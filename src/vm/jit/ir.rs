use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write};

use crate::{Value, ValueType, VmError, VmResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SsaValueId(u32);

impl SsaValueId {
    pub(crate) fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for SsaValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SsaBlockId(u32);

impl SsaBlockId {
    pub(crate) fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for SsaBlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SsaExitId(u32);

impl SsaExitId {
    pub(crate) fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for SsaExitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SsaValueRepr {
    Tagged,
    I64,
    F64,
    Bool,
    HeapPtr(ValueType),
}

impl fmt::Display for SsaValueRepr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tagged => write!(f, "tagged"),
            Self::I64 => write!(f, "i64"),
            Self::F64 => write!(f, "f64"),
            Self::Bool => write!(f, "bool"),
            Self::HeapPtr(tag) => write!(f, "ptr<{tag:?}>"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SsaValue {
    pub(crate) id: SsaValueId,
    pub(crate) repr: SsaValueRepr,
}

impl SsaValue {
    pub(crate) fn new(id: SsaValueId, repr: SsaValueRepr) -> Self {
        Self { id, repr }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SsaBlockParam {
    pub(crate) value: SsaValue,
    pub(crate) label: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SsaInstKind {
    Constant(Value),
    UnboxInt {
        input: SsaValueId,
    },
    UnboxFloat {
        input: SsaValueId,
    },
    UnboxBool {
        input: SsaValueId,
    },
    UnboxHeapPtr {
        input: SsaValueId,
        tag: ValueType,
    },
    ValueLen {
        value: SsaValueId,
    },
    StringLen {
        text: SsaValueId,
    },
    BytesLen {
        bytes: SsaValueId,
    },
    StringSlice {
        text: SsaValueId,
        start: SsaValueId,
        length: SsaValueId,
    },
    BytesSlice {
        bytes: SsaValueId,
        start: SsaValueId,
        length: SsaValueId,
    },
    StringGet {
        text: SsaValueId,
        index: SsaValueId,
    },
    BytesGet {
        bytes: SsaValueId,
        index: SsaValueId,
    },
    BytesHas {
        bytes: SsaValueId,
        index: SsaValueId,
    },
    StringContains {
        text: SsaValueId,
        needle: SsaValueId,
    },
    RegexMatch {
        pattern: SsaValueId,
        text: SsaValueId,
    },
    RegexReplace {
        pattern: SsaValueId,
        text: SsaValueId,
        replacement: SsaValueId,
    },
    StringReplaceLiteral {
        text: SsaValueId,
        needle: SsaValueId,
        replacement: SsaValueId,
    },
    StringLowerAscii {
        text: SsaValueId,
    },
    TypeOf {
        value: SsaValueId,
    },
    ToString {
        value: SsaValueId,
    },
    StringSplitLiteral {
        text: SsaValueId,
        delimiter: SsaValueId,
    },
    StringConcat {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    BytesConcat {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    BytesFromArrayU8 {
        array: SsaValueId,
    },
    BytesToUtf8Ascii {
        bytes: SsaValueId,
    },
    BytesToArrayU8 {
        bytes: SsaValueId,
    },
    ArrayNew,
    ArrayLen {
        array: SsaValueId,
    },
    ArrayGet {
        array: SsaValueId,
        index: SsaValueId,
    },
    ArrayHas {
        array: SsaValueId,
        index: SsaValueId,
    },
    ArraySet {
        array: SsaValueId,
        index: SsaValueId,
        value: SsaValueId,
    },
    ArrayPush {
        array: SsaValueId,
        value: SsaValueId,
    },
    MapLen {
        map: SsaValueId,
    },
    MapGet {
        map: SsaValueId,
        key: SsaValueId,
    },
    MapHas {
        map: SsaValueId,
        key: SsaValueId,
    },
    MapSet {
        map: SsaValueId,
        key: SsaValueId,
        value: SsaValueId,
    },
    MapIterNext {
        slot: SsaValueId,
    },
    MapIterTakeKey {
        slot: SsaValueId,
    },
    MapIterTakeValue {
        slot: SsaValueId,
    },
    HostCall {
        import: u16,
        args: Vec<SsaValueId>,
    },

    IntNeg {
        input: SsaValueId,
    },
    IntAdd {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntAddImm {
        lhs: SsaValueId,
        imm: i64,
    },
    IntSub {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntSubImm {
        lhs: SsaValueId,
        imm: i64,
    },
    IntMul {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntMulImm {
        lhs: SsaValueId,
        imm: i64,
    },
    IntDiv {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntDivImm {
        lhs: SsaValueId,
        imm: i64,
    },
    IntMod {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntModImm {
        lhs: SsaValueId,
        imm: i64,
    },
    IntShl {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntShlImm {
        lhs: SsaValueId,
        amount: u32,
    },
    IntShr {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntShrImm {
        lhs: SsaValueId,
        amount: u32,
    },
    IntLshr {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntLshrImm {
        lhs: SsaValueId,
        amount: u32,
    },
    BoolAnd {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    BoolOr {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    BoolNot {
        input: SsaValueId,
    },
    FloatNeg {
        input: SsaValueId,
    },
    FloatAdd {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatSub {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatMul {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatDiv {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatMod {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatCmpEq {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatCmpLt {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    FloatCmpGt {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntCmpEq {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    ValueCmpEq {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntCmpLt {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntCmpLtImm {
        lhs: SsaValueId,
        imm: i64,
    },
    IntCmpGt {
        lhs: SsaValueId,
        rhs: SsaValueId,
    },
    IntCmpGtImm {
        lhs: SsaValueId,
        imm: i64,
    },
}

impl SsaInstKind {
    pub(crate) fn inputs(&self) -> Vec<SsaValueId> {
        match self {
            Self::Constant(_) => Vec::new(),
            Self::HostCall { args, .. } => args.clone(),

            Self::UnboxInt { input }
            | Self::UnboxFloat { input }
            | Self::UnboxBool { input }
            | Self::UnboxHeapPtr { input, .. }
            | Self::ValueLen { value: input }
            | Self::StringLen { text: input }
            | Self::BytesLen { bytes: input }
            | Self::ArrayLen { array: input }
            | Self::MapLen { map: input }
            | Self::IntNeg { input }
            | Self::BoolNot { input }
            | Self::FloatNeg { input } => vec![*input],
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
            Self::StringContains { text, needle } => vec![*text, *needle],
            Self::RegexMatch { pattern, text } => vec![*pattern, *text],
            Self::RegexReplace {
                pattern,
                text,
                replacement,
            } => vec![*pattern, *text, *replacement],
            Self::StringReplaceLiteral {
                text,
                needle,
                replacement,
            } => vec![*text, *needle, *replacement],
            Self::StringLowerAscii { text } => vec![*text],
            Self::TypeOf { value } | Self::ToString { value } => vec![*value],
            Self::StringSplitLiteral { text, delimiter } => vec![*text, *delimiter],
            Self::StringConcat { lhs, rhs } | Self::BytesConcat { lhs, rhs } => vec![*lhs, *rhs],
            Self::BytesFromArrayU8 { array } => vec![*array],
            Self::BytesToUtf8Ascii { bytes } | Self::BytesToArrayU8 { bytes } => vec![*bytes],
            Self::ArrayNew => Vec::new(),
            Self::ArrayGet { array, index } => vec![*array, *index],
            Self::ArrayHas { array, index } => vec![*array, *index],
            Self::ArraySet {
                array,
                index,
                value,
            } => vec![*array, *index, *value],
            Self::ArrayPush { array, value } => vec![*array, *value],
            Self::MapGet { map, key } => vec![*map, *key],
            Self::MapHas { map, key } => vec![*map, *key],
            Self::MapSet { map, key, value } => vec![*map, *key, *value],
            Self::MapIterNext { slot }
            | Self::MapIterTakeKey { slot }
            | Self::MapIterTakeValue { slot } => vec![*slot],
            Self::IntAdd { lhs, rhs }
            | Self::IntSub { lhs, rhs }
            | Self::IntMul { lhs, rhs }
            | Self::IntDiv { lhs, rhs }
            | Self::IntMod { lhs, rhs }
            | Self::IntShl { lhs, rhs }
            | Self::IntShr { lhs, rhs }
            | Self::IntLshr { lhs, rhs }
            | Self::BoolAnd { lhs, rhs }
            | Self::BoolOr { lhs, rhs }
            | Self::FloatAdd { lhs, rhs }
            | Self::FloatSub { lhs, rhs }
            | Self::FloatMul { lhs, rhs }
            | Self::FloatDiv { lhs, rhs }
            | Self::FloatMod { lhs, rhs }
            | Self::FloatCmpEq { lhs, rhs }
            | Self::FloatCmpLt { lhs, rhs }
            | Self::FloatCmpGt { lhs, rhs }
            | Self::IntCmpEq { lhs, rhs }
            | Self::ValueCmpEq { lhs, rhs }
            | Self::IntCmpLt { lhs, rhs }
            | Self::IntCmpGt { lhs, rhs } => vec![*lhs, *rhs],
            Self::IntAddImm { lhs, .. }
            | Self::IntSubImm { lhs, .. }
            | Self::IntMulImm { lhs, .. }
            | Self::IntDivImm { lhs, .. }
            | Self::IntModImm { lhs, .. }
            | Self::IntShlImm { lhs, .. }
            | Self::IntShrImm { lhs, .. }
            | Self::IntLshrImm { lhs, .. }
            | Self::IntCmpLtImm { lhs, .. }
            | Self::IntCmpGtImm { lhs, .. } => vec![*lhs],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SsaInst {
    pub(crate) ip: usize,
    pub(crate) output: Option<SsaValue>,
    pub(crate) kind: SsaInstKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SsaMaterialization {
    Value(SsaValueId),
    BoxInt(SsaValueId),
    BoxFloat(SsaValueId),
    BoxBool(SsaValueId),
    BoxHeapPtr { value: SsaValueId, tag: ValueType },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SsaExit {
    pub(crate) id: SsaExitId,
    pub(crate) exit_ip: usize,
    pub(crate) stack: Vec<SsaMaterialization>,
    pub(crate) locals: Vec<SsaMaterialization>,
    pub(crate) dirty_locals: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SsaBranchTarget {
    Block {
        target: SsaBlockId,
        args: Vec<SsaValueId>,
    },
    Exit(SsaExitId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SsaTerminator {
    Jump {
        target: SsaBlockId,
        args: Vec<SsaValueId>,
    },
    BranchBool {
        condition: SsaValueId,
        if_true: SsaBranchTarget,
        if_false: SsaBranchTarget,
    },
    Exit {
        exit: SsaExitId,
    },
    Return {
        exit: SsaExitId,
    },
    CallValue {
        argc: u8,
        call_ip: usize,
        resume_ip: usize,
        exit: SsaExitId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SsaBlock {
    pub(crate) id: SsaBlockId,
    pub(crate) params: Vec<SsaBlockParam>,
    pub(crate) insts: Vec<SsaInst>,
    pub(crate) terminator: Option<SsaTerminator>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SsaTrace {
    pub(crate) root_ip: usize,
    pub(crate) entry_stack_depth: usize,
    pub(crate) entry: SsaBlockId,
    pub(crate) blocks: Vec<SsaBlock>,
    pub(crate) exits: Vec<SsaExit>,
}

impl SsaTrace {
    pub(crate) fn verify(&self) -> Result<(), SsaVerifyError> {
        let mut block_ids = HashSet::new();
        for block in &self.blocks {
            if !block_ids.insert(block.id) {
                return Err(SsaVerifyError::DuplicateBlock(block.id));
            }
            if block.terminator.is_none() {
                return Err(SsaVerifyError::MissingTerminator(block.id));
            }
        }
        let Some(entry) = self.blocks.get(self.entry.index()) else {
            return Err(SsaVerifyError::UnknownEntry(self.entry));
        };
        if entry.id != self.entry {
            return Err(SsaVerifyError::UnknownEntry(self.entry));
        }
        if self.entry_stack_depth > entry.params.len() {
            return Err(SsaVerifyError::EntryStackDepthMismatch {
                depth: self.entry_stack_depth,
                params: entry.params.len(),
            });
        }

        let block_param_reprs = self
            .blocks
            .iter()
            .map(|block| {
                (
                    block.id,
                    block
                        .params
                        .iter()
                        .map(|param| param.value.repr)
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        let exit_ids = self
            .exits
            .iter()
            .map(|exit| exit.id)
            .collect::<HashSet<_>>();
        let mut value_reprs = HashMap::new();

        for block in &self.blocks {
            let mut scope = HashMap::new();
            for param in &block.params {
                if value_reprs
                    .insert(param.value.id, param.value.repr)
                    .is_some()
                {
                    return Err(SsaVerifyError::DuplicateValue(param.value.id));
                }
                scope.insert(param.value.id, param.value.repr);
            }
            for inst in &block.insts {
                for input in inst.kind.inputs() {
                    if !scope.contains_key(&input) {
                        return Err(SsaVerifyError::UseBeforeDef {
                            block: block.id,
                            value: input,
                        });
                    }
                }
                if let Some(output) = inst.output {
                    if value_reprs.insert(output.id, output.repr).is_some() {
                        return Err(SsaVerifyError::DuplicateValue(output.id));
                    }
                    scope.insert(output.id, output.repr);
                }
            }
            verify_terminator(
                block.id,
                block.terminator.as_ref().expect("terminator checked above"),
                &scope,
                &block_param_reprs,
                &exit_ids,
            )?;
        }

        for exit in &self.exits {
            if exit.dirty_locals.len() != exit.locals.len() {
                return Err(SsaVerifyError::DirtyLocalLengthMismatch {
                    exit: exit.id,
                    locals: exit.locals.len(),
                    dirty_locals: exit.dirty_locals.len(),
                });
            }
            for materialization in exit.stack.iter().chain(exit.locals.iter()) {
                verify_materialization(materialization, &value_reprs)?;
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            &mut out,
            "trace root_ip={} entry_stack_depth={}",
            self.root_ip, self.entry_stack_depth
        );
        for block in &self.blocks {
            let _ = write!(&mut out, "{}(", block.id);
            for (index, param) in block.params.iter().enumerate() {
                if index != 0 {
                    let _ = write!(&mut out, ", ");
                }
                let _ = write!(
                    &mut out,
                    "{}:{} {}",
                    param.value.id, param.value.repr, param.label
                );
            }
            let _ = writeln!(&mut out, "):");
            for inst in &block.insts {
                let _ = write!(&mut out, "  @{} ", inst.ip);
                if let Some(output) = inst.output {
                    let _ = write!(&mut out, "{}:{} = ", output.id, output.repr);
                }
                let _ = writeln!(&mut out, "{}", render_inst_kind(&inst.kind));
            }
            if let Some(terminator) = &block.terminator {
                let _ = writeln!(&mut out, "  {}", render_terminator(terminator));
            }
        }
        for exit in &self.exits {
            let _ = writeln!(&mut out, "{} ip={}", exit.id, exit.exit_ip);
            let stack = exit
                .stack
                .iter()
                .map(render_materialization)
                .collect::<Vec<_>>()
                .join(", ");
            let locals = exit
                .locals
                .iter()
                .map(render_materialization)
                .collect::<Vec<_>>()
                .join(", ");
            let dirty_locals = exit
                .dirty_locals
                .iter()
                .enumerate()
                .filter_map(|(index, dirty)| dirty.then_some(index.to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(&mut out, "  stack=[{}]", stack);
            let _ = writeln!(&mut out, "  locals=[{}]", locals);
            let _ = writeln!(&mut out, "  dirty_locals=[{}]", dirty_locals);
        }
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SsaVerifyError {
    DuplicateBlock(SsaBlockId),
    DuplicateValue(SsaValueId),
    MissingTerminator(SsaBlockId),
    UnknownEntry(SsaBlockId),
    EntryStackDepthMismatch {
        depth: usize,
        params: usize,
    },
    UseBeforeDef {
        block: SsaBlockId,
        value: SsaValueId,
    },
    NonBoolBranchCondition(SsaValueId),
    UnknownExit(SsaExitId),
    UnknownBlockTarget(SsaBlockId),
    JumpArityMismatch {
        target: SsaBlockId,
        expected: usize,
        got: usize,
    },
    JumpTypeMismatch {
        target: SsaBlockId,
        index: usize,
        expected: SsaValueRepr,
        got: SsaValueRepr,
    },
    InvalidMaterialization {
        value: SsaValueId,
        expected: SsaValueRepr,
        actual: SsaValueRepr,
    },
    DirtyLocalLengthMismatch {
        exit: SsaExitId,
        locals: usize,
        dirty_locals: usize,
    },
}

pub(crate) struct SsaTraceBuilder {
    trace: SsaTrace,
    next_value: u32,
}

impl SsaTraceBuilder {
    pub(crate) fn new(root_ip: usize, entry_stack_depth: usize) -> Self {
        let entry = SsaBlockId::new(0);
        Self {
            trace: SsaTrace {
                root_ip,
                entry_stack_depth,
                entry,
                blocks: vec![SsaBlock {
                    id: entry,
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: None,
                }],
                exits: Vec::new(),
            },
            next_value: 0,
        }
    }

    pub(crate) fn entry(&self) -> SsaBlockId {
        self.trace.entry
    }

    pub(crate) fn defining_inst(&self, value: SsaValueId) -> Option<&SsaInst> {
        self.trace
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .find(|inst| inst.output.is_some_and(|output| output.id == value))
    }

    pub(crate) fn create_block(&mut self) -> SsaBlockId {
        let id = SsaBlockId::new(self.trace.blocks.len() as u32);
        self.trace.blocks.push(SsaBlock {
            id,
            params: Vec::new(),
            insts: Vec::new(),
            terminator: None,
        });
        id
    }

    pub(crate) fn append_param(
        &mut self,
        block: SsaBlockId,
        repr: SsaValueRepr,
        label: impl Into<String>,
    ) -> VmResult<SsaValue> {
        let value = self.alloc_value(repr);
        self.block_mut(block)?.params.push(SsaBlockParam {
            value,
            label: label.into(),
        });
        Ok(value)
    }

    pub(crate) fn append_value_inst(
        &mut self,
        block: SsaBlockId,
        ip: usize,
        repr: SsaValueRepr,
        kind: SsaInstKind,
    ) -> VmResult<SsaValue> {
        let output = self.alloc_value(repr);
        self.block_mut(block)?.insts.push(SsaInst {
            ip,
            output: Some(output),
            kind,
        });
        Ok(output)
    }

    pub(crate) fn add_exit(
        &mut self,
        exit_ip: usize,
        stack: Vec<SsaMaterialization>,
        locals: Vec<SsaMaterialization>,
        dirty_locals: Vec<bool>,
    ) -> SsaExitId {
        let id = SsaExitId::new(self.trace.exits.len() as u32);
        self.trace.exits.push(SsaExit {
            id,
            exit_ip,
            stack,
            locals,
            dirty_locals,
        });
        id
    }

    pub(crate) fn merge_exit_dirty_locals(&mut self, loop_dirty_locals: &[bool]) -> VmResult<()> {
        for exit in &mut self.trace.exits {
            if exit.dirty_locals.len() != loop_dirty_locals.len() {
                return Err(VmError::JitNative(format!(
                    "SSA loop dirty-local count {} does not match exit {:?} count {}",
                    loop_dirty_locals.len(),
                    exit.id,
                    exit.dirty_locals.len()
                )));
            }
            for (dirty, loop_dirty) in exit
                .dirty_locals
                .iter_mut()
                .zip(loop_dirty_locals.iter().copied())
            {
                *dirty |= loop_dirty;
            }
        }
        Ok(())
    }

    pub(crate) fn set_terminator(
        &mut self,
        block: SsaBlockId,
        terminator: SsaTerminator,
    ) -> VmResult<()> {
        let block = self.block_mut(block)?;
        block.terminator = Some(terminator);
        Ok(())
    }

    pub(crate) fn finish(self) -> SsaTrace {
        self.trace
    }

    fn alloc_value(&mut self, repr: SsaValueRepr) -> SsaValue {
        let id = SsaValueId::new(self.next_value);
        self.next_value += 1;
        SsaValue::new(id, repr)
    }

    fn block_mut(&mut self, block: SsaBlockId) -> VmResult<&mut SsaBlock> {
        let Some(found) = self.trace.blocks.get_mut(block.index()) else {
            return Err(VmError::JitNative(format!(
                "invalid SSA block {}",
                block.index()
            )));
        };
        Ok(found)
    }
}

fn verify_terminator(
    block: SsaBlockId,
    terminator: &SsaTerminator,
    scope: &HashMap<SsaValueId, SsaValueRepr>,
    block_param_reprs: &HashMap<SsaBlockId, Vec<SsaValueRepr>>,
    exit_ids: &HashSet<SsaExitId>,
) -> Result<(), SsaVerifyError> {
    match terminator {
        SsaTerminator::Jump { target, args } => {
            verify_jump_target(*target, args, scope, block_param_reprs)
        }
        SsaTerminator::BranchBool {
            condition,
            if_true,
            if_false,
        } => {
            if scope.get(condition) != Some(&SsaValueRepr::Bool) {
                return Err(SsaVerifyError::NonBoolBranchCondition(*condition));
            }
            verify_branch_target(*condition, if_true, scope, block_param_reprs, exit_ids)?;
            verify_branch_target(*condition, if_false, scope, block_param_reprs, exit_ids)
        }
        SsaTerminator::Exit { exit }
        | SsaTerminator::Return { exit }
        | SsaTerminator::CallValue { exit, .. } => {
            if !exit_ids.contains(exit) {
                return Err(SsaVerifyError::UnknownExit(*exit));
            }
            let _ = block;
            Ok(())
        }
    }
}

fn verify_branch_target(
    _condition: SsaValueId,
    target: &SsaBranchTarget,
    scope: &HashMap<SsaValueId, SsaValueRepr>,
    block_param_reprs: &HashMap<SsaBlockId, Vec<SsaValueRepr>>,
    exit_ids: &HashSet<SsaExitId>,
) -> Result<(), SsaVerifyError> {
    match target {
        SsaBranchTarget::Block { target, args } => {
            verify_jump_target(*target, args, scope, block_param_reprs)
        }
        SsaBranchTarget::Exit(exit) => {
            if !exit_ids.contains(exit) {
                return Err(SsaVerifyError::UnknownExit(*exit));
            }
            Ok(())
        }
    }
}

fn verify_jump_target(
    target: SsaBlockId,
    args: &[SsaValueId],
    scope: &HashMap<SsaValueId, SsaValueRepr>,
    block_param_reprs: &HashMap<SsaBlockId, Vec<SsaValueRepr>>,
) -> Result<(), SsaVerifyError> {
    let Some(expected_reprs) = block_param_reprs.get(&target) else {
        return Err(SsaVerifyError::UnknownBlockTarget(target));
    };
    if expected_reprs.len() != args.len() {
        return Err(SsaVerifyError::JumpArityMismatch {
            target,
            expected: expected_reprs.len(),
            got: args.len(),
        });
    }
    for (index, arg) in args.iter().copied().enumerate() {
        let Some(actual) = scope.get(&arg).copied() else {
            return Err(SsaVerifyError::UseBeforeDef {
                block: target,
                value: arg,
            });
        };
        let expected = expected_reprs[index];
        if actual != expected {
            return Err(SsaVerifyError::JumpTypeMismatch {
                target,
                index,
                expected,
                got: actual,
            });
        }
    }
    Ok(())
}

fn verify_materialization(
    materialization: &SsaMaterialization,
    value_reprs: &HashMap<SsaValueId, SsaValueRepr>,
) -> Result<(), SsaVerifyError> {
    let check = |value: SsaValueId, expected: SsaValueRepr| match value_reprs.get(&value).copied() {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(SsaVerifyError::InvalidMaterialization {
            value,
            expected,
            actual,
        }),
        None => Err(SsaVerifyError::UseBeforeDef {
            block: SsaBlockId::new(u32::MAX),
            value,
        }),
    };
    match materialization {
        SsaMaterialization::Value(value) => check(*value, SsaValueRepr::Tagged),
        SsaMaterialization::BoxInt(value) => check(*value, SsaValueRepr::I64),
        SsaMaterialization::BoxFloat(value) => check(*value, SsaValueRepr::F64),
        SsaMaterialization::BoxBool(value) => check(*value, SsaValueRepr::Bool),
        SsaMaterialization::BoxHeapPtr { value, tag } => check(*value, SsaValueRepr::HeapPtr(*tag)),
    }
}

#[allow(dead_code)]
fn render_inst_kind(kind: &SsaInstKind) -> String {
    match kind {
        SsaInstKind::Constant(value) => format!("const {value:?}"),
        SsaInstKind::UnboxInt { input } => format!("unbox_int {input}"),
        SsaInstKind::UnboxFloat { input } => format!("unbox_float {input}"),
        SsaInstKind::UnboxBool { input } => format!("unbox_bool {input}"),
        SsaInstKind::UnboxHeapPtr { input, tag } => format!("unbox_ptr {input}, {tag:?}"),
        SsaInstKind::ValueLen { value } => format!("value_len {value}"),
        SsaInstKind::StringLen { text } => format!("string_len {text}"),
        SsaInstKind::BytesLen { bytes } => format!("bytes_len {bytes}"),
        SsaInstKind::StringSlice {
            text,
            start,
            length,
        } => format!("string_slice {text}, {start}, {length}"),
        SsaInstKind::BytesSlice {
            bytes,
            start,
            length,
        } => format!("bytes_slice {bytes}, {start}, {length}"),
        SsaInstKind::StringGet { text, index } => format!("string_get {text}, {index}"),
        SsaInstKind::BytesGet { bytes, index } => format!("bytes_get {bytes}, {index}"),
        SsaInstKind::BytesHas { bytes, index } => format!("bytes_has {bytes}, {index}"),
        SsaInstKind::StringContains { text, needle } => {
            format!("string_contains {text}, {needle}")
        }
        SsaInstKind::RegexMatch { pattern, text } => {
            format!("regex_match {pattern}, {text}")
        }
        SsaInstKind::RegexReplace {
            pattern,
            text,
            replacement,
        } => format!("regex_replace {pattern}, {text}, {replacement}"),
        SsaInstKind::StringReplaceLiteral {
            text,
            needle,
            replacement,
        } => {
            format!("string_replace_literal {text}, {needle}, {replacement}")
        }
        SsaInstKind::StringLowerAscii { text } => format!("string_lower_ascii {text}"),
        SsaInstKind::TypeOf { value } => format!("type_of {value}"),
        SsaInstKind::ToString { value } => format!("to_string {value}"),
        SsaInstKind::StringSplitLiteral { text, delimiter } => {
            format!("string_split_literal {text}, {delimiter}")
        }
        SsaInstKind::StringConcat { lhs, rhs } => format!("string_concat {lhs}, {rhs}"),
        SsaInstKind::BytesConcat { lhs, rhs } => format!("bytes_concat {lhs}, {rhs}"),
        SsaInstKind::BytesFromArrayU8 { array } => format!("bytes_from_array_u8 {array}"),
        SsaInstKind::BytesToUtf8Ascii { bytes } => format!("bytes_to_utf8_ascii {bytes}"),
        SsaInstKind::BytesToArrayU8 { bytes } => format!("bytes_to_array_u8 {bytes}"),
        SsaInstKind::ArrayNew => "array_new".to_string(),
        SsaInstKind::ArrayLen { array } => format!("array_len {array}"),
        SsaInstKind::ArrayGet { array, index } => format!("array_get {array}, {index}"),
        SsaInstKind::ArrayHas { array, index } => format!("array_has {array}, {index}"),
        SsaInstKind::ArraySet {
            array,
            index,
            value,
        } => format!("array_set {array}, {index}, {value}"),
        SsaInstKind::ArrayPush { array, value } => format!("array_push {array}, {value}"),
        SsaInstKind::MapLen { map } => format!("map_len {map}"),
        SsaInstKind::MapGet { map, key } => format!("map_get {map}, {key}"),
        SsaInstKind::MapHas { map, key } => format!("map_has {map}, {key}"),
        SsaInstKind::MapSet { map, key, value } => {
            format!("map_set {map}, {key}, {value}")
        }
        SsaInstKind::MapIterNext { slot } => format!("map_iter_next {slot}"),
        SsaInstKind::MapIterTakeKey { slot } => format!("map_iter_take_key {slot}"),
        SsaInstKind::MapIterTakeValue { slot } => format!("map_iter_take_value {slot}"),
        SsaInstKind::HostCall { import, args } => {
            format!("host_call {import}({})", render_value_list(args))
        }

        SsaInstKind::IntNeg { input } => format!("ineg {input}"),
        SsaInstKind::IntAdd { lhs, rhs } => format!("iadd {lhs}, {rhs}"),
        SsaInstKind::IntAddImm { lhs, imm } => format!("iadd_imm {lhs}, {imm}"),
        SsaInstKind::IntSub { lhs, rhs } => format!("isub {lhs}, {rhs}"),
        SsaInstKind::IntSubImm { lhs, imm } => format!("isub_imm {lhs}, {imm}"),
        SsaInstKind::IntMul { lhs, rhs } => format!("imul {lhs}, {rhs}"),
        SsaInstKind::IntMulImm { lhs, imm } => format!("imul_imm {lhs}, {imm}"),
        SsaInstKind::IntDiv { lhs, rhs } => format!("idiv {lhs}, {rhs}"),
        SsaInstKind::IntDivImm { lhs, imm } => format!("idiv_imm {lhs}, {imm}"),
        SsaInstKind::IntMod { lhs, rhs } => format!("imod {lhs}, {rhs}"),
        SsaInstKind::IntModImm { lhs, imm } => format!("imod_imm {lhs}, {imm}"),
        SsaInstKind::IntShl { lhs, rhs } => format!("ishl {lhs}, {rhs}"),
        SsaInstKind::IntShlImm { lhs, amount } => format!("ishl_imm {lhs}, {amount}"),
        SsaInstKind::IntShr { lhs, rhs } => format!("ishr {lhs}, {rhs}"),
        SsaInstKind::IntShrImm { lhs, amount } => format!("ishr_imm {lhs}, {amount}"),
        SsaInstKind::IntLshr { lhs, rhs } => format!("ilshr {lhs}, {rhs}"),
        SsaInstKind::IntLshrImm { lhs, amount } => format!("ilshr_imm {lhs}, {amount}"),
        SsaInstKind::BoolAnd { lhs, rhs } => format!("bool_and {lhs}, {rhs}"),
        SsaInstKind::BoolOr { lhs, rhs } => format!("bool_or {lhs}, {rhs}"),
        SsaInstKind::BoolNot { input } => format!("bool_not {input}"),
        SsaInstKind::FloatNeg { input } => format!("fneg {input}"),
        SsaInstKind::FloatAdd { lhs, rhs } => format!("fadd {lhs}, {rhs}"),
        SsaInstKind::FloatSub { lhs, rhs } => format!("fsub {lhs}, {rhs}"),
        SsaInstKind::FloatMul { lhs, rhs } => format!("fmul {lhs}, {rhs}"),
        SsaInstKind::FloatDiv { lhs, rhs } => format!("fdiv {lhs}, {rhs}"),
        SsaInstKind::FloatMod { lhs, rhs } => format!("fmod {lhs}, {rhs}"),
        SsaInstKind::FloatCmpEq { lhs, rhs } => format!("fcmp_eq {lhs}, {rhs}"),
        SsaInstKind::FloatCmpLt { lhs, rhs } => format!("fcmp_lt {lhs}, {rhs}"),
        SsaInstKind::FloatCmpGt { lhs, rhs } => format!("fcmp_gt {lhs}, {rhs}"),
        SsaInstKind::IntCmpEq { lhs, rhs } => format!("icmp_eq {lhs}, {rhs}"),
        SsaInstKind::ValueCmpEq { lhs, rhs } => format!("value_eq {lhs}, {rhs}"),
        SsaInstKind::IntCmpLt { lhs, rhs } => format!("icmp_lt {lhs}, {rhs}"),
        SsaInstKind::IntCmpLtImm { lhs, imm } => format!("icmp_lt_imm {lhs}, {imm}"),
        SsaInstKind::IntCmpGt { lhs, rhs } => format!("icmp_gt {lhs}, {rhs}"),
        SsaInstKind::IntCmpGtImm { lhs, imm } => format!("icmp_gt_imm {lhs}, {imm}"),
    }
}

#[allow(dead_code)]
fn render_terminator(terminator: &SsaTerminator) -> String {
    match terminator {
        SsaTerminator::Jump { target, args } => {
            format!("jump {}({})", target, render_value_list(args))
        }
        SsaTerminator::BranchBool {
            condition,
            if_true,
            if_false,
        } => format!(
            "branch_bool {} true:{} false:{}",
            condition,
            render_branch_target(if_true),
            render_branch_target(if_false)
        ),
        SsaTerminator::Exit { exit } => format!("exit {exit}"),
        SsaTerminator::Return { exit } => format!("return {exit}"),
        SsaTerminator::CallValue {
            argc,
            call_ip,
            resume_ip,
            exit,
        } => format!("call_value argc={argc} call_ip={call_ip} resume_ip={resume_ip} {exit}"),
    }
}

#[allow(dead_code)]
fn render_branch_target(target: &SsaBranchTarget) -> String {
    match target {
        SsaBranchTarget::Block { target, args } => {
            format!("{}({})", target, render_value_list(args))
        }
        SsaBranchTarget::Exit(exit) => exit.to_string(),
    }
}

#[allow(dead_code)]
fn render_value_list(values: &[SsaValueId]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[allow(dead_code)]
fn render_materialization(materialization: &SsaMaterialization) -> String {
    match materialization {
        SsaMaterialization::Value(value) => format!("value({value})"),
        SsaMaterialization::BoxInt(value) => format!("box_int({value})"),
        SsaMaterialization::BoxFloat(value) => format!("box_float({value})"),
        SsaMaterialization::BoxBool(value) => format!("box_bool({value})"),
        SsaMaterialization::BoxHeapPtr { value, tag } => {
            format!("box_ptr({value}, {tag:?})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_accepts_simple_loop_shape() {
        let mut builder = SsaTraceBuilder::new(12, 0);
        let entry = builder.entry();
        let local = builder
            .append_param(entry, SsaValueRepr::I64, "local0")
            .expect("entry local");
        let cond = builder
            .append_param(entry, SsaValueRepr::Bool, "cond")
            .expect("entry cond");
        let inc = builder
            .append_value_inst(
                entry,
                14,
                SsaValueRepr::I64,
                SsaInstKind::IntAddImm {
                    lhs: local.id,
                    imm: 1,
                },
            )
            .expect("add");
        let exit = builder.add_exit(
            20,
            Vec::new(),
            vec![SsaMaterialization::BoxInt(inc.id)],
            vec![true],
        );
        builder
            .set_terminator(
                entry,
                SsaTerminator::BranchBool {
                    condition: cond.id,
                    if_true: SsaBranchTarget::Exit(exit),
                    if_false: SsaBranchTarget::Block {
                        target: entry,
                        args: vec![inc.id, cond.id],
                    },
                },
            )
            .expect("term");
        let trace = builder.finish();
        assert_eq!(trace.verify(), Ok(()));
    }

    #[test]
    fn verifier_rejects_jump_arity_mismatch() {
        let mut builder = SsaTraceBuilder::new(1, 0);
        let entry = builder.entry();
        let next = builder.create_block();
        let value = builder
            .append_param(entry, SsaValueRepr::I64, "local0")
            .expect("entry local");
        builder
            .append_param(next, SsaValueRepr::I64, "loop_local")
            .expect("loop local");
        builder
            .set_terminator(
                entry,
                SsaTerminator::Jump {
                    target: next,
                    args: Vec::new(),
                },
            )
            .expect("jump");
        let exit = builder.add_exit(
            2,
            Vec::new(),
            vec![SsaMaterialization::BoxInt(value.id)],
            vec![false],
        );
        builder
            .set_terminator(next, SsaTerminator::Return { exit })
            .expect("return");
        let trace = builder.finish();
        assert_eq!(
            trace.verify(),
            Err(SsaVerifyError::JumpArityMismatch {
                target: next,
                expected: 1,
                got: 0,
            })
        );
    }

    #[test]
    fn verifier_rejects_entry_stack_depth_beyond_entry_params() {
        let mut builder = SsaTraceBuilder::new(1, 1);
        let entry = builder.entry();
        let exit = builder.add_exit(2, Vec::new(), Vec::new(), Vec::new());
        builder
            .set_terminator(entry, SsaTerminator::Return { exit })
            .expect("return");
        let trace = builder.finish();
        assert!(trace.verify().is_err());
    }

    #[test]
    fn verifier_rejects_dirty_local_length_mismatch() {
        let mut builder = SsaTraceBuilder::new(1, 0);
        let entry = builder.entry();
        let local = builder
            .append_param(entry, SsaValueRepr::Tagged, "local0")
            .expect("entry local");
        let exit = builder.add_exit(
            2,
            Vec::new(),
            vec![SsaMaterialization::Value(local.id)],
            Vec::new(),
        );
        builder
            .set_terminator(entry, SsaTerminator::Return { exit })
            .expect("return");

        assert_eq!(
            builder.finish().verify(),
            Err(SsaVerifyError::DirtyLocalLengthMismatch {
                exit,
                locals: 1,
                dirty_locals: 0,
            })
        );
    }
}
