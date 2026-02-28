use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::frontends::{is_ident_continue, is_ident_start};
use super::{
    SourceError, SourceFlavor, SourcePathError, frontends,
    linker::{ParsedUnit, sanitize_scope_prefix},
};

#[derive(Clone, Debug)]
struct NamedImport {
    imported: String,
    local: String,
}

#[derive(Clone, Debug)]
enum ImportClause {
    AllPublic,
    Named(Vec<NamedImport>),
    Namespace(String),
    Prefix(String),
}

#[derive(Clone, Debug)]
struct ModuleImport {
    spec: String,
    clause: ImportClause,
    line: usize,
}

const VM_HOST_NAMESPACE_SPEC: &str = "vm";

pub(super) fn load_units_for_source_file(
    path: &Path,
    flavor: SourceFlavor,
    source_raw: &str,
) -> Result<(String, Vec<ParsedUnit>), SourcePathError> {
    let root_imports = parse_module_imports(source_raw, flavor, path)?;
    let source = strip_import_directives(source_raw, flavor);

    let mut units = Vec::new();
    let mut visiting = vec![path.to_path_buf()];
    let mut seen = HashSet::new();
    let mut module_exports = HashMap::<PathBuf, HashMap<String, u8>>::new();
    collect_module_units(
        path,
        source_raw,
        flavor,
        &mut visiting,
        &mut seen,
        &mut units,
        &mut module_exports,
    )?;

    let rewritten_root_source =
        rewrite_imported_call_sites(&source, flavor, path, &root_imports, &module_exports)?;
    let root_parse_source = match flavor {
        SourceFlavor::Scheme => {
            let mut prelude = build_scheme_import_prelude(path, &root_imports, &module_exports)?;
            prelude.push_str(&rewritten_root_source);
            prelude
        }
        SourceFlavor::RustScript | SourceFlavor::JavaScript | SourceFlavor::Lua => {
            let mut prelude =
                build_rustscript_import_prelude(path, &root_imports, &module_exports)?;
            prelude.push_str(&rewritten_root_source);
            prelude
        }
    };

    let root_parsed = frontends::parse_source(&root_parse_source, flavor)
        .map_err(SourceError::Parse)
        .map_err(SourcePathError::Source)?;
    units.push(ParsedUnit {
        parsed: root_parsed,
        scope_prefix: None,
    });

    Ok((root_parse_source, units))
}

fn parse_module_imports(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
) -> Result<Vec<ModuleImport>, SourcePathError> {
    match flavor {
        SourceFlavor::RustScript => parse_rustscript_imports(source, path),
        SourceFlavor::JavaScript => Ok(parse_js_imports(source)),
        SourceFlavor::Lua => Ok(parse_lua_imports(source)),
        SourceFlavor::Scheme => parse_scheme_imports(source, path),
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
    if module_path == VM_HOST_NAMESPACE_SPEC {
        return Ok(VM_HOST_NAMESPACE_SPEC.to_string());
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

fn parse_js_imports(source: &str) -> Vec<ModuleImport> {
    let mut imports = Vec::new();
    let mut in_import_block = false;
    let mut block = String::new();
    let mut block_line = 1usize;

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if in_import_block {
            block.push(' ');
            block.push_str(line);
            if line.contains(" from ") || line.ends_with(';') {
                in_import_block = false;
                if let Some(import) = parse_js_import_from_block(&block, block_line) {
                    imports.push(import);
                }
                block.clear();
            }
            continue;
        }
        if line.starts_with("import ") {
            block_line = line_no;
            block.clear();
            block.push_str(line);
            if line.contains(" from ") || line.ends_with(';') {
                if let Some(import) = parse_js_import_from_block(&block, block_line) {
                    imports.push(import);
                }
                block.clear();
            } else {
                in_import_block = true;
            }
            continue;
        }
        if let Some(spec) = parse_require_spec(line) {
            imports.push(ModuleImport {
                spec,
                clause: ImportClause::AllPublic,
                line: line_no,
            });
        }
    }
    imports
}

fn parse_js_import_from_block(block: &str, line: usize) -> Option<ModuleImport> {
    if let Some(from_idx) = block.find(" from ") {
        let head = block["import ".len()..from_idx].trim();
        let clause = parse_import_clause_head(head)?;
        let tail = &block[from_idx + " from ".len()..];
        let (spec, _) = extract_quoted_literal(tail)?;
        return Some(ModuleImport {
            spec: spec.to_string(),
            clause,
            line,
        });
    }
    let tail = block.strip_prefix("import ")?;
    extract_quoted_literal(tail).map(|(spec, _)| ModuleImport {
        spec: spec.to_string(),
        clause: ImportClause::AllPublic,
        line,
    })
}

fn parse_lua_imports(source: &str) -> Vec<ModuleImport> {
    let mut imports = Vec::new();
    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim().trim_end_matches(';').trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, rhs)) = parse_lua_local_assignment(line)
            && let Some(import) = parse_lua_require_binding(name, rhs, line_no)
        {
            imports.push(import);
            continue;
        }
        if let Some(spec) = parse_require_spec(line) {
            imports.push(ModuleImport {
                spec,
                clause: ImportClause::AllPublic,
                line: line_no,
            });
        }
    }
    imports
}

fn parse_scheme_imports(source: &str, path: &Path) -> Result<Vec<ModuleImport>, SourcePathError> {
    let mut imports = Vec::new();
    for form in collect_scheme_top_level_forms(source) {
        if form.head != "import" && form.head != "require" {
            continue;
        }
        parse_scheme_import_form(path, form.start_line, &form.text, &mut imports)?;
    }
    Ok(imports)
}

fn parse_import_clause_head(head: &str) -> Option<ImportClause> {
    let trimmed = head.trim();
    if let Some(namespace) = trimmed.strip_prefix("* as ") {
        let namespace = namespace.trim();
        if is_valid_ident(namespace) {
            return Some(ImportClause::Namespace(namespace.to_string()));
        }
        return None;
    }

    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        let inner = &trimmed[1..trimmed.len().saturating_sub(1)];
        let named = parse_named_imports(inner)?;
        return Some(ImportClause::Named(named));
    }

    None
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

fn is_valid_ident(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

fn parse_lua_local_assignment(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("local ")?;
    let (name, rhs) = rest.split_once('=')?;
    let name = name.trim();
    let rhs = rhs.trim();
    if is_valid_ident(name) {
        Some((name, rhs))
    } else {
        None
    }
}

fn parse_lua_require_binding(name: &str, rhs: &str, line: usize) -> Option<ModuleImport> {
    let require_idx = rhs.find("require(")?;
    let require_head = rhs[..require_idx].trim();
    if !require_head.is_empty() {
        return None;
    }

    let tail = &rhs[require_idx + "require(".len()..];
    let (spec, rest) = extract_quoted_literal(tail)?;
    let rest = rest.trim();
    if rest.is_empty() || rest == ")" {
        return Some(ModuleImport {
            spec: spec.to_string(),
            clause: ImportClause::Namespace(name.to_string()),
            line,
        });
    }

    if let Some(member) = rest.strip_prefix(").") {
        let member = member.trim();
        if is_valid_ident(member) {
            return Some(ModuleImport {
                spec: spec.to_string(),
                clause: ImportClause::Named(vec![NamedImport {
                    imported: member.to_string(),
                    local: name.to_string(),
                }]),
                line,
            });
        }
    }

    None
}

#[derive(Clone, Debug)]
struct SchemeTopLevelForm {
    text: String,
    head: String,
    start_line: usize,
}

fn collect_scheme_top_level_forms(source: &str) -> Vec<SchemeTopLevelForm> {
    let bytes = source.as_bytes();
    let mut forms = Vec::new();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut start_line = 1usize;
    let mut line = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_line_comment = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
                line += 1;
            }
            i += 1;
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            if b == b'\n' {
                line += 1;
            }
            i += 1;
            continue;
        }

        if b == b';' {
            in_line_comment = true;
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            escaped = false;
            i += 1;
            continue;
        }

        if b == b'(' {
            if depth == 0 {
                start = i;
                start_line = line;
            }
            depth += 1;
            i += 1;
            continue;
        }

        if b == b')' {
            if depth > 0 {
                depth -= 1;
                if depth == 0 {
                    let end = i + 1;
                    let text = source[start..end].to_string();
                    let head = scheme_form_head(&text);
                    forms.push(SchemeTopLevelForm {
                        text,
                        head,
                        start_line,
                    });
                }
            }
            i += 1;
            continue;
        }

        if b == b'\n' {
            line += 1;
        }
        i += 1;
    }

    forms
}

fn scheme_form_head(form: &str) -> String {
    let mut chars = form.chars().peekable();
    while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
    if chars.next() != Some('(') {
        return String::new();
    }
    while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
    let mut out = String::new();
    while let Some(ch) = chars.peek().copied() {
        if ch.is_whitespace() || ch == '(' || ch == ')' {
            break;
        }
        out.push(ch);
        let _ = chars.next();
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SchemeImportToken {
    LParen,
    RParen,
    String(String),
    Symbol(String),
}

fn tokenize_scheme_import_form(form: &str) -> Option<Vec<SchemeImportToken>> {
    let bytes = form.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b';' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'(' {
            tokens.push(SchemeImportToken::LParen);
            i += 1;
            continue;
        }
        if b == b')' {
            tokens.push(SchemeImportToken::RParen);
            i += 1;
            continue;
        }
        if b == b'"' {
            i += 1;
            let start = i;
            let mut escaped = false;
            while i < bytes.len() {
                let ch = bytes[i];
                if escaped {
                    escaped = false;
                    i += 1;
                    continue;
                }
                if ch == b'\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if ch == b'"' {
                    break;
                }
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            tokens.push(SchemeImportToken::String(form[start..i].to_string()));
            i += 1;
            continue;
        }

        let start = i;
        while i < bytes.len() {
            let ch = bytes[i];
            if ch.is_ascii_whitespace() || ch == b'(' || ch == b')' || ch == b';' {
                break;
            }
            i += 1;
        }
        if start == i {
            i += 1;
            continue;
        }
        tokens.push(SchemeImportToken::Symbol(form[start..i].to_string()));
    }
    Some(tokens)
}

fn parse_scheme_import_form(
    path: &Path,
    line: usize,
    form: &str,
    imports: &mut Vec<ModuleImport>,
) -> Result<(), SourcePathError> {
    let tokens =
        tokenize_scheme_import_form(form).ok_or_else(|| SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "invalid scheme import form".to_string(),
        })?;
    let mut pos = 0usize;

    expect_scheme_token(path, line, &tokens, &mut pos, SchemeImportToken::LParen)?;
    let head = expect_scheme_symbol(path, line, &tokens, &mut pos)?;
    if head != "import" && head != "require" {
        return Ok(());
    }

    while pos < tokens.len() {
        if matches!(tokens[pos], SchemeImportToken::RParen) {
            pos += 1;
            break;
        }
        parse_scheme_import_set(path, line, &head, &tokens, &mut pos, imports)?;
    }

    if pos != tokens.len() {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "unexpected tokens after scheme import form".to_string(),
        });
    }

    Ok(())
}

fn parse_scheme_import_set(
    path: &Path,
    line: usize,
    form_head: &str,
    tokens: &[SchemeImportToken],
    pos: &mut usize,
    imports: &mut Vec<ModuleImport>,
) -> Result<(), SourcePathError> {
    match tokens.get(*pos) {
        Some(SchemeImportToken::String(spec)) => {
            imports.push(ModuleImport {
                spec: spec.clone(),
                clause: ImportClause::AllPublic,
                line,
            });
            *pos += 1;
            Ok(())
        }
        Some(SchemeImportToken::LParen) => {
            *pos += 1;
            let keyword = expect_scheme_symbol(path, line, tokens, pos)?;
            match (form_head, keyword.as_str()) {
                ("import", "only") | ("require", "only-in") => {
                    let spec = expect_scheme_string(path, line, tokens, pos)?;
                    let mut named = Vec::new();
                    while !matches!(tokens.get(*pos), Some(SchemeImportToken::RParen)) {
                        let symbol = expect_scheme_symbol(path, line, tokens, pos)?;
                        named.push(NamedImport {
                            imported: symbol.clone(),
                            local: symbol,
                        });
                    }
                    if named.is_empty() {
                        return Err(SourcePathError::InvalidImportSyntax {
                            path: path.to_path_buf(),
                            line,
                            message: "only import set requires at least one symbol".to_string(),
                        });
                    }
                    *pos += 1;
                    imports.push(ModuleImport {
                        spec,
                        clause: ImportClause::Named(named),
                        line,
                    });
                    Ok(())
                }
                ("import", "rename") | ("require", "rename-in") => {
                    let spec = expect_scheme_string(path, line, tokens, pos)?;
                    let mut named = Vec::new();
                    while !matches!(tokens.get(*pos), Some(SchemeImportToken::RParen)) {
                        expect_scheme_token(path, line, tokens, pos, SchemeImportToken::LParen)?;
                        let imported = expect_scheme_symbol(path, line, tokens, pos)?;
                        let local = expect_scheme_symbol(path, line, tokens, pos)?;
                        expect_scheme_token(path, line, tokens, pos, SchemeImportToken::RParen)?;
                        named.push(NamedImport { imported, local });
                    }
                    if named.is_empty() {
                        return Err(SourcePathError::InvalidImportSyntax {
                            path: path.to_path_buf(),
                            line,
                            message: "rename import set requires at least one rename pair"
                                .to_string(),
                        });
                    }
                    *pos += 1;
                    imports.push(ModuleImport {
                        spec,
                        clause: ImportClause::Named(named),
                        line,
                    });
                    Ok(())
                }
                ("import", "prefix") => {
                    let spec = expect_scheme_string(path, line, tokens, pos)?;
                    let prefix = expect_scheme_symbol(path, line, tokens, pos)?;
                    expect_scheme_token(path, line, tokens, pos, SchemeImportToken::RParen)?;
                    imports.push(ModuleImport {
                        spec,
                        clause: ImportClause::Prefix(prefix),
                        line,
                    });
                    Ok(())
                }
                ("require", "prefix-in") => {
                    let prefix = expect_scheme_symbol(path, line, tokens, pos)?;
                    let spec = expect_scheme_string(path, line, tokens, pos)?;
                    expect_scheme_token(path, line, tokens, pos, SchemeImportToken::RParen)?;
                    imports.push(ModuleImport {
                        spec,
                        clause: ImportClause::Prefix(prefix),
                        line,
                    });
                    Ok(())
                }
                (_, "library") | (_, "module") => {
                    let spec = expect_scheme_string(path, line, tokens, pos)?;
                    expect_scheme_token(path, line, tokens, pos, SchemeImportToken::RParen)?;
                    imports.push(ModuleImport {
                        spec,
                        clause: ImportClause::AllPublic,
                        line,
                    });
                    Ok(())
                }
                _ => Err(SourcePathError::InvalidImportSyntax {
                    path: path.to_path_buf(),
                    line,
                    message: format!("unsupported scheme import set '{keyword}'"),
                }),
            }
        }
        _ => Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "expected scheme import set".to_string(),
        }),
    }
}

fn expect_scheme_token(
    path: &Path,
    line: usize,
    tokens: &[SchemeImportToken],
    pos: &mut usize,
    expected: SchemeImportToken,
) -> Result<(), SourcePathError> {
    if matches!(tokens.get(*pos), Some(token) if *token == expected) {
        *pos += 1;
        return Ok(());
    }
    Err(SourcePathError::InvalidImportSyntax {
        path: path.to_path_buf(),
        line,
        message: "unexpected token in scheme import form".to_string(),
    })
}

fn expect_scheme_symbol(
    path: &Path,
    line: usize,
    tokens: &[SchemeImportToken],
    pos: &mut usize,
) -> Result<String, SourcePathError> {
    let Some(token) = tokens.get(*pos) else {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "unexpected end of scheme import form".to_string(),
        });
    };
    if let SchemeImportToken::Symbol(value) = token {
        *pos += 1;
        return Ok(value.clone());
    }
    Err(SourcePathError::InvalidImportSyntax {
        path: path.to_path_buf(),
        line,
        message: "expected symbol in scheme import form".to_string(),
    })
}

fn expect_scheme_string(
    path: &Path,
    line: usize,
    tokens: &[SchemeImportToken],
    pos: &mut usize,
) -> Result<String, SourcePathError> {
    let Some(token) = tokens.get(*pos) else {
        return Err(SourcePathError::InvalidImportSyntax {
            path: path.to_path_buf(),
            line,
            message: "unexpected end of scheme import form".to_string(),
        });
    };
    if let SchemeImportToken::String(value) = token {
        *pos += 1;
        return Ok(value.clone());
    }
    Err(SourcePathError::InvalidImportSyntax {
        path: path.to_path_buf(),
        line,
        message: "expected string module path in scheme import form".to_string(),
    })
}

fn parse_require_spec(line: &str) -> Option<String> {
    let require_idx = line.find("require(")?;
    let tail = &line[require_idx + "require(".len()..];
    let (spec, _) = extract_quoted_literal(tail)?;
    Some(spec.to_string())
}

fn extract_quoted_literal(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let mut start_idx = None;
    let mut quote = b'"';
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'"' || *byte == b'\'' {
            start_idx = Some(idx);
            quote = *byte;
            break;
        }
    }
    let start = start_idx?;
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            return Some((&input[start + 1..i], &input[i + 1..]));
        }
        i += 1;
    }
    None
}

fn is_module_specifier(spec: &str) -> bool {
    spec.ends_with(".rss")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
}

fn resolve_module_path(base_path: &Path, spec: &str) -> Result<PathBuf, SourcePathError> {
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

fn strip_import_directives(source: &str, flavor: SourceFlavor) -> String {
    match flavor {
        SourceFlavor::RustScript => source
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("use ")
                    && !is_vm_use_directive_line(line.trim_start())
                {
                    String::new()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        SourceFlavor::Scheme => source.to_string(),
        SourceFlavor::JavaScript | SourceFlavor::Lua => source.to_string(),
    }
}

fn is_vm_use_directive_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with("use ") {
        return false;
    }
    let Some((directive_body, _)) = trimmed["use ".len()..].split_once(';') else {
        return false;
    };
    let directive_body = directive_body.trim();
    if directive_body == VM_HOST_NAMESPACE_SPEC {
        return true;
    }
    if directive_body.starts_with("vm as ") {
        return true;
    }
    directive_body.starts_with("vm::")
}

fn collect_module_units(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    visiting: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    units: &mut Vec<ParsedUnit>,
    module_exports: &mut HashMap<PathBuf, HashMap<String, u8>>,
) -> Result<(), SourcePathError> {
    let imports = parse_module_imports(source, flavor, path)?;
    for import in imports {
        let spec = import.spec;
        if !is_module_specifier(&spec) {
            continue;
        }
        let resolved = resolve_module_path(path, &spec)?;
        let key = resolved.clone();
        if visiting.contains(&key) {
            return Err(SourcePathError::ImportCycle(key));
        }
        if seen.contains(&key) {
            continue;
        }

        let module_source_raw = std::fs::read_to_string(&resolved)?;
        visiting.push(key.clone());
        collect_module_units(
            &resolved,
            &module_source_raw,
            SourceFlavor::RustScript,
            visiting,
            seen,
            units,
            module_exports,
        )?;
        visiting.pop();

        let module_source = strip_import_directives(&module_source_raw, SourceFlavor::RustScript);
        let parsed = frontends::parse_source(&module_source, SourceFlavor::RustScript)
            .map_err(SourceError::Parse)
            .map_err(SourcePathError::Source)?;
        let exports = parsed
            .functions
            .iter()
            .filter(|func| func.exported)
            .map(|func| (func.name.clone(), func.arity))
            .collect::<HashMap<_, _>>();
        units.push(ParsedUnit {
            parsed,
            scope_prefix: Some(sanitize_scope_prefix(&resolved)),
        });
        module_exports.insert(key.clone(), exports);
        seen.insert(key);
    }
    Ok(())
}

fn build_rustscript_import_prelude(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
) -> Result<String, SourcePathError> {
    let declared = collect_imported_module_functions(path, imports, module_exports)?;
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

fn build_scheme_import_prelude(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
) -> Result<String, SourcePathError> {
    let declared = collect_imported_module_functions(path, imports, module_exports)?;
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
) -> Result<Vec<(String, u8)>, SourcePathError> {
    let mut imported_functions = HashMap::<String, u8>::new();

    for import in imports {
        if !is_module_specifier(&import.spec) {
            continue;
        }

        let resolved = resolve_module_path(path, &import.spec)?;
        let exports =
            module_exports
                .get(&resolved)
                .ok_or_else(|| SourcePathError::InvalidImportSyntax {
                    path: path.to_path_buf(),
                    line: import.line,
                    message: format!("module '{}' did not load", import.spec),
                })?;

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

fn rewrite_imported_call_sites(
    source: &str,
    flavor: SourceFlavor,
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
) -> Result<String, SourcePathError> {
    let mut alias_calls = HashMap::<String, String>::new();
    let mut namespace_calls = HashMap::<String, HashSet<String>>::new();
    let namespace_wildcards = HashSet::<String>::new();
    let mut prefix_aliases = Vec::<String>::new();

    for import in imports {
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

        let resolved = resolve_module_path(path, &import.spec)?;
        let Some(exports) = module_exports.get(&resolved) else {
            continue;
        };

        match &import.clause {
            ImportClause::AllPublic => {}
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

    if alias_calls.is_empty()
        && namespace_calls.is_empty()
        && namespace_wildcards.is_empty()
        && prefix_aliases.is_empty()
    {
        return Ok(source.to_string());
    }

    if flavor == SourceFlavor::Scheme {
        Ok(rewrite_scheme_call_heads(
            source,
            &alias_calls,
            &namespace_calls,
            &namespace_wildcards,
            &prefix_aliases,
        ))
    } else {
        Ok(rewrite_function_call_paths(
            source,
            flavor,
            &alias_calls,
            &namespace_calls,
            &namespace_wildcards,
            &prefix_aliases,
        ))
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
