use super::*;

mod abi_blocks;
mod abi_render;
mod catalog;
mod graph;
mod render;

#[derive(Clone, Debug)]
pub(super) struct FlowActionStatement {
    pub(super) rustscript: String,
    pub(super) javascript: String,
    pub(super) lua: String,
    pub(super) scheme: String,
}

pub(super) fn ui_block_catalog() -> Vec<UiBlockDefinition> {
    catalog::ui_block_catalog()
}

pub(super) fn render_ui_sources(
    blocks: &[UiBlockInstance],
    nodes: &[UiGraphNode],
    edges: &[UiGraphEdge],
) -> Result<UiSourceBundle, (StatusCode, Json<ErrorResponse>)> {
    graph::render_ui_sources(blocks, nodes, edges)
}

pub(super) fn parse_ui_flavor(
    value: Option<&str>,
) -> Result<(SourceFlavor, &'static str), (StatusCode, Json<ErrorResponse>)> {
    render::parse_ui_flavor(value)
}

pub(super) fn source_for_flavor(bundle: &UiSourceBundle, flavor: SourceFlavor) -> String {
    render::source_for_flavor(bundle, flavor)
}
