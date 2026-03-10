use rt_format::{FormatArgument, Specifier};

use crate::compiler::source_map::{SourceId, Span};

use super::{ParseError, ParserDialect};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TokenKind {
    Ident(String),
    Int(i64),
    IntMinMagnitude(String),
    Float(f64),
    String(String),
    True,
    False,
    Null,
    Pub,
    Use,
    Import,
    From,
    As,
    Fn,
    Struct,
    Let,
    For,
    If,
    Else,
    Match,
    While,
    Break,
    Continue,
    Bang,
    BangEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Ampersand,
    AmpersandAmpersand,
    PipePipe,
    Pipe,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Question,
    Dot,
    Semicolon,
    Equal,
    EqualEqual,
    FatArrow,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Token {
    pub(super) kind: TokenKind,
    pub(super) line: usize,
    pub(super) span: Span,
}

enum NumberLiteral {
    Int(i64),
    IntMinMagnitude(String),
    Float(f64),
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ParserFormatArg;

impl FormatArgument for ParserFormatArg {
    fn supports_format(&self, _specifier: &Specifier) -> bool {
        true
    }

    fn fmt_display(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_debug(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_octal(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_lower_hex(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_upper_hex(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_binary(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_lower_exp(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn fmt_upper_exp(&self, _f: &mut std::fmt::Formatter) -> std::fmt::Result {
        Ok(())
    }

    fn to_usize(&self) -> Result<usize, ()> {
        Ok(0)
    }
}

pub(super) struct Lexer<'a> {
    chars: std::str::Chars<'a>,
    current: Option<char>,
    line: usize,
    offset: usize,
    source_id: SourceId,
    dialect: &'static dyn ParserDialect,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(
        source: &'a str,
        source_id: SourceId,
        dialect: &'static dyn ParserDialect,
    ) -> Self {
        let mut chars = source.chars();
        let current = chars.next();
        Self {
            chars,
            current,
            line: 1,
            offset: 0,
            source_id,
            dialect,
        }
    }

    pub(super) fn next_token(&mut self) -> Result<Token, ParseError> {
        self.skip_whitespace_and_comments()?;
        let line = self.line;
        let start = self.offset;
        let Some(ch) = self.current else {
            return Ok(Token {
                kind: TokenKind::Eof,
                line,
                span: Span::new(self.source_id, start, start),
            });
        };

        let token = match ch {
            '+' => {
                self.advance();
                TokenKind::Plus
            }
            '!' => {
                self.advance();
                if self.current == Some('=') {
                    self.advance();
                    TokenKind::BangEqual
                } else {
                    TokenKind::Bang
                }
            }
            '-' => {
                self.advance();
                TokenKind::Minus
            }
            '*' => {
                self.advance();
                TokenKind::Star
            }
            '/' => {
                self.advance();
                TokenKind::Slash
            }
            '%' => {
                self.advance();
                TokenKind::Percent
            }
            '&' => {
                self.advance();
                if self.current == Some('&') {
                    self.advance();
                    TokenKind::AmpersandAmpersand
                } else {
                    TokenKind::Ampersand
                }
            }
            '|' => {
                self.advance();
                if self.current == Some('|') {
                    self.advance();
                    TokenKind::PipePipe
                } else {
                    TokenKind::Pipe
                }
            }
            '(' => {
                self.advance();
                TokenKind::LParen
            }
            ')' => {
                self.advance();
                TokenKind::RParen
            }
            '[' => {
                self.advance();
                TokenKind::LBracket
            }
            ']' => {
                self.advance();
                TokenKind::RBracket
            }
            '{' => {
                self.advance();
                TokenKind::LBrace
            }
            '}' => {
                self.advance();
                TokenKind::RBrace
            }
            ',' => {
                self.advance();
                TokenKind::Comma
            }
            ':' => {
                self.advance();
                TokenKind::Colon
            }
            '?' => {
                self.advance();
                TokenKind::Question
            }
            '.' => {
                self.advance();
                TokenKind::Dot
            }
            ';' => {
                self.advance();
                TokenKind::Semicolon
            }
            '<' => {
                self.advance();
                if self.current == Some('=') {
                    self.advance();
                    TokenKind::LessEqual
                } else {
                    TokenKind::Less
                }
            }
            '>' => {
                self.advance();
                if self.current == Some('=') {
                    self.advance();
                    TokenKind::GreaterEqual
                } else {
                    TokenKind::Greater
                }
            }
            '=' => {
                self.advance();
                if self.current == Some('=') {
                    self.advance();
                    TokenKind::EqualEqual
                } else if self.current == Some('>') {
                    self.advance();
                    TokenKind::FatArrow
                } else {
                    TokenKind::Equal
                }
            }
            '"' | '\'' => {
                let value = self.consume_string(ch)?;
                TokenKind::String(value)
            }
            c if c.is_ascii_digit() => match self.consume_number()? {
                NumberLiteral::Int(value) => TokenKind::Int(value),
                NumberLiteral::IntMinMagnitude(value) => TokenKind::IntMinMagnitude(value),
                NumberLiteral::Float(value) => TokenKind::Float(value),
            },
            c if is_ident_start(c) => {
                let ident = self.consume_ident();
                match ident.as_str() {
                    "pub" => TokenKind::Pub,
                    "use" => TokenKind::Use,
                    _ if self.dialect.is_import_keyword(&ident) => TokenKind::Import,
                    _ if self.dialect.is_from_keyword(&ident) => TokenKind::From,
                    "as" => TokenKind::As,
                    "fn" => TokenKind::Fn,
                    _ if self.dialect.is_fn_alias_keyword(&ident) => TokenKind::Fn,
                    "struct" => TokenKind::Struct,
                    "let" => TokenKind::Let,
                    _ if self.dialect.is_let_alias_keyword(&ident) => TokenKind::Let,
                    "for" => TokenKind::For,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "match" => TokenKind::Match,
                    "while" => TokenKind::While,
                    "break" => TokenKind::Break,
                    "continue" => TokenKind::Continue,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    "null" => TokenKind::Null,
                    _ => TokenKind::Ident(ident),
                }
            }
            other => {
                return Err(ParseError {
                    line,
                    message: format!("unexpected character '{other}'"),
                    span: Some(Span::new(self.source_id, start, self.offset.max(start + 1))),
                    code: None,
                });
            }
        };

        Ok(Token {
            kind: token,
            line,
            span: Span::new(self.source_id, start, self.offset),
        })
    }

    fn advance(&mut self) {
        if let Some(current) = self.current {
            if current == '\n' {
                self.line += 1;
            }
            self.offset += current.len_utf8();
        }
        self.current = self.chars.next();
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), ParseError> {
        loop {
            while matches!(self.current, Some(c) if c.is_whitespace()) {
                self.advance();
            }

            let mut peek = self.chars.clone();
            if self.current == Some('/') && peek.next() == Some('/') {
                while let Some(ch) = self.current {
                    self.advance();
                    if ch == '\n' {
                        break;
                    }
                }
                continue;
            }
            let mut peek = self.chars.clone();
            if self.current == Some('/') && peek.next() == Some('*') {
                let start_line = self.line;
                self.advance();
                self.advance();
                loop {
                    let Some(ch) = self.current else {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: start_line,
                            message: "unterminated block comment".to_string(),
                        });
                    };
                    if ch == '*' {
                        let mut close = self.chars.clone();
                        if close.next() == Some('/') {
                            self.advance();
                            self.advance();
                            break;
                        }
                    }
                    self.advance();
                }
                continue;
            }
            break;
        }
        Ok(())
    }

    fn consume_number(&mut self) -> Result<NumberLiteral, ParseError> {
        let line = self.line;
        let mut text = String::new();
        if self.current == Some('0') {
            let mut peek = self.chars.clone();
            if matches!(peek.next(), Some('x' | 'X')) {
                text.push('0');
                self.advance();
                let Some(prefix) = self.current else {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: "invalid number '0x'".to_string(),
                    });
                };
                text.push(prefix);
                self.advance();
                let start_len = text.len();
                while let Some(ch) = self.current {
                    if ch.is_ascii_hexdigit() {
                        text.push(ch);
                        self.advance();
                    } else {
                        break;
                    }
                }
                if text.len() == start_len {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: format!("invalid number '{text}'"),
                    });
                }
                return Self::parse_integer_literal(&text, &text[2..], 16, line);
            }
        }
        while let Some(ch) = self.current {
            if ch.is_ascii_digit() {
                text.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        let mut is_float = false;
        if self.current == Some('.') {
            let mut peek = self.chars.clone();
            if peek.next().is_some_and(|ch| ch.is_ascii_digit()) {
                is_float = true;
                text.push('.');
                self.advance();
                while let Some(ch) = self.current {
                    if ch.is_ascii_digit() {
                        text.push(ch);
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }

        if is_float {
            return text
                .parse::<f64>()
                .map(NumberLiteral::Float)
                .map_err(|_| ParseError {
                    span: None,
                    code: None,
                    line,
                    message: format!("invalid number '{text}'"),
                });
        }

        Self::parse_integer_literal(&text, &text, 10, line)
    }

    fn parse_integer_literal(
        text: &str,
        digits: &str,
        radix: u32,
        line: usize,
    ) -> Result<NumberLiteral, ParseError> {
        match u64::from_str_radix(digits, radix) {
            Ok(value) if value <= i64::MAX as u64 => Ok(NumberLiteral::Int(value as i64)),
            Ok(value) if value == (i64::MAX as u64) + 1 => {
                Ok(NumberLiteral::IntMinMagnitude(text.to_string()))
            }
            Ok(_) => Err(ParseError {
                span: None,
                code: None,
                line,
                message: format!("integer literal '{text}' is out of range for i64"),
            }),
            Err(_) => Err(ParseError {
                span: None,
                code: None,
                line,
                message: format!("invalid number '{text}'"),
            }),
        }
    }

    fn consume_string(&mut self, delimiter: char) -> Result<String, ParseError> {
        let line = self.line;
        if self.current != Some(delimiter) {
            return Err(ParseError {
                span: None,
                code: None,
                line,
                message: "string literal has invalid delimiter".to_string(),
            });
        }
        self.advance();

        let mut out = String::new();
        loop {
            let Some(ch) = self.current else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line,
                    message: "unterminated string literal".to_string(),
                });
            };

            match ch {
                quote if quote == delimiter => {
                    self.advance();
                    break;
                }
                '\\' => {
                    self.advance();
                    let Some(escaped) = self.current else {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line,
                            message: "unterminated string escape".to_string(),
                        });
                    };
                    let mapped = match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '"' => '"',
                        '\'' => '\'',
                        '0' => '\0',
                        'x' => {
                            self.advance();
                            let Some(hi) = self.current else {
                                return Err(ParseError {
                                    span: None,
                                    code: None,
                                    line,
                                    message: "unterminated string escape".to_string(),
                                });
                            };
                            let Some(hi) = hex_nibble(hi) else {
                                return Err(ParseError {
                                    span: None,
                                    code: None,
                                    line,
                                    message: "invalid escape '\\x'".to_string(),
                                });
                            };
                            self.advance();
                            let Some(lo) = self.current else {
                                return Err(ParseError {
                                    span: None,
                                    code: None,
                                    line,
                                    message: "unterminated string escape".to_string(),
                                });
                            };
                            let Some(lo) = hex_nibble(lo) else {
                                return Err(ParseError {
                                    span: None,
                                    code: None,
                                    line,
                                    message: "invalid escape '\\x'".to_string(),
                                });
                            };
                            out.push(((hi << 4) | lo) as char);
                            self.advance();
                            continue;
                        }
                        other => {
                            return Err(ParseError {
                                span: None,
                                code: None,
                                line,
                                message: format!("invalid escape '\\{other}'"),
                            });
                        }
                    };
                    out.push(mapped);
                    self.advance();
                }
                other => {
                    out.push(other);
                    self.advance();
                }
            }
        }

        Ok(out)
    }

    fn consume_ident(&mut self) -> String {
        let mut text = String::new();
        while let Some(ch) = self.current {
            if is_ident_continue(ch) {
                text.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        text
    }
}

pub(super) fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

pub(super) fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn hex_nibble(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some((ch as u8) - b'0'),
        'a'..='f' => Some((ch as u8) - b'a' + 10),
        'A'..='F' => Some((ch as u8) - b'A' + 10),
        _ => None,
    }
}
