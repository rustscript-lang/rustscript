use std::collections::{HashMap, HashSet};

use rt_format::{FormatArgument, NoNamedArguments, ParsedFormat, Specifier};

use crate::builtins::{
    BuiltinFunction, builtin_namespace_hint, is_builtin_namespace, namespace_supports_regex_flags,
    resolve_builtin_namespace_call,
};
use crate::compiler::source_map::{SourceId, Span};

use super::{
    ParseError, STDLIB_PRINT_ARITY, STDLIB_PRINT_NAME,
    ir::{
        ClosureExpr, Expr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern, MatchTypePattern,
        Stmt,
    },
};

fn is_virtual_host_namespace_spec(spec: &str) -> bool {
    if spec.contains('/') || spec.ends_with(".rss") {
        return false;
    }

    let mut chars = spec.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
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
struct Token {
    kind: TokenKind,
    line: usize,
    span: Span,
}

enum NumberLiteral {
    Int(i64),
    IntMinMagnitude(String),
    Float(f64),
}

#[derive(Clone, Copy, Debug, Default)]
struct ParserFormatArg;

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

pub(super) trait ParserDialect {
    fn is_import_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn is_from_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn is_fn_alias_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn is_let_alias_keyword(&self, _ident: &str) -> bool {
        false
    }

    fn allow_import_stmt(&self) -> bool {
        false
    }

    fn allow_return_stmt(&self) -> bool {
        false
    }

    fn allow_require_declaration(&self) -> bool {
        false
    }

    fn allow_typeof_operator(&self) -> bool {
        false
    }

    fn allow_arrow_closure(&self) -> bool {
        false
    }

    fn allow_dotted_call(&self) -> bool {
        false
    }

    fn allow_namespace_path_separator(&self) -> bool {
        true
    }

    fn allow_let_mut_binding(&self) -> bool {
        false
    }

    fn allow_macro_calls(&self) -> bool {
        false
    }
}

struct Lexer<'a> {
    chars: std::str::Chars<'a>,
    current: Option<char>,
    line: usize,
    offset: usize,
    source_id: SourceId,
    dialect: &'static dyn ParserDialect,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str, source_id: SourceId, dialect: &'static dyn ParserDialect) -> Self {
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

    fn next_token(&mut self) -> Result<Token, ParseError> {
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

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_continue(ch: char) -> bool {
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

pub(super) struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    locals: HashMap<String, LocalSlot>,
    next_local: LocalSlot,
    functions: HashMap<String, FunctionDecl>,
    function_list: Vec<FunctionDecl>,
    function_impls: HashMap<u16, FunctionImpl>,
    next_function: u16,
    closure_scopes: Vec<HashMap<String, LocalSlot>>,
    closure_capture_contexts: Vec<ClosureCaptureContext>,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    enforce_mutable_bindings: bool,
    dialect: &'static dyn ParserDialect,
    loop_depth: usize,
    function_body_depth: usize,
    host_namespace_aliases: HashMap<String, String>,
    direct_host_call_aliases: HashMap<String, String>,
    direct_host_wildcard_imports: HashSet<String>,
    mutable_locals: Vec<bool>,
}

struct ClosureCaptureContext {
    by_name: HashMap<String, LocalSlot>,
    capture_copies: Vec<(LocalSlot, LocalSlot)>,
}

impl Parser {
    pub(super) fn new(
        source: &str,
        source_id: SourceId,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
        enforce_mutable_bindings: bool,
        dialect: &'static dyn ParserDialect,
    ) -> Result<Self, ParseError> {
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
        Ok(Self {
            tokens,
            pos: 0,
            locals: HashMap::new(),
            next_local: 0,
            functions: HashMap::new(),
            function_list: Vec::new(),
            function_impls: HashMap::new(),
            next_function: 0,
            closure_scopes: Vec::new(),
            closure_capture_contexts: Vec::new(),
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            dialect,
            loop_depth: 0,
            function_body_depth: 0,
            host_namespace_aliases: HashMap::new(),
            direct_host_call_aliases: HashMap::new(),
            direct_host_wildcard_imports: HashSet::new(),
            mutable_locals: Vec::new(),
        })
    }

    pub(super) fn parse_program(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::Eof) {
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        if self.match_kind(&TokenKind::Pub) {
            if self.match_kind(&TokenKind::Fn) {
                return self.parse_fn_decl(true);
            }
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "expected 'fn' after 'pub'".to_string(),
            });
        }
        if self.match_kind(&TokenKind::Use) {
            return self.parse_use_stmt();
        }
        if self.dialect.allow_import_stmt() && self.match_kind(&TokenKind::Import) {
            return self.parse_js_import_stmt();
        }
        if self.dialect.allow_return_stmt() && self.check_ident_literal("return") {
            return self.parse_return_stmt();
        }
        if self.match_kind(&TokenKind::Fn) {
            return self.parse_fn_decl(false);
        }
        if self.match_kind(&TokenKind::Let) {
            if self.dialect.allow_require_declaration() && self.check_js_require_declaration_start()
            {
                return self.parse_js_require_declaration_after_let();
            }
            return self.parse_let_with_terminator(true);
        }
        if self.match_kind(&TokenKind::For) {
            return self.parse_for();
        }
        if self.match_kind(&TokenKind::If) {
            return self.parse_if();
        }
        if self.match_kind(&TokenKind::While) {
            return self.parse_while();
        }
        if self.match_kind(&TokenKind::Break) {
            return self.parse_loop_control_stmt(true);
        }
        if self.match_kind(&TokenKind::Continue) {
            return self.parse_loop_control_stmt(false);
        }
        if self.check_index_assignment_start() {
            return self.parse_index_assign_with_terminator(true);
        }
        if self.check_assignment_start() {
            return self.parse_assign_with_terminator(true);
        }

        let line = self.current_line_u32();
        let expr = self.parse_expr()?;
        self.consume_stmt_terminator("expected ';' after expression")?;
        Ok(Stmt::Expr { expr, line })
    }

    fn parse_loop_control_stmt(&mut self, is_break: bool) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        if self.loop_depth == 0 {
            return Err(ParseError {
                span: None,
                code: None,
                line: line as usize,
                message: if is_break {
                    "'break' is only allowed inside loops".to_string()
                } else {
                    "'continue' is only allowed inside loops".to_string()
                },
            });
        }
        self.consume_stmt_terminator(if is_break {
            "expected ';' after break"
        } else {
            "expected ';' after continue"
        })?;
        Ok(if is_break {
            Stmt::Break { line }
        } else {
            Stmt::Continue { line }
        })
    }

    fn parse_use_stmt(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let namespace = self.expect_ident("expected namespace after 'use'")?;
        if self.match_kind(&TokenKind::Semicolon) {
            self.host_namespace_aliases
                .insert(namespace.clone(), namespace);
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::As) {
            let alias = self.expect_ident("expected namespace alias after 'as'")?;
            self.expect(&TokenKind::Semicolon, "expected ';' after use alias")?;
            self.host_namespace_aliases.insert(alias, namespace);
            return Ok(Stmt::Noop { line });
        }

        if !self.match_path_separator() {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "unsupported use syntax for host namespace '{namespace}'; expected ';', 'as <alias>', or '::{{...}}'"
                ),
            });
        }

        if is_builtin_namespace(&namespace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "unsupported use list for builtin namespace '{namespace}'; import the namespace and call members through it"
                ),
            });
        }

        if self.match_kind(&TokenKind::Star) {
            self.direct_host_wildcard_imports.insert(namespace);
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after host wildcard import",
            )?;
            return Ok(Stmt::Noop { line });
        }

        self.expect(&TokenKind::LBrace, "expected '{' after host import path")?;
        if self.match_kind(&TokenKind::Star) {
            self.direct_host_wildcard_imports.insert(namespace);
            self.expect(&TokenKind::RBrace, "expected '}' after '*'")?;
            self.expect(&TokenKind::Semicolon, "expected ';' after use list")?;
            return Ok(Stmt::Noop { line });
        }

        loop {
            let imported = self.expect_ident("expected host function name in use list")?;
            let local = if self.match_kind(&TokenKind::As) {
                self.expect_ident("expected local alias after 'as'")?
            } else {
                imported.clone()
            };
            let target = format!("{namespace}::{imported}");
            if let Some(existing) = self.direct_host_call_aliases.get(&local)
                && existing != &target
            {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "host import alias '{local}' already maps to '{existing}', cannot remap to '{target}'"
                    ),
                });
            }
            self.direct_host_call_aliases.insert(local, target);

            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect(&TokenKind::RBrace, "expected '}' after use list")?;
        self.expect(&TokenKind::Semicolon, "expected ';' after use list")?;
        Ok(Stmt::Noop { line })
    }

    fn parse_js_import_stmt(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();

        if self.match_string().is_some() {
            self.consume_stmt_terminator("expected ';' after import statement")?;
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::Star) {
            self.expect(&TokenKind::As, "expected 'as' in namespace import")?;
            let alias = self.expect_ident("expected namespace alias in import")?;
            self.expect(&TokenKind::From, "expected 'from' in import statement")?;
            let spec = self.expect_string_literal("expected module string after 'from'")?;
            self.consume_stmt_terminator("expected ';' after import statement")?;
            if is_builtin_namespace(&spec) || is_virtual_host_namespace_spec(&spec) {
                self.host_namespace_aliases.insert(alias, spec);
            }
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::LBrace) {
            let named = self.parse_js_named_import_list()?;
            self.expect(&TokenKind::RBrace, "expected '}' after import list")?;
            self.expect(&TokenKind::From, "expected 'from' in import statement")?;
            let spec = self.expect_string_literal("expected module string after 'from'")?;
            self.consume_stmt_terminator("expected ';' after import statement")?;
            if is_virtual_host_namespace_spec(&spec) && !is_builtin_namespace(&spec) {
                for (imported, local) in named {
                    self.direct_host_call_aliases
                        .insert(local, format!("{spec}::{imported}"));
                }
            }
            return Ok(Stmt::Noop { line });
        }

        let default_ident = self.expect_ident("expected import clause after 'import'")?;
        if self.match_kind(&TokenKind::Comma) {
            if self.match_kind(&TokenKind::Star) {
                self.expect(&TokenKind::As, "expected 'as' in namespace import")?;
                let _alias = self.expect_ident("expected namespace alias in import")?;
            } else if self.match_kind(&TokenKind::LBrace) {
                let _ = self.parse_js_named_import_list()?;
                self.expect(&TokenKind::RBrace, "expected '}' after import list")?;
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unsupported import clause after ','".to_string(),
                });
            }
        }
        self.expect(&TokenKind::From, "expected 'from' in import statement")?;
        let spec = self.expect_string_literal("expected module string after 'from'")?;
        self.consume_stmt_terminator("expected ';' after import statement")?;
        if is_builtin_namespace(&spec) || is_virtual_host_namespace_spec(&spec) {
            self.host_namespace_aliases.insert(default_ident, spec);
        }
        Ok(Stmt::Noop { line })
    }

    fn parse_js_named_import_list(&mut self) -> Result<Vec<(String, String)>, ParseError> {
        let mut named = Vec::<(String, String)>::new();
        if self.check(&TokenKind::RBrace) {
            return Ok(named);
        }
        loop {
            let imported = self.expect_ident("expected imported symbol in import list")?;
            let local = if self.match_kind(&TokenKind::As) {
                self.expect_ident("expected local alias after 'as'")?
            } else {
                imported.clone()
            };
            named.push((imported, local));
            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        Ok(named)
    }

    fn parse_return_stmt(&mut self) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let _ = self.match_ident_literal("return");

        if self.function_body_depth == 0 {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "'return' is only valid inside function bodies".to_string(),
            });
        }

        let expr = if self.match_kind(&TokenKind::Semicolon)
            || self.check(&TokenKind::RBrace)
            || self.check(&TokenKind::Eof)
        {
            Expr::Null
        } else {
            let expr = self.parse_expr()?;
            self.consume_stmt_terminator("expected ';' after return value")?;
            expr
        };
        Ok(Stmt::Expr { expr, line })
    }

    fn check_js_require_declaration_start(&self) -> bool {
        if self.check(&TokenKind::LBrace) {
            let mut cursor = self.pos + 1;
            while cursor < self.tokens.len() {
                match self.tokens[cursor].kind {
                    TokenKind::RBrace => {
                        return self.check_kind_at(cursor + 1, &TokenKind::Equal)
                            && self.check_ident_literal_at(cursor + 2, "require")
                            && self.check_kind_at(cursor + 3, &TokenKind::LParen)
                            && self.check_string_at(cursor + 4)
                            && self.check_kind_at(cursor + 5, &TokenKind::RParen);
                    }
                    TokenKind::Eof => return false,
                    _ => cursor += 1,
                }
            }
            return false;
        }

        self.check_ident_at(self.pos)
            && self.check_kind_at(self.pos + 1, &TokenKind::Equal)
            && self.check_ident_literal_at(self.pos + 2, "require")
            && self.check_kind_at(self.pos + 3, &TokenKind::LParen)
            && self.check_string_at(self.pos + 4)
            && self.check_kind_at(self.pos + 5, &TokenKind::RParen)
    }

    fn parse_js_require_declaration_after_let(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();

        if self.match_kind(&TokenKind::LBrace) {
            while !self.check(&TokenKind::RBrace) {
                if self.check(&TokenKind::Eof) {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "unexpected end of input in require destructuring".to_string(),
                    });
                }
                self.pos += 1;
            }
            self.expect(&TokenKind::RBrace, "expected '}' in require destructuring")?;
            self.expect(
                &TokenKind::Equal,
                "expected '=' after destructuring in require declaration",
            )?;
            let _spec = self.parse_js_require_call()?;
            self.consume_stmt_terminator("expected ';' after require declaration")?;
            return Ok(Stmt::Noop { line });
        }

        let alias = self.expect_ident("expected identifier after 'let'")?;
        self.expect(
            &TokenKind::Equal,
            "expected '=' in require declaration after identifier",
        )?;
        let spec = self.parse_js_require_call()?;
        self.consume_stmt_terminator("expected ';' after require declaration")?;
        if is_builtin_namespace(&spec) || is_virtual_host_namespace_spec(&spec) {
            self.host_namespace_aliases.insert(alias, spec);
        }
        Ok(Stmt::Noop { line })
    }

    fn parse_js_require_call(&mut self) -> Result<String, ParseError> {
        if !self.match_ident_literal("require") {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "expected 'require(...)' in declaration".to_string(),
            });
        }
        self.expect(&TokenKind::LParen, "expected '(' after require")?;
        let spec = self.expect_string_literal("expected module string in require")?;
        self.expect(
            &TokenKind::RParen,
            "expected ')' after require module string",
        )?;
        Ok(spec)
    }

    fn parse_fn_decl(&mut self, exported: bool) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let name = self.expect_ident("expected function name after 'fn'")?;
        self.expect(&TokenKind::LParen, "expected '(' after function name")?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let param = self.expect_ident("expected parameter name")?;
                params.push(param);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after parameters")?;

        let arity = u8::try_from(params.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        if self.functions.contains_key(&name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("duplicate function '{name}'"),
            });
        }
        if self.locals.contains_key(&name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let decl = FunctionDecl {
            name: name.clone(),
            arity,
            index,
            args: params.clone(),
            exported,
        };
        self.functions.insert(name.clone(), decl.clone());
        self.function_list.push(decl.clone());

        if self.match_kind(&TokenKind::Equal) {
            let function_impl = self.parse_function_impl_expr(&params)?;
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after function definition",
            )?;
            self.function_impls.insert(index, function_impl);
        } else if self.match_kind(&TokenKind::LBrace) {
            let function_impl = self.parse_function_impl_block(&params)?;
            self.expect(&TokenKind::RBrace, "expected '}' after function body")?;
            self.function_impls.insert(index, function_impl);
            // Optional trailing semicolon for compatibility.
            self.match_kind(&TokenKind::Semicolon);
        } else {
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after function declaration",
            )?;
        }

        Ok(Stmt::FuncDecl {
            name,
            index,
            arity,
            args: params,
            exported,
            line,
        })
    }

    fn parse_function_impl_expr(&mut self, params: &[String]) -> Result<FunctionImpl, ParseError> {
        self.parse_function_impl(params, |parser| Ok((Vec::new(), parser.parse_expr()?)))
    }

    fn parse_function_impl_block(&mut self, params: &[String]) -> Result<FunctionImpl, ParseError> {
        self.parse_function_impl(params, |parser| {
            let mut body_stmts = Vec::new();
            while !parser.check(&TokenKind::RBrace) {
                if parser.check(&TokenKind::Eof) {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: parser.current_line(),
                        message: "unexpected end of input in function body".to_string(),
                    });
                }
                body_stmts.push(parser.parse_stmt()?);
            }

            let Some(last_stmt) = body_stmts.pop() else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: parser.current_line(),
                    message: "function body must end with an expression statement".to_string(),
                });
            };
            let body_expr = if let Stmt::Expr { expr, .. } = last_stmt {
                expr
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: parser.current_line(),
                    message: "function body must end with an expression statement".to_string(),
                });
            };

            Ok((body_stmts, body_expr))
        })
    }

    fn parse_function_impl<F>(
        &mut self,
        params: &[String],
        parse_body: F,
    ) -> Result<FunctionImpl, ParseError>
    where
        F: FnOnce(&mut Self) -> Result<(Vec<Stmt>, Expr), ParseError>,
    {
        let mut param_scope = HashMap::new();
        let mut param_slots = Vec::new();
        for param in params {
            if param_scope.contains_key(param) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("duplicate function parameter '{param}'"),
                });
            }
            let slot = self.allocate_hidden_local()?;
            param_scope.insert(param.clone(), slot);
            param_slots.push(slot);
        }
        self.closure_scopes.push(param_scope);
        self.closure_capture_contexts.push(ClosureCaptureContext {
            by_name: HashMap::new(),
            capture_copies: Vec::new(),
        });
        self.function_body_depth += 1;
        let (body_stmts, body_expr) = parse_body(self)?;
        self.function_body_depth = self.function_body_depth.saturating_sub(1);
        let capture_context = self
            .closure_capture_contexts
            .pop()
            .ok_or_else(|| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "internal function capture state error".to_string(),
            })?;
        self.closure_scopes.pop();
        Ok(FunctionImpl {
            param_slots,
            capture_copies: capture_context.capture_copies,
            body_stmts,
            body_expr,
        })
    }

    fn parse_let_with_terminator(&mut self, expect_terminator: bool) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let declared_mutable =
            self.dialect.allow_let_mut_binding() && self.match_ident_literal("mut");
        let name = if declared_mutable {
            self.expect_ident("expected identifier after 'let mut'")?
        } else {
            self.expect_ident("expected identifier after 'let'")?
        };
        self.expect(&TokenKind::Equal, "expected '=' after identifier")?;
        let expr = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after let")?;
        }

        if !self.closure_scopes.is_empty() {
            if let Some(index) = self
                .closure_scopes
                .last()
                .and_then(|scope| scope.get(&name))
                .copied()
            {
                self.apply_let_binding_mutability(index, declared_mutable, false);
                return Ok(Stmt::Let { index, expr, line });
            }
            let index = self.allocate_hidden_local()?;
            if let Some(scope) = self.closure_scopes.last_mut() {
                scope.insert(name, index);
            }
            self.apply_let_binding_mutability(index, declared_mutable, true);
            return Ok(Stmt::Let { index, expr, line });
        }

        let (index, created) = self.get_or_assign_local(&name)?;
        self.apply_let_binding_mutability(index, declared_mutable, created);
        Ok(Stmt::Let { index, expr, line })
    }

    fn parse_assign_with_terminator(
        &mut self,
        expect_terminator: bool,
    ) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let name = self.expect_ident("expected identifier before '='")?;
        self.expect(&TokenKind::Equal, "expected '=' after identifier")?;
        let expr = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after assignment")?;
        }

        let index = self.get_local(&name)?;
        self.require_local_mutable_for_operation(index, Some(name.as_str()), line, "assign to")?;
        Ok(Stmt::Assign { index, expr, line })
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        self.expect(&TokenKind::LParen, "expected '(' after 'for'")?;

        let init = if self.match_kind(&TokenKind::Let) {
            self.parse_let_with_terminator(false)?
        } else if self.check_assignment_start() {
            self.parse_assign_with_terminator(false)?
        } else {
            let init_line = self.current_line_u32();
            let expr = self.parse_expr()?;
            Stmt::Expr {
                expr,
                line: init_line,
            }
        };
        self.expect(&TokenKind::Semicolon, "expected ';' after for initializer")?;

        let condition = self.parse_expr()?;
        self.expect(&TokenKind::Semicolon, "expected ';' after for condition")?;

        let post = if self.check_assignment_start() {
            self.parse_assign_with_terminator(false)?
        } else {
            let post_line = self.current_line_u32();
            let expr = self.parse_expr()?;
            Stmt::Expr {
                expr,
                line: post_line,
            }
        };
        self.expect(&TokenKind::RParen, "expected ')' after for clauses")?;
        self.loop_depth += 1;
        let body = self.parse_block("expected '{' after for clauses")?;
        self.loop_depth -= 1;
        Ok(Stmt::For {
            init: Box::new(init),
            condition,
            post: Box::new(post),
            body,
            line,
        })
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let condition = self.parse_expr()?;
        let then_branch = self.parse_block("expected '{' after if condition")?;
        let else_branch = if self.match_kind(&TokenKind::Else) {
            if self.match_kind(&TokenKind::If) {
                vec![self.parse_if()?]
            } else {
                self.parse_block("expected '{' after else")?
            }
        } else {
            Vec::new()
        };
        Ok(Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let condition = self.parse_expr()?;
        self.loop_depth += 1;
        let body = self.parse_block("expected '{' after while condition")?;
        self.loop_depth -= 1;
        Ok(Stmt::While {
            condition,
            body,
            line,
        })
    }

    fn parse_block(&mut self, message: &str) -> Result<Vec<Stmt>, ParseError> {
        self.expect(&TokenKind::LBrace, message)?;
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unexpected end of input in block".to_string(),
                });
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect(&TokenKind::RBrace, "expected '}' to close block")?;
        Ok(stmts)
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_and()?;
        while self.match_kind(&TokenKind::PipePipe) {
            let rhs = self.parse_and()?;
            expr = Expr::Or(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_comparison()?;
        while self.match_kind(&TokenKind::AmpersandAmpersand) {
            let rhs = self.parse_comparison()?;
            expr = Expr::And(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_term()?;
        loop {
            if self.match_kind(&TokenKind::EqualEqual) {
                let rhs = self.parse_term()?;
                expr = Expr::Eq(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::BangEqual) {
                let rhs = self.parse_term()?;
                expr = Expr::Not(Box::new(Expr::Eq(Box::new(expr), Box::new(rhs))));
            } else if self.match_kind(&TokenKind::Less) {
                let rhs = self.parse_term()?;
                expr = Expr::Lt(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::LessEqual) {
                let rhs = self.parse_term()?;
                expr = self.build_non_strict_comparison(expr, rhs, Expr::Lt)?;
            } else if self.match_kind(&TokenKind::Greater) {
                let rhs = self.parse_term()?;
                expr = Expr::Gt(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::GreaterEqual) {
                let rhs = self.parse_term()?;
                expr = self.build_non_strict_comparison(expr, rhs, Expr::Gt)?;
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn build_non_strict_comparison(
        &mut self,
        lhs: Expr,
        rhs: Expr,
        build_strict: fn(Box<Expr>, Box<Expr>) -> Expr,
    ) -> Result<Expr, ParseError> {
        let lhs_slot = self.allocate_hidden_local()?;
        let rhs_slot = self.allocate_hidden_local()?;
        let line = self.last_line();
        let lhs_var = Expr::Var(lhs_slot);
        let rhs_var = Expr::Var(rhs_slot);
        Ok(Expr::Block {
            stmts: vec![
                Stmt::Let {
                    index: lhs_slot,
                    expr: lhs,
                    line,
                },
                Stmt::Let {
                    index: rhs_slot,
                    expr: rhs,
                    line,
                },
            ],
            expr: Box::new(Expr::Or(
                Box::new(build_strict(
                    Box::new(lhs_var.clone()),
                    Box::new(rhs_var.clone()),
                )),
                Box::new(Expr::Eq(Box::new(lhs_var), Box::new(rhs_var))),
            )),
        })
    }

    fn parse_term(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_factor()?;
        loop {
            if self.match_kind(&TokenKind::Plus) {
                let rhs = self.parse_factor()?;
                expr = Expr::Add(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::Minus) {
                let rhs = self.parse_factor()?;
                expr = Expr::Sub(Box::new(expr), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_factor(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_unary()?;
        loop {
            if self.match_kind(&TokenKind::Star) {
                let rhs = self.parse_unary()?;
                expr = Expr::Mul(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::Slash) {
                let rhs = self.parse_unary()?;
                expr = Expr::Div(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::Percent) {
                let rhs = self.parse_unary()?;
                expr = Expr::Mod(Box::new(expr), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::Ampersand) {
            if self.match_ident_literal("mut") {
                let inner = self.parse_unary()?;
                self.require_mut_borrow_target(&inner)?;
                self.require_mut_borrow_binding_mutable(&inner)?;
                return Ok(Expr::BorrowMut(Box::new(inner)));
            }
            let inner = self.parse_unary()?;
            return Ok(Expr::Borrow(Box::new(inner)));
        }
        if self.dialect.allow_typeof_operator() && self.match_ident_literal("typeof") {
            let inner = self.parse_unary()?;
            return self.build_builtin_call_expr(BuiltinFunction::TypeOf, vec![inner]);
        }
        if self.match_kind(&TokenKind::Minus) {
            if let Some(text) = self.match_int_min_magnitude() {
                let _ = text;
                return Ok(Expr::Int(i64::MIN));
            }
            let inner = self.parse_unary()?;
            Ok(Expr::Neg(Box::new(inner)))
        } else if self.match_kind(&TokenKind::Bang) {
            let inner = self.parse_unary()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }

    fn require_mut_borrow_target(&self, expr: &Expr) -> Result<(), ParseError> {
        if self.is_mut_borrow_target(expr) {
            return Ok(());
        }
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "mutable borrow target must be a local, local field, or local index"
                .to_string(),
        })
    }

    fn is_mut_borrow_target(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Var(_) => true,
            Expr::Call(index, args) => {
                if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Get)
                    || args.len() != 2
                {
                    return false;
                }
                matches!(args.first(), Some(Expr::Var(_)))
            }
            _ => false,
        }
    }

    fn require_mut_borrow_binding_mutable(&self, expr: &Expr) -> Result<(), ParseError> {
        let Some(root_slot) = self.extract_mut_borrow_root_slot(expr) else {
            return Ok(());
        };
        self.require_local_mutable_for_operation(
            root_slot,
            None,
            self.current_line_u32(),
            "take a mutable borrow of",
        )
    }

    fn extract_mut_borrow_root_slot(&self, expr: &Expr) -> Option<LocalSlot> {
        match expr {
            Expr::Var(slot) => Some(*slot),
            Expr::Call(index, args) => {
                if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Get)
                    || args.len() != 2
                {
                    return None;
                }
                match args.first() {
                    Some(Expr::Var(slot)) => Some(*slot),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn require_local_mutable_for_operation(
        &self,
        index: LocalSlot,
        name_hint: Option<&str>,
        line: u32,
        action: &str,
    ) -> Result<(), ParseError> {
        if !self.enforce_mutable_bindings || self.is_local_slot_mutable(index) {
            return Ok(());
        }
        let display = name_hint
            .map(str::to_string)
            .or_else(|| self.find_local_name_by_slot(index))
            .unwrap_or_else(|| format!("#{index}"));
        Err(ParseError {
            span: None,
            code: Some("E_IMMUTABLE_LOCAL".to_string()),
            line: line as usize,
            message: format!(
                "cannot {action} immutable local '{display}'; declare it as 'let mut {display} = ...'"
            ),
        })
    }

    fn apply_let_binding_mutability(
        &mut self,
        index: LocalSlot,
        declared_mutable: bool,
        created: bool,
    ) {
        if !self.enforce_mutable_bindings {
            return;
        }
        if created {
            self.set_local_slot_mutable(index, declared_mutable);
            return;
        }
        if declared_mutable {
            self.set_local_slot_mutable(index, true);
        }
    }

    fn is_local_slot_mutable(&self, index: LocalSlot) -> bool {
        self.mutable_locals
            .get(index as usize)
            .copied()
            .unwrap_or(true)
    }

    fn set_local_slot_mutable(&mut self, index: LocalSlot, is_mutable: bool) {
        let slot = index as usize;
        if slot >= self.mutable_locals.len() {
            self.mutable_locals.resize(slot + 1, true);
        }
        self.mutable_locals[slot] = is_mutable;
    }

    fn find_local_name_by_slot(&self, index: LocalSlot) -> Option<String> {
        for scope in self.closure_scopes.iter().rev() {
            if let Some((name, _)) = scope.iter().find(|(_, slot)| **slot == index) {
                return Some(name.clone());
            }
        }
        self.locals
            .iter()
            .find(|(_, slot)| **slot == index)
            .map(|(name, _)| name.clone())
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::If) {
            return self.parse_if_expr();
        }
        if self.match_kind(&TokenKind::Match) {
            return self.parse_match_expr();
        }
        if self.match_kind(&TokenKind::True) {
            return Ok(Expr::Bool(true));
        }
        if self.match_kind(&TokenKind::False) {
            return Ok(Expr::Bool(false));
        }
        if self.match_kind(&TokenKind::Null) {
            return Ok(Expr::Null);
        }
        self.reject_out_of_range_int_literal()?;
        if let Some(value) = self.match_int() {
            return Ok(Expr::Int(value));
        }
        if let Some(value) = self.match_float() {
            return Ok(Expr::Float(value));
        }
        if let Some(value) = self.match_string() {
            return Ok(Expr::String(value));
        }
        if self.match_kind(&TokenKind::Pipe) {
            return self.parse_closure_literal();
        }
        if self.match_kind(&TokenKind::PipePipe) {
            return self.parse_closure_expr_with_params(Vec::new());
        }
        if self.dialect.allow_arrow_closure() && self.check_parenthesized_arrow_closure_start() {
            return self.parse_parenthesized_arrow_closure();
        }
        if self.dialect.allow_arrow_closure() && self.check_single_param_arrow_closure_start() {
            return self.parse_single_param_arrow_closure();
        }
        if let Some(name) = self.match_ident() {
            if self.dialect.allow_dotted_call()
                && let Some(expr) = self.try_parse_js_dotted_call(&name)?
            {
                return Ok(expr);
            }
            if !self.dialect.allow_namespace_path_separator() && self.check_path_separator() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "namespace calls use '.' in this language (for example 'json.encode(...)'), not '::'"
                        .to_string(),
                });
            }
            if self.dialect.allow_namespace_path_separator() && self.match_path_separator() {
                let mut path_segments = Vec::new();
                path_segments
                    .push(self.expect_namespace_segment("expected function name after '::'")?);
                while self.match_path_separator() {
                    path_segments
                        .push(self.expect_namespace_segment("expected function name after '::'")?);
                }
                self.expect(
                    &TokenKind::LParen,
                    "expected '(' after namespaced function name",
                )?;
                let args = self.parse_call_args()?;
                let member = path_segments
                    .first()
                    .ok_or_else(|| ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "expected function name after '::'".to_string(),
                    })?
                    .clone();
                let subpath = path_segments
                    .get(1..)
                    .map(|tail| tail.to_vec())
                    .unwrap_or_default();
                let mut args = args;
                if let Some((builtin_namespace, builtin_member)) =
                    self.resolve_builtins_call_path(&name, &member, &subpath)
                {
                    let builtin_namespace = builtin_namespace.to_string();
                    let builtin_member = builtin_member.to_string();
                    if namespace_supports_regex_flags(&builtin_namespace) {
                        if let Some(builtin) =
                            self.try_re_namespace_builtin_call(&builtin_member, &mut args)?
                        {
                            let expr = self.build_builtin_call_expr(builtin, args)?;
                            return Ok(expr);
                        }
                    } else if let Some(builtin) =
                        resolve_builtin_namespace_call(&builtin_namespace, &builtin_member)
                    {
                        let expr = self.build_builtin_call_expr(builtin, args)?;
                        return Ok(expr);
                    }
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: format!(
                            "unknown builtin function '{}::{}'",
                            builtin_namespace, builtin_member
                        ),
                    });
                }
                let host_name = self
                    .resolve_host_namespace_call_target(&name, &member, &subpath)
                    .ok_or_else(|| ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: format!(
                            "unknown namespace call '{}::{}'; import builtin namespaces first ({}), and for host calls import the matching host namespace or alias",
                            name,
                            path_segments.join("::"),
                            builtin_namespace_hint()
                        ),
                    })?;
                let expr = self.build_host_call_expr(&host_name, args)?;
                return Ok(expr);
            }

            let mut expr = if self.dialect.allow_macro_calls() && self.match_kind(&TokenKind::Bang)
            {
                self.parse_macro_call(&name)?
            } else if self.match_kind(&TokenKind::LParen) {
                let args = self.parse_call_args()?;
                if self.has_local_binding(&name) {
                    let local = self.get_local(&name)?;
                    Expr::LocalCall(local, args)
                } else if self.functions.contains_key(&name) {
                    let builtin_alias_call = if matches!(name.as_str(), "print" | "println") {
                        self.functions
                            .get(&name)
                            .map(|decl| !self.function_impls.contains_key(&decl.index))
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    if builtin_alias_call {
                        if let Some(expr) = self.try_build_language_builtin_call(&name, &args)? {
                            expr
                        } else {
                            let decl = self.resolve_function_for_call(&name, args.len())?;
                            Expr::Call(decl.index, args)
                        }
                    } else {
                        let decl = self.resolve_function_for_call(&name, args.len())?;
                        Expr::Call(decl.index, args)
                    }
                } else if let Some(expr) = self.try_build_language_builtin_call(&name, &args)? {
                    expr
                } else if let Some(host_name) = self.resolve_direct_host_call_target(&name) {
                    self.build_host_call_expr(&host_name, args)?
                } else {
                    let decl = self.resolve_function_for_call(&name, args.len())?;
                    Expr::Call(decl.index, args)
                }
            } else if self.has_local_binding(&name) {
                let index = self.get_local(&name)?;
                Expr::Var(index)
            } else if let Some(decl) = self.functions.get(&name) {
                Expr::FunctionRef(decl.index)
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("unknown local '{name}'"),
                });
            };
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }
        if self.match_kind(&TokenKind::LParen) {
            let mut expr = self.parse_expr()?;
            self.expect(&TokenKind::RParen, "expected ')' after expression")?;
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }
        if self.match_kind(&TokenKind::LBracket) {
            let mut expr = self.parse_array_literal()?;
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }
        if self.match_kind(&TokenKind::LBrace) {
            let mut expr = self.parse_brace_literal()?;
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "expected expression".to_string(),
        })
    }

    fn parse_if_expr(&mut self) -> Result<Expr, ParseError> {
        let condition = self.parse_expr()?;
        self.expect(
            &TokenKind::FatArrow,
            "expected '=>' after if condition in expression form",
        )?;
        let then_expr = self.parse_if_expr_branch()?;
        self.expect(&TokenKind::Else, "if expression requires an else branch")?;
        let else_expr = if self.match_kind(&TokenKind::If) {
            self.parse_if_expr()?
        } else {
            self.expect(
                &TokenKind::FatArrow,
                "expected '=>' after else in expression form",
            )?;
            self.parse_if_expr_branch()?
        };
        Ok(Expr::IfElse {
            condition: Box::new(condition),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        })
    }

    fn parse_if_expr_branch(&mut self) -> Result<Expr, ParseError> {
        self.expect(
            &TokenKind::LBrace,
            "expected '{' after '=>' in if expression branch",
        )?;

        let mut stmts = Vec::<Stmt>::new();
        let mut trailing_expr: Option<Expr> = None;
        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unexpected end of input in if expression branch".to_string(),
                });
            }

            if self.starts_if_expr_branch_statement() {
                stmts.push(self.parse_stmt()?);
                continue;
            }

            let line = self.current_line_u32();
            let expr = self.parse_expr()?;
            if self.check(&TokenKind::RBrace) {
                trailing_expr = Some(expr);
                break;
            }
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after expression in if expression branch",
            )?;
            stmts.push(Stmt::Expr { expr, line });
        }

        self.expect(
            &TokenKind::RBrace,
            "expected '}' to close if expression branch",
        )?;

        let expr = if let Some(expr) = trailing_expr {
            expr
        } else {
            let Some(last_stmt) = stmts.pop() else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "if expression branch must end with an expression".to_string(),
                });
            };
            if let Stmt::Expr { expr, .. } = last_stmt {
                expr
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "if expression branch must end with an expression".to_string(),
                });
            }
        };

        if stmts.is_empty() {
            Ok(expr)
        } else {
            Ok(Expr::Block {
                stmts,
                expr: Box::new(expr),
            })
        }
    }

    fn parse_match_expr(&mut self) -> Result<Expr, ParseError> {
        let value = self.parse_expr()?;
        self.expect(&TokenKind::LBrace, "expected '{' after match value")?;

        let value_slot = self.allocate_hidden_local()?;
        let result_slot = self.allocate_hidden_local()?;
        let mut arms = Vec::<(MatchPattern, Expr)>::new();
        let mut default: Option<Expr> = None;

        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unexpected end of input in match expression".to_string(),
                });
            }

            let pattern_token_line = self.current_line();
            let pattern = self.parse_match_pattern()?;
            self.expect(&TokenKind::FatArrow, "expected '=>' in match arm")?;
            let arm_expr = self.parse_expr()?;

            match pattern {
                Some(pattern) => {
                    if default.is_some() {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: pattern_token_line,
                            message: "non-wildcard match arm cannot appear after '_' arm"
                                .to_string(),
                        });
                    }
                    arms.push((pattern, arm_expr));
                }
                None => {
                    if default.is_some() {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: pattern_token_line,
                            message: "duplicate '_' match arm".to_string(),
                        });
                    }
                    default = Some(arm_expr);
                }
            }

            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            if !self.check(&TokenKind::RBrace) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "expected ',' or '}' after match arm".to_string(),
                });
            }
        }
        self.expect(&TokenKind::RBrace, "expected '}' after match expression")?;

        let default = default.ok_or_else(|| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "match expression requires a wildcard arm '_ => ...'".to_string(),
        })?;

        Ok(Expr::Match {
            value_slot,
            result_slot,
            value: Box::new(value),
            arms,
            default: Box::new(default),
        })
    }

    fn parse_match_pattern(&mut self) -> Result<Option<MatchPattern>, ParseError> {
        if self.match_kind(&TokenKind::LParen) {
            let pattern = self.parse_match_pattern()?;
            self.expect(
                &TokenKind::RParen,
                "expected ')' after parenthesized match pattern",
            )?;
            return Ok(pattern);
        }
        if let Some(value) = self.match_int() {
            return Ok(Some(MatchPattern::Int(value)));
        }
        self.reject_out_of_range_int_literal()?;
        if let Some(value) = self.match_string() {
            return Ok(Some(MatchPattern::String(value)));
        }
        if self.match_kind(&TokenKind::Null) {
            return Ok(Some(MatchPattern::Null));
        }
        if let Some(name) = self.match_ident() {
            if name == "_" {
                return Ok(None);
            }
            if let Some(type_pattern) = self.parse_match_type_constructor_pattern(&name)? {
                return Ok(Some(MatchPattern::Type(type_pattern)));
            }
        }
        Err(ParseError { span: None, code: None,
            line: self.current_line(),
            message:
                "match patterns currently support int/string/null literals, type patterns via Some(TypeName), and '_'"
                    .to_string(),
        })
    }

    fn parse_match_type_constructor_pattern(
        &mut self,
        head: &str,
    ) -> Result<Option<MatchTypePattern>, ParseError> {
        if head == "Some" {
            return self.parse_some_type_pattern();
        }
        Ok(None)
    }

    fn parse_some_type_pattern(&mut self) -> Result<Option<MatchTypePattern>, ParseError> {
        self.expect(
            &TokenKind::LParen,
            "expected '(' after Some in match type pattern",
        )?;
        let type_name = self.expect_ident("expected type name inside Some(...)")?;
        let type_pattern = match_type_pattern_from_ident(&type_name).ok_or_else(|| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message:
                "unknown match type pattern; expected one of Int/Float/Number/Bool/String/Array/Map"
                    .to_string(),
        })?;
        self.expect(
            &TokenKind::RParen,
            "expected ')' after Some(TypeName) match pattern",
        )?;
        Ok(Some(type_pattern))
    }

    fn parse_postfix_access(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        loop {
            if self.match_kind(&TokenKind::LBracket) {
                if self.match_kind(&TokenKind::Colon) {
                    let end = if self.check(&TokenKind::RBracket) {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect(&TokenKind::RBracket, "expected ']' after slice expression")?;
                    expr = self.build_slice_access_expr(expr, None, end)?;
                    continue;
                }

                let first = self.parse_expr()?;
                if self.match_kind(&TokenKind::Colon) {
                    let end = if self.check(&TokenKind::RBracket) {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect(&TokenKind::RBracket, "expected ']' after slice expression")?;
                    expr = self.build_slice_access_expr(expr, Some(first), end)?;
                    continue;
                }

                self.expect(&TokenKind::RBracket, "expected ']' after index expression")?;
                expr = self.build_builtin_call_expr(BuiltinFunction::Get, vec![expr, first])?;
                continue;
            }
            if self.match_kind(&TokenKind::Dot) {
                let member = self.expect_namespace_segment("expected member name after '.'")?;
                if member == "copy" {
                    self.expect(
                        &TokenKind::LParen,
                        "expected '(' after '.copy' (use '.copy()')",
                    )?;
                    self.expect(
                        &TokenKind::RParen,
                        "copy does not take arguments; use '.copy()'",
                    )?;
                    expr = Expr::ToOwned(Box::new(expr));
                } else if member == "length" {
                    expr = self.build_builtin_call_expr(BuiltinFunction::Len, vec![expr])?;
                } else if member == "keys" {
                    expr = self.build_builtin_call_expr(BuiltinFunction::Keys, vec![expr])?;
                } else {
                    expr = self.build_builtin_call_expr(
                        BuiltinFunction::Get,
                        vec![expr, Expr::String(member)],
                    )?;
                }
                continue;
            }
            if self.match_kind(&TokenKind::Question) {
                self.expect(&TokenKind::Dot, "expected '.' after '?' in optional access")?;
                if self.match_kind(&TokenKind::LBracket) {
                    let key = self.parse_expr()?;
                    self.expect(
                        &TokenKind::RBracket,
                        "expected ']' after optional index expression",
                    )?;
                    expr = self.build_optional_get_expr(expr, key)?;
                    continue;
                }
                let member = self.expect_namespace_segment("expected member name after '?.'")?;
                expr = self.build_optional_get_expr(expr, Expr::String(member))?;
                continue;
            }
            break;
        }
        Ok(expr)
    }

    fn build_slice_access_expr(
        &mut self,
        container: Expr,
        start: Option<Expr>,
        end: Option<Expr>,
    ) -> Result<Expr, ParseError> {
        let (container_slot, container_bind) = match container {
            Expr::Var(slot) => (slot, None),
            other => (self.allocate_hidden_local()?, Some(other)),
        };
        let start_slot = self.allocate_hidden_local()?;
        let start_expr = start.unwrap_or(Expr::Int(0));

        let slice_len = if let Some(end_expr) = end {
            let end_slot = self.allocate_hidden_local()?;
            let end_var = Expr::Var(end_slot);
            let end_is_negative = Expr::Lt(Box::new(end_var.clone()), Box::new(Expr::Int(0)));
            let len_expr = self
                .build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
            let adjusted_end = Expr::IfElse {
                condition: Box::new(end_is_negative),
                then_expr: Box::new(Expr::Add(Box::new(len_expr), Box::new(end_var.clone()))),
                else_expr: Box::new(end_var),
            };
            let slice_len = Expr::Sub(Box::new(adjusted_end), Box::new(Expr::Var(start_slot)));
            let slice_expr = self.build_builtin_call_expr(
                BuiltinFunction::Slice,
                vec![Expr::Var(container_slot), Expr::Var(start_slot), slice_len],
            )?;
            let with_end = self.bind_hidden_local_expr(end_slot, end_expr, slice_expr)?;
            self.bind_hidden_local_expr(start_slot, start_expr, with_end)?
        } else {
            let end_expr = self
                .build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
            let slice_len = Expr::Sub(Box::new(end_expr), Box::new(Expr::Var(start_slot)));
            let slice_expr = self.build_builtin_call_expr(
                BuiltinFunction::Slice,
                vec![Expr::Var(container_slot), Expr::Var(start_slot), slice_len],
            )?;
            self.bind_hidden_local_expr(start_slot, start_expr, slice_expr)?
        };
        if let Some(container_expr) = container_bind {
            self.bind_hidden_local_expr(container_slot, container_expr, slice_len)
        } else {
            Ok(slice_len)
        }
    }

    fn build_optional_get_expr(&mut self, container: Expr, key: Expr) -> Result<Expr, ParseError> {
        let container_slot = self.allocate_hidden_local()?;
        let key_slot = self.allocate_hidden_local()?;

        let is_null = self.build_type_check_expr(Expr::Var(container_slot), "null")?;
        let is_map = self.build_type_check_expr(Expr::Var(container_slot), "map")?;
        let is_array = self.build_type_check_expr(Expr::Var(container_slot), "array")?;
        let is_string = self.build_type_check_expr(Expr::Var(container_slot), "string")?;
        let map_lookup = self.build_optional_map_lookup_expr(container_slot, key_slot)?;
        let array_lookup = self.build_optional_index_lookup_expr(container_slot, key_slot)?;
        let string_lookup = self.build_optional_index_lookup_expr(container_slot, key_slot)?;
        let typed_lookup = Expr::IfElse {
            condition: Box::new(is_map),
            then_expr: Box::new(map_lookup),
            else_expr: Box::new(Expr::IfElse {
                condition: Box::new(is_array),
                then_expr: Box::new(array_lookup),
                else_expr: Box::new(Expr::IfElse {
                    condition: Box::new(is_string),
                    then_expr: Box::new(string_lookup),
                    else_expr: Box::new(Expr::Null),
                }),
            }),
        };
        let guarded = Expr::IfElse {
            condition: Box::new(is_null),
            then_expr: Box::new(Expr::Null),
            else_expr: Box::new(typed_lookup),
        };

        let key_bound = self.bind_hidden_local_expr(key_slot, key, guarded)?;
        self.bind_hidden_local_expr(container_slot, container, key_bound)
    }

    fn build_type_check_expr(&mut self, value: Expr, expected: &str) -> Result<Expr, ParseError> {
        let value_type = self.build_builtin_call_expr(BuiltinFunction::TypeOf, vec![value])?;
        Ok(Expr::Eq(
            Box::new(value_type),
            Box::new(Expr::String(expected.to_string())),
        ))
    }

    fn build_optional_map_lookup_expr(
        &mut self,
        container_slot: LocalSlot,
        key_slot: LocalSlot,
    ) -> Result<Expr, ParseError> {
        let set_probe = self.build_builtin_call_expr(
            BuiltinFunction::Set,
            vec![Expr::Var(container_slot), Expr::Var(key_slot), Expr::Null],
        )?;
        let set_probe_len = self.build_builtin_call_expr(BuiltinFunction::Len, vec![set_probe])?;
        let container_len =
            self.build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
        let key_present = Expr::Eq(Box::new(set_probe_len), Box::new(container_len));
        let value = self.build_builtin_call_expr(
            BuiltinFunction::Get,
            vec![Expr::Var(container_slot), Expr::Var(key_slot)],
        )?;
        Ok(Expr::IfElse {
            condition: Box::new(key_present),
            then_expr: Box::new(value),
            else_expr: Box::new(Expr::Null),
        })
    }

    fn build_optional_index_lookup_expr(
        &mut self,
        container_slot: LocalSlot,
        key_slot: LocalSlot,
    ) -> Result<Expr, ParseError> {
        let key_is_int = self.build_type_check_expr(Expr::Var(key_slot), "int")?;
        let key_is_negative = Expr::Lt(Box::new(Expr::Var(key_slot)), Box::new(Expr::Int(0)));
        let container_len =
            self.build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
        let key_in_bounds = Expr::Lt(Box::new(Expr::Var(key_slot)), Box::new(container_len));
        let value = self.build_builtin_call_expr(
            BuiltinFunction::Get,
            vec![Expr::Var(container_slot), Expr::Var(key_slot)],
        )?;
        let in_range_value = Expr::IfElse {
            condition: Box::new(key_in_bounds),
            then_expr: Box::new(value),
            else_expr: Box::new(Expr::Null),
        };
        let non_negative_value = Expr::IfElse {
            condition: Box::new(key_is_negative),
            then_expr: Box::new(Expr::Null),
            else_expr: Box::new(in_range_value),
        };
        Ok(Expr::IfElse {
            condition: Box::new(key_is_int),
            then_expr: Box::new(non_negative_value),
            else_expr: Box::new(Expr::Null),
        })
    }

    fn bind_hidden_local_expr(
        &mut self,
        value_slot: LocalSlot,
        value: Expr,
        body: Expr,
    ) -> Result<Expr, ParseError> {
        let result_slot = self.allocate_hidden_local()?;
        Ok(Expr::Match {
            value_slot,
            result_slot,
            value: Box::new(value),
            arms: Vec::new(),
            default: Box::new(body),
        })
    }

    fn parse_array_literal(&mut self) -> Result<Expr, ParseError> {
        let mut out = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
        if !self.check(&TokenKind::RBracket) {
            loop {
                let value = self.parse_expr()?;
                out = self.build_builtin_call_expr(BuiltinFunction::ArrayPush, vec![out, value])?;
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RBracket, "expected ']' after array literal")?;
        Ok(out)
    }

    fn parse_brace_literal(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::RBrace) {
            return self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new());
        }

        enum BraceLiteralEntry {
            Array(Expr),
            Map { key: Expr, value: Expr },
        }

        let mut entries = Vec::<BraceLiteralEntry>::new();
        let mut has_array_entries = false;
        let mut has_map_entries = false;

        loop {
            let is_map_entry = self.check_map_entry_start();
            if is_map_entry {
                has_map_entries = true;
                let key = if self.match_kind(&TokenKind::LBracket) {
                    let expr = self.parse_expr()?;
                    self.expect(
                        &TokenKind::RBracket,
                        "expected ']' after map key expression",
                    )?;
                    expr
                } else {
                    self.parse_map_key_literal()?
                };
                if !(self.match_kind(&TokenKind::Colon) || self.match_kind(&TokenKind::Equal)) {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "expected ':' or '=' after map key".to_string(),
                    });
                }
                let value = self.parse_expr()?;
                entries.push(BraceLiteralEntry::Map { key, value });
            } else {
                has_array_entries = true;
                let value = self.parse_expr()?;
                entries.push(BraceLiteralEntry::Array(value));
            }

            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }

        self.expect(&TokenKind::RBrace, "expected '}' after brace literal")?;

        if has_map_entries {
            let mut out = self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new())?;
            let mut next_array_index = 0i64;
            for entry in entries {
                match entry {
                    BraceLiteralEntry::Array(value) => {
                        out = self.build_builtin_call_expr(
                            BuiltinFunction::Set,
                            vec![out, Expr::Int(next_array_index), value],
                        )?;
                        next_array_index = next_array_index.saturating_add(1);
                    }
                    BraceLiteralEntry::Map { key, value } => {
                        out = self
                            .build_builtin_call_expr(BuiltinFunction::Set, vec![out, key, value])?;
                    }
                }
            }
            Ok(out)
        } else if has_array_entries {
            let mut out = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
            for entry in entries {
                if let BraceLiteralEntry::Array(value) = entry {
                    out =
                        self.build_builtin_call_expr(BuiltinFunction::ArrayPush, vec![out, value])?;
                }
            }
            Ok(out)
        } else {
            self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new())
        }
    }

    fn parse_map_key_literal(&mut self) -> Result<Expr, ParseError> {
        if let Some(name) = self.match_ident() {
            return Ok(Expr::String(name));
        }
        if let Some(value) = self.match_string() {
            return Ok(Expr::String(value));
        }
        if let Some(value) = self.match_int() {
            return Ok(Expr::Int(value));
        }
        self.reject_out_of_range_int_literal()?;
        if let Some(value) = self.match_float() {
            return Ok(Expr::Float(value));
        }
        if self.match_kind(&TokenKind::True) {
            return Ok(Expr::Bool(true));
        }
        if self.match_kind(&TokenKind::False) {
            return Ok(Expr::Bool(false));
        }
        if self.match_kind(&TokenKind::Null) {
            return Ok(Expr::Null);
        }
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "map keys must be identifier/string/int/float/bool/null literals".to_string(),
        })
    }

    fn check_map_entry_start(&self) -> bool {
        let Some(current) = self.tokens.get(self.pos) else {
            return false;
        };
        if matches!(current.kind, TokenKind::LBracket) {
            let mut depth = 0usize;
            let mut cursor = self.pos;
            while let Some(token) = self.tokens.get(cursor) {
                match token.kind {
                    TokenKind::LBracket => depth += 1,
                    TokenKind::RBracket => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return matches!(
                                self.tokens.get(cursor + 1),
                                Some(Token {
                                    kind: TokenKind::Colon | TokenKind::Equal,
                                    ..
                                })
                            );
                        }
                    }
                    TokenKind::Eof => return false,
                    _ => {}
                }
                cursor += 1;
            }
            return false;
        }
        let Some(next) = self.tokens.get(self.pos + 1) else {
            return false;
        };
        let is_key = matches!(
            current.kind,
            TokenKind::Ident(_)
                | TokenKind::String(_)
                | TokenKind::Int(_)
                | TokenKind::IntMinMagnitude(_)
                | TokenKind::Float(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
        );
        let is_delim = matches!(next.kind, TokenKind::Colon | TokenKind::Equal);
        is_key && is_delim
    }

    fn build_builtin_call_expr(
        &mut self,
        builtin: BuiltinFunction,
        args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        if arity != builtin.arity() {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "function '{}' expects {} arguments",
                    builtin.name(),
                    builtin.arity()
                ),
            });
        }
        Ok(Expr::Call(builtin.call_index(), args))
    }

    fn try_build_language_builtin_call(
        &mut self,
        name: &str,
        args: &[Expr],
    ) -> Result<Option<Expr>, ParseError> {
        match name {
            "print" if self.dialect.allow_macro_calls() => {
                Ok(Some(self.lower_print_call(args.to_vec())?))
            }
            "print" => Ok(Some(self.lower_plain_print_call(args.to_vec())?)),
            "println" if self.dialect.allow_macro_calls() => {
                Ok(Some(self.lower_println_call(args.to_vec())?))
            }
            "println" => Ok(Some(self.lower_plain_println_call(args.to_vec())?)),
            "type" | "typeof" => {
                if args.len() != 1 {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "type expects exactly one argument".to_string(),
                    });
                }
                Ok(Some(self.build_builtin_call_expr(
                    BuiltinFunction::TypeOf,
                    args.to_vec(),
                )?))
            }
            "assert" => Ok(Some(
                self.build_builtin_call_expr(BuiltinFunction::Assert, args.to_vec())?,
            )),
            _ => Ok(None),
        }
    }

    fn parse_macro_call(&mut self, name: &str) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LParen, "expected '(' after macro name")?;
        let _args = self.parse_call_args()?;
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("unknown macro '{name}!'"),
        })
    }

    fn lower_print_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = match args.as_slice() {
            [] => {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "print expects at least one argument".to_string(),
                });
            }
            [value] => value.clone(),
            [format_expr, format_args @ ..] => {
                let format_literal = self.expect_format_literal("print", format_expr)?;
                self.build_ruststyle_format_expr("print", format_literal, format_args.to_vec())?
            }
        };
        self.build_print_call_expr(rendered)
    }

    fn lower_plain_print_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = self.render_plain_print_args(args)?;
        self.build_print_call_expr(rendered)
    }

    fn lower_println_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = match args.as_slice() {
            [] => Expr::String("\n".to_string()),
            [value] => {
                let value = self.build_to_string_expr(value.clone())?;
                self.append_newline_expr(value)
            }
            [format_expr, format_args @ ..] => {
                let format_literal = self.expect_format_literal("println", format_expr)?;
                let rendered = self.build_ruststyle_format_expr(
                    "println",
                    format_literal,
                    format_args.to_vec(),
                )?;
                self.append_newline_expr(rendered)
            }
        };
        self.build_print_call_expr(rendered)
    }

    fn lower_plain_println_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = match args.is_empty() {
            true => Expr::String("\n".to_string()),
            false => {
                let rendered = self.render_plain_print_args(args)?;
                let value = self.build_to_string_expr(rendered)?;
                self.append_newline_expr(value)
            }
        };
        self.build_print_call_expr(rendered)
    }

    fn render_plain_print_args(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        match args.len() {
            0 => Ok(Expr::String(String::new())),
            1 => Ok(args
                .into_iter()
                .next()
                .expect("single print arg should exist")),
            _ => {
                let mut args = args.into_iter();
                let mut rendered =
                    self.build_to_string_expr(args.next().expect("first print arg should exist"))?;
                for arg in args {
                    rendered =
                        Expr::Add(Box::new(rendered), Box::new(Expr::String(" ".to_string())));
                    rendered = Expr::Add(
                        Box::new(rendered),
                        Box::new(self.build_to_string_expr(arg)?),
                    );
                }
                Ok(rendered)
            }
        }
    }

    fn expect_format_literal<'a>(
        &self,
        callee: &str,
        format_expr: &'a Expr,
    ) -> Result<&'a str, ParseError> {
        match format_expr {
            Expr::String(value) => Ok(value.as_str()),
            _ => Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "{callee} formatting requires a string literal as the first argument"
                ),
            }),
        }
    }

    fn build_ruststyle_format_expr(
        &mut self,
        callee: &str,
        format_literal: &str,
        args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        self.validate_ruststyle_format_template(callee, format_literal, args.len())?;
        let positional_args = self.build_array_expr(args)?;
        self.build_builtin_call_expr(
            BuiltinFunction::FormatTemplate,
            vec![Expr::String(format_literal.to_string()), positional_args],
        )
    }

    fn validate_ruststyle_format_template(
        &self,
        callee: &str,
        format_literal: &str,
        arg_count: usize,
    ) -> Result<(), ParseError> {
        let template_args = vec![ParserFormatArg; arg_count];
        ParsedFormat::parse(format_literal, template_args.as_slice(), &NoNamedArguments)
            .map(|_| ())
            .map_err(|offset| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("{callee} format string is invalid near byte {offset}"),
            })
    }

    fn build_array_expr(&mut self, values: Vec<Expr>) -> Result<Expr, ParseError> {
        let mut out = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
        for value in values {
            out = self.build_builtin_call_expr(BuiltinFunction::ArrayPush, vec![out, value])?;
        }
        Ok(out)
    }

    fn build_print_call_expr(&mut self, argument: Expr) -> Result<Expr, ParseError> {
        let decl = self.resolve_function_for_call(STDLIB_PRINT_NAME, 1)?;
        Ok(Expr::Call(decl.index, vec![argument]))
    }

    fn build_to_string_expr(&mut self, value: Expr) -> Result<Expr, ParseError> {
        self.build_builtin_call_expr(BuiltinFunction::ToString, vec![value])
    }

    fn append_newline_expr(&self, value: Expr) -> Expr {
        Expr::Add(Box::new(value), Box::new(Expr::String("\n".to_string())))
    }

    fn build_host_call_expr(
        &mut self,
        host_name: &str,
        args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        let decl = self.define_host_function(host_name, arity)?;
        Ok(Expr::Call(decl.index, args))
    }

    fn resolve_direct_host_call_target(&self, name: &str) -> Option<String> {
        if let Some(mapped) = self.direct_host_call_aliases.get(name) {
            return Some(mapped.clone());
        }
        if self.direct_host_wildcard_imports.len() == 1
            && let Some(namespace) = self.direct_host_wildcard_imports.iter().next()
        {
            return Some(format!("{namespace}::{name}"));
        }
        None
    }

    fn resolve_host_namespace_call_target(
        &self,
        namespace: &str,
        member: &str,
        subpath: &[String],
    ) -> Option<String> {
        let mut host_name = if let Some(host_root) = self.host_namespace_aliases.get(namespace) {
            if member.is_empty() {
                host_root.clone()
            } else {
                format!("{host_root}::{member}")
            }
        } else {
            return None;
        };

        for segment in subpath {
            host_name.push_str("::");
            host_name.push_str(segment);
        }
        Some(host_name)
    }

    fn resolve_imported_builtin_namespace<'a>(&'a self, namespace: &'a str) -> Option<&'a str> {
        let root = self.host_namespace_aliases.get(namespace)?.as_str();
        if is_builtin_namespace(root) {
            Some(root)
        } else {
            None
        }
    }

    fn resolve_builtins_call_path<'a>(
        &'a self,
        namespace: &'a str,
        member: &'a str,
        subpath: &'a [String],
    ) -> Option<(&'a str, &'a str)> {
        if subpath.is_empty()
            && let Some(imported_root) = self.resolve_imported_builtin_namespace(namespace)
        {
            return Some((imported_root, member));
        }

        None
    }

    fn try_re_namespace_builtin_call(
        &mut self,
        member: &str,
        args: &mut Vec<Expr>,
    ) -> Result<Option<BuiltinFunction>, ParseError> {
        let (builtin, base_arity, supports_optional_flags) = match member {
            "match" | "is_match" => (BuiltinFunction::ReIsMatch, 2usize, true),
            "find" => (BuiltinFunction::ReFind, 2usize, true),
            "replace" => (BuiltinFunction::ReReplace, 3usize, true),
            "split" => (BuiltinFunction::ReSplit, 2usize, true),
            "captures" => (BuiltinFunction::ReCaptures, 2usize, true),
            _ => return Ok(None),
        };

        if args.len() == base_arity {
            return Ok(Some(builtin));
        }

        if supports_optional_flags && args.len() == base_arity + 1 {
            let flags = args.pop().ok_or_else(|| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "missing regex flags argument".to_string(),
            })?;
            let pattern = args.first().cloned().ok_or_else(|| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "missing regex pattern argument".to_string(),
            })?;
            args[0] = self.apply_regex_flags_to_pattern_expr(pattern, flags)?;
            return Ok(Some(builtin));
        }

        let expected = if supports_optional_flags {
            format!("{base_arity} or {}", base_arity + 1)
        } else {
            base_arity.to_string()
        };
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("function 're::{member}' expects {expected} arguments"),
        })
    }

    fn apply_regex_flags_to_pattern_expr(
        &mut self,
        pattern: Expr,
        flags: Expr,
    ) -> Result<Expr, ParseError> {
        let prefix = self.build_builtin_call_expr(
            BuiltinFunction::Concat,
            vec![Expr::String("(?".to_string()), flags],
        )?;
        let prefix = self.build_builtin_call_expr(
            BuiltinFunction::Concat,
            vec![prefix, Expr::String(")".to_string())],
        )?;
        self.build_builtin_call_expr(BuiltinFunction::Concat, vec![prefix, pattern])
    }

    fn parse_index_assign_with_terminator(
        &mut self,
        expect_terminator: bool,
    ) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let name = self.expect_ident("expected identifier before indexed assignment")?;
        let key = if self.match_kind(&TokenKind::LBracket) {
            let key = self.parse_expr()?;
            self.expect(&TokenKind::RBracket, "expected ']' after assignment index")?;
            key
        } else if self.match_kind(&TokenKind::Dot) {
            Expr::String(self.expect_ident("expected member name after '.'")?)
        } else {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "expected '[' or '.' in indexed assignment".to_string(),
            });
        };
        self.expect(
            &TokenKind::Equal,
            "expected '=' after indexed assignment target",
        )?;
        let value = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after indexed assignment")?;
        }

        let index = self.get_local(&name)?;
        self.require_local_mutable_for_operation(index, Some(name.as_str()), line, "mutate")?;
        let expr =
            self.build_builtin_call_expr(BuiltinFunction::Set, vec![Expr::Var(index), key, value])?;
        Ok(Stmt::Assign { index, expr, line })
    }

    fn check_index_assignment_start(&self) -> bool {
        let Some(Token {
            kind: TokenKind::Ident(_),
            ..
        }) = self.tokens.get(self.pos)
        else {
            return false;
        };

        if matches!(
            (
                self.tokens.get(self.pos + 1),
                self.tokens.get(self.pos + 2),
                self.tokens.get(self.pos + 3),
            ),
            (
                Some(Token {
                    kind: TokenKind::Dot,
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Ident(_),
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Equal,
                    ..
                })
            )
        ) {
            return true;
        }

        if !matches!(
            self.tokens.get(self.pos + 1),
            Some(Token {
                kind: TokenKind::LBracket,
                ..
            })
        ) {
            return false;
        }

        let mut depth = 0usize;
        let mut cursor = self.pos + 1;
        while let Some(token) = self.tokens.get(cursor) {
            match token.kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(cursor + 1),
                            Some(Token {
                                kind: TokenKind::Equal,
                                ..
                            })
                        );
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            cursor += 1;
        }
        false
    }

    fn check_parenthesized_arrow_closure_start(&self) -> bool {
        if !self.check(&TokenKind::LParen) {
            return false;
        }
        let mut cursor = self.pos + 1;
        if self.check_kind_at(cursor, &TokenKind::RParen) {
            return self.check_kind_at(cursor + 1, &TokenKind::FatArrow);
        }

        let mut expect_ident = true;
        while cursor < self.tokens.len() {
            if expect_ident {
                if !self.check_ident_at(cursor) {
                    return false;
                }
                expect_ident = false;
                cursor += 1;
                continue;
            }
            if self.check_kind_at(cursor, &TokenKind::Comma) {
                expect_ident = true;
                cursor += 1;
                continue;
            }
            if self.check_kind_at(cursor, &TokenKind::RParen) {
                return self.check_kind_at(cursor + 1, &TokenKind::FatArrow);
            }
            return false;
        }
        false
    }

    fn check_single_param_arrow_closure_start(&self) -> bool {
        self.check_ident_at(self.pos) && self.check_kind_at(self.pos + 1, &TokenKind::FatArrow)
    }

    fn parse_parenthesized_arrow_closure(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LParen, "expected '(' to start arrow parameters")?;
        let mut params = Vec::<String>::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                params.push(self.expect_ident("expected arrow parameter name")?);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after arrow parameters")?;
        self.expect(&TokenKind::FatArrow, "expected '=>' after arrow parameters")?;
        if self.check(&TokenKind::LBrace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "arrow closures with block bodies are not supported in this subset"
                    .to_string(),
            });
        }
        self.parse_closure_expr_with_params(params)
    }

    fn parse_single_param_arrow_closure(&mut self) -> Result<Expr, ParseError> {
        let param = self.expect_ident("expected arrow parameter name")?;
        self.expect(&TokenKind::FatArrow, "expected '=>' after arrow parameter")?;
        if self.check(&TokenKind::LBrace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "arrow closures with block bodies are not supported in this subset"
                    .to_string(),
            });
        }
        self.parse_closure_expr_with_params(vec![param])
    }

    fn try_parse_js_dotted_call(&mut self, base: &str) -> Result<Option<Expr>, ParseError> {
        let save_pos = self.pos;
        if !self.match_kind(&TokenKind::Dot) {
            return Ok(None);
        }

        let mut segments = Vec::<String>::new();
        loop {
            let member = self.expect_namespace_segment("expected member name after '.'")?;
            segments.push(member);
            if self.match_kind(&TokenKind::Dot) {
                continue;
            }
            break;
        }

        if !self.match_kind(&TokenKind::LParen) {
            self.pos = save_pos;
            return Ok(None);
        }
        let mut args = self.parse_call_args()?;

        if base == "console" && segments.len() == 1 && segments[0] == "log" {
            return Ok(Some(
                self.lower_plain_print_call(std::mem::take(&mut args))?,
            ));
        }

        if segments.is_empty() {
            self.pos = save_pos;
            return Ok(None);
        }

        if let Some(imported_root) = self.resolve_imported_builtin_namespace(base) {
            let imported_root = imported_root.to_string();
            if segments.len() != 1 {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "unknown builtin function '{}::{}'",
                        imported_root,
                        segments.join("::")
                    ),
                });
            }
            let member = segments[0].as_str();
            if namespace_supports_regex_flags(&imported_root) {
                if let Some(builtin) = self.try_re_namespace_builtin_call(member, &mut args)? {
                    return Ok(Some(self.build_builtin_call_expr(builtin, args)?));
                }
            } else if let Some(builtin) = resolve_builtin_namespace_call(&imported_root, member) {
                return Ok(Some(self.build_builtin_call_expr(builtin, args)?));
            }
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("unknown builtin function '{}::{}'", imported_root, member),
            });
        }

        let member = segments[0].clone();
        let subpath = segments.into_iter().skip(1).collect::<Vec<_>>();
        if let Some(host_name) = self.resolve_host_namespace_call_target(base, &member, &subpath) {
            return Ok(Some(self.build_host_call_expr(&host_name, args)?));
        }

        self.pos = save_pos;
        Ok(None)
    }

    fn parse_closure_literal(&mut self) -> Result<Expr, ParseError> {
        let mut params = Vec::<String>::new();
        if !self.check(&TokenKind::Pipe) {
            loop {
                params.push(self.expect_ident("expected closure parameter name")?);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::Pipe, "expected '|' after closure parameters")?;
        self.parse_closure_expr_with_params(params)
    }

    fn parse_closure_expr_with_params(&mut self, params: Vec<String>) -> Result<Expr, ParseError> {
        let mut param_slots = Vec::new();
        let mut param_scope = HashMap::new();
        for param_name in &params {
            if param_scope.contains_key(param_name) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("duplicate closure parameter '{param_name}'"),
                });
            }
            let slot = self.allocate_hidden_local()?;
            param_scope.insert(param_name.clone(), slot);
            param_slots.push(slot);
        }

        self.closure_scopes.push(param_scope);
        self.closure_capture_contexts.push(ClosureCaptureContext {
            by_name: HashMap::new(),
            capture_copies: Vec::new(),
        });
        let body = self.parse_expr()?;
        let capture_context = self
            .closure_capture_contexts
            .pop()
            .ok_or_else(|| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "internal closure capture state error".to_string(),
            })?;
        self.closure_scopes.pop();
        Ok(Expr::Closure(ClosureExpr {
            param_slots,
            capture_copies: capture_context.capture_copies,
            body: Box::new(body),
        }))
    }

    fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after arguments")?;
        Ok(args)
    }

    fn expect(&mut self, kind: &TokenKind, message: &str) -> Result<(), ParseError> {
        if self.match_kind(kind) {
            Ok(())
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    fn expect_ident(&mut self, message: &str) -> Result<String, ParseError> {
        if let Some(name) = self.match_ident() {
            Ok(name)
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    fn expect_string_literal(&mut self, message: &str) -> Result<String, ParseError> {
        if let Some(value) = self.match_string() {
            Ok(value)
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    fn expect_namespace_segment(&mut self, message: &str) -> Result<String, ParseError> {
        if let Some(name) = self.match_namespace_segment() {
            Ok(name)
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    fn get_local(&mut self, name: &str) -> Result<LocalSlot, ParseError> {
        if let Some(current_scope) = self.closure_scopes.last()
            && let Some(&index) = current_scope.get(name)
        {
            return Ok(index);
        }

        if self.closure_scopes.len() > 1 {
            for scope in self.closure_scopes[..self.closure_scopes.len() - 1]
                .iter()
                .rev()
            {
                if let Some(&source_index) = scope.get(name) {
                    return self.capture_or_direct_local(name, source_index);
                }
            }
        }

        if let Some(source_index) = self.locals.get(name).copied() {
            return self.capture_or_direct_local(name, source_index);
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("unknown local '{name}'"),
        })
    }

    fn capture_or_direct_local(
        &mut self,
        name: &str,
        source_index: LocalSlot,
    ) -> Result<LocalSlot, ParseError> {
        if let Some(capture_idx) = self.closure_capture_contexts.len().checked_sub(1) {
            if let Some(&captured_slot) =
                self.closure_capture_contexts[capture_idx].by_name.get(name)
            {
                return Ok(captured_slot);
            }
            let captured_slot = self.allocate_hidden_local()?;
            let source_mutable = self.is_local_slot_mutable(source_index);
            self.set_local_slot_mutable(captured_slot, source_mutable);
            self.closure_capture_contexts[capture_idx]
                .by_name
                .insert(name.to_string(), captured_slot);
            self.closure_capture_contexts[capture_idx]
                .capture_copies
                .push((source_index, captured_slot));
            return Ok(captured_slot);
        }
        Ok(source_index)
    }

    fn has_local_binding(&self, name: &str) -> bool {
        for scope in self.closure_scopes.iter().rev() {
            if scope.contains_key(name) {
                return true;
            }
        }
        self.locals.contains_key(name)
    }

    fn resolve_function_for_call(
        &mut self,
        name: &str,
        arg_count: usize,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(decl) = self.functions.get(name).cloned() {
            if decl.arity as usize != arg_count {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", decl.arity),
                });
            }
            return Ok(decl);
        }

        if name == STDLIB_PRINT_NAME {
            let arg_arity = u8::try_from(arg_count).map_err(|_| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "function arity too large".to_string(),
            })?;
            if arg_arity != STDLIB_PRINT_ARITY {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "function '{STDLIB_PRINT_NAME}' expects {STDLIB_PRINT_ARITY} arguments"
                    ),
                });
            }
            return self.define_builtin_function(STDLIB_PRINT_NAME, STDLIB_PRINT_ARITY);
        }
        if self.allow_implicit_externs {
            let arity = u8::try_from(arg_count).map_err(|_| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "function arity too large".to_string(),
            })?;
            return self.define_external_function(name, arity);
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("unknown function '{name}'"),
        })
    }

    fn define_builtin_function(
        &mut self,
        name: &str,
        arity: u8,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args: (0..arity).map(|idx| format!("arg{idx}")).collect(),
            exported: true,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    fn define_external_function(
        &mut self,
        name: &str,
        arity: u8,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            if existing.arity != arity {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", existing.arity),
                });
            }
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let args = (0..arity).map(|idx| format!("arg{idx}")).collect();
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args,
            exported: true,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    fn define_host_function(&mut self, name: &str, arity: u8) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            if existing.arity != arity {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", existing.arity),
                });
            }
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let args = (0..arity).map(|idx| format!("arg{idx}")).collect();
        let decl = FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args,
            exported: false,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    fn get_or_assign_local(&mut self, name: &str) -> Result<(LocalSlot, bool), ParseError> {
        if let Some(&index) = self.locals.get(name) {
            return Ok((index, false));
        }
        let index = self.allocate_hidden_local()?;
        self.locals.insert(name.to_string(), index);
        Ok((index, true))
    }

    fn allocate_hidden_local(&mut self) -> Result<LocalSlot, ParseError> {
        let index = self.next_local;
        self.next_local = self.next_local.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "local index overflow".to_string(),
        })?;
        self.mutable_locals.push(true);
        Ok(index)
    }

    fn match_kind(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn check_path_separator(&self) -> bool {
        matches!(
            (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)),
            (
                Some(Token {
                    kind: TokenKind::Colon,
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Colon,
                    ..
                })
            )
        )
    }

    fn match_path_separator(&mut self) -> bool {
        if self.check_path_separator() {
            self.pos += 2;
            true
        } else {
            false
        }
    }

    fn match_int(&mut self) -> Option<i64> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::Int(value),
                ..
            }) => {
                let value = *value;
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    fn match_int_min_magnitude(&mut self) -> Option<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::IntMinMagnitude(value),
                ..
            }) => {
                let value = value.clone();
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    fn match_float(&mut self) -> Option<f64> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::Float(value),
                ..
            }) => {
                let value = *value;
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    fn match_ident(&mut self) -> Option<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::Ident(name),
                ..
            }) => {
                let name = name.clone();
                self.pos += 1;
                Some(name)
            }
            _ => None,
        }
    }

    fn match_namespace_segment(&mut self) -> Option<String> {
        if let Some(name) = self.match_ident() {
            return Some(name);
        }
        if self.match_kind(&TokenKind::Match) {
            return Some("match".to_string());
        }
        None
    }

    fn match_string(&mut self) -> Option<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::String(value),
                ..
            }) => {
                let value = value.clone();
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    fn check_kind_at(&self, index: usize, kind: &TokenKind) -> bool {
        matches!(
            self.tokens.get(index),
            Some(token) if std::mem::discriminant(&token.kind) == std::mem::discriminant(kind)
        )
    }

    fn check_ident_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index),
            Some(Token {
                kind: TokenKind::Ident(_),
                ..
            })
        )
    }

    fn check_string_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index),
            Some(Token {
                kind: TokenKind::String(_),
                ..
            })
        )
    }

    fn check_ident_literal_at(&self, index: usize, literal: &str) -> bool {
        matches!(
            self.tokens.get(index),
            Some(Token {
                kind: TokenKind::Ident(name),
                ..
            }) if name == literal
        )
    }

    fn check_ident_literal(&self, literal: &str) -> bool {
        self.check_ident_literal_at(self.pos, literal)
    }

    fn reject_out_of_range_int_literal(&self) -> Result<(), ParseError> {
        let Some(Token {
            kind: TokenKind::IntMinMagnitude(text),
            ..
        }) = self.tokens.get(self.pos)
        else {
            return Ok(());
        };
        Err(ParseError {
            span: Some(self.current_span()),
            code: None,
            line: self.current_line(),
            message: format!(
                "integer literal '{text}' is out of range for i64; write '-{text}' for i64::MIN"
            ),
        })
    }

    fn match_ident_literal(&mut self, literal: &str) -> bool {
        if self.check_ident_literal(literal) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn check_assignment_start(&self) -> bool {
        matches!(
            (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)),
            (
                Some(Token {
                    kind: TokenKind::Ident(_),
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Equal,
                    ..
                })
            )
        )
    }

    fn consume_stmt_terminator(&mut self, message: &str) -> Result<(), ParseError> {
        if self.match_kind(&TokenKind::Semicolon) {
            return Ok(());
        }
        if !self.allow_implicit_semicolons {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: message.to_string(),
            });
        }

        if self.check(&TokenKind::RBrace) || self.check(&TokenKind::Eof) {
            return Ok(());
        }

        let previous_line = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|token| token.line)
            .unwrap_or(1);
        if self.current_line() > previous_line {
            return Ok(());
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: message.to_string(),
        })
    }

    fn starts_if_expr_branch_statement(&self) -> bool {
        if self.check(&TokenKind::Pub)
            || self.check(&TokenKind::Use)
            || self.check(&TokenKind::Fn)
            || self.check(&TokenKind::Let)
            || self.check(&TokenKind::For)
            || self.check(&TokenKind::While)
            || self.check(&TokenKind::Break)
            || self.check(&TokenKind::Continue)
            || self.check_assignment_start()
            || self.check_index_assignment_start()
        {
            return true;
        }
        self.check(&TokenKind::If) && !self.check_if_expression_start()
    }

    fn check_if_expression_start(&self) -> bool {
        if !self.check(&TokenKind::If) {
            return false;
        }
        let mut cursor = self.pos + 1;
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut brace_depth = 0usize;

        while let Some(token) = self.tokens.get(cursor) {
            match token.kind {
                TokenKind::LParen => paren_depth += 1,
                TokenKind::RParen => {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                TokenKind::LBracket => bracket_depth += 1,
                TokenKind::RBracket => {
                    bracket_depth = bracket_depth.saturating_sub(1);
                }
                TokenKind::LBrace => {
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                        return false;
                    }
                    brace_depth += 1;
                }
                TokenKind::RBrace => {
                    if brace_depth == 0 {
                        return false;
                    }
                    brace_depth -= 1;
                }
                TokenKind::FatArrow
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    return true;
                }
                TokenKind::Semicolon | TokenKind::Eof
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    return false;
                }
                _ => {}
            }
            cursor += 1;
        }
        false
    }

    fn check(&self, kind: &TokenKind) -> bool {
        matches!(self.peek_kind(), Some(k) if std::mem::discriminant(k) == std::mem::discriminant(kind))
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos).map(|token| &token.kind)
    }

    fn current_line(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|token| token.line)
            .unwrap_or(1)
    }

    fn current_line_u32(&self) -> u32 {
        u32::try_from(self.current_line()).unwrap_or(u32::MAX)
    }

    fn current_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|token| token.span)
            .unwrap_or_else(|| {
                self.tokens
                    .last()
                    .map(|token| Span::new(token.span.source_id, token.span.hi, token.span.hi))
                    .unwrap_or(Span::new(0, 0, 0))
            })
    }

    fn last_line(&self) -> u32 {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map(|token| token.line)
            .unwrap_or(1) as u32
    }

    pub(super) fn local_count(&self) -> usize {
        self.next_local as usize
    }

    pub(super) fn function_decls(&self) -> Vec<FunctionDecl> {
        self.function_list.clone()
    }

    pub(super) fn function_impls(&self) -> HashMap<u16, FunctionImpl> {
        self.function_impls.clone()
    }

    pub(super) fn local_bindings(&self) -> Vec<(String, LocalSlot)> {
        let mut locals: Vec<(String, LocalSlot)> = self
            .locals
            .iter()
            .map(|(name, index)| (name.clone(), *index))
            .collect();
        locals.sort_by_key(|(_, index)| *index);
        locals
    }
}

fn match_type_pattern_from_ident(name: &str) -> Option<MatchTypePattern> {
    match name {
        "Int" | "int" => Some(MatchTypePattern::Int),
        "Float" | "float" => Some(MatchTypePattern::Float),
        "Number" | "number" => Some(MatchTypePattern::Number),
        "Bool" | "bool" => Some(MatchTypePattern::Bool),
        "String" | "string" => Some(MatchTypePattern::String),
        "Array" | "array" => Some(MatchTypePattern::Array),
        "Map" | "map" => Some(MatchTypePattern::Map),
        _ => None,
    }
}
