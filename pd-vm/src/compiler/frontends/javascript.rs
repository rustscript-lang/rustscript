use super::super::{ParseError, STDLIB_PRINT_NAME};
use super::{is_ident_continue, is_ident_start};
use crate::compiler::source_map::{LineSpanMapping, LoweredSource};
use std::collections::HashSet;

pub(super) fn lower(source: &str) -> Result<LoweredSource, ParseError> {
    let return_rewritten = rewrite_js_return_statements(source);
    reject_js_direct_builtin_calls(&return_rewritten)?;
    let console_rewritten = rewrite_console_log_calls(&return_rewritten);
    let typeof_rewritten = rewrite_js_typeof_operator(&console_rewritten);
    let keyword_rewritten = rewrite_keywords(&typeof_rewritten, |ident| match ident {
        "function" => Some("fn"),
        "const" => Some("let"),
        _ => None,
    });

    let mut lines = Vec::new();
    let mut line_map = Vec::new();
    let mut in_import_block = false;
    let mut import_block = String::new();
    let mut vm_import_emitted = false;
    let mut vm_namespace_aliases = HashSet::new();
    for (index, raw_line) in keyword_rewritten.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw_line.trim();
        if in_import_block {
            if !import_block.is_empty() {
                import_block.push(' ');
            }
            import_block.push_str(trimmed);
            if !vm_import_emitted && is_js_vm_import_block(&import_block) {
                if let Some(alias) = parse_js_vm_namespace_alias_from_import_block(&import_block) {
                    vm_namespace_aliases.insert(alias);
                }
                lines.push("use vm::*;".to_string());
                line_map.push(line_no);
                vm_import_emitted = true;
            } else {
                lines.push(String::new());
                line_map.push(line_no);
            }
            if trimmed.contains(" from ") || trimmed.ends_with(';') {
                in_import_block = false;
                import_block.clear();
            }
            continue;
        }
        if trimmed.starts_with("import ") {
            import_block.clear();
            import_block.push_str(trimmed);
            if !vm_import_emitted && is_js_vm_import_block(&import_block) {
                if let Some(alias) = parse_js_vm_namespace_alias_from_import_block(&import_block) {
                    vm_namespace_aliases.insert(alias);
                }
                lines.push("use vm::*;".to_string());
                line_map.push(line_no);
                vm_import_emitted = true;
            } else {
                lines.push(String::new());
                line_map.push(line_no);
            }
            if !trimmed.contains(" from ") && !trimmed.ends_with(';') {
                in_import_block = true;
            }
            continue;
        }
        if is_js_vm_require_line(raw_line) {
            if let Some(alias) = parse_js_vm_require_namespace_alias(raw_line) {
                vm_namespace_aliases.insert(alias);
            }
            if !vm_import_emitted {
                lines.push("use vm::*;".to_string());
                line_map.push(line_no);
                vm_import_emitted = true;
            } else {
                lines.push(String::new());
                line_map.push(line_no);
            }
            continue;
        }
        if is_js_external_decl_line(raw_line) {
            lines.push(String::new());
            line_map.push(line_no);
            continue;
        }
        let namespace_rewritten = rewrite_js_vm_namespace_calls(raw_line, &vm_namespace_aliases);
        lines.push(rewrite_js_arrow_line(&namespace_rewritten, line_no)?);
        line_map.push(line_no);
    }
    Ok(LoweredSource {
        text: lines.join("\n"),
        mapping: LineSpanMapping {
            lowered_to_original_line: line_map,
        },
    })
}

fn reject_js_direct_builtin_calls(source: &str) -> Result<(), ParseError> {
    let bytes = source.as_bytes();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

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

        if in_block_comment {
            if b == b'\n' {
                line += 1;
                i += 1;
                continue;
            }
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(delim) = in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                in_string = None;
            } else if b == b'\n' {
                line += 1;
            }
            i += 1;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }

        if b == b'"' || b == b'\'' || b == b'`' {
            in_string = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if b == b'\n' {
            line += 1;
            i += 1;
            continue;
        }

        if !is_ident_start(b as char) {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i] as char) {
            i += 1;
        }
        let ident = &source[start..i];
        if !is_forbidden_js_builtin_name(ident) {
            continue;
        }

        let prev_non_ws = previous_non_whitespace(bytes, start);
        if prev_non_ws.is_some_and(|idx| bytes[idx] == b'.') {
            continue;
        }

        let next = skip_js_whitespace(bytes, i);
        if next < bytes.len() && bytes[next] == b'(' {
            let hint = js_builtin_syntax_hint(ident);
            return Err(ParseError {
                span: None,
                code: None,
                line,
                message: format!(
                    "direct builtin call '{ident}(...)' is not exposed in JavaScript frontend; {hint}"
                ),
            });
        }
    }

    Ok(())
}

fn is_forbidden_js_builtin_name(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "slice"
            | "concat"
            | "array_new"
            | "array_push"
            | "map_new"
            | "get"
            | "set"
            | "count"
            | "__to_string"
            | "type_of"
            | "io_open"
            | "io_popen"
            | "io_read_all"
            | "io_read_line"
            | "io_write"
            | "io_flush"
            | "io_close"
            | "io_exists"
            | "re_is_match"
            | "re_find"
            | "re_replace"
            | "re_split"
            | "re_captures"
    )
}

fn js_builtin_syntax_hint(name: &str) -> &'static str {
    match name {
        "len" | "count" => "use '.length' syntax",
        "type_of" => "use 'typeof value'",
        "get" => "use index/member access syntax ('obj[key]' or 'obj.member')",
        "set" => "use assignment syntax ('obj[key] = value')",
        "concat" => "use '+' for string/array concatenation",
        "slice" => "use slice index syntax ('value[start:end]')",
        "io_open" | "io_popen" | "io_read_all" | "io_read_line" | "io_write" | "io_flush"
        | "io_close" | "io_exists" => "use io namespace syntax (for example 'io::open(...)')",
        "re_is_match" | "re_find" | "re_replace" | "re_split" | "re_captures" => {
            "use re namespace syntax (for example 're::match(pattern, text, \"i\")')"
        }
        _ => "use frontend language syntax instead of VM builtin helpers",
    }
}

fn rewrite_js_return_statements(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

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

        if let Some(delim) = in_string {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                in_string = None;
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
            in_string = Some(b);
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
            if ident != "return" {
                out.push_str(ident);
                continue;
            }

            let prev_non_ws = previous_non_whitespace(bytes, start);
            let next_non_ws = skip_js_ignorable(bytes, i);
            let is_member = prev_non_ws.is_some_and(|idx| bytes[idx] == b'.');
            let is_property_key = next_non_ws < bytes.len() && bytes[next_non_ws] == b':';
            if is_member || is_property_key {
                out.push_str(ident);
                continue;
            }

            if next_non_ws < bytes.len() && bytes[next_non_ws] == b';' {
                out.push_str("null");
            }
            out.push_str(&source[i..next_non_ws]);
            i = next_non_ws;
            continue;
        }

        out.push(b as char);
        i += 1;
    }

    out
}

fn rewrite_js_typeof_operator(source: &str) -> String {
    const TYPEOF: &[u8] = b"typeof";

    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

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

        if in_line_comment {
            out.push(b as char);
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if let Some(delim) = in_string {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                in_string = None;
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
            in_string = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        let is_ident_boundary = i == 0 || !is_ident_continue(bytes[i - 1] as char);
        let prev_non_ws = previous_non_whitespace(bytes, i);
        let is_member_name = prev_non_ws.is_some_and(|idx| bytes[idx] == b'.');
        if is_ident_boundary
            && !is_member_name
            && i + TYPEOF.len() <= bytes.len()
            && &bytes[i..i + TYPEOF.len()] == TYPEOF
            && (i + TYPEOF.len() == bytes.len()
                || !is_ident_continue(bytes[i + TYPEOF.len()] as char))
        {
            let operand_start = skip_js_whitespace(bytes, i + TYPEOF.len());
            if let Some(operand_end) = parse_js_typeof_operand_end(source, operand_start) {
                let operand = source[operand_start..operand_end].trim();
                out.push_str("type(");
                out.push_str(operand);
                out.push(')');
                i = operand_end;
                continue;
            }
        }

        out.push(b as char);
        i += 1;
    }

    out
}

fn parse_js_typeof_operand_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if start >= bytes.len() {
        return None;
    }

    let mut cursor = match bytes[start] {
        b'(' => parse_js_balanced_segment_end(source, start, b'(', b')')?,
        b'"' | b'\'' | b'`' => parse_js_string_end(source, start)?,
        b if is_ident_start(b as char) => {
            let mut end = start + 1;
            while end < bytes.len() && is_ident_continue(bytes[end] as char) {
                end += 1;
            }
            end
        }
        b if b.is_ascii_digit() => {
            let mut end = start + 1;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
                end += 1;
            }
            end
        }
        _ => return None,
    };

    loop {
        let ws = skip_js_whitespace(bytes, cursor);
        if ws >= bytes.len() {
            return Some(ws);
        }

        if bytes[ws] == b'.' {
            let member_start = skip_js_whitespace(bytes, ws + 1);
            if member_start < bytes.len() && is_ident_start(bytes[member_start] as char) {
                let mut member_end = member_start + 1;
                while member_end < bytes.len() && is_ident_continue(bytes[member_end] as char) {
                    member_end += 1;
                }
                cursor = member_end;
                continue;
            }
            return Some(ws);
        }

        if bytes[ws] == b'[' {
            cursor = parse_js_balanced_segment_end(source, ws, b'[', b']')?;
            continue;
        }

        if bytes[ws] == b'(' {
            cursor = parse_js_balanced_segment_end(source, ws, b'(', b')')?;
            continue;
        }

        return Some(ws);
    }
}

fn parse_js_balanced_segment_end(source: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = source.as_bytes();
    if start >= bytes.len() || bytes[start] != open {
        return None;
    }

    let mut i = start;
    let mut depth = 0usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(delim) = in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                in_string = None;
            }
            i += 1;
            continue;
        }

        if b == b'"' || b == b'\'' || b == b'`' {
            in_string = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if b == open {
            depth += 1;
            i += 1;
            continue;
        }

        if b == close {
            depth = depth.saturating_sub(1);
            i += 1;
            if depth == 0 {
                return Some(i);
            }
            continue;
        }

        i += 1;
    }

    None
}

fn parse_js_string_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let quote = bytes[start];
    if quote != b'"' && quote != b'\'' && quote != b'`' {
        return None;
    }
    let mut i = start + 1;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == quote {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn skip_js_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn skip_js_ignorable(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        if index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'/' {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }

        if index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            if index + 1 < bytes.len() {
                index += 2;
            } else {
                return bytes.len();
            }
            continue;
        }

        return index;
    }
}

fn previous_non_whitespace(bytes: &[u8], mut index: usize) -> Option<usize> {
    while index > 0 {
        index -= 1;
        if !bytes[index].is_ascii_whitespace() {
            return Some(index);
        }
    }
    None
}

fn is_js_external_decl_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.starts_with("import ") {
        return true;
    }

    if !(trimmed.starts_with("let ")
        || trimmed.starts_with("const ")
        || trimmed.starts_with("var "))
    {
        return false;
    }

    trimmed.contains("require(")
}

fn is_js_vm_require_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.contains("require(\"vm\")") || trimmed.contains("require('vm')")
}

fn is_js_vm_import_block(block: &str) -> bool {
    let trimmed = block.trim();
    if !trimmed.starts_with("import ") {
        return false;
    }
    if let Some(from_idx) = trimmed.find(" from ") {
        let tail = &trimmed[from_idx + " from ".len()..];
        return extract_quoted_literal(tail).is_some_and(|(spec, _)| spec == "vm");
    }
    let tail = &trimmed["import ".len()..];
    extract_quoted_literal(tail).is_some_and(|(spec, _)| spec == "vm")
}

fn parse_js_vm_namespace_alias_from_import_block(block: &str) -> Option<String> {
    let trimmed = block.trim();
    if !is_js_vm_import_block(trimmed) {
        return None;
    }
    let from_idx = trimmed.find(" from ")?;
    let head = trimmed["import ".len()..from_idx].trim();
    let alias = head.strip_prefix("* as ")?;
    let alias = alias.trim();
    if is_valid_ident(alias) {
        Some(alias.to_string())
    } else {
        None
    }
}

fn parse_js_vm_require_namespace_alias(line: &str) -> Option<String> {
    let trimmed = line.trim().trim_end_matches(';').trim();
    let rest = if let Some(rest) = trimmed.strip_prefix("let ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("const ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("var ") {
        rest
    } else {
        return None;
    };
    let (name, rhs) = rest.split_once('=')?;
    let name = name.trim();
    if !is_valid_ident(name) {
        return None;
    }
    let (spec, remainder) = parse_js_require_call(rhs.trim())?;
    if spec == "vm" && remainder.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

fn parse_js_require_call(input: &str) -> Option<(String, String)> {
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

fn rewrite_js_vm_namespace_calls(line: &str, vm_namespace_aliases: &HashSet<String>) -> String {
    if vm_namespace_aliases.is_empty() {
        return line.to_string();
    }

    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            out.push(b as char);
            i += 1;
            continue;
        }

        if let Some(delim) = in_string {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                in_string = None;
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

        if b == b'"' || b == b'\'' || b == b'`' {
            out.push(b as char);
            in_string = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if is_ident_start(b as char) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = &line[start..i];
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
                    let mut k = j;
                    let mut segments = Vec::<String>::new();
                    loop {
                        if k >= bytes.len() || bytes[k] != b'.' {
                            break;
                        }
                        k += 1;
                        while k < bytes.len()
                            && bytes[k].is_ascii_whitespace()
                            && bytes[k] != b'\n'
                            && bytes[k] != b'\r'
                        {
                            k += 1;
                        }
                        if k >= bytes.len() || !is_ident_start(bytes[k] as char) {
                            segments.clear();
                            break;
                        }
                        let member_start = k;
                        k += 1;
                        while k < bytes.len() && is_ident_continue(bytes[k] as char) {
                            k += 1;
                        }
                        segments.push(line[member_start..k].to_string());
                        let mut next = k;
                        while next < bytes.len()
                            && bytes[next].is_ascii_whitespace()
                            && bytes[next] != b'\n'
                            && bytes[next] != b'\r'
                        {
                            next += 1;
                        }
                        if next < bytes.len() && bytes[next] == b'.' {
                            k = next;
                            continue;
                        }
                        k = next;
                        break;
                    }
                    if !segments.is_empty() && k < bytes.len() && bytes[k] == b'(' {
                        if segments.len() == 1 {
                            out.push_str(&segments[0]);
                        } else {
                            out.push_str("vm::");
                            out.push_str(&segments.join("::"));
                        }
                        i = k;
                        continue;
                    }
                }
            }
            out.push_str(ident);
            continue;
        }

        out.push(b as char);
        i += 1;
    }

    out
}

fn is_valid_ident(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
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

fn rewrite_keywords<F>(source: &str, mut rewrite: F) -> String
where
    F: FnMut(&str) -> Option<&'static str>,
{
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while let Some(ch) = chars.next() {
        if in_line_comment {
            out.push(ch);
            if ch == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        if in_block_comment {
            out.push(ch);
            if ch == '*' && chars.peek().copied() == Some('/') {
                out.push('/');
                let _ = chars.next();
                in_block_comment = false;
            }
            continue;
        }

        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '/' {
            if chars.peek().copied() == Some('/') {
                out.push('/');
                out.push('/');
                let _ = chars.next();
                in_line_comment = true;
                continue;
            }
            if chars.peek().copied() == Some('*') {
                out.push('/');
                out.push('*');
                let _ = chars.next();
                in_block_comment = true;
                continue;
            }
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if is_ident_start(ch) {
            let mut ident = String::new();
            ident.push(ch);
            while let Some(next) = chars.peek().copied() {
                if is_ident_continue(next) {
                    ident.push(next);
                    let _ = chars.next();
                } else {
                    break;
                }
            }
            if let Some(value) = rewrite(&ident) {
                out.push_str(value);
            } else {
                out.push_str(&ident);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn rewrite_console_log_calls(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    const CONSOLE_DOT_LOG: &[u8] = b"console.log";

    while i < bytes.len() {
        let b = bytes[i];

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

        if b == b'"' {
            out.push('"');
            i += 1;
            in_string = true;
            continue;
        }

        let is_ident_boundary = i == 0 || !is_ident_continue(bytes[i - 1] as char);
        if is_ident_boundary
            && i + CONSOLE_DOT_LOG.len() <= bytes.len()
            && &bytes[i..i + CONSOLE_DOT_LOG.len()] == CONSOLE_DOT_LOG
        {
            let mut j = i + CONSOLE_DOT_LOG.len();
            while j < bytes.len()
                && bytes[j].is_ascii_whitespace()
                && bytes[j] != b'\n'
                && bytes[j] != b'\r'
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                out.push_str(STDLIB_PRINT_NAME);
                i += CONSOLE_DOT_LOG.len();
                continue;
            }
        }

        out.push(b as char);
        i += 1;
    }

    out
}

fn rewrite_js_arrow_line(line: &str, line_no: usize) -> Result<String, ParseError> {
    let Some(arrow_index) = line.find("=>") else {
        return Ok(line.to_string());
    };

    let left = &line[..arrow_index];
    let right = line[arrow_index + 2..].trim_start();
    if right.starts_with('{') {
        return Err(ParseError {
            span: None,
            code: None,
            line: line_no,
            message: "arrow closures with block bodies are not supported in this subset"
                .to_string(),
        });
    }

    let left_trimmed = left.trim_end();
    let (prefix, params_text) = if left_trimmed.ends_with(')') {
        let mut depth = 0usize;
        let mut open_index = None;
        for (idx, ch) in left_trimmed.char_indices().rev() {
            match ch {
                ')' => depth += 1,
                '(' => {
                    if depth == 0 {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: line_no,
                            message: "malformed arrow closure parameters".to_string(),
                        });
                    }
                    depth -= 1;
                    if depth == 0 {
                        open_index = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        let open = open_index.ok_or(ParseError {
            span: None,
            code: None,
            line: line_no,
            message: "could not find '(' for arrow closure parameters".to_string(),
        })?;
        (
            &left_trimmed[..open],
            &left_trimmed[open + 1..left_trimmed.len() - 1],
        )
    } else {
        let mut split_index = 0usize;
        for (idx, ch) in left_trimmed.char_indices().rev() {
            if !(ch.is_ascii_alphanumeric() || ch == '_') {
                split_index = idx + ch.len_utf8();
                break;
            }
        }
        (&left_trimmed[..split_index], &left_trimmed[split_index..])
    };

    let params = params_text.trim();
    if params.is_empty() {
        Ok(format!("{prefix}| | {right}"))
    } else {
        Ok(format!("{prefix}|{params}| {right}"))
    }
}
