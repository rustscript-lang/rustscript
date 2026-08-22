use std::path::{Component, Path, PathBuf};

use crate::builtins::is_builtin_namespace;

use super::super::frontends::{is_ident_continue, is_ident_start};
use super::super::modules::{UseDecl, use_path_to_spec};
use super::super::{
    CompileSourceFileOptions, SourceError, SourceFlavor, SourcePathError, frontends,
};
use super::model::ModuleImport;

pub(super) fn parse_module_imports(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
    options: &CompileSourceFileOptions,
) -> Result<Vec<ModuleImport>, SourcePathError> {
    scan_module_imports(source, flavor, path, options).map(|(imports, _)| imports)
}

/// Scan the module imports of one source.
///
/// For RustScript this parses the source once with the real frontend parser
/// and consumes the structured `use` declaration nodes, so import discovery
/// shares the parser's spans, clauses, and syntax validation instead of
/// treating line-prefix stripping as the authoritative import parser. The
/// paired [`UseDecl`] list is returned alongside the legacy `ModuleImport`
/// list so graph construction can preserve spans. Other flavors keep their
/// plugin-based discovery and contribute no structured declarations.
pub(super) fn scan_module_imports(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
    options: &CompileSourceFileOptions,
) -> Result<(Vec<ModuleImport>, Vec<UseDecl>), SourcePathError> {
    match flavor {
        SourceFlavor::RustScript => {
            let decls = parse_rustscript_use_declarations(source, options)?;
            let imports = use_declarations_to_module_imports(path, &decls)?;
            Ok((imports, decls))
        }
        SourceFlavor::JavaScript | SourceFlavor::Lua => {
            let imports = options
                .source_plugin_for_flavor(flavor)
                .ok_or(SourcePathError::MissingFrontendPlugin(flavor))?
                .parse_module_imports(source, path)?;
            Ok((imports, Vec::new()))
        }
    }
}

/// Parse all `use` directives of a RustScript source into structured nodes.
///
/// The whole source is parsed with the real frontend parser (with implicit
/// externs enabled and file-path host aliases recorded) so that discovery
/// tolerates calls to functions imported from other modules, which the
/// loader's semantic resolution pass resolves later.
fn parse_rustscript_use_declarations(
    source: &str,
    options: &CompileSourceFileOptions,
) -> Result<Vec<UseDecl>, SourcePathError> {
    let ir = frontends::parse_source_for_import_scan(source, options)
        .map_err(|err| SourcePathError::Source(SourceError::Parse(err)))?;
    Ok(ir.use_declarations)
}

fn use_declarations_to_module_imports(
    path: &Path,
    decls: &[UseDecl],
) -> Result<Vec<ModuleImport>, SourcePathError> {
    decls
        .iter()
        .map(|decl| {
            let spec = use_path_to_spec(path, decl.line, &decl.path)?;
            Ok(ModuleImport {
                spec,
                clause: decl.clause.clone(),
                line: decl.line,
            })
        })
        .collect()
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
        return Ok(module_identity(path));
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
        return Ok(module_identity(path));
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
    Ok(module_identity(path))
}

/// Resolve the module identity for a normalized path.
///
/// Files that exist on disk use their canonical path so that lexically
/// distinct but equivalent paths (`.`, `..`, symlinks) collapse to one
/// module identity for `seen`/`visiting`/exports/overrides. Paths that do not
/// exist on disk (virtual source overrides, in-memory entry points) keep the
/// normalized lexical path as their explicit virtual identity.
pub(super) fn module_identity(path: PathBuf) -> PathBuf {
    let normalized = normalize_module_path(path);
    if normalized.is_file()
        && let Ok(canonical) = normalized.canonicalize()
    {
        return canonical;
    }
    normalized
}

fn normalize_module_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::ParentDir) | None => normalized.push(component.as_os_str()),
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::CurDir) => {
                    unreachable!("normalized paths omit current-dir components")
                }
            },
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// Prepare source text for the compile parse.
///
/// RustScript no longer strips `use` directives: the parser consumes every
/// directive into a structured node (host-namespace forms keep their existing
/// dedicated handling), so line-prefix stripping is no longer an authority
/// for import discovery. Other flavors keep plugin-defined stripping.
pub(super) fn strip_import_directives(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    match flavor {
        SourceFlavor::RustScript => Ok(source.to_string()),
        SourceFlavor::JavaScript | SourceFlavor::Lua => Ok(options
            .source_plugin_for_flavor(flavor)
            .ok_or(SourcePathError::MissingFrontendPlugin(flavor))?
            .strip_import_directives(source)),
    }
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

#[cfg(test)]
mod tests {
    use super::super::super::modules::UsePathSegment;
    use super::super::SourceFlavor;
    use super::super::model::ImportClause;
    use super::{
        module_identity, normalize_module_path, parse_module_imports, scan_module_imports,
    };
    use std::path::PathBuf;

    #[test]
    fn normalize_module_path_preserves_unmatched_parent_components() {
        assert_eq!(
            normalize_module_path(PathBuf::from("../foo/../../bar")),
            PathBuf::from("../../bar")
        );
        assert_eq!(
            normalize_module_path(PathBuf::from("foo/../../../bar")),
            PathBuf::from("../../bar")
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalize_module_path_does_not_escape_absolute_root() {
        assert_eq!(
            normalize_module_path(PathBuf::from("/foo/../../bar")),
            PathBuf::from("/bar")
        );
    }

    #[test]
    fn module_identity_keeps_normalized_virtual_path_for_missing_files() {
        assert_eq!(
            module_identity(PathBuf::from("/no/such/dir/../virtual/nested.rss")),
            PathBuf::from("/no/such/virtual/nested.rss")
        );
    }

    #[test]
    fn module_identity_uses_canonical_path_for_existing_files() {
        let unique = format!(
            "pd-vm-module-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let module = root.join("a.rss");
        std::fs::write(&module, "pub fn value() -> int { 1 }\n").expect("module should write");

        let via_dot = module_identity(root.join("./a.rss"));
        let via_parent = module_identity(root.join("sub/../a.rss"));
        let canonical = module.canonicalize().expect("module should canonicalize");

        assert_eq!(via_dot, canonical);
        assert_eq!(via_parent, canonical);
        assert_eq!(via_dot, via_parent);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn structured_scan_preserves_spans_clauses_and_lines() {
        let source = "use self::nested as nested;\nuse sibling::{value as v, other};\nuse super::shared;\nuse io;\n";
        let path = PathBuf::from("/root/pkg/main.rss");
        let (imports, decls) =
            scan_module_imports(source, SourceFlavor::RustScript, &path, &Default::default())
                .expect("scan should succeed");

        assert_eq!(imports.len(), 4);
        assert_eq!(imports[0].spec, "./nested.rss");
        assert_eq!(imports[1].spec, "sibling.rss");
        assert_eq!(imports[2].spec, "../shared.rss");
        assert_eq!(imports[3].spec, "io.rss");

        assert_eq!(decls.len(), 4);
        assert_eq!(
            decls[0].path,
            vec![
                UsePathSegment::Self_,
                UsePathSegment::Ident("nested".to_string())
            ]
        );
        assert!(matches!(&decls[0].clause, ImportClause::Namespace(alias) if alias == "nested"));
        assert_eq!(decls[0].line, 1);
        assert_eq!(decls[1].line, 2);
        assert!(
            decls[0].span.lo < decls[0].span.hi,
            "span must cover the directive"
        );
        assert!(matches!(&decls[1].clause, ImportClause::Named(named) if named.len() == 2));
        assert!(matches!(&decls[3].clause, ImportClause::AllPublic));
    }

    #[test]
    fn structured_scan_handles_wildcard_and_alias_forms() {
        let source = "use a::b::*;\nuse c::d::{x};\nuse e as f;\n";
        let path = PathBuf::from("/root/main.rss");
        let (imports, decls) =
            scan_module_imports(source, SourceFlavor::RustScript, &path, &Default::default())
                .expect("scan should succeed");

        assert_eq!(imports[0].spec, "a/b.rss");
        assert!(matches!(imports[0].clause, ImportClause::AllPublic));
        assert_eq!(imports[1].spec, "c/d.rss");
        assert!(matches!(&imports[1].clause, ImportClause::Named(named) if named.len() == 1));
        assert_eq!(imports[2].spec, "e.rss");
        assert!(matches!(&imports[2].clause, ImportClause::Namespace(alias) if alias == "f"));
        assert_eq!(decls[1].path.len(), 2);
    }

    #[test]
    fn structured_scan_ignores_comment_text_and_parses_multiline_aliases() {
        let source = "/*\nuse self::missing;\n*/\n\tuse self::module::{\n    value /* comment */ as answer,\n}; // trailing comment\n";
        let path = PathBuf::from("/root/main.rss");
        let (imports, decls) =
            scan_module_imports(source, SourceFlavor::RustScript, &path, &Default::default())
                .expect("comment and multiline syntax should scan");

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].spec, "./module.rss");
        assert_eq!(imports[0].line, 4);
        assert!(matches!(&imports[0].clause, ImportClause::Named(named)
            if named.len() == 1
                && named[0].imported == "value"
                && named[0].local == "answer"));
        assert_eq!(decls.len(), 1);
    }

    #[test]
    fn structured_scan_rejects_import_keyword() {
        let source = "import \"./module.rss\";\n";
        let path = PathBuf::from("/root/main.rss");
        let err =
            parse_module_imports(source, SourceFlavor::RustScript, &path, &Default::default())
                .expect_err("import keyword should be rejected");
        assert!(
            err.to_string().contains("expected ';' after expression"),
            "unexpected parser diagnostic: {err}"
        );
    }

    #[test]
    fn structured_scan_rejects_crate_paths() {
        let source = "use crate::x;\n";
        let path = PathBuf::from("/root/main.rss");
        let err =
            parse_module_imports(source, SourceFlavor::RustScript, &path, &Default::default())
                .expect_err("crate:: paths should be rejected");
        assert!(
            err.to_string().contains("crate:: paths are not supported"),
            "unexpected error: {err}"
        );
    }
}
