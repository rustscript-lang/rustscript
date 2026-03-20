use super::ir::{SsaExit, SsaMaterialization, SsaValue, SsaValueId, SsaValueRepr};

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
    for materialization in exit.stack.iter().chain(exit.locals.iter()) {
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
    out
}
