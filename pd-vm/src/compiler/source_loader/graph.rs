use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::compiler::source_map::SourceMap;

use super::super::{
    CompileSourceFileOptions, SourceError, SourceFlavor, SourcePathError, frontends,
    linker::{ParsedUnit, sanitize_scope_prefix},
};
use super::imports::{
    is_builtin_host_namespace_spec, is_module_specifier, is_virtual_host_namespace_spec,
    parse_module_imports, resolve_module_path, should_treat_missing_module_as_host_namespace,
    strip_import_directives,
};
use super::model::{ImportClause, ModuleCollectState, ModuleImport};

pub(super) fn collect_module_units(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    state: &mut ModuleCollectState,
) -> Result<(), SourcePathError> {
    let imports = parse_module_imports(source, flavor, path)?;
    for import in imports {
        let spec = import.spec;
        if is_builtin_host_namespace_spec(&spec) {
            continue;
        }
        if !is_module_specifier(&spec) {
            continue;
        }
        let resolved = resolve_module_path(path, &spec, options)?;
        let key = resolved.clone();
        if key == path && is_virtual_host_namespace_spec(&spec, options) {
            // `use io;` / `use re;` inside files named `io.rss` / `re.rss` should
            // keep behaving as host-namespace imports instead of self-module cycles.
            continue;
        }
        if state.visiting.contains(&key) {
            return Err(SourcePathError::ImportCycle(key));
        }
        if state.seen.contains(&key) {
            continue;
        }

        let module_source_raw =
            if let Some(source) = module_source_override(options, &spec, &resolved) {
                source.to_string()
            } else {
                match std::fs::read_to_string(&resolved) {
                    Ok(source) => source,
                    Err(err) => {
                        if should_treat_missing_module_as_host_namespace(&spec, options, &err) {
                            continue;
                        }
                        return Err(SourcePathError::Io(err));
                    }
                }
            };
        state.visiting.push(key.clone());
        collect_module_units(
            &resolved,
            &module_source_raw,
            SourceFlavor::RustScript,
            options,
            state,
        )?;
        state.visiting.pop();

        let module_source = strip_import_directives(&module_source_raw, SourceFlavor::RustScript);
        let mut module_source_map = SourceMap::new();
        let module_source_id =
            module_source_map.add_source(resolved.display().to_string(), module_source.clone());
        let parsed = frontends::parse_source(&module_source, SourceFlavor::RustScript)
            .map_err(|err| {
                SourceError::Parse(
                    err.with_line_span_from_source(&module_source_map, module_source_id),
                )
            })
            .map_err(SourcePathError::Source)?;
        let exports = parsed
            .functions
            .iter()
            .filter(|func| func.exported)
            .map(|func| (func.name.clone(), func.arity))
            .collect::<HashMap<_, _>>();
        state.units.push(ParsedUnit {
            parsed,
            scope_prefix: Some(sanitize_scope_prefix(&resolved)),
        });
        state.module_exports.insert(key.clone(), exports);
        state.seen.insert(key);
    }
    Ok(())
}

fn module_source_override<'a>(
    options: &'a CompileSourceFileOptions,
    spec: &str,
    resolved_path: &Path,
) -> Option<&'a str> {
    options.module_override_source(spec).or_else(|| {
        options.module_override_source(&resolved_path.to_string_lossy().replace('\\', "/"))
    })
}

pub(super) fn build_rustscript_import_prelude(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    let declared = collect_imported_module_functions(path, imports, module_exports, options)?;
    let mut prelude = String::new();
    for (name, arity) in declared {
        let args = (0..arity)
            .map(|idx| format!("arg{idx}"))
            .collect::<Vec<_>>()
            .join(", ");
        prelude.push_str(&format!("pub fn {name}({args});\n"));
    }
    Ok(prelude)
}

pub(super) fn build_scheme_import_prelude(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    let declared = collect_imported_module_functions(path, imports, module_exports, options)?;
    let mut prelude = String::new();
    for (name, arity) in declared {
        let args = (0..arity)
            .map(|idx| format!("arg{idx}"))
            .collect::<Vec<_>>()
            .join(" ");
        prelude.push_str(&format!("(declare ({name} {args}))\n"));
    }
    Ok(prelude)
}

fn collect_imported_module_functions(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<Vec<(String, u8)>, SourcePathError> {
    let mut imported_functions = HashMap::<String, u8>::new();

    for import in imports {
        if is_builtin_host_namespace_spec(&import.spec) {
            continue;
        }
        if !is_module_specifier(&import.spec) {
            continue;
        }

        let resolved = resolve_module_path(path, &import.spec, options)?;
        let Some(exports) = module_exports.get(&resolved) else {
            if is_virtual_host_namespace_spec(&import.spec, options) {
                continue;
            }
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line: import.line,
                message: format!("module '{}' did not load", import.spec),
            });
        };

        match &import.clause {
            ImportClause::AllPublic | ImportClause::Namespace(_) | ImportClause::Prefix(_) => {
                for (name, arity) in exports {
                    imported_functions
                        .entry(name.clone())
                        .and_modify(|existing| *existing = (*existing).max(*arity))
                        .or_insert(*arity);
                }
            }
            ImportClause::Named(named) => {
                for binding in named {
                    let arity = exports.get(&binding.imported).copied().ok_or_else(|| {
                        SourcePathError::InvalidImportSyntax {
                            path: path.to_path_buf(),
                            line: import.line,
                            message: format!(
                                "module '{}' has no public function '{}'",
                                import.spec, binding.imported
                            ),
                        }
                    })?;
                    imported_functions
                        .entry(binding.imported.clone())
                        .and_modify(|existing| *existing = (*existing).max(arity))
                        .or_insert(arity);
                }
            }
        }
    }

    let mut declared = imported_functions.into_iter().collect::<Vec<_>>();
    declared.sort_by(|(lhs_name, _), (rhs_name, _)| lhs_name.cmp(rhs_name));
    Ok(declared)
}
