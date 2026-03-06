use super::super::ParseError;
use super::super::parser::ParserDialect;
use super::is_ident_continue;
use crate::compiler::source_map::LoweredSource;

struct RustScriptDialect;

impl ParserDialect for RustScriptDialect {}

static RUSTSCRIPT_DIALECT: RustScriptDialect = RustScriptDialect;

pub(super) fn parser_dialect() -> &'static dyn ParserDialect {
    &RUSTSCRIPT_DIALECT
}

pub(super) fn lower(source: &str) -> Result<LoweredSource, ParseError> {
    let print_rewritten = rewrite_rss_print_macro(source);
    Ok(LoweredSource::identity(print_rewritten))
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
