use super::ir::{SsaInstKind, SsaMaterialization, SsaTrace};

pub(crate) fn boxed_load_site_count(trace: &SsaTrace) -> u64 {
    trace
        .blocks
        .iter()
        .flat_map(|block| block.insts.iter())
        .filter(|inst| {
            matches!(
                inst.kind,
                SsaInstKind::UnboxInt { .. }
                    | SsaInstKind::UnboxFloat { .. }
                    | SsaInstKind::UnboxBool { .. }
                    | SsaInstKind::UnboxHeapPtr { .. }
            )
        })
        .count() as u64
}

pub(crate) fn boxed_store_site_count(trace: &SsaTrace) -> u64 {
    trace
        .exits
        .iter()
        .flat_map(|exit| exit.stack.iter().chain(exit.locals.iter()))
        .filter(|materialization| {
            matches!(
                materialization,
                SsaMaterialization::BoxInt(_)
                    | SsaMaterialization::BoxFloat(_)
                    | SsaMaterialization::BoxBool(_)
                    | SsaMaterialization::BoxHeapPtr { .. }
            )
        })
        .count() as u64
}
