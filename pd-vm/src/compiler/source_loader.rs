use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::builtins::is_builtin_namespace;
use crate::compiler::source_map::SourceMap;

use super::frontends::{is_ident_continue, is_ident_start};
use super::{
    CompileSourceFileOptions, SourceError, SourceFlavor, SourcePathError, frontends,
    ir::{Expr, FrontendIr, Stmt},
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

#[derive(Default)]
struct ModuleCollectState {
    visiting: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    units: Vec<ParsedUnit>,
    module_exports: HashMap<PathBuf, HashMap<String, u8>>,
}

const VM_HOST_NAMESPACE_SPEC: &str = "vm";

struct ImportRewriteResult {
    source: String,
    requires_vm_namespace: bool,
}

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

    let rewritten_root = rewrite_imported_call_sites(
        &source,
        flavor,
        path,
        &root_imports,
        &collect_state.module_exports,
        options,
    )?;
    let mut prelude = match flavor {
        SourceFlavor::Scheme => build_scheme_import_prelude(
            path,
            &source,
            &root_imports,
            &collect_state.module_exports,
            options,
        )?,
        SourceFlavor::Lua => build_lua_import_prelude(
            path,
            &source,
            &root_imports,
            &collect_state.module_exports,
            options,
        )?,
        SourceFlavor::RustScript | SourceFlavor::JavaScript => {
            let mut generated = build_rustscript_import_prelude(
                path,
                &root_imports,
                &collect_state.module_exports,
                options,
            )?;
            if flavor == SourceFlavor::RustScript
                && rewritten_root.requires_vm_namespace
                && !vm_namespace_direct_calls_supported(&root_imports)
            {
                generated.push_str("use vm;\n");
            }
            generated
        }
    };
    let prelude_line_count = count_lines(&prelude);
    prelude.push_str(&rewritten_root.source);
    let root_parse_source = prelude;

    let mut root_source_map = SourceMap::new();
    let root_source_id =
        root_source_map.add_source(path.display().to_string(), root_parse_source.clone());
    let mut root_parsed = frontends::parse_source(&root_parse_source, flavor)
        .map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&root_source_map, root_source_id))
        })
        .map_err(SourcePathError::Source)?;
    shift_frontend_ir_lines(&mut root_parsed, prelude_line_count);
    collect_state.units.push(ParsedUnit {
        parsed: root_parsed,
        scope_prefix: None,
    });

    Ok((root_parse_source, collect_state.units))
}

fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        0
    } else {
        text.lines().count() as u32
    }
}

fn shift_frontend_ir_lines(ir: &mut FrontendIr, shift_down: u32) {
    if shift_down == 0 {
        return;
    }
    for stmt in &mut ir.stmts {
        shift_stmt_lines(stmt, shift_down);
    }
    for function_impl in ir.function_impls.values_mut() {
        for stmt in &mut function_impl.body_stmts {
            shift_stmt_lines(stmt, shift_down);
        }
        shift_expr_lines(&mut function_impl.body_expr, shift_down);
    }
}

fn shift_stmt_lines(stmt: &mut Stmt, shift_down: u32) {
    match stmt {
        Stmt::Noop { line }
        | Stmt::ClosureLet { line, .. }
        | Stmt::FuncDecl { line, .. }
        | Stmt::Break { line }
        | Stmt::Continue { line } => {
            *line = shifted_line(*line, shift_down);
        }
        Stmt::Let { expr, line, .. }
        | Stmt::Assign { expr, line, .. }
        | Stmt::Expr { expr, line } => {
            *line = shifted_line(*line, shift_down);
            shift_expr_lines(expr, shift_down);
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        } => {
            *line = shifted_line(*line, shift_down);
            shift_expr_lines(condition, shift_down);
            for nested in then_branch {
                shift_stmt_lines(nested, shift_down);
            }
            for nested in else_branch {
                shift_stmt_lines(nested, shift_down);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            line,
        } => {
            *line = shifted_line(*line, shift_down);
            shift_stmt_lines(init, shift_down);
            shift_expr_lines(condition, shift_down);
            shift_stmt_lines(post, shift_down);
            for nested in body {
                shift_stmt_lines(nested, shift_down);
            }
        }
        Stmt::While {
            condition,
            body,
            line,
        } => {
            *line = shifted_line(*line, shift_down);
            shift_expr_lines(condition, shift_down);
            for nested in body {
                shift_stmt_lines(nested, shift_down);
            }
        }
    }
}

fn shift_expr_lines(expr: &mut Expr, shift_down: u32) {
    match expr {
        Expr::Call(_, args) | Expr::LocalCall(_, args) => {
            for arg in args {
                shift_expr_lines(arg, shift_down);
            }
        }
        Expr::Closure(closure) => {
            shift_expr_lines(&mut closure.body, shift_down);
        }
        Expr::ClosureCall(closure, args) => {
            shift_expr_lines(&mut closure.body, shift_down);
            for arg in args {
                shift_expr_lines(arg, shift_down);
            }
        }
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => {
            shift_expr_lines(lhs, shift_down);
            shift_expr_lines(rhs, shift_down);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            shift_expr_lines(inner, shift_down);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            shift_expr_lines(condition, shift_down);
            shift_expr_lines(then_expr, shift_down);
            shift_expr_lines(else_expr, shift_down);
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            shift_expr_lines(value, shift_down);
            for (_, arm_expr) in arms {
                shift_expr_lines(arm_expr, shift_down);
            }
            shift_expr_lines(default, shift_down);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                shift_stmt_lines(stmt, shift_down);
            }
            shift_expr_lines(expr, shift_down);
        }
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_)
        | Expr::Var(_) => {}
    }
}

fn shifted_line(line: u32, shift_down: u32) -> u32 {
    line.saturating_sub(shift_down).max(1)
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
        if is_builtin_namespace_use_directive_line(line) {
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
    start_byte: usize,
    end_byte: usize,
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
                        start_byte: start,
                        end_byte: end,
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
                ("import", "prefix-in") | ("require", "prefix-in") => {
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

fn resolve_module_path(
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
                    && !is_preserved_rustscript_use_directive_line(line.trim_start())
                {
                    String::new()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        SourceFlavor::Lua => strip_lua_import_directives(source),
        SourceFlavor::Scheme => strip_scheme_import_directives(source),
        SourceFlavor::JavaScript => source.to_string(),
    }
}

fn strip_lua_import_directives(source: &str) -> String {
    source
        .lines()
        .map(|raw_line| {
            let line = raw_line.trim().trim_end_matches(';').trim();
            if let Some((name, rhs)) = parse_lua_local_assignment(line)
                && parse_lua_require_binding(name, rhs, 1).is_some()
            {
                return String::new();
            }
            if parse_require_spec(line).is_some() {
                return String::new();
            }
            raw_line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_scheme_import_directives(source: &str) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for form in collect_scheme_top_level_forms(source) {
        if form.head != "import" && form.head != "require" {
            continue;
        }
        for byte in &mut bytes[form.start_byte..form.end_byte] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn is_preserved_rustscript_use_directive_line(line: &str) -> bool {
    is_vm_use_directive_line(line) || is_builtin_namespace_use_directive_line(line)
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

fn is_builtin_namespace_use_directive_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with("use ") {
        return false;
    }
    let Some((directive_body, _)) = trimmed["use ".len()..].split_once(';') else {
        return false;
    };
    let directive_body = directive_body.trim();
    if is_builtin_host_namespace_spec(directive_body) {
        return true;
    }
    if let Some((root, alias)) = directive_body.rsplit_once(" as ") {
        return is_builtin_host_namespace_spec(root.trim()) && is_valid_ident(alias.trim());
    }
    false
}

fn is_builtin_host_namespace_spec(spec: &str) -> bool {
    is_builtin_namespace(spec)
}

fn vm_namespace_direct_calls_supported(imports: &[ModuleImport]) -> bool {
    imports.iter().any(|import| {
        if import.spec != VM_HOST_NAMESPACE_SPEC {
            return false;
        }
        match &import.clause {
            ImportClause::AllPublic => true,
            ImportClause::Namespace(alias) => alias == VM_HOST_NAMESPACE_SPEC,
            ImportClause::Named(_) | ImportClause::Prefix(_) => false,
        }
    })
}

fn host_namespace_root_from_spec(spec: &str) -> Option<String> {
    if spec == VM_HOST_NAMESPACE_SPEC {
        return None;
    }
    if spec.contains('/') {
        return None;
    }
    let stem = Path::new(spec).file_stem()?.to_str()?;
    if !is_valid_ident(stem) {
        return None;
    }
    Some(stem.to_string())
}

fn is_virtual_host_namespace_spec(spec: &str, options: &CompileSourceFileOptions) -> bool {
    options.module_override_path(spec).is_none() && host_namespace_root_from_spec(spec).is_some()
}

fn should_treat_missing_module_as_host_namespace(
    spec: &str,
    options: &CompileSourceFileOptions,
    err: &std::io::Error,
) -> bool {
    err.kind() == std::io::ErrorKind::NotFound && is_virtual_host_namespace_spec(spec, options)
}

fn collect_module_units(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
    state: &mut ModuleCollectState,
) -> Result<(), SourcePathError> {
    let imports = parse_module_imports(source, flavor, path)?;
    for import in imports {
        let spec = import.spec;
        if !is_module_specifier(&spec) {
            continue;
        }
        let resolved = resolve_module_path(path, &spec, options)?;
        let key = resolved.clone();
        if state.visiting.contains(&key) {
            return Err(SourcePathError::ImportCycle(key));
        }
        if state.seen.contains(&key) {
            continue;
        }

        let module_source_raw = match std::fs::read_to_string(&resolved) {
            Ok(source) => source,
            Err(err) => {
                if should_treat_missing_module_as_host_namespace(&spec, options, &err) {
                    continue;
                }
                return Err(SourcePathError::Io(err));
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

fn build_rustscript_import_prelude(
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

fn build_lua_import_prelude(
    path: &Path,
    source: &str,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    let declared = collect_declared_import_functions(
        path,
        SourceFlavor::Lua,
        source,
        imports,
        module_exports,
        options,
    )?;
    let mut prelude = String::new();
    if source_uses_print_call(source, SourceFlavor::Lua) {
        prelude.push_str("declare print 1\n");
    }
    for (name, arity) in declared {
        if let Some(arity) = arity {
            prelude.push_str(&format!("declare {name} {arity}\n"));
        } else {
            prelude.push_str(&format!("declare {name}\n"));
        }
    }
    Ok(prelude)
}

fn build_scheme_import_prelude(
    path: &Path,
    source: &str,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<String, SourcePathError> {
    let declared = collect_declared_import_functions(
        path,
        SourceFlavor::Scheme,
        source,
        imports,
        module_exports,
        options,
    )?;
    let mut prelude = String::new();
    if source_uses_print_call(source, SourceFlavor::Scheme) {
        prelude.push_str("(declare (print value))\n");
    }
    for (name, arity) in declared {
        if let Some(arity) = arity {
            let args = (0..arity)
                .map(|idx| format!("arg{idx}"))
                .collect::<Vec<_>>()
                .join(" ");
            prelude.push_str(&format!("(declare ({name} {args}))\n"));
        } else {
            prelude.push_str(&format!("(declare {name})\n"));
        }
    }
    Ok(prelude)
}

fn collect_declared_import_functions(
    path: &Path,
    flavor: SourceFlavor,
    source: &str,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<Vec<(String, Option<u8>)>, SourcePathError> {
    let mut declared = HashMap::<String, Option<u8>>::new();

    for import in imports {
        if is_module_specifier(&import.spec) {
            let resolved = resolve_module_path(path, &import.spec, options)?;
            let Some(exports) = module_exports.get(&resolved) else {
                return Err(SourcePathError::InvalidImportSyntax {
                    path: path.to_path_buf(),
                    line: import.line,
                    message: format!("module '{}' did not load", import.spec),
                });
            };

            match &import.clause {
                ImportClause::AllPublic | ImportClause::Namespace(_) | ImportClause::Prefix(_) => {
                    for (name, arity) in exports {
                        merge_declared_arity(&mut declared, name.clone(), Some(*arity));
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
                        merge_declared_arity(&mut declared, binding.imported.clone(), Some(arity));
                    }
                }
            }
            continue;
        }

        match &import.clause {
            ImportClause::Named(named) => {
                for binding in named {
                    merge_declared_arity(&mut declared, binding.imported.clone(), None);
                }
            }
            ImportClause::Namespace(namespace) => {
                for member in collect_namespace_member_calls(source, flavor, namespace) {
                    merge_declared_arity(&mut declared, member, None);
                }
            }
            ImportClause::Prefix(prefix) => {
                for member in collect_prefixed_call_targets(source, flavor, prefix) {
                    merge_declared_arity(&mut declared, member, None);
                }
            }
            ImportClause::AllPublic => {}
        }
    }

    let mut out = declared.into_iter().collect::<Vec<_>>();
    out.sort_by(|(lhs_name, _), (rhs_name, _)| lhs_name.cmp(rhs_name));
    Ok(out)
}

fn merge_declared_arity(
    declared: &mut HashMap<String, Option<u8>>,
    name: String,
    arity: Option<u8>,
) {
    declared
        .entry(name)
        .and_modify(|existing| {
            *existing = match (*existing, arity) {
                (Some(lhs), Some(rhs)) => Some(lhs.max(rhs)),
                (Some(lhs), None) => Some(lhs),
                (None, Some(rhs)) => Some(rhs),
                (None, None) => None,
            };
        })
        .or_insert(arity);
}

fn collect_namespace_member_calls(
    source: &str,
    flavor: SourceFlavor,
    namespace: &str,
) -> HashSet<String> {
    if namespace.is_empty() {
        return HashSet::new();
    }
    match flavor {
        SourceFlavor::Lua => collect_lua_namespace_member_calls(source, namespace),
        SourceFlavor::Scheme => collect_scheme_namespace_member_calls(source, namespace),
        SourceFlavor::RustScript | SourceFlavor::JavaScript => HashSet::new(),
    }
}

fn collect_prefixed_call_targets(
    source: &str,
    flavor: SourceFlavor,
    prefix: &str,
) -> HashSet<String> {
    if prefix.is_empty() {
        return HashSet::new();
    }
    match flavor {
        SourceFlavor::Lua => collect_lua_prefixed_call_targets(source, prefix),
        SourceFlavor::Scheme => collect_scheme_prefixed_call_targets(source, prefix),
        SourceFlavor::RustScript | SourceFlavor::JavaScript => HashSet::new(),
    }
}

fn source_uses_print_call(source: &str, flavor: SourceFlavor) -> bool {
    match flavor {
        SourceFlavor::Lua => source_uses_lua_function_call(source, "print"),
        SourceFlavor::Scheme => collect_scheme_call_head_symbols(source)
            .iter()
            .any(|symbol| symbol == "print"),
        SourceFlavor::RustScript | SourceFlavor::JavaScript => false,
    }
}

fn source_uses_lua_function_call(source: &str, target: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b']' && i + 1 < bytes.len() && bytes[i + 1] == b']' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(delim) = string_delim {
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

        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            if i + 3 < bytes.len() && bytes[i + 2] == b'[' && bytes[i + 3] == b'[' {
                in_block_comment = true;
                i += 4;
                continue;
            }
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if !is_ident_start(b as char) {
            i += 1;
            continue;
        }

        let ident_start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i] as char) {
            i += 1;
        }
        if &source[ident_start..i] != target {
            continue;
        }
        let mut cursor = i;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'(' {
            return true;
        }
    }

    false
}

fn collect_lua_namespace_member_calls(source: &str, namespace: &str) -> HashSet<String> {
    let bytes = source.as_bytes();
    let mut out = HashSet::new();
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b']' && i + 1 < bytes.len() && bytes[i + 1] == b']' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(delim) = string_delim {
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

        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            if i + 3 < bytes.len() && bytes[i + 2] == b'[' && bytes[i + 3] == b'[' {
                in_block_comment = true;
                i += 4;
                continue;
            }
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if !is_ident_start(b as char) {
            i += 1;
            continue;
        }

        let ident_start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i] as char) {
            i += 1;
        }
        let ident = &source[ident_start..i];
        if ident != namespace {
            continue;
        }

        let mut cursor = i;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'.' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || !is_ident_start(bytes[cursor] as char) {
            continue;
        }
        let member_start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_ident_continue(bytes[cursor] as char) {
            cursor += 1;
        }
        let member = &source[member_start..cursor];
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'(' {
            out.insert(member.to_string());
        }
    }

    out
}

fn collect_lua_prefixed_call_targets(source: &str, prefix: &str) -> HashSet<String> {
    let bytes = source.as_bytes();
    let mut out = HashSet::new();
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b']' && i + 1 < bytes.len() && bytes[i + 1] == b']' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(delim) = string_delim {
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

        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            if i + 3 < bytes.len() && bytes[i + 2] == b'[' && bytes[i + 3] == b'[' {
                in_block_comment = true;
                i += 4;
                continue;
            }
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if !is_ident_start(b as char) {
            i += 1;
            continue;
        }

        let ident_start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i] as char) {
            i += 1;
        }
        let ident = &source[ident_start..i];
        let Some(rem) = ident.strip_prefix(prefix) else {
            continue;
        };
        if rem.is_empty() || !is_valid_ident(rem) {
            continue;
        }
        let mut cursor = i;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && bytes[cursor] == b'(' {
            out.insert(rem.to_string());
        }
    }

    out
}

fn collect_scheme_namespace_member_calls(source: &str, namespace: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for symbol in collect_scheme_call_head_symbols(source) {
        let Some((target_namespace, member)) = symbol.split_once('.') else {
            continue;
        };
        if target_namespace == namespace && is_valid_ident(member) {
            out.insert(member.to_string());
        }
    }
    out
}

fn collect_scheme_prefixed_call_targets(source: &str, prefix: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for symbol in collect_scheme_call_head_symbols(source) {
        let Some(member) = symbol.strip_prefix(prefix) else {
            continue;
        };
        if !member.is_empty() && is_valid_ident(member) {
            out.insert(member.to_string());
        }
    }
    out
}

fn collect_scheme_call_head_symbols(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
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

        if b != b'(' {
            i += 1;
            continue;
        }

        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
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
        if i > symbol_start {
            out.push(source[symbol_start..i].to_string());
        }
    }

    out
}

fn collect_imported_module_functions(
    path: &Path,
    imports: &[ModuleImport],
    module_exports: &HashMap<PathBuf, HashMap<String, u8>>,
    options: &CompileSourceFileOptions,
) -> Result<Vec<(String, u8)>, SourcePathError> {
    let mut imported_functions = HashMap::<String, u8>::new();

    for import in imports {
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

fn rewrite_imported_call_sites(
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
    let mut namespace_wildcards = HashSet::<String>::new();
    let mut prefix_aliases = Vec::<String>::new();
    let mut requires_vm_namespace = false;

    for import in imports {
        if import.spec == VM_HOST_NAMESPACE_SPEC {
            match &import.clause {
                ImportClause::AllPublic => {
                    if matches!(flavor, SourceFlavor::Lua | SourceFlavor::Scheme)
                        && let Some(namespace) = module_default_namespace(&import.spec)
                    {
                        namespace_wildcards.insert(namespace);
                    }
                }
                ImportClause::Named(named) => {
                    for binding in named {
                        if binding.local != binding.imported {
                            alias_calls.insert(binding.local.clone(), binding.imported.clone());
                        }
                    }
                }
                ImportClause::Namespace(namespace) => {
                    if matches!(flavor, SourceFlavor::Lua | SourceFlavor::Scheme) {
                        namespace_wildcards.insert(namespace.clone());
                    }
                }
                ImportClause::Prefix(prefix) => {
                    if matches!(flavor, SourceFlavor::Lua | SourceFlavor::Scheme) {
                        prefix_aliases.push(prefix.clone());
                        if let Some(namespace) = prefix.strip_suffix('.')
                            && is_valid_ident(namespace)
                        {
                            namespace_wildcards.insert(namespace.to_string());
                        }
                    }
                }
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
                if matches!(flavor, SourceFlavor::Lua | SourceFlavor::Scheme) {
                    match &import.clause {
                        ImportClause::AllPublic => {
                            if let Some(namespace) = module_default_namespace(&import.spec) {
                                namespace_wildcards.insert(namespace);
                            }
                        }
                        ImportClause::Named(named) => {
                            for binding in named {
                                if binding.local != binding.imported {
                                    alias_calls
                                        .insert(binding.local.clone(), binding.imported.clone());
                                }
                            }
                        }
                        ImportClause::Namespace(namespace) => {
                            namespace_wildcards.insert(namespace.clone());
                        }
                        ImportClause::Prefix(prefix) => {
                            prefix_aliases.push(prefix.clone());
                            if let Some(namespace) = prefix.strip_suffix('.')
                                && is_valid_ident(namespace)
                            {
                                namespace_wildcards.insert(namespace.to_string());
                            }
                        }
                    }
                } else {
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
