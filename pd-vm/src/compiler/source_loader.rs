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
use imports::{parse_module_imports, strip_import_directives, vm_namespace_direct_calls_supported};
use line_map::remap_frontend_ir_line_numbers;
use model::ModuleCollectState;
use rewrite::{build_scheme_import_context, rewrite_imported_call_sites};

pub(super) fn load_units_for_source_file(
    path: &Path,
    flavor: SourceFlavor,
    source_raw: &str,
    options: &CompileSourceFileOptions,
) -> Result<(String, Vec<ParsedUnit>), SourcePathError> {
    let root_imports = parse_module_imports(source_raw, flavor, path)?;
    let source = strip_import_directives(source_raw, flavor);

    let mut collect_state = ModuleCollectState::default();
    collect_state.visiting.push(path.to_path_buf());
    collect_module_units(path, source_raw, flavor, options, &mut collect_state)?;

    let mut scheme_import_context = None;
    let (root_parse_source, root_prelude_lines) = match flavor {
        SourceFlavor::Scheme => {
            scheme_import_context = Some(build_scheme_import_context(
                path,
                &root_imports,
                &collect_state.module_exports,
                options,
            )?);
            (source.clone(), 0)
        }
        SourceFlavor::RustScript | SourceFlavor::JavaScript | SourceFlavor::Lua => {
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
            if flavor == SourceFlavor::RustScript
                && rewritten_root.requires_vm_namespace
                && !vm_namespace_direct_calls_supported(&root_imports)
            {
                prelude.push_str("use vm;\n");
            }
            let prelude_lines = prelude.lines().count();
            prelude.push_str(&rewritten_root.source);
            (prelude, prelude_lines)
        }
    };

    let mut root_source_map = SourceMap::new();
    let root_source_id = root_source_map.add_source(path.display().to_string(), source_raw);
    let mut root_parsed = frontends::parse_source_with_scheme_import_context(
        &root_parse_source,
        flavor,
        scheme_import_context.as_ref(),
    )
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
    });

    Ok((root_parse_source, collect_state.units))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn scheme_module_prefix_imports_compile_without_rewriting_source() {
        let source = "(import (prefix-in mod: \"./math.rss\"))\n(define (entry x) (mod:inc x))\n";
        let path = Path::new("tests/main.scm");
        let options = CompileSourceFileOptions::default()
            .with_module_override_source("./math.rss", "pub fn inc(arg0) { arg0 + 1; }\n");

        let (root_parse_source, units) =
            load_units_for_source_file(path, SourceFlavor::Scheme, source, &options)
                .expect("scheme source with module prefix import should load");

        assert_eq!(root_parse_source, source);
        let root_unit = units.last().expect("root unit should be present");
        assert!(
            root_unit
                .parsed
                .functions
                .iter()
                .any(|func| func.name == "inc"),
            "imported module function should be declared in lowered scheme IR"
        );
    }

    #[test]
    fn scheme_vm_rename_imports_resolve_during_lowering() {
        let source =
            "(require (rename-in \"vm\" (print say-print)))\n(define (entry x) (say-print x))\n";
        let path = Path::new("tests/main.scm");

        let (root_parse_source, units) = load_units_for_source_file(
            path,
            SourceFlavor::Scheme,
            source,
            &CompileSourceFileOptions::default(),
        )
        .expect("scheme vm rename import should load");

        assert_eq!(root_parse_source, source);
        let root_unit = units.last().expect("root unit should be present");
        assert!(
            root_unit
                .parsed
                .functions
                .iter()
                .any(|func| func.name == "print"),
            "renamed vm import should lower to the underlying host function name"
        );
    }

    #[test]
    fn scheme_virtual_host_namespace_imports_resolve_during_lowering() {
        let source = "(import \"http\")\n(define (entry url) (http.get url))\n";
        let path = Path::new("tests/main.scm");

        let (root_parse_source, units) = load_units_for_source_file(
            path,
            SourceFlavor::Scheme,
            source,
            &CompileSourceFileOptions::default(),
        )
        .expect("scheme virtual host namespace import should load");

        assert_eq!(root_parse_source, source);
        let root_unit = units.last().expect("root unit should be present");
        assert!(
            root_unit
                .parsed
                .functions
                .iter()
                .any(|func| func.name == "http::get"),
            "virtual host namespace import should lower to a host call path"
        );
    }
}
