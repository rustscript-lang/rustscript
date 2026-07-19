#![allow(dead_code)]
use super::ir::{
    SsaExit, SsaExitId, SsaMaterialization, SsaTrace, SsaValue, SsaValueId, SsaValueRepr,
};

pub(crate) fn materialize_ssa_values(
    values: impl IntoIterator<Item = SsaValue>,
) -> Vec<SsaMaterialization> {
    values.into_iter().map(materialize_ssa_value).collect()
}

pub(crate) fn materialize_ssa_value(value: SsaValue) -> SsaMaterialization {
    match value.repr {
        SsaValueRepr::Tagged => SsaMaterialization::Value(value.id),
        SsaValueRepr::I64 => SsaMaterialization::BoxInt(value.id),
        SsaValueRepr::F64 => SsaMaterialization::BoxFloat(value.id),
        SsaValueRepr::Bool => SsaMaterialization::BoxBool(value.id),
        SsaValueRepr::HeapPtr(tag) => SsaMaterialization::BoxHeapPtr {
            value: value.id,
            tag,
        },
    }
}

pub(crate) fn exit_inputs(exit: &SsaExit) -> Vec<SsaValueId> {
    let mut out = Vec::new();
    let dirty_locals = exit
        .locals
        .iter()
        .zip(&exit.dirty_locals)
        .filter_map(|(materialization, dirty)| dirty.then_some(materialization));
    for materialization in exit.stack.iter().chain(dirty_locals) {
        let value = match materialization {
            SsaMaterialization::Value(value)
            | SsaMaterialization::BoxInt(value)
            | SsaMaterialization::BoxFloat(value)
            | SsaMaterialization::BoxBool(value) => *value,
            SsaMaterialization::BoxHeapPtr { value, .. } => *value,
        };
        if !out.contains(&value) {
            out.push(value);
        }
    }
    for frame in &exit.virtual_frames {
        let dirty_locals = frame
            .locals
            .iter()
            .zip(&frame.dirty_locals)
            .filter_map(|(materialization, dirty)| dirty.then_some(materialization));
        for materialization in frame.operand_stack.iter().chain(dirty_locals) {
            let value = match materialization {
                SsaMaterialization::Value(value)
                | SsaMaterialization::BoxInt(value)
                | SsaMaterialization::BoxFloat(value)
                | SsaMaterialization::BoxBool(value) => *value,
                SsaMaterialization::BoxHeapPtr { value, .. } => *value,
            };
            if !out.contains(&value) {
                out.push(value);
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SideTraceImport {
    pub(crate) parent_exit: SsaExitId,
    pub(crate) stack_depth: usize,
    pub(crate) local_count: usize,
    pub(crate) dirty_locals: Vec<bool>,
    pub(crate) args: Vec<SsaMaterialization>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideTraceImportError {
    UnknownParentExit(SsaExitId),
    ExitIpMismatch { parent: usize, child: usize },
    StackDepthMismatch { parent: usize, child: usize },
    LocalCountMismatch { parent: usize, child: usize },
    InvalidChildEntry,
}

pub(crate) fn side_trace_import(
    parent: &SsaTrace,
    parent_exit: SsaExitId,
    child: &SsaTrace,
) -> Result<SideTraceImport, SideTraceImportError> {
    let exit = parent
        .exits
        .iter()
        .find(|exit| exit.id == parent_exit)
        .ok_or(SideTraceImportError::UnknownParentExit(parent_exit))?;
    if exit.exit_ip != child.root_ip {
        return Err(SideTraceImportError::ExitIpMismatch {
            parent: exit.exit_ip,
            child: child.root_ip,
        });
    }
    if exit.stack.len() != child.entry_stack_depth {
        return Err(SideTraceImportError::StackDepthMismatch {
            parent: exit.stack.len(),
            child: child.entry_stack_depth,
        });
    }
    let child_entry = child
        .blocks
        .get(child.entry.index())
        .ok_or(SideTraceImportError::InvalidChildEntry)?;
    let child_local_count = child_entry
        .params
        .len()
        .checked_sub(child.entry_stack_depth)
        .ok_or(SideTraceImportError::InvalidChildEntry)?;
    if exit.locals.len() != child_local_count {
        return Err(SideTraceImportError::LocalCountMismatch {
            parent: exit.locals.len(),
            child: child_local_count,
        });
    }

    Ok(SideTraceImport {
        parent_exit,
        stack_depth: exit.stack.len(),
        local_count: exit.locals.len(),
        dirty_locals: exit.dirty_locals.clone(),
        args: exit.stack.iter().chain(&exit.locals).cloned().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::jit::ir::{SsaTerminator, SsaTraceBuilder, SsaValueRepr};

    #[test]
    fn side_trace_import_maps_parent_stack_then_locals_to_child_entry() {
        let mut parent = SsaTraceBuilder::new(0, 1);
        let parent_entry = parent.entry();
        let stack = parent
            .append_param(parent_entry, SsaValueRepr::Tagged, "stack0".to_string())
            .unwrap();
        let local = parent
            .append_param(parent_entry, SsaValueRepr::I64, "local0".to_string())
            .unwrap();
        let exit_id = parent.add_exit(
            12,
            vec![SsaMaterialization::Value(stack.id)],
            vec![SsaMaterialization::BoxInt(local.id)],
            vec![true],
        );
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: exit_id })
            .unwrap();
        let parent = parent.finish();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "stack0".to_string())
            .unwrap();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "local0".to_string())
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();
        let child = child.finish();

        let import = side_trace_import(&parent, exit_id, &child).unwrap();

        assert_eq!(import.parent_exit, exit_id);
        assert_eq!(import.stack_depth, 1);
        assert_eq!(import.local_count, 1);
        assert_eq!(import.dirty_locals, vec![true]);
        assert_eq!(
            import.args,
            vec![
                SsaMaterialization::Value(stack.id),
                SsaMaterialization::BoxInt(local.id),
            ]
        );
    }

    #[test]
    fn side_trace_import_rejects_mismatched_exit_ip() {
        let mut parent = SsaTraceBuilder::new(0, 0);
        let parent_entry = parent.entry();
        let exit_id = parent.add_exit(12, Vec::new(), Vec::new(), Vec::new());
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: exit_id })
            .unwrap();
        let parent = parent.finish();

        let mut child = SsaTraceBuilder::new(13, 0);
        let child_entry = child.entry();
        let child_exit = child.add_exit(14, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();
        let child = child.finish();

        assert_eq!(
            side_trace_import(&parent, exit_id, &child),
            Err(SideTraceImportError::ExitIpMismatch {
                parent: 12,
                child: 13,
            })
        );
    }

    #[test]
    fn side_trace_import_rejects_mismatched_stack_depth() {
        let mut parent = SsaTraceBuilder::new(0, 0);
        let parent_entry = parent.entry();
        let exit_id = parent.add_exit(12, Vec::new(), Vec::new(), Vec::new());
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: exit_id })
            .unwrap();
        let parent = parent.finish();

        let mut child = SsaTraceBuilder::new(12, 1);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "stack0".to_string())
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();
        let child = child.finish();

        assert_eq!(
            side_trace_import(&parent, exit_id, &child),
            Err(SideTraceImportError::StackDepthMismatch {
                parent: 0,
                child: 1,
            })
        );
    }

    #[test]
    fn side_trace_import_rejects_mismatched_local_count() {
        let mut parent = SsaTraceBuilder::new(0, 0);
        let parent_entry = parent.entry();
        let exit_id = parent.add_exit(12, Vec::new(), Vec::new(), Vec::new());
        parent
            .set_terminator(parent_entry, SsaTerminator::Exit { exit: exit_id })
            .unwrap();
        let parent = parent.finish();

        let mut child = SsaTraceBuilder::new(12, 0);
        let child_entry = child.entry();
        child
            .append_param(child_entry, SsaValueRepr::Tagged, "local0".to_string())
            .unwrap();
        let child_exit = child.add_exit(13, Vec::new(), Vec::new(), Vec::new());
        child
            .set_terminator(child_entry, SsaTerminator::Exit { exit: child_exit })
            .unwrap();
        let child = child.finish();

        assert_eq!(
            side_trace_import(&parent, exit_id, &child),
            Err(SideTraceImportError::LocalCountMismatch {
                parent: 0,
                child: 1,
            })
        );
    }

    #[test]
    fn exit_inputs_include_virtual_frame_values_once_in_frame_order() {
        use crate::vm::jit::ir::VirtualFrameSnapshot;

        let mut builder = SsaTraceBuilder::new(0, 0);
        let entry = builder.entry();
        let caller = builder
            .append_param(entry, SsaValueRepr::Tagged, "caller")
            .unwrap();
        let callee_stack = builder
            .append_param(entry, SsaValueRepr::I64, "callee_stack")
            .unwrap();
        let callee_local = builder
            .append_param(entry, SsaValueRepr::Bool, "callee_local")
            .unwrap();
        let exit_id = builder.add_exit_with_virtual_frames(
            20,
            vec![SsaMaterialization::Value(caller.id)],
            Vec::new(),
            Vec::new(),
            vec![VirtualFrameSnapshot {
                prototype_id: 1,
                call_ip: 10,
                return_ip: 12,
                resume_ip: 20,
                operand_stack: vec![SsaMaterialization::BoxInt(callee_stack.id)],
                locals: vec![
                    SsaMaterialization::Value(caller.id),
                    SsaMaterialization::BoxBool(callee_local.id),
                ],
                dirty_locals: vec![true, true],
            }],
        );
        builder
            .set_terminator(entry, SsaTerminator::Exit { exit: exit_id })
            .unwrap();
        let trace = builder.finish();
        assert_eq!(
            exit_inputs(&trace.exits[0]),
            vec![caller.id, callee_stack.id, callee_local.id]
        );
    }
}
