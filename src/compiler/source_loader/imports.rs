use std::path::{Path, PathBuf};

use crate::builtins::is_builtin_namespace;

use super::super::frontends::{is_ident_continue, is_ident_start};
use super::super::{CompileSourceFileOptions, SourceFlavor, SourcePathError};
use super::model::{ImportClause, ModuleImport, NamedImport};

pub(super) fn parse_module_imports(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
    options: &CompileSourceFileOptions,
) -> Result<Vec<ModuleImport>, SourcePathError> {
    match flavor {
        SourceFlavor::RustScript => parse_rustscript_imports(source, path),
        SourceFlavor::JavaScript | SourceFlavor::Lua => options
            .source_plugin_for_flavor(flavor)
            .ok_or(SourcePathError::MissingFrontendPlugin(flavor))?
            .parse_module_imports(source, path),
    }
}

fn parse_rustscript_imports(
    source: &str,
    path: &Path,
) -> Result<Vec<ModuleImport>, SourcePathError> {
    let mut imports = Vec::new();
    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.starts_with("import ") {
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line: line_no,
                message: "RustScript uses 'use', not 'import'".to_string(),
            });
        }
        if !line.starts_with("use ") {
            continue;
        }
        let tail = line["use ".len()..].trim();
        let (spec, clause) = parse_rustscript_use(path, line_no, tail)?;
        imports.push(ModuleImport {
            spec,
            clause,
            line: line_no,
        });
    }
    Ok(imports)
}

fn parse_rustscript_use(
    path: &Path,
    line: usize,
    tail: &str,
) -> Result<(String, ImportClause), SourcePathError> {
    let Some((directive_body, _)) = tail.split_once(';') else {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "expected ';' at end of use directive".to_string(),
        });
    };
    let directive_body = directive_body.trim();
    if directive_body.is_empty() {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "expected module path after 'use'".to_string(),
        });
    }

    if let Some(module_path) = directive_body.strip_suffix("::*") {
        let spec = rustscript_use_module_to_spec(path, line, module_path.trim())?;
        return Ok((spec, ImportClause::AllPublic));
    }

    if let Some(open_idx) = directive_body.find("::{") {
        if !directive_body.ends_with('}') {
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line,
                message: "expected '}' to close use list".to_string(),
            });
        }
        let module_path = directive_body[..open_idx].trim();
        let spec = rustscript_use_module_to_spec(path, line, module_path)?;
        let inner = directive_body[open_idx + 3..directive_body.len() - 1].trim();
        if inner == "*" {
            return Ok((spec, ImportClause::AllPublic));
        }
        if inner.is_empty() {
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line,
                message: "use list requires at least one symbol".to_string(),
            });
        }
        let named =
            parse_named_imports(inner).ok_or_else(|| SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line,
                message:
                    "invalid use list; expected comma-separated names with optional 'as' aliases"
                        .to_string(),
            })?;
        if named.is_empty() {
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line,
                message: "use list requires at least one symbol".to_string(),
            });
        }
        return Ok((spec, ImportClause::Named(named)));
    }

    if let Some((module_path, alias)) = directive_body.rsplit_once(" as ") {
        let spec = rustscript_use_module_to_spec(path, line, module_path.trim())?;
        let alias = alias.trim();
        if !is_valid_ident(alias) {
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line,
                message: "invalid namespace alias in use directive".to_string(),
            });
        }
        return Ok((spec, ImportClause::Namespace(alias.to_string())));
    }

    let spec = rustscript_use_module_to_spec(path, line, directive_body)?;
    Ok((spec, ImportClause::AllPublic))
}

fn rustscript_use_module_to_spec(
    path: &Path,
    line: usize,
    module_path: &str,
) -> Result<String, SourcePathError> {
    if module_path.is_empty() {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "expected module path after 'use'".to_string(),
        });
    }
    let segments = module_path
        .split("::")
        .map(|segment| segment.trim())
        .collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "invalid module path in use directive".to_string(),
        });
    }

    let mut path_prefix = PathBuf::new();
    let mut cursor = 0usize;
    while cursor < segments.len() {
        match segments[cursor] {
            "self" => cursor += 1,
            "super" => {
                path_prefix.push("..");
                cursor += 1;
            }
            "crate" => {
                return Err(SourcePathError::InvalidImportSyntax {
                    path: path.to_path_buf(),
                    line,
                    message: "crate:: paths are not supported; use relative module paths"
                        .to_string(),
                });
            }
            _ => break,
        }
    }

    if cursor >= segments.len() {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "expected module name after path qualifiers".to_string(),
        });
    }

    for segment in &segments[cursor..] {
        if !is_valid_ident(segment) {
            return Err(SourcePathError::InvalidImportSyntax {
                path: path.to_path_buf(),
                line,
                message: format!("invalid module path segment '{segment}' in use directive"),
            });
        }
        path_prefix.push(segment);
    }

    let mut spec = path_prefix.to_string_lossy().replace('\\', "/");
    if spec.is_empty() {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "expected module path after 'use'".to_string(),
        });
    }
    if !spec.ends_with(".rss") {
        spec.push_str(".rss");
    }
    Ok(spec)
}

fn parse_named_imports(input: &str) -> Option<Vec<NamedImport>> {
    let mut named = Vec::new();
    for part in input.split(',') {
        let entry = part.trim();
        if entry.is_empty() {
            continue;
        }

        if let Some((imported, local)) = entry.split_once(" as ") {
            let imported = imported.trim();
            let local = local.trim();
            if !is_valid_ident(imported) || !is_valid_ident(local) {
                return None;
            }
            named.push(NamedImport {
                imported: imported.to_string(),
                local: local.to_string(),
            });
            continue;
        }

        if !is_valid_ident(entry) {
            return None;
        }
        named.push(NamedImport {
            imported: entry.to_string(),
            local: entry.to_string(),
        });
    }

    Some(named)
}

pub(super) fn is_valid_ident(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

pub(super) fn is_module_specifier(spec: &str) -> bool {
    spec.ends_with(".rss")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
}

pub(super) fn resolve_module_path(
    base_path: &Path,
    spec: &str,
    options: &CompileSourceFileOptions,
) -> Result<PathBuf, SourcePathError> {
    if let Some(override_path) = options.module_override_path(spec) {
        let path = if override_path.is_absolute() {
            override_path.to_path_buf()
        } else {
            let parent = base_path
                .parent()
                .ok_or_else(|| SourcePathError::ImportWithoutParent(base_path.to_path_buf()))?;
            parent.join(override_path)
        };
        if path.extension().and_then(|value| value.to_str()) != Some("rss") {
            return Err(SourcePathError::NonRustScriptModule(path));
        }
        return Ok(path);
    }
    if options.module_override_source(spec).is_some() {
        let parent = base_path
            .parent()
            .ok_or_else(|| SourcePathError::ImportWithoutParent(base_path.to_path_buf()))?;
        let mut path = if Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            parent.join(spec)
        };
        if path.extension().is_none() {
            path.set_extension("rss");
        }
        if path.extension().and_then(|value| value.to_str()) != Some("rss") {
            return Err(SourcePathError::NonRustScriptModule(path));
        }
        return Ok(path);
    }

    let parent = base_path
        .parent()
        .ok_or_else(|| SourcePathError::ImportWithoutParent(base_path.to_path_buf()))?;
    let mut path = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        parent.join(spec)
    };
    if path.extension().is_none() {
        path.set_extension("rss");
    }
    if path.extension().and_then(|value| value.to_str()) != Some("rss") {
        return Err(SourcePathError::NonRustScriptModule(path));
    }
    Ok(path)
}

pub(super) fn strip_import_directives(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    let stripped = match flavor {
        SourceFlavor::RustScript => source
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("use ")
                    && !is_direct_host_namespace_use_directive_line(line.trim_start())
                    && !is_builtin_namespace_use_directive_line(line.trim_start())
                {
                    String::new()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        SourceFlavor::JavaScript | SourceFlavor::Lua => options
            .source_plugin_for_flavor(flavor)
            .ok_or(SourcePathError::MissingFrontendPlugin(flavor))?
            .strip_import_directives(source),
    };
    Ok(stripped)
}

fn is_direct_host_namespace_use_directive_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with("use ") {
        return false;
    }
    let Some((directive_body, _)) = trimmed["use ".len()..].split_once(';') else {
        return false;
    };
    let directive_body = directive_body.trim();
    if directive_body.contains("::{") || directive_body.ends_with("::*") {
        return false;
    }
    if let Some((namespace, alias)) = directive_body.split_once(" as ") {
        return is_virtual_host_namespace_spec(
            namespace.trim(),
            &CompileSourceFileOptions::default(),
        ) && is_valid_ident(alias.trim());
    }
    is_virtual_host_namespace_spec(directive_body, &CompileSourceFileOptions::default())
}

fn is_builtin_namespace_use_directive_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with("use ") {
        return false;
    }
    let Some((directive_body, _)) = trimmed["use ".len()..].split_once(';') else {
        return false;
    };
    let directive_body = directive_body.trim();
    if let Some((namespace, _alias)) = directive_body.split_once(" as ") {
        return is_builtin_namespace(namespace.trim());
    }
    is_builtin_namespace(directive_body)
}

pub(super) fn host_namespace_root_from_spec(spec: &str) -> Option<String> {
    if spec.contains('/') {
        return None;
    }
    let stem = Path::new(spec).file_stem()?.to_str()?;
    if !is_valid_ident(stem) {
        return None;
    }
    Some(stem.to_string())
}

pub(super) fn is_virtual_host_namespace_spec(
    spec: &str,
    options: &CompileSourceFileOptions,
) -> bool {
    options.module_override_path(spec).is_none()
        && options.module_override_source(spec).is_none()
        && host_namespace_root_from_spec(spec).is_some()
}

pub(super) fn is_builtin_host_namespace_spec(spec: &str) -> bool {
    host_namespace_root_from_spec(spec)
        .as_deref()
        .is_some_and(is_builtin_namespace)
}

pub(super) fn should_treat_missing_module_as_host_namespace(
    spec: &str,
    options: &CompileSourceFileOptions,
    err: &std::io::Error,
) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::Unsupported
    ) && is_virtual_host_namespace_spec(spec, options)
}
