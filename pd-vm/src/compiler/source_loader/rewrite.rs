use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::super::frontends::{SchemeImportContext, is_ident_continue, is_ident_start};
use super::super::{CompileSourceFileOptions, SourceFlavor, SourcePathError};
use super::graph::collect_imported_module_functions;
use super::imports::{
    host_namespace_root_from_spec, is_builtin_host_namespace_spec, is_module_specifier,
    is_valid_ident, is_virtual_host_namespace_spec, resolve_module_path,
};
use super::model::{ImportClause, ImportRewriteResult, ModuleImport, ROOT_HOST_NAMESPACE_SPEC};

struct ImportCallResolution {
    alias_calls: HashMap<String, String>,
    namespace_calls: HashMap<String, HashSet<String>>,
    namespace_prefix_calls: HashMap<String, String>,
}

pub(super) fn build_scheme_import_context(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<SchemeImportContext, SourcePathError> {
    let resolution =
        resolve_import_call_paths(SourceFlavor::Scheme, path, imports, module_exports, options)?;

    let mut declared_functions = HashMap::<String, Option<u8>>::new();
    for (name, arity) in collect_imported_module_functions(path, imports, module_exports, options)?
    {
        declared_functions
            .entry(name)
            .and_modify(|existing| {
                if let Some(existing_arity) = existing {
                    *existing_arity = (*existing_arity).max(arity);
                } else {
                    *existing = Some(arity);
                }
            })
            .or_insert(Some(arity));
    }

    for import in imports {
        if import.spec != ROOT_HOST_NAMESPACE_SPEC {
            continue;
        }
        let ImportClause::Named(named) = &import.clause else {
            continue;
        };
        for binding in named {
            declared_functions
                .entry(binding.imported.clone())
                .or_insert(None);
        }
    }

    let mut declared_functions = declared_functions.into_iter().collect::<Vec<_>>();
    declared_functions.sort_by(|(lhs_name, _), (rhs_name, _)| lhs_name.cmp(rhs_name));

    Ok(SchemeImportContext {
        declared_functions,
        direct_aliases: resolution.alias_calls,
        namespace_imports: resolution.namespace_calls,
        namespace_prefixes: resolution.namespace_prefix_calls,
    })
}

fn resolve_import_call_paths(
    flavor: SourceFlavor,
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<ImportCallResolution, SourcePathError> {
    let mut alias_calls = HashMap::<String, String>::new();
    let mut namespace_calls = HashMap::<String, HashSet<String>>::new();
    let mut namespace_prefix_calls = HashMap::<String, String>::new();
    for import in imports {
        if is_builtin_host_namespace_spec(&import.spec) {
            continue;
        }
        if import.spec == ROOT_HOST_NAMESPACE_SPEC {
            match &import.clause {
                ImportClause::AllPublic => {}
                ImportClause::Named(named) => {
                    for binding in named {
                        if binding.local != binding.imported {
                            alias_calls.insert(binding.local.clone(), binding.imported.clone());
                        }
                    }
                }
                ImportClause::Namespace(_) => {}
                ImportClause::Prefix(_) => {}
            }
            continue;
        }
        if !is_module_specifier(&import.spec) {
            if let Some(host_root) = host_namespace_root_from_spec(&import.spec)
                && is_virtual_host_namespace_spec(&import.spec, options)
                && let Some(host_prefix) = virtual_host_namespace_prefix(flavor, &host_root)
            {
                match &import.clause {
                    ImportClause::AllPublic => {
                        if flavor == SourceFlavor::Scheme
                            && let Some(namespace) = module_default_namespace(&import.spec)
                        {
                            namespace_prefix_calls.insert(namespace, host_prefix.clone());
                        }
                    }
                    ImportClause::Named(named) => {
                        for binding in named {
                            alias_calls.insert(
                                binding.local.clone(),
                                format!("{host_prefix}::{}", binding.imported),
                            );
                        }
                    }
                    ImportClause::Namespace(namespace) => {
                        if flavor == SourceFlavor::Scheme {
                            namespace_prefix_calls.insert(namespace.clone(), host_prefix.clone());
                        }
                    }
                    ImportClause::Prefix(_) => {}
                }
            }
            continue;
        }

        let resolved = resolve_module_path(path, &import.spec, options)?;
        let Some(exports) = module_exports.get(&resolved) else {
            if let Some(host_root) = host_namespace_root_from_spec(&import.spec)
                && is_virtual_host_namespace_spec(&import.spec, options)
                && let Some(host_prefix) = virtual_host_namespace_prefix(flavor, &host_root)
            {
                match &import.clause {
                    ImportClause::AllPublic => {
                        if flavor == SourceFlavor::Scheme
                            && let Some(namespace) = module_default_namespace(&import.spec)
                        {
                            namespace_prefix_calls.insert(namespace, host_prefix.clone());
                        }
                    }
                    ImportClause::Named(named) => {
                        for binding in named {
                            alias_calls.insert(
                                binding.local.clone(),
                                format!("{host_prefix}::{}", binding.imported),
                            );
                        }
                    }
                    ImportClause::Namespace(namespace) => {
                        if flavor == SourceFlavor::Scheme {
                            namespace_prefix_calls.insert(namespace.clone(), host_prefix.clone());
                        }
                    }
                    ImportClause::Prefix(_) => {}
                }
            }
            continue;
        };

        match &import.clause {
            ImportClause::AllPublic => {
                // Bare `use module;` keeps direct calls (`fn_name(...)`) and now also
                // supports namespace-style calls (`module::fn_name(...)`) for ergonomics.
                if let Some(namespace) = module_default_namespace(&import.spec) {
                    let entries = namespace_calls.entry(namespace).or_default();
                    for name in exports.keys() {
                        entries.insert(name.clone());
                    }
                }
            }
            ImportClause::Named(named) => {
                for binding in named {
                    if !exports.contains_key(&binding.imported) {
                        return Err(SourcePathError::InvalidImportSyntax {
                            path: path.to_path_buf(),
                            line: import.line,
                            message: format!(
                                "module '{}' has no public function '{}'",
                                import.spec, binding.imported
                            ),
                        });
                    }
                    if binding.local != binding.imported {
                        alias_calls.insert(binding.local.clone(), binding.imported.clone());
                    }
                }
            }
            ImportClause::Namespace(namespace) => {
                let entries = namespace_calls.entry(namespace.clone()).or_default();
                for name in exports.keys() {
                    entries.insert(name.clone());
                }
            }
            ImportClause::Prefix(prefix) => {
                for name in exports.keys() {
                    alias_calls.insert(format!("{prefix}{name}"), name.clone());
                }
            }
        }
    }

    Ok(ImportCallResolution {
        alias_calls,
        namespace_calls,
        namespace_prefix_calls,
    })
}

fn virtual_host_namespace_prefix(flavor: SourceFlavor, host_root: &str) -> Option<String> {
    match flavor {
        SourceFlavor::RustScript | SourceFlavor::Scheme => Some(host_root.to_string()),
        SourceFlavor::JavaScript | SourceFlavor::Lua => None,
    }
}

pub(super) fn rewrite_imported_call_sites(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<ImportRewriteResult, SourcePathError> {
    let resolution = resolve_import_call_paths(flavor, path, imports, module_exports, options)?;
    let alias_calls = resolution.alias_calls;
    let namespace_calls = resolution.namespace_calls;
    let namespace_prefix_calls = resolution.namespace_prefix_calls;
    let namespace_wildcards = HashSet::<String>::new();
    let prefix_aliases = Vec::<String>::new();

    let rewritten = rewrite_host_namespace_call_paths(source, flavor, &namespace_prefix_calls);

    if alias_calls.is_empty()
        && namespace_calls.is_empty()
        && namespace_wildcards.is_empty()
        && prefix_aliases.is_empty()
    {
        return Ok(ImportRewriteResult { source: rewritten });
    }

    if flavor == SourceFlavor::Scheme {
        Ok(ImportRewriteResult {
            source: rewrite_scheme_call_heads(
                &rewritten,
                &alias_calls,
                &namespace_calls,
                &namespace_wildcards,
                &prefix_aliases,
            ),
        })
    } else {
        Ok(ImportRewriteResult {
            source: rewrite_function_call_paths(
                &rewritten,
                flavor,
                &alias_calls,
                &namespace_calls,
                &namespace_wildcards,
                &prefix_aliases,
            ),
        })
    }
}

fn rewrite_host_namespace_call_paths(
    source: &str,
    flavor: SourceFlavor,
    namespace_prefix_calls: &HashMap<String, String>,
) -> String {
    if namespace_prefix_calls.is_empty() {
        return source.to_string();
    }

    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];

        if let Some(delim) = string_delim {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                string_delim = None;
            }
            i += 1;
            continue;
        }

        if in_line_comment {
            out.push(b as char);
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            out.push(b as char);
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                out.push('/');
                i += 2;
                in_block_comment = false;
                continue;
            }
            i += 1;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            out.push('/');
            out.push('/');
            i += 2;
            in_line_comment = true;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            out.push('/');
            out.push('*');
            i += 2;
            in_block_comment = true;
            continue;
        }

        if b == b'"' || b == b'\'' || b == b'`' {
            out.push(b as char);
            i += 1;
            string_delim = Some(b);
            escaped = false;
            continue;
        }

        if !is_ident_start(b as char) {
            out.push(b as char);
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i] as char) {
            i += 1;
        }
        let ident = &source[start..i];

        if let Some(prefix) = namespace_prefix_calls.get(ident)
            && namespace_call_target_is_function(source, i, flavor)
        {
            out.push_str(prefix);
            continue;
        }

        out.push_str(ident);
    }

    out
}

fn namespace_call_target_is_function(source: &str, index: usize, flavor: SourceFlavor) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = index;
    if !consume_namespace_separator(bytes, &mut cursor, flavor) {
        return false;
    }

    loop {
        while cursor < bytes.len()
            && bytes[cursor].is_ascii_whitespace()
            && bytes[cursor] != b'\n'
            && bytes[cursor] != b'\r'
        {
            cursor += 1;
        }
        if cursor >= bytes.len() || !is_ident_start(bytes[cursor] as char) {
            return false;
        }
        cursor += 1;
        while cursor < bytes.len() && is_ident_continue(bytes[cursor] as char) {
            cursor += 1;
        }

        while cursor < bytes.len()
            && bytes[cursor].is_ascii_whitespace()
            && bytes[cursor] != b'\n'
            && bytes[cursor] != b'\r'
        {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'(' {
            return true;
        }
        if !consume_namespace_separator(bytes, &mut cursor, flavor) {
            return false;
        }
    }
}

fn consume_namespace_separator(bytes: &[u8], cursor: &mut usize, flavor: SourceFlavor) -> bool {
    if *cursor >= bytes.len() {
        return false;
    }
    if flavor == SourceFlavor::RustScript {
        if bytes[*cursor] != b':' {
            return false;
        }
        *cursor += 1;
        while *cursor < bytes.len()
            && bytes[*cursor].is_ascii_whitespace()
            && bytes[*cursor] != b'\n'
            && bytes[*cursor] != b'\r'
        {
            *cursor += 1;
        }
        if *cursor >= bytes.len() || bytes[*cursor] != b':' {
            return false;
        }
        *cursor += 1;
        return true;
    }

    if bytes[*cursor] == b'.' {
        *cursor += 1;
        return true;
    }
    false
}

fn module_default_namespace(spec: &str) -> Option<String> {
    let stem = Path::new(spec).file_stem()?.to_str()?;
    if is_valid_ident(stem) {
        Some(stem.to_string())
    } else {
        None
    }
}

fn rewrite_function_call_paths(
    source: &str,
    flavor: SourceFlavor,
    alias_calls: &HashMap<String, String>,
    namespace_calls: &HashMap<String, HashSet<String>>,
    namespace_wildcards: &HashSet<String>,
    prefix_aliases: &[String],
) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];

        if let Some(delim) = string_delim {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                string_delim = None;
            }
            i += 1;
            continue;
        }

        if in_line_comment {
            out.push(b as char);
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            out.push(b as char);
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                out.push('/');
                i += 2;
                in_block_comment = false;
                continue;
            }
            i += 1;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            out.push('/');
            out.push('/');
            i += 2;
            in_line_comment = true;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            out.push('/');
            out.push('*');
            i += 2;
            in_block_comment = true;
            continue;
        }

        if b == b'"' || b == b'\'' || b == b'`' {
            out.push(b as char);
            i += 1;
            string_delim = Some(b);
            escaped = false;
            continue;
        }

        if is_ident_start(b as char) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &source[start..i];

            let namespace_methods = namespace_calls.get(ident);
            let namespace_wildcard = namespace_wildcards.contains(ident);
            if namespace_methods.is_some() || namespace_wildcard {
                let mut j = i;
                while j < bytes.len()
                    && bytes[j].is_ascii_whitespace()
                    && bytes[j] != b'\n'
                    && bytes[j] != b'\r'
                {
                    j += 1;
                }

                let mut sep_end = None;
                if flavor == SourceFlavor::RustScript {
                    if j < bytes.len() && bytes[j] == b':' {
                        let mut k = j + 1;
                        while k < bytes.len()
                            && bytes[k].is_ascii_whitespace()
                            && bytes[k] != b'\n'
                            && bytes[k] != b'\r'
                        {
                            k += 1;
                        }
                        if k < bytes.len() && bytes[k] == b':' {
                            sep_end = Some(k + 1);
                        }
                    }
                } else if j < bytes.len() && bytes[j] == b'.' {
                    sep_end = Some(j + 1);
                }

                if let Some(mut k) = sep_end {
                    while k < bytes.len()
                        && bytes[k].is_ascii_whitespace()
                        && bytes[k] != b'\n'
                        && bytes[k] != b'\r'
                    {
                        k += 1;
                    }
                    if k < bytes.len() && is_ident_start(bytes[k] as char) {
                        let member_start = k;
                        k += 1;
                        while k < bytes.len() && is_ident_continue(bytes[k] as char) {
                            k += 1;
                        }
                        let member = &source[member_start..k];
                        let mut call_check = k;
                        while call_check < bytes.len()
                            && bytes[call_check].is_ascii_whitespace()
                            && bytes[call_check] != b'\n'
                            && bytes[call_check] != b'\r'
                        {
                            call_check += 1;
                        }
                        if call_check < bytes.len()
                            && bytes[call_check] == b'('
                            && (namespace_wildcard
                                || namespace_methods
                                    .is_some_and(|methods| methods.contains(member)))
                        {
                            out.push_str(member);
                            i = k;
                            continue;
                        }
                    }
                }
            }

            if let Some(target) = alias_calls.get(ident) {
                let mut j = i;
                while j < bytes.len()
                    && bytes[j].is_ascii_whitespace()
                    && bytes[j] != b'\n'
                    && bytes[j] != b'\r'
                {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    out.push_str(target);
                    continue;
                }
            }

            let mut rewritten_by_prefix = false;
            for prefix in prefix_aliases {
                if !ident.starts_with(prefix) {
                    continue;
                }
                let rem = &ident[prefix.len()..];
                if rem.is_empty() || !is_valid_ident(rem) {
                    continue;
                }
                let mut j = i;
                while j < bytes.len()
                    && bytes[j].is_ascii_whitespace()
                    && bytes[j] != b'\n'
                    && bytes[j] != b'\r'
                {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    out.push_str(rem);
                    rewritten_by_prefix = true;
                    break;
                }
            }
            if rewritten_by_prefix {
                continue;
            }

            out.push_str(ident);
            continue;
        }

        out.push(b as char);
        i += 1;
    }

    out
}

fn rewrite_scheme_call_heads(
    source: &str,
    alias_calls: &HashMap<String, String>,
    namespace_calls: &HashMap<String, HashSet<String>>,
    namespace_wildcards: &HashSet<String>,
    prefix_aliases: &[String],
) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            out.push(b as char);
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_string {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if b == b';' {
            in_line_comment = true;
            out.push(';');
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            escaped = false;
            out.push('"');
            i += 1;
            continue;
        }

        if b == b'(' {
            out.push('(');
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                out.push(bytes[i] as char);
                i += 1;
            }

            let symbol_start = i;
            while i < bytes.len() {
                let ch = bytes[i];
                if ch.is_ascii_whitespace() || ch == b'(' || ch == b')' || ch == b';' {
                    break;
                }
                i += 1;
            }
            if i == symbol_start {
                continue;
            }

            let symbol = &source[symbol_start..i];
            if let Some(target) = alias_calls.get(symbol) {
                out.push_str(target);
                continue;
            }

            if let Some((namespace, member)) = symbol.split_once('.')
                && let Some(entries) = namespace_calls.get(namespace)
                && entries.contains(member)
            {
                out.push_str(member);
                continue;
            }

            if let Some((namespace, member)) = symbol.split_once('.')
                && namespace_wildcards.contains(namespace)
                && !member.is_empty()
            {
                out.push_str(member);
                continue;
            }

            let mut rewritten_by_prefix = false;
            for prefix in prefix_aliases {
                if let Some(rem) = symbol.strip_prefix(prefix)
                    && !rem.is_empty()
                {
                    out.push_str(rem);
                    rewritten_by_prefix = true;
                    break;
                }
            }
            if rewritten_by_prefix {
                continue;
            }

            out.push_str(symbol);
            continue;
        }

        out.push(b as char);
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::compiler::CompileSourceFileOptions;
    use crate::compiler::source_loader::model::{ImportClause, ModuleImport, NamedImport};

    #[test]
    fn rustscript_namespace_import_calls_rewrite_to_direct_calls() {
        let source = r#"
string::non_empty("rss");
is_empty("");
"#;
        let path = Path::new("tests/main.rss");
        let imports = vec![
            ModuleImport {
                spec: "strings.rss".to_string(),
                clause: ImportClause::Namespace("string".to_string()),
                line: 1,
            },
            ModuleImport {
                spec: "strings.rss".to_string(),
                clause: ImportClause::Named(vec![NamedImport {
                    imported: "is_empty".to_string(),
                    local: "is_empty".to_string(),
                }]),
                line: 2,
            },
        ];
        let mut module_exports = HashMap::<PathBuf, HashMap<String, u8>>::new();
        module_exports.insert(
            PathBuf::from("tests").join("strings.rss"),
            HashMap::from([("is_empty".to_string(), 1), ("non_empty".to_string(), 1)]),
        );

        let rewritten = rewrite_imported_call_sites(
            source,
            SourceFlavor::RustScript,
            path,
            &imports,
            &module_exports,
            &CompileSourceFileOptions::default(),
        )
        .expect("rewrite should succeed");

        assert_eq!(
            rewritten.source.trim(),
            r#"
non_empty("rss");
is_empty("");
"#
            .trim()
        );
    }

    #[test]
    fn rustscript_all_public_import_namespace_calls_rewrite_to_direct_calls() {
        let source = "runtime::sleep(3);\n";
        let path = Path::new("tests/main.rss");
        let imports = vec![ModuleImport {
            spec: "runtime.rss".to_string(),
            clause: ImportClause::AllPublic,
            line: 1,
        }];
        let mut module_exports = HashMap::<PathBuf, HashMap<String, u8>>::new();
        module_exports.insert(
            PathBuf::from("tests").join("runtime.rss"),
            HashMap::from([("sleep".to_string(), 1)]),
        );

        let rewritten = rewrite_imported_call_sites(
            source,
            SourceFlavor::RustScript,
            path,
            &imports,
            &module_exports,
            &CompileSourceFileOptions::default(),
        )
        .expect("rewrite should succeed");

        assert_eq!(rewritten.source.trim(), "sleep(3);");
    }

    #[test]
    fn javascript_virtual_host_namespace_imports_keep_namespace_calls() {
        let source = "runtime.sleep(3);\n";
        let path = Path::new("tests/main.js");
        let imports = vec![ModuleImport {
            spec: "runtime".to_string(),
            clause: ImportClause::Namespace("runtime".to_string()),
            line: 1,
        }];

        let rewritten = rewrite_imported_call_sites(
            source,
            SourceFlavor::JavaScript,
            path,
            &imports,
            &HashMap::new(),
            &CompileSourceFileOptions::default(),
        )
        .expect("rewrite should succeed");

        assert_eq!(rewritten.source.trim(), "runtime.sleep(3);");
    }
}
