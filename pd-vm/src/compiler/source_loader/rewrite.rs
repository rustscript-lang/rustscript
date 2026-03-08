use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::super::frontends::{is_ident_continue, is_ident_start};
use super::super::{CompileSourceFileOptions, SourceFlavor, SourcePathError};
use super::imports::{
    host_namespace_root_from_spec, is_builtin_host_namespace_spec, is_module_specifier,
    is_valid_ident, is_virtual_host_namespace_spec, resolve_module_path,
};
use super::model::{ImportClause, ImportRewriteResult, ModuleImport, VM_HOST_NAMESPACE_SPEC};

pub(super) fn rewrite_imported_call_sites(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<ImportRewriteResult, SourcePathError> {
    let mut alias_calls = HashMap::<String, String>::new();
    let mut namespace_calls = HashMap::<String, HashSet<String>>::new();
    let mut namespace_prefix_calls = HashMap::<String, String>::new();
    let namespace_wildcards = HashSet::<String>::new();
    let mut prefix_aliases = Vec::<String>::new();
    let mut requires_vm_namespace = false;

    for import in imports {
        if is_builtin_host_namespace_spec(&import.spec) {
            continue;
        }
        if import.spec == VM_HOST_NAMESPACE_SPEC {
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
            continue;
        }

        let resolved = resolve_module_path(path, &import.spec, options)?;
        let Some(exports) = module_exports.get(&resolved) else {
            if let Some(host_root) = host_namespace_root_from_spec(&import.spec)
                && is_virtual_host_namespace_spec(&import.spec, options)
            {
                let vm_prefix = format!("vm::{host_root}");
                match &import.clause {
                    ImportClause::AllPublic => {
                        if let Some(namespace) = module_default_namespace(&import.spec) {
                            namespace_prefix_calls.insert(namespace, vm_prefix.clone());
                            requires_vm_namespace = true;
                        }
                    }
                    ImportClause::Named(named) => {
                        for binding in named {
                            alias_calls.insert(
                                binding.local.clone(),
                                format!("{vm_prefix}::{}", binding.imported),
                            );
                        }
                        if !named.is_empty() {
                            requires_vm_namespace = true;
                        }
                    }
                    ImportClause::Namespace(namespace) => {
                        namespace_prefix_calls.insert(namespace.clone(), vm_prefix.clone());
                        requires_vm_namespace = true;
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

    prefix_aliases.sort();
    prefix_aliases.dedup();

    let rewritten = rewrite_host_namespace_call_paths(source, flavor, &namespace_prefix_calls);

    if alias_calls.is_empty()
        && namespace_calls.is_empty()
        && namespace_wildcards.is_empty()
        && prefix_aliases.is_empty()
    {
        return Ok(ImportRewriteResult {
            source: rewritten,
            requires_vm_namespace,
        });
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
            requires_vm_namespace,
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
            requires_vm_namespace,
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
