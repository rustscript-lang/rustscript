use super::*;

mod catalog;
mod graph;
mod render;

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
