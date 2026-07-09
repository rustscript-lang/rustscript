use crate::compiler::source_map::SourceId;

use super::lexer::{Lexer, Token, TokenKind};
use super::{ParseError, ParserDialect};

pub(super) fn lint_trailing_function_return_semicolons(
    source: &str,
    source_id: SourceId,
    dialect: &'static dyn ParserDialect,
) -> Result<Vec<ParseError>, ParseError> {
    let mut lexer = Lexer::new(source, source_id, dialect);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token()?;
        let is_eof = matches!(token.kind, TokenKind::Eof);
        tokens.push(token);
        if is_eof {
            break;
        }
    }

    Ok(find_function_return_semicolon_diagnostics(&tokens))
}

fn find_function_return_semicolon_diagnostics(tokens: &[Token]) -> Vec<ParseError> {
    let mut diagnostics = Vec::new();
    let mut cursor = 0usize;
    while cursor < tokens.len() {
        if !matches!(tokens[cursor].kind, TokenKind::Fn) {
            cursor += 1;
            continue;
        }

        let Some(block_start) = find_function_block_start(tokens, cursor + 1) else {
            cursor += 1;
            continue;
        };
        let Some(block_end) = find_matching_block_end(tokens, block_start) else {
            break;
        };
        if let Some(diagnostic) = lint_function_block_tail(tokens, block_start, block_end) {
            diagnostics.push(diagnostic);
        }
        cursor = block_end.saturating_add(1);
    }
    diagnostics
}

fn find_function_block_start(tokens: &[Token], start: usize) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut cursor = start;
    while let Some(token) = tokens.get(cursor) {
        match token.kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBrace if paren_depth == 0 => return Some(cursor),
            TokenKind::Equal | TokenKind::Semicolon | TokenKind::Eof if paren_depth == 0 => {
                return None;
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn find_matching_block_end(tokens: &[Token], block_start: usize) -> Option<usize> {
    let mut brace_depth = 1usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (cursor, token) in tokens.iter().enumerate().skip(block_start + 1) {
        match token.kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace if paren_depth == 0 && bracket_depth == 0 => brace_depth += 1,
            TokenKind::RBrace if paren_depth == 0 && bracket_depth == 0 => {
                brace_depth = brace_depth.saturating_sub(1);
                if brace_depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
    }
    None
}

fn lint_function_block_tail(
    tokens: &[Token],
    block_start: usize,
    block_end: usize,
) -> Option<ParseError> {
    let mut stmt_start = block_start + 1;
    let mut last_terminated_stmt: Option<(usize, usize)> = None;
    let mut saw_top_level_tokens_after_last_semicolon = false;
    let mut brace_depth = 1usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    for cursor in block_start + 1..=block_end {
        let token = tokens.get(cursor)?;
        match token.kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::LBracket => bracket_depth += 1,
            TokenKind::RBracket => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::LBrace if paren_depth == 0 && bracket_depth == 0 => brace_depth += 1,
            TokenKind::Semicolon if brace_depth == 1 && paren_depth == 0 && bracket_depth == 0 => {
                last_terminated_stmt = Some((stmt_start, cursor));
                stmt_start = cursor + 1;
                saw_top_level_tokens_after_last_semicolon = false;
            }
            TokenKind::RBrace if paren_depth == 0 && bracket_depth == 0 => {
                brace_depth = brace_depth.saturating_sub(1);
                if brace_depth == 0 {
                    let (stmt_start, semi_index) = last_terminated_stmt?;
                    if saw_top_level_tokens_after_last_semicolon
                        || !statement_looks_like_function_return(tokens, stmt_start, semi_index)
                    {
                        return None;
                    }
                    let semi = tokens.get(semi_index)?;
                    return Some(ParseError {
                        line: semi.line,
                        message: "function return expression should not end with ';'".to_string(),
                        span: Some(semi.span),
                        code: None,
                    });
                }
            }
            _ => {
                if brace_depth == 1
                    && paren_depth == 0
                    && bracket_depth == 0
                    && !matches!(token.kind, TokenKind::Eof)
                {
                    saw_top_level_tokens_after_last_semicolon = true;
                }
            }
        }
    }

    None
}

fn statement_looks_like_function_return(tokens: &[Token], start: usize, end: usize) -> bool {
    let first = tokens
        .iter()
        .skip(start)
        .take(end.saturating_sub(start) + 1)
        .find(|token| !matches!(token.kind, TokenKind::Semicolon));

    let Some(first) = first else {
        return false;
    };

    match &first.kind {
        TokenKind::Let
        | TokenKind::Fn
        | TokenKind::Struct
        | TokenKind::Pub
        | TokenKind::Use
        | TokenKind::Import
        | TokenKind::For
        | TokenKind::If
        | TokenKind::While
        | TokenKind::Break
        | TokenKind::Continue => false,
        TokenKind::Ident(name) if name == "return" => false,
        _ => true,
    }
}
