use super::super::ParseError;
use super::{is_ident_continue, is_ident_start};
use std::collections::HashSet;

enum LuaBlock {
    If,
    For,
    While,
    FunctionDecl,
}

pub(super) fn lower(source: &str) -> Result<String, ParseError> {
    let cleaned_source = remove_lua_comments(source)?;
    let mut out = Vec::new();
    let mut blocks = Vec::new();
    let mut vm_namespace_aliases = HashSet::new();
    let mut vm_import_emitted = false;

    for (index, raw_line) in cleaned_source.lines().enumerate() {
        let line_no = index + 1;
        let trimmed_raw = raw_line.trim();
        if trimmed_raw.is_empty() {
            out.push(String::new());
            continue;
        }
        if let Some(vm_import) = lower_lua_vm_require_line(trimmed_raw) {
            if let Some(namespace_alias) = vm_import.namespace_alias {
                vm_namespace_aliases.insert(namespace_alias);
            }
            out.push(vm_import.use_stmt);
            if !vm_import_emitted {
                out.push("use vm::*;".to_string());
                vm_import_emitted = true;
            }
            continue;
        }
        if is_lua_require_line(trimmed_raw) {
            out.push(String::new());
            continue;
        }
        let rewritten = rewrite_lua_inline_function_literal(trimmed_raw, line_no)?;
        let trimmed = rewritten.trim();

        if let Some(rest) = trimmed.strip_prefix("local ") {
            out.push(format!(
                "let {};",
                rewrite_lua_expr(
                    rest.trim().trim_end_matches(';').trim(),
                    &vm_namespace_aliases
                )
            ));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("function ") {
            let signature = rest.trim().trim_end_matches(';').trim();
            if !signature.ends_with(')') {
                return Err(ParseError {
                    line: line_no,
                    message: "lua function declaration must end with ')'".to_string(),
                });
            }
            out.push(format!("fn {signature};"));
            if !trimmed.ends_with(';') {
                blocks.push(LuaBlock::FunctionDecl);
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("if ")
            && let Some(condition) = rest.strip_suffix(" then")
        {
            out.push(format!(
                "if {} {{",
                rewrite_lua_expr(condition.trim(), &vm_namespace_aliases)
            ));
            blocks.push(LuaBlock::If);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("while ")
            && let Some(condition) = rest.strip_suffix(" do")
        {
            out.push(format!(
                "while {} {{",
                rewrite_lua_expr(condition.trim(), &vm_namespace_aliases)
            ));
            blocks.push(LuaBlock::While);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("for ")
            && let Some(header) = rest.strip_suffix(" do")
        {
            let eq_index = header.find('=').ok_or(ParseError {
                line: line_no,
                message: "lua for loop must contain '='".to_string(),
            })?;
            let name = header[..eq_index].trim();
            let mut name_chars = name.chars();
            let valid_name = match name_chars.next() {
                Some(first) if is_ident_start(first) => name_chars.all(is_ident_continue),
                _ => false,
            };
            if !valid_name {
                return Err(ParseError {
                    line: line_no,
                    message: "invalid lua for loop variable".to_string(),
                });
            }
            let rhs = header[eq_index + 1..].trim();
            let parts = split_top_level_csv(rhs);
            if parts.len() < 2 || parts.len() > 3 {
                return Err(ParseError {
                    line: line_no,
                    message: "lua numeric for loop must be 'for name = start, end [, step] do'"
                        .to_string(),
                });
            }
            let start_expr = rewrite_lua_expr(parts[0].trim(), &vm_namespace_aliases);
            let end_expr = rewrite_lua_expr(parts[1].trim(), &vm_namespace_aliases);
            let step_expr = rewrite_lua_expr(
                parts.get(2).map(|s| s.trim()).unwrap_or("1"),
                &vm_namespace_aliases,
            );
            if step_expr.starts_with('-') {
                return Err(ParseError {
                    line: line_no,
                    message: "negative lua for steps are not supported in this subset".to_string(),
                });
            }
            out.push(format!(
                "for (let {name} = {start_expr}; {name} < (({end_expr}) + 1); {name} = {name} + ({step_expr})) {{"
            ));
            blocks.push(LuaBlock::For);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("elseif ")
            && let Some(condition) = rest.strip_suffix(" then")
        {
            if !matches!(blocks.last(), Some(LuaBlock::If)) {
                return Err(ParseError {
                    line: line_no,
                    message: "lua 'elseif' without matching 'if'".to_string(),
                });
            }
            out.push(format!(
                "}} else if {} {{",
                rewrite_lua_expr(condition.trim(), &vm_namespace_aliases)
            ));
            continue;
        }

        if trimmed == "else" {
            if !matches!(blocks.last(), Some(LuaBlock::If)) {
                return Err(ParseError {
                    line: line_no,
                    message: "lua 'else' without matching 'if'".to_string(),
                });
            }
            out.push("} else {".to_string());
            continue;
        }

        if trimmed == "end" {
            let block = blocks.pop().ok_or(ParseError {
                line: line_no,
                message: "lua 'end' without matching block".to_string(),
            })?;
            match block {
                LuaBlock::FunctionDecl => out.push(String::new()),
                LuaBlock::If | LuaBlock::For | LuaBlock::While => out.push("}".to_string()),
            }
            continue;
        }

        if trimmed == "::continue::" {
            out.push(String::new());
            continue;
        }

        if trimmed == "goto continue" || trimmed == "goto continue;" {
            out.push("continue;".to_string());
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("return ") {
            out.push(format!(
                "{};",
                rewrite_lua_expr(
                    rest.trim().trim_end_matches(';').trim(),
                    &vm_namespace_aliases
                )
            ));
            continue;
        }

        out.push(format!(
            "{};",
            rewrite_lua_expr(trimmed.trim_end_matches(';'), &vm_namespace_aliases)
        ));
    }

    if !blocks.is_empty() {
        return Err(ParseError {
            line: source.lines().count().max(1),
            message: "unterminated lua block: expected 'end'".to_string(),
        });
    }

    Ok(out.join("\n"))
}

fn is_lua_require_line(line: &str) -> bool {
    let trimmed = line.trim().trim_end_matches(';').trim();
    if parse_lua_require_call(trimmed).is_some() {
        return true;
    }
    if let Some((_, rhs)) = parse_lua_local_assignment(trimmed) {
        return parse_lua_require_call(rhs).is_some();
    }
    false
}

struct VmRequireImport {
    use_stmt: String,
    namespace_alias: Option<String>,
}

fn lower_lua_vm_require_line(line: &str) -> Option<VmRequireImport> {
    let trimmed = line.trim().trim_end_matches(';').trim();

    if let Some((name, rhs)) = parse_lua_local_assignment(trimmed) {
        let (spec, remainder) = parse_lua_require_call(rhs)?;
        if spec != "vm" {
            return None;
        }

        if remainder.is_empty() {
            if name == "vm" {
                return Some(VmRequireImport {
                    use_stmt: "use vm;".to_string(),
                    namespace_alias: Some("vm".to_string()),
                });
            }
            return Some(VmRequireImport {
                use_stmt: format!("use vm as {name};"),
                namespace_alias: Some(name.to_string()),
            });
        }

        if let Some(member) = remainder.strip_prefix('.') {
            let member = member.trim();
            if is_valid_lua_ident(member) {
                let use_stmt = if name == member {
                    format!("use vm::{{{member}}};")
                } else {
                    format!("use vm::{{{member} as {name}}};")
                };
                return Some(VmRequireImport {
                    use_stmt,
                    namespace_alias: None,
                });
            }
        }
        return None;
    }

    let (spec, remainder) = parse_lua_require_call(trimmed)?;
    if spec != "vm" || !remainder.is_empty() {
        return None;
    }
    Some(VmRequireImport {
        use_stmt: "use vm;".to_string(),
        namespace_alias: Some("vm".to_string()),
    })
}

fn parse_lua_require_call(input: &str) -> Option<(String, String)> {
    let mut rest = input.trim().strip_prefix("require")?.trim_start();
    rest = rest.strip_prefix('(')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    rest = &rest[quote.len_utf8()..];
    let mut end = None;
    for (idx, ch) in rest.char_indices() {
        if ch == quote {
            end = Some(idx);
            break;
        }
    }
    let end = end?;
    let spec = rest[..end].to_string();
    let tail = rest[end + quote.len_utf8()..].trim_start();
    if !tail.starts_with(')') {
        return None;
    }
    let remainder = tail[1..].trim().to_string();
    Some((spec, remainder))
}

fn is_valid_lua_ident(input: &str) -> bool {
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
    if is_valid_lua_ident(name) {
        Some((name, rhs))
    } else {
        None
    }
}

fn rewrite_lua_inline_function_literal(line: &str, line_no: usize) -> Result<String, ParseError> {
    let Some(function_index) = line.find("function") else {
        return Ok(line.to_string());
    };
    let function_end = function_index + "function".len();
    if function_index > 0 {
        let before = line[..function_index].chars().next_back();
        if before.is_some_and(is_ident_continue) {
            return Ok(line.to_string());
        }
    }
    let after_keyword_char = line[function_end..].chars().next();
    if after_keyword_char.is_some_and(is_ident_continue) {
        return Ok(line.to_string());
    }
    let prefix = &line[..function_index];
    if !prefix.contains('=') {
        return Ok(line.to_string());
    }
    let after_keyword = line[function_end..].trim_start();
    if !after_keyword.starts_with('(') {
        return Ok(line.to_string());
    }

    let mut depth = 0usize;
    let mut close_index = None;
    for (idx, ch) in after_keyword.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Err(ParseError {
                        line: line_no,
                        message: "malformed lua function literal parameters".to_string(),
                    });
                }
                depth -= 1;
                if depth == 0 {
                    close_index = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }

    let close_index = close_index.ok_or(ParseError {
        line: line_no,
        message: "lua function literal missing ')'".to_string(),
    })?;
    let params = after_keyword[1..close_index].trim();
    if params.is_empty() {
        return Err(ParseError {
            line: line_no,
            message: "lua function literal parameters cannot be empty".to_string(),
        });
    }

    let body_and_end = after_keyword[close_index + 1..].trim();
    let body_raw = body_and_end.strip_suffix("end").ok_or(ParseError {
        line: line_no,
        message: "lua function literal must end with 'end'".to_string(),
    })?;
    let body_raw = body_raw.trim();
    if !body_raw.starts_with("return") {
        return Err(ParseError {
            line: line_no,
            message: "lua function literal must use 'return <expr>'".to_string(),
        });
    }
    let after_return = &body_raw["return".len()..];
    if after_return.is_empty()
        || !after_return
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_whitespace())
    {
        return Err(ParseError {
            line: line_no,
            message: "lua function literal must use 'return <expr>'".to_string(),
        });
    }
    let body = after_return.trim().trim_end_matches(';').trim();
    if body.is_empty() {
        return Err(ParseError {
            line: line_no,
            message: "lua function literal return expression cannot be empty".to_string(),
        });
    }

    Ok(format!("{prefix}|{params}| {body}"))
}

fn rewrite_lua_expr(expr: &str, vm_namespace_aliases: &HashSet<String>) -> String {
    let bytes = expr.as_bytes();
    let mut out = String::with_capacity(expr.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
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

        if b == b'"' {
            out.push('"');
            in_string = true;
            i += 1;
            continue;
        }

        if b == b'~' && i + 1 < bytes.len() && bytes[i + 1] == b'=' {
            out.push_str("!=");
            i += 2;
            continue;
        }

        let ch = b as char;
        if is_ident_start(ch) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &expr[start..i];

            if vm_namespace_aliases.contains(ident) {
                let mut j = i;
                while j < bytes.len()
                    && bytes[j].is_ascii_whitespace()
                    && bytes[j] != b'\n'
                    && bytes[j] != b'\r'
                {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'.' {
                    let mut k = j + 1;
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
                        let member = &expr[member_start..k];
                        let mut call_check = k;
                        while call_check < bytes.len()
                            && bytes[call_check].is_ascii_whitespace()
                            && bytes[call_check] != b'\n'
                            && bytes[call_check] != b'\r'
                        {
                            call_check += 1;
                        }
                        if call_check < bytes.len() && bytes[call_check] == b'(' {
                            out.push_str(ident);
                            out.push_str("::");
                            out.push_str(member);
                            i = k;
                            continue;
                        }
                    }
                }
            }

            if ident == "not" {
                out.push('!');
            } else {
                out.push_str(ident);
            }
            continue;
        }

        out.push(ch);
        i += 1;
    }

    out
}

fn split_top_level_csv(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for ch in input.chars() {
        if in_string {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                current.push(ch);
            }
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if paren_depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn remove_lua_comments(source: &str) -> Result<String, ParseError> {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut line = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                out.push('\n');
                in_line_comment = false;
                line += 1;
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
            if b == b'\n' {
                out.push('\n');
                line += 1;
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
            } else if b == b'\n' {
                line += 1;
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

        if b == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }

        if b == b'\n' {
            line += 1;
        }
        out.push(b as char);
        i += 1;
    }

    if in_block_comment {
        return Err(ParseError {
            line,
            message: "unterminated lua block comment".to_string(),
        });
    }
    Ok(out)
}
