use super::super::ParseError;
use super::{is_ident_continue, is_ident_start};

#[derive(Clone, Debug)]
pub(super) struct SchemeForm {
    pub(super) line: usize,
    pub(super) node: SchemeNode,
}

impl SchemeForm {
    pub(super) fn as_symbol(&self) -> Option<&str> {
        match &self.node {
            SchemeNode::Symbol(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn as_list(&self) -> Option<&[SchemeForm]> {
        match &self.node {
            SchemeNode::List(values) => Some(values),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum SchemeNode {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    String(String),
    Symbol(String),
    List(Vec<SchemeForm>),
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    LParen,
    RParen,
    Quote,
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    String(String),
    Symbol(String),
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    line: usize,
}

struct SchemeLexer<'a> {
    chars: std::str::Chars<'a>,
    current: Option<char>,
    line: usize,
}

impl<'a> SchemeLexer<'a> {
    fn new(source: &'a str) -> Self {
        let mut chars = source.chars();
        let current = chars.next();
        Self {
            chars,
            current,
            line: 1,
        }
    }

    fn next_token(&mut self) -> Result<Token, ParseError> {
        self.skip_whitespace_and_comments();
        let line = self.line;

        let token = match self.current {
            None => TokenKind::Eof,
            Some('(') => {
                self.advance();
                TokenKind::LParen
            }
            Some(')') => {
                self.advance();
                TokenKind::RParen
            }
            Some('\'') => {
                self.advance();
                TokenKind::Quote
            }
            Some('"') => TokenKind::String(self.consume_string()?),
            Some(_) => {
                let atom = self.consume_atom();
                self.classify_atom(atom, line)?
            }
        };

        Ok(Token { kind: token, line })
    }

    fn advance(&mut self) {
        if self.current == Some('\n') {
            self.line += 1;
        }
        self.current = self.chars.next();
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            while matches!(self.current, Some(ch) if ch.is_whitespace()) {
                self.advance();
            }

            if self.current == Some(';') {
                while let Some(ch) = self.current {
                    self.advance();
                    if ch == '\n' {
                        break;
                    }
                }
                continue;
            }

            if self.current == Some('#') {
                let saved = self.chars.clone();
                let saved_line = self.line;
                self.advance();
                if self.current == Some('|') {
                    self.advance();
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.current {
                            None => break,
                            Some('#') => {
                                self.advance();
                                if self.current == Some('|') {
                                    self.advance();
                                    depth += 1;
                                }
                            }
                            Some('|') => {
                                self.advance();
                                if self.current == Some('#') {
                                    self.advance();
                                    depth -= 1;
                                }
                            }
                            _ => self.advance(),
                        }
                    }
                    continue;
                } else {
                    self.chars = saved;
                    self.line = saved_line;
                    self.current = Some('#');
                }
            }

            break;
        }
    }

    fn consume_string(&mut self) -> Result<String, ParseError> {
        let line = self.line;
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
                '"' => {
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
                        '0' => '\0',
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

    fn consume_atom(&mut self) -> String {
        let mut out = String::new();
        while let Some(ch) = self.current {
            if is_scheme_delimiter(ch) {
                break;
            }
            out.push(ch);
            self.advance();
        }
        out
    }

    fn classify_atom(&self, atom: String, line: usize) -> Result<TokenKind, ParseError> {
        if atom.is_empty() {
            return Err(ParseError {
                span: None,
                code: None,
                line,
                message: "expected token".to_string(),
            });
        }

        if atom == "#t" || atom == "#true" {
            return Ok(TokenKind::Bool(true));
        }
        if atom == "#f" || atom == "#false" {
            return Ok(TokenKind::Bool(false));
        }

        if let Some(rest) = atom.strip_prefix("#\\") {
            let ch = match rest {
                "space" => ' ',
                "newline" => '\n',
                "tab" => '\t',
                "return" => '\r',
                "nul" | "null" => '\0',
                s if s.chars().count() == 1 => s.chars().next().unwrap(),
                _ => {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: format!("unknown character literal '#\\{rest}'"),
                    });
                }
            };
            return Ok(TokenKind::Char(ch));
        }

        if let Some(kind) = parse_number_atom(&atom) {
            return Ok(kind);
        }

        Ok(TokenKind::Symbol(atom))
    }
}

fn is_scheme_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '(' | ')' | ';' | '\'' | '"')
}

fn parse_number_atom(atom: &str) -> Option<TokenKind> {
    if atom.is_empty() {
        return None;
    }

    let body = atom.strip_prefix('-').unwrap_or(atom);
    if body.is_empty() || !body.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }

    if body.contains('.') {
        return atom.parse::<f64>().ok().map(TokenKind::Float);
    }

    if body.chars().all(|ch| ch.is_ascii_digit()) {
        return atom.parse::<i64>().ok().map(TokenKind::Int);
    }

    None
}

pub(super) struct SchemeParser {
    tokens: Vec<Token>,
    pos: usize,
}

impl SchemeParser {
    pub(super) fn new(source: &str) -> Result<Self, ParseError> {
        let mut lexer = SchemeLexer::new(source);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token()?;
            let is_eof = matches!(token.kind, TokenKind::Eof);
            tokens.push(token);
            if is_eof {
                break;
            }
        }

        Ok(Self { tokens, pos: 0 })
    }

    pub(super) fn parse_program(&mut self) -> Result<Vec<SchemeForm>, ParseError> {
        let mut forms = Vec::new();
        while !self.check_eof() {
            forms.push(self.parse_form()?);
        }
        Ok(forms)
    }

    fn parse_form(&mut self) -> Result<SchemeForm, ParseError> {
        while self.check_datum_comment() {
            self.advance();
            if self.check_eof() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current().line,
                    message: "expected form after #;".to_string(),
                });
            }
            self.parse_form()?;
        }

        let token = self.advance().clone();
        match token.kind {
            TokenKind::LParen => self.parse_list(token.line),
            TokenKind::RParen => Err(ParseError {
                span: None,
                code: None,
                line: token.line,
                message: "unexpected ')'".to_string(),
            }),
            TokenKind::Quote => {
                let inner = self.parse_form()?;
                Ok(SchemeForm {
                    line: token.line,
                    node: SchemeNode::List(vec![
                        SchemeForm {
                            line: token.line,
                            node: SchemeNode::Symbol("quote".to_string()),
                        },
                        inner,
                    ]),
                })
            }
            TokenKind::Int(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Int(value),
            }),
            TokenKind::Float(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Float(value),
            }),
            TokenKind::Bool(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Bool(value),
            }),
            TokenKind::Char(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Char(value),
            }),
            TokenKind::String(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::String(value),
            }),
            TokenKind::Symbol(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Symbol(value),
            }),
            TokenKind::Eof => Err(ParseError {
                span: None,
                code: None,
                line: token.line,
                message: "unexpected end of input".to_string(),
            }),
        }
    }

    fn check_datum_comment(&self) -> bool {
        matches!(&self.current().kind, TokenKind::Symbol(s) if s == "#;")
    }

    fn parse_list(&mut self, line: usize) -> Result<SchemeForm, ParseError> {
        let mut items = Vec::new();
        while !self.check_rparen() {
            if self.check_eof() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line,
                    message: "unterminated list".to_string(),
                });
            }
            items.push(self.parse_form()?);
        }

        let _ = self.advance();
        Ok(SchemeForm {
            line,
            node: SchemeNode::List(items),
        })
    }

    fn check_rparen(&self) -> bool {
        matches!(self.current().kind, TokenKind::RParen)
    }

    fn check_eof(&self) -> bool {
        matches!(self.current().kind, TokenKind::Eof)
    }

    fn current(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or_else(|| {
            self.tokens
                .last()
                .expect("scheme parser token stream is never empty")
        })
    }

    fn advance(&mut self) -> &Token {
        let idx = self.pos;
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        self.tokens.get(idx).unwrap_or_else(|| {
            self.tokens
                .last()
                .expect("scheme parser token stream is never empty")
        })
    }
}

pub(super) fn is_valid_member_ident(member: &str) -> bool {
    let mut chars = member.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
}

pub(super) fn canonicalize_call_path(path: &str) -> Option<Vec<String>> {
    let canonical = path.replace("::", ".").replace(':', ".");
    if !canonical.contains('.') {
        return canonicalize_identifier(path).map(|value| vec![value]);
    }

    let mut segments = Vec::new();
    for segment in canonical.split('.') {
        if segment.is_empty() {
            return None;
        }
        segments.push(canonicalize_identifier(segment)?);
    }
    if segments.len() < 2 {
        return None;
    }
    Some(segments)
}

pub(super) fn split_namespace_segments(
    head: &str,
    line: usize,
) -> Result<Option<Vec<String>>, ParseError> {
    if !head.replace("::", ".").replace(':', ".").contains('.') {
        return Ok(None);
    }
    canonicalize_call_path(head)
        .map(Some)
        .ok_or_else(|| ParseError {
            span: None,
            code: None,
            line,
            message: format!("invalid namespace call target '{head}'"),
        })
}

pub(super) fn is_forbidden_scheme_builtin_name(name: &str) -> bool {
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
            | "re_match"
            | "re_find"
            | "re_replace"
            | "re_split"
            | "re_captures"
    )
}

pub(super) fn scheme_builtin_syntax_hint(name: &str) -> &'static str {
    match name {
        "len" | "count" => "use (length value)",
        "type_of" => "use (type value) or (type-of value)",
        "get" => "use (vector-ref v i) or (hash-ref m k)",
        "set" => "use (vector-set! v i x) or (hash-set! m k x)",
        "concat" => "use (+ a b) for strings or (append xs ys) for lists",
        "slice" => "use (slice-range ...), (slice-to ...), or (slice-from ...)",
        "io_open" | "io_popen" | "io_read_all" | "io_read_line" | "io_write" | "io_flush"
        | "io_close" | "io_exists" => "use io namespace syntax (for example io::open)",
        "re_match" | "re_find" | "re_replace" | "re_split" | "re_captures" => {
            "use re namespace syntax (for example re::match)"
        }
        _ => "use Scheme frontend forms instead of VM builtin helpers",
    }
}

pub(super) fn fold_int_add(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_add(rhs))
}

#[inline]
pub(super) fn fold_int_sub(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_sub(rhs))
}

#[inline]
pub(super) fn fold_int_mul(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_mul(rhs))
}

#[inline]
pub(super) fn fold_int_div(lhs: i64, rhs: i64) -> Option<i64> {
    if rhs == 0 || (lhs == i64::MIN && rhs == -1) {
        return None;
    }
    Some(lhs / rhs)
}

pub(super) fn normalize_identifier(
    name: &str,
    line: usize,
    context: &str,
) -> Result<String, ParseError> {
    if name.is_empty() {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("{context} cannot be empty"),
        });
    }

    let Some(out) = canonicalize_identifier(name) else {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("unsupported identifier '{name}' in {context}"),
        });
    };

    if is_reserved_identifier(&out) {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("identifier '{name}' is reserved"),
        });
    }

    Ok(out)
}

pub(super) fn canonicalize_identifier(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }

    let mut out = String::new();
    for ch in name.chars() {
        let mapped = if ch == '-' { '_' } else { ch };
        out.push(mapped);
    }

    let mut chars = out.chars();
    let first = chars.next()?;
    if !is_ident_start(first) || !chars.all(is_ident_continue) {
        return None;
    }
    Some(out)
}

fn is_reserved_identifier(name: &str) -> bool {
    matches!(
        name,
        "fn" | "let" | "for" | "if" | "else" | "while" | "break" | "continue" | "true" | "false"
    )
}
