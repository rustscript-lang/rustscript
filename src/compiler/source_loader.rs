use std::path::Path;

use crate::compiler::source_map::SourceMap;

use super::{
    CompileSourceFileOptions, SourceError, SourceFlavor, SourcePathError, frontends,
    linker::ParsedUnit,
};

mod graph;
mod imports;
mod line_map;
mod model;
mod rewrite;

use graph::{build_rustscript_import_prelude, collect_module_units};
use imports::{parse_module_imports, strip_import_directives};
use line_map::remap_frontend_ir_line_numbers;
use model::ModuleCollectState;
pub use model::{FrontendImportSyntax, ImportClause, ModuleImport, NamedImport};
use rewrite::rewrite_imported_call_sites;

pub(super) fn load_units_for_source_file(
    path: &Path,
    flavor: SourceFlavor,
    source_raw: &str,
    options: &CompileSourceFileOptions,
) -> Result<(String, Vec<ParsedUnit>), SourcePathError> {
    let root_imports = parse_module_imports(source_raw, flavor, path, options)?;
    let source = strip_import_directives(source_raw, flavor, options)?;

    let mut collect_state = ModuleCollectState::default();
    collect_state.visiting.push(path.to_path_buf());
    collect_module_units(path, source_raw, flavor, options, &mut collect_state)?;

    let rewritten_root = rewrite_imported_call_sites(
        &source,
        flavor,
        path,
        &root_imports,
        &collect_state.module_exports,
        options,
    )?;
    let mut prelude = build_rustscript_import_prelude(
        path,
        &root_imports,
        &collect_state.module_exports,
        options,
    )?;
    let root_prelude_lines = prelude.lines().count();
    prelude.push_str(&rewritten_root.source);
    let root_parse_source = prelude;

    let mut root_source_map = SourceMap::new();
    let root_source_id = root_source_map.add_source(path.display().to_string(), source_raw);
    let mut root_parsed = frontends::parse_source(&root_parse_source, flavor, options)
        .map_err(|mut err| {
            if root_prelude_lines > 0 {
                err.line = err.line.saturating_sub(root_prelude_lines).max(1);
                // Reattach span against original source text for diagnostics.
                err.span = None;
            }
            SourceError::Parse(err.with_line_span_from_source(&root_source_map, root_source_id))
        })
        .map_err(SourcePathError::Source)?;
    if root_prelude_lines > 0 {
        remap_frontend_ir_line_numbers(&mut root_parsed, root_prelude_lines);
    }
    collect_state.units.push(ParsedUnit {
        parsed: root_parsed,
        scope_prefix: None,
        source_name: path.display().to_string(),
    });

    Ok((root_parse_source, collect_state.units))
}
