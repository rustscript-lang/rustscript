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
use super::line_map::remap_frontend_ir_source_metadata;
use super::model::{ExportedFunctionSignature, ImportClause, ModuleCollectState, ModuleImport};
use super::rewrite::rewrite_imported_call_sites;

pub(super) fn collect_module_units(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    state: &mut ModuleCollectState,
) -> Result<(), SourcePathError> {
    let imports = parse_module_imports(source, flavor, path, options)?;
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

        let module_imports = parse_module_imports(
            &module_source_raw,
            SourceFlavor::RustScript,
            &resolved,
            options,
        )?;
        let module_source =
            strip_import_directives(&module_source_raw, SourceFlavor::RustScript, options)?;
        let rewritten_module = rewrite_imported_call_sites(
            &module_source,
            SourceFlavor::RustScript,
            &resolved,
            &module_imports,
            &state.module_exports,
            options,
        )?;
        let mut module_parse_source = build_rustscript_import_prelude(
            &resolved,
            &module_imports,
            &state.module_exports,
            options,
        )?;
        let module_prelude_lines = module_parse_source.lines().count();
        module_parse_source.push_str(&rewritten_module.source);

        let mut module_source_map = SourceMap::new();
        let module_source_id = module_source_map
            .add_source(resolved.display().to_string(), module_source_raw.as_str());
        let mut parsed =
            frontends::parse_source(&module_parse_source, SourceFlavor::RustScript, options)
                .map_err(|mut err| {
                    if module_prelude_lines > 0 {
                        err.line = err.line.saturating_sub(module_prelude_lines).max(1);
                        err.span = None;
                    }
                    let mut parse =
                        err.with_line_span_from_source(&module_source_map, module_source_id);
                    parse.message = format!("{}: {}", resolved.display(), parse.message);
                    SourceError::Parse(parse)
                })?;
        if module_prelude_lines > 0 {
            remap_frontend_ir_source_metadata(
                &mut parsed,
                &module_parse_source,
                &module_source_raw,
                module_source_id,
                module_prelude_lines,
            );
        }
        let exports = parsed
            .functions
            .iter()
            .filter(|func| func.exported)
            .map(|func| {
                (
                    func.name.clone(),
                    ExportedFunctionSignature {
                        arity: func.arity,
                        type_params: func.type_params.clone(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        state.units.push(ParsedUnit {
            parsed,
            scope_prefix: Some(sanitize_scope_prefix(&resolved)),
            source_name: resolved.display().to_string(),
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
    module_exports: &HashMap<PathBuf, HashMap<String, ExportedFunctionSignature>>,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    let declared = collect_imported_module_functions(path, imports, module_exports, options)?;
    let mut prelude = String::new();
    for (name, signature) in declared {
        let type_params = if signature.type_params.is_empty() {
            String::new()
        } else {
            format!("<{}>", signature.type_params.join(", "))
        };
        let args = (0..signature.arity)
            .map(|idx| format!("arg{idx}"))
            .collect::<Vec<_>>()
            .join(", ");
        prelude.push_str(&format!("fn {name}{type_params}({args});\n"));
    }
    Ok(prelude)
}

pub(super) fn collect_imported_module_functions(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, ExportedFunctionSignature>>,
    options: &CompileSourceFileOptions,
) -> Result<Vec<(String, ExportedFunctionSignature)>, SourcePathError> {
    let mut imported_functions = HashMap::<String, ExportedFunctionSignature>::new();

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
                for (name, signature) in exports {
                    merge_imported_function_signature(
                        &mut imported_functions,
                        name,
                        signature,
                        path,
                        import.line,
                    )?;
                }
            }
            ImportClause::Named(named) => {
                for binding in named {
                    let signature = exports.get(&binding.imported).cloned().ok_or_else(|| {
                        SourcePathError::InvalidImportSyntax {
                            path: path.to_path_buf(),
                            line: import.line,
                            message: format!(
                                "module '{}' has no public function '{}'",
                                import.spec, binding.imported
                            ),
                        }
                    })?;
                    merge_imported_function_signature(
                        &mut imported_functions,
                        &binding.imported,
                        &signature,
                        path,
                        import.line,
                    )?;
                }
            }
        }
    }

    let mut declared = imported_functions.into_iter().collect::<Vec<_>>();
    declared.sort_by(|(lhs_name, _), (rhs_name, _)| lhs_name.cmp(rhs_name));
    Ok(declared)
}

fn merge_imported_function_signature(
    imported_functions: &mut HashMap<String, ExportedFunctionSignature>,
    name: &str,
    signature: &ExportedFunctionSignature,
    path: &Path,
    line: usize,
) -> Result<(), SourcePathError> {
    if let Some(existing) = imported_functions.get_mut(name) {
        existing.arity = existing.arity.max(signature.arity);
        if existing.type_params != signature.type_params {
            if existing.type_params.is_empty() {
                existing.type_params = signature.type_params.clone();
            } else if !signature.type_params.is_empty() {
                return Err(SourcePathError::InvalidImportSyntax {
                    path: path.to_path_buf(),
                    line,
                    message: format!(
                        "function '{name}' declared with conflicting type parameters across imported modules"
                    ),
                });
            }
        }
        return Ok(());
    }

    imported_functions.insert(name.to_string(), signature.clone());
    Ok(())
}
