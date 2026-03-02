use super::super::ParseError;
use super::super::parser::ParserDialect;
use super::{is_ident_continue, is_ident_start};
use crate::compiler::source_map::LoweredSource;

struct RustScriptDialect;

impl ParserDialect for RustScriptDialect {}

static RUSTSCRIPT_DIALECT: RustScriptDialect = RustScriptDialect;

pub(super) fn parser_dialect() -> &'static dyn ParserDialect {
    &RUSTSCRIPT_DIALECT
}

pub(super) fn lower(source: &str) -> Result<LoweredSource, ParseError> {
    let print_rewritten = rewrite_rss_print_macro(source);
    let alias_rewritten = rewrite_rss_aliases(&print_rewritten);
    Ok(LoweredSource::identity(alias_rewritten))
}

fn rewrite_rss_print_macro(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
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

        if b == b'"' || b == b'\'' {
            out.push(b as char);
            i += 1;
            string_delim = Some(b);
            continue;
        }

        let is_ident_boundary = i == 0 || !is_ident_continue(bytes[i - 1] as char);
        if is_ident_boundary
            && i + 6 <= bytes.len()
            && &bytes[i..i + 5] == b"print"
            && bytes[i + 5] == b'!'
        {
            let mut j = i + 6;
            while j < bytes.len()
                && bytes[j].is_ascii_whitespace()
                && bytes[j] != b'\n'
                && bytes[j] != b'\r'
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                out.push_str("print");
                i += 6;
                continue;
            }
        }

        out.push(b as char);
        i += 1;
    }

    out
}

fn rewrite_rss_aliases(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
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

        if b == b'"' || b == b'\'' {
            out.push(b as char);
            i += 1;
            string_delim = Some(b);
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
        if ident == "Option"
            && let Some((member, member_end)) = try_parse_option_member(source, i)
        {
            if member == "None" {
                out.push_str("null");
                i = member_end;
                continue;
            }
            if member == "Some" {
                let mut after_member = skip_inline_whitespace(bytes, member_end);
                if after_member < bytes.len() && bytes[after_member] == b'(' {
                    out.push('(');
                    after_member += 1;
                    i = after_member;
                    continue;
                }
            }
        }

        out.push_str(ident);
    }

    out
}

fn try_parse_option_member(source: &str, index: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let mut cursor = skip_inline_whitespace(bytes, index);
    if cursor + 1 >= bytes.len() || bytes[cursor] != b':' || bytes[cursor + 1] != b':' {
        return None;
    }
    cursor += 2;
    cursor = skip_inline_whitespace(bytes, cursor);
    if cursor >= bytes.len() || !is_ident_start(bytes[cursor] as char) {
        return None;
    }
    let member_start = cursor;
    cursor += 1;
    while cursor < bytes.len() && is_ident_continue(bytes[cursor] as char) {
        cursor += 1;
    }
    Some((&source[member_start..cursor], cursor))
}

fn skip_inline_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len()
        && bytes[index].is_ascii_whitespace()
        && bytes[index] != b'\n'
        && bytes[index] != b'\r'
    {
        index += 1;
    }
    index
}
