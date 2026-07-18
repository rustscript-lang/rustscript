use std::collections::HashMap;

use crate::{VmError, VmResult};

use super::JitTrace;
use super::deopt::SideTraceImport;
use super::ir::{
    SsaBlock, SsaBlockId, SsaBranchTarget, SsaExit, SsaExitId, SsaInst, SsaInstKind,
    SsaMaterialization, SsaTerminator, SsaTrace, SsaValueId,
};
use super::trace::TraceExitKey;

#[derive(Clone, Debug)]
pub(crate) struct FusedRegionLink {
    pub(crate) exit: SsaExitId,
    pub(crate) child_entry: SsaBlockId,
    pub(crate) args: Vec<SsaMaterialization>,
}

#[derive(Clone, Debug)]
pub(crate) struct FusedRegion {
    pub(crate) trace: JitTrace,
    pub(crate) link: FusedRegionLink,
    pub(crate) exit_keys: HashMap<u32, TraceExitKey>,
}

pub(crate) fn fuse_two_trace_region(
    parent: &JitTrace,
    child: &JitTrace,
    import: &SideTraceImport,
) -> VmResult<FusedRegion> {
    let value_offset = parent
        .ssa
        .blocks
        .iter()
        .flat_map(|block| {
            block.params.iter().map(|param| param.value.id).chain(
                block
                    .insts
                    .iter()
                    .filter_map(|inst| inst.output.map(|out| out.id)),
            )
        })
        .map(SsaValueId::raw)
        .max()
        .map_or(0, |max| max.saturating_add(1));
    let block_offset = u32::try_from(parent.ssa.blocks.len())
        .map_err(|_| VmError::JitNative("region parent block count exceeds u32".to_string()))?;
    let exit_offset = u32::try_from(parent.ssa.exits.len())
        .map_err(|_| VmError::JitNative("region parent exit count exceeds u32".to_string()))?;

    let mut blocks = parent.ssa.blocks.clone();
    blocks.extend(
        child
            .ssa
            .blocks
            .iter()
            .cloned()
            .map(|block| offset_block(block, value_offset, block_offset, exit_offset))
            .collect::<VmResult<Vec<_>>>()?,
    );
    let mut exits = parent.ssa.exits.clone();
    exits.extend(
        child
            .ssa
            .exits
            .iter()
            .cloned()
            .map(|exit| offset_exit(exit, value_offset, exit_offset))
            .collect::<VmResult<Vec<_>>>()?,
    );

    let child_entry = offset_block_id(child.ssa.entry, block_offset)?;
    let ssa = SsaTrace {
        root_ip: parent.ssa.root_ip,
        entry_stack_depth: parent.ssa.entry_stack_depth,
        entry: parent.ssa.entry,
        blocks,
        exits,
    };
    ssa.verify().map_err(|err| {
        VmError::JitNative(format!(
            "fused two-trace region failed SSA verification: {err:?}"
        ))
    })?;

    let mut trace = parent.clone();
    trace.ssa = ssa;
    trace.has_call |= child.has_call;
    trace.has_yielding_call |= child.has_yielding_call;
    trace.op_names.extend(child.op_names.iter().cloned());

    let mut exit_keys = HashMap::new();
    for exit in &parent.ssa.exits {
        exit_keys.insert(
            exit.id.raw(),
            TraceExitKey {
                parent_trace_id: parent.id,
                exit_id: exit.id,
            },
        );
    }
    for exit in &child.ssa.exits {
        let fused_exit = offset_exit_id(exit.id, exit_offset)?;
        exit_keys.insert(
            fused_exit.raw(),
            TraceExitKey {
                parent_trace_id: child.id,
                exit_id: exit.id,
            },
        );
    }

    Ok(FusedRegion {
        trace,
        link: FusedRegionLink {
            exit: import.parent_exit,
            child_entry,
            args: import.args.clone(),
        },
        exit_keys,
    })
}

fn offset_block(
    mut block: SsaBlock,
    value_offset: u32,
    block_offset: u32,
    exit_offset: u32,
) -> VmResult<SsaBlock> {
    block.id = offset_block_id(block.id, block_offset)?;
    for param in &mut block.params {
        param.value.id = offset_value_id(param.value.id, value_offset)?;
    }
    for inst in &mut block.insts {
        offset_inst(inst, value_offset)?;
    }
    if let Some(terminator) = &mut block.terminator {
        offset_terminator(terminator, value_offset, block_offset, exit_offset)?;
    }
    Ok(block)
}

fn offset_exit(mut exit: SsaExit, value_offset: u32, exit_offset: u32) -> VmResult<SsaExit> {
    exit.id = offset_exit_id(exit.id, exit_offset)?;
    for materialization in exit.stack.iter_mut().chain(&mut exit.locals) {
        offset_materialization(materialization, value_offset)?;
    }
    Ok(exit)
}

fn offset_inst(inst: &mut SsaInst, value_offset: u32) -> VmResult<()> {
    if let Some(output) = &mut inst.output {
        output.id = offset_value_id(output.id, value_offset)?;
    }
    remap_inst_inputs(&mut inst.kind, |value| offset_value_id(value, value_offset))
}

fn remap_inst_inputs(
    kind: &mut SsaInstKind,
    mut remap: impl FnMut(SsaValueId) -> VmResult<SsaValueId>,
) -> VmResult<()> {
    macro_rules! one {
        ($field:expr) => {{
            *$field = remap(*$field)?;
        }};
    }
    macro_rules! two {
        ($lhs:expr, $rhs:expr) => {{
            one!($lhs);
            one!($rhs);
        }};
    }
    match kind {
        SsaInstKind::Constant(_) => {}
        SsaInstKind::HostCall { args, .. } => {
            for arg in args {
                one!(arg);
            }
        }
        SsaInstKind::UnboxInt { input }
        | SsaInstKind::UnboxFloat { input }
        | SsaInstKind::UnboxBool { input }
        | SsaInstKind::UnboxHeapPtr { input, .. }
        | SsaInstKind::ValueLen { value: input }
        | SsaInstKind::StringLen { text: input }
        | SsaInstKind::BytesLen { bytes: input }
        | SsaInstKind::ArrayLen { array: input }
        | SsaInstKind::MapLen { map: input }
        | SsaInstKind::IntNeg { input }
        | SsaInstKind::BoolNot { input }
        | SsaInstKind::FloatNeg { input }
        | SsaInstKind::StringLowerAscii { text: input }
        | SsaInstKind::TypeOf { value: input }
        | SsaInstKind::ToString { value: input }
        | SsaInstKind::BytesFromArrayU8 { array: input }
        | SsaInstKind::BytesToArrayU8 { bytes: input }
        | SsaInstKind::MapIterNext { slot: input }
        | SsaInstKind::MapIterTakeKey { slot: input }
        | SsaInstKind::MapIterTakeValue { slot: input } => one!(input),
        SsaInstKind::StringSlice {
            text,
            start,
            length,
        }
        | SsaInstKind::BytesSlice {
            bytes: text,
            start,
            length,
        } => {
            one!(text);
            one!(start);
            one!(length);
        }
        SsaInstKind::RegexReplace {
            pattern,
            text,
            replacement,
        }
        | SsaInstKind::StringReplaceLiteral {
            text,
            needle: pattern,
            replacement,
        } => {
            one!(pattern);
            one!(text);
            one!(replacement);
        }
        SsaInstKind::ArraySet {
            array,
            index,
            value,
        }
        | SsaInstKind::MapSet {
            map: array,
            key: index,
            value,
        } => {
            one!(array);
            one!(index);
            one!(value);
        }
        SsaInstKind::StringGet { text, index }
        | SsaInstKind::BytesGet { bytes: text, index }
        | SsaInstKind::BytesHas { bytes: text, index }
        | SsaInstKind::StringContains {
            text,
            needle: index,
        }
        | SsaInstKind::RegexMatch {
            pattern: text,
            text: index,
        }
        | SsaInstKind::StringSplitLiteral {
            text,
            delimiter: index,
        }
        | SsaInstKind::StringConcat {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::BytesConcat {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::ArrayGet { array: text, index }
        | SsaInstKind::ArrayHas { array: text, index }
        | SsaInstKind::ArrayPush {
            array: text,
            value: index,
        }
        | SsaInstKind::MapGet {
            map: text,
            key: index,
        }
        | SsaInstKind::MapHas {
            map: text,
            key: index,
        }
        | SsaInstKind::IntAdd {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntSub {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntMul {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntDiv {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntMod {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntShl {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntShr {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntLshr {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::BoolAnd {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::BoolOr {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatAdd {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatSub {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatMul {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatDiv {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatMod {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatCmpEq {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatCmpLt {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::FloatCmpGt {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntCmpEq {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::ValueCmpEq {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntCmpLt {
            lhs: text,
            rhs: index,
        }
        | SsaInstKind::IntCmpGt {
            lhs: text,
            rhs: index,
        } => two!(text, index),
        SsaInstKind::IntAddImm { lhs, .. }
        | SsaInstKind::IntSubImm { lhs, .. }
        | SsaInstKind::IntMulImm { lhs, .. }
        | SsaInstKind::IntDivImm { lhs, .. }
        | SsaInstKind::IntModImm { lhs, .. }
        | SsaInstKind::IntShlImm { lhs, .. }
        | SsaInstKind::IntShrImm { lhs, .. }
        | SsaInstKind::IntLshrImm { lhs, .. }
        | SsaInstKind::IntCmpLtImm { lhs, .. }
        | SsaInstKind::IntCmpGtImm { lhs, .. } => one!(lhs),
    }
    Ok(())
}

fn offset_terminator(
    terminator: &mut SsaTerminator,
    value_offset: u32,
    block_offset: u32,
    exit_offset: u32,
) -> VmResult<()> {
    match terminator {
        SsaTerminator::Jump { target, args } => {
            *target = offset_block_id(*target, block_offset)?;
            for arg in args {
                *arg = offset_value_id(*arg, value_offset)?;
            }
        }
        SsaTerminator::BranchBool {
            condition,
            if_true,
            if_false,
        } => {
            *condition = offset_value_id(*condition, value_offset)?;
            offset_branch_target(if_true, value_offset, block_offset, exit_offset)?;
            offset_branch_target(if_false, value_offset, block_offset, exit_offset)?;
        }
        SsaTerminator::Exit { exit }
        | SsaTerminator::Return { exit }
        | SsaTerminator::CallValue { exit, .. } => {
            *exit = offset_exit_id(*exit, exit_offset)?;
        }
    }
    Ok(())
}

fn offset_branch_target(
    target: &mut SsaBranchTarget,
    value_offset: u32,
    block_offset: u32,
    exit_offset: u32,
) -> VmResult<()> {
    match target {
        SsaBranchTarget::Block { target, args } => {
            *target = offset_block_id(*target, block_offset)?;
            for arg in args {
                *arg = offset_value_id(*arg, value_offset)?;
            }
        }
        SsaBranchTarget::Exit(exit) => *exit = offset_exit_id(*exit, exit_offset)?,
    }
    Ok(())
}

fn offset_materialization(
    materialization: &mut SsaMaterialization,
    value_offset: u32,
) -> VmResult<()> {
    let value = match materialization {
        SsaMaterialization::Value(value)
        | SsaMaterialization::BoxInt(value)
        | SsaMaterialization::BoxFloat(value)
        | SsaMaterialization::BoxBool(value)
        | SsaMaterialization::BoxHeapPtr { value, .. } => value,
    };
    *value = offset_value_id(*value, value_offset)?;
    Ok(())
}

fn offset_value_id(value: SsaValueId, offset: u32) -> VmResult<SsaValueId> {
    value
        .raw()
        .checked_add(offset)
        .map(SsaValueId::new)
        .ok_or_else(|| VmError::JitNative("region SSA value id overflow".to_string()))
}

fn offset_block_id(block: SsaBlockId, offset: u32) -> VmResult<SsaBlockId> {
    u32::try_from(block.index())
        .ok()
        .and_then(|raw| raw.checked_add(offset))
        .map(SsaBlockId::new)
        .ok_or_else(|| VmError::JitNative("region SSA block id overflow".to_string()))
}

fn offset_exit_id(exit: SsaExitId, offset: u32) -> VmResult<SsaExitId> {
    exit.raw()
        .checked_add(offset)
        .map(SsaExitId::new)
        .ok_or_else(|| VmError::JitNative("region SSA exit id overflow".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::jit::JitTraceTerminal;
    use crate::vm::jit::ir::{SsaTerminator, SsaTraceBuilder};

    fn test_trace(id: usize, root_ip: usize, exit_ip: usize) -> (JitTrace, SsaExitId) {
        let mut ssa = SsaTraceBuilder::new(root_ip, 0);
        let entry = ssa.entry();
        let exit = ssa.add_exit(exit_ip, Vec::new(), Vec::new(), Vec::new());
        ssa.set_terminator(entry, SsaTerminator::Exit { exit })
            .unwrap();
        (
            JitTrace {
                id,
                frame_key: 7,
                root_ip,
                entry_stack_depth: 0,
                start_line: None,
                has_call: false,
                has_yielding_call: false,
                op_names: vec![format!("trace{id}")],
                terminal: JitTraceTerminal::BranchExit,
                executions: 0,
                ssa: ssa.finish(),
            },
            exit,
        )
    }

    #[test]
    fn two_trace_region_remaps_child_ids_and_preserves_exit_identity() {
        let (parent, parent_exit) = test_trace(3, 0, 12);
        let (child, child_exit) = test_trace(5, 12, 24);
        let import = SideTraceImport {
            parent_exit,
            stack_depth: 0,
            local_count: 0,
            dirty_locals: Vec::new(),
            args: Vec::new(),
        };

        let region = fuse_two_trace_region(&parent, &child, &import).unwrap();

        assert_eq!(region.trace.ssa.blocks.len(), 2);
        assert_eq!(region.trace.ssa.exits.len(), 2);
        assert_eq!(region.link.exit, parent_exit);
        assert_eq!(region.link.child_entry, SsaBlockId::new(1));
        assert!(region.link.args.is_empty());
        assert_eq!(
            region.exit_keys.get(&parent_exit.raw()),
            Some(&TraceExitKey {
                parent_trace_id: parent.id,
                exit_id: parent_exit,
            })
        );
        assert_eq!(
            region.exit_keys.get(&(child_exit.raw() + 1)),
            Some(&TraceExitKey {
                parent_trace_id: child.id,
                exit_id: child_exit,
            })
        );
    }
}
