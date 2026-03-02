use super::super::ParseError;
use super::super::ir::{Expr, FrontendIr, LocalIrBuilder, Stmt};
use super::{is_ident_continue, is_ident_start};

pub(super) fn lower_to_ir(source: &str) -> Result<FrontendIr, ParseError> {
    if let Some(ir) = try_lower_direct_subset_to_ir(source)? {
        return Ok(ir);
    }
    Err(ParseError {
        span: None,
        code: None,
        line: 1,
        message: "unsupported Scheme syntax".to_string(),
    })
}

fn try_lower_direct_subset_to_ir(source: &str) -> Result<Option<FrontendIr>, ParseError> {
    let mut parser = SchemeParser::new(source)?;
    let forms = parser.parse_program()?;

    let mut builder = LocalIrBuilder::new();
    let mut stmts = Vec::<Stmt>::new();
    for form in &forms {
        let Some(mut lowered) = lower_scheme_direct_stmt(form, &mut builder)? else {
            return Ok(None);
        };
        stmts.append(&mut lowered);
    }
    Ok(Some(builder.finish(stmts)))
}

fn lower_scheme_direct_stmt(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
) -> Result<Option<Vec<Stmt>>, ParseError> {
    if let Some(items) = form.as_list()
        && let Some(head) = items.first().and_then(|item| item.as_symbol())
    {
        let args = &items[1..];
        let line = u32::try_from(form.line).unwrap_or(u32::MAX);
        match head {
            "define" => {
                if args.len() != 2 {
                    return Ok(None);
                }
                let Some(name_raw) = args[0].as_symbol() else {
                    return Ok(None);
                };
                let name = normalize_identifier(name_raw, args[0].line, "define target")?;
                let Some(expr) = lower_scheme_direct_expr(&args[1], builder)? else {
                    return Ok(None);
                };
                return Ok(Some(vec![builder.lower_local(&name, expr, line)?]));
            }
            "set!" => {
                if args.len() != 2 {
                    return Ok(None);
                }
                let Some(name_raw) = args[0].as_symbol() else {
                    return Ok(None);
                };
                let name = normalize_identifier(name_raw, args[0].line, "set! target")?;
                let Some(expr) = lower_scheme_direct_expr(&args[1], builder)? else {
                    return Ok(None);
                };
                return Ok(Some(vec![builder.lower_assign(&name, expr, line)?]));
            }
            "if" => {
                if !(2..=3).contains(&args.len()) {
                    return Ok(None);
                }
                let Some(condition) = lower_scheme_direct_expr(&args[0], builder)? else {
                    return Ok(None);
                };
                let Some(then_branch) = lower_scheme_direct_branch(&args[1], builder)? else {
                    return Ok(None);
                };
                let else_branch = if args.len() == 3 {
                    let Some(branch) = lower_scheme_direct_branch(&args[2], builder)? else {
                        return Ok(None);
                    };
                    branch
                } else {
                    Vec::new()
                };
                return Ok(Some(vec![Stmt::IfElse {
                    condition,
                    then_branch,
                    else_branch,
                    line,
                }]));
            }
            "while" => {
                if args.is_empty() {
                    return Ok(None);
                }
                let Some(condition) = lower_scheme_direct_expr(&args[0], builder)? else {
                    return Ok(None);
                };
                let mut body = Vec::new();
                for body_form in &args[1..] {
                    let Some(mut lowered) = lower_scheme_direct_stmt(body_form, builder)? else {
                        return Ok(None);
                    };
                    body.append(&mut lowered);
                }
                return Ok(Some(vec![Stmt::While {
                    condition,
                    body,
                    line,
                }]));
            }
            "begin" => {
                let mut out = Vec::new();
                for expr in args {
                    let Some(mut lowered) = lower_scheme_direct_stmt(expr, builder)? else {
                        return Ok(None);
                    };
                    out.append(&mut lowered);
                }
                return Ok(Some(out));
            }
            _ => {}
        }
    }

    let line = u32::try_from(form.line).unwrap_or(u32::MAX);
    let Some(expr) = lower_scheme_direct_expr(form, builder)? else {
        return Ok(None);
    };
    Ok(Some(vec![Stmt::Expr { expr, line }]))
}

fn lower_scheme_direct_branch(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
) -> Result<Option<Vec<Stmt>>, ParseError> {
    if let Some(items) = form.as_list()
        && items
            .first()
            .and_then(|item| item.as_symbol())
            .is_some_and(|head| head == "begin")
    {
        let mut out = Vec::new();
        for nested in &items[1..] {
            let Some(mut lowered) = lower_scheme_direct_stmt(nested, builder)? else {
                return Ok(None);
            };
            out.append(&mut lowered);
        }
        return Ok(Some(out));
    }
    let line = u32::try_from(form.line).unwrap_or(u32::MAX);
    let Some(expr) = lower_scheme_direct_expr(form, builder)? else {
        return Ok(None);
    };
    Ok(Some(vec![Stmt::Expr { expr, line }]))
}

fn lower_scheme_direct_expr(
    form: &SchemeForm,
    builder: &LocalIrBuilder,
) -> Result<Option<Expr>, ParseError> {
    match &form.node {
        SchemeNode::Int(value) => Ok(Some(Expr::Int(*value))),
        SchemeNode::Float(value) => Ok(Some(Expr::Float(*value))),
        SchemeNode::Bool(value) => Ok(Some(Expr::Bool(*value))),
        SchemeNode::Char(value) => Ok(Some(Expr::String(value.to_string()))),
        SchemeNode::String(value) => Ok(Some(Expr::String(value.clone()))),
        SchemeNode::Symbol(symbol) => {
            if symbol == "null" || symbol == "nil" {
                return Ok(Some(Expr::Null));
            }
            if symbol == "true" {
                return Ok(Some(Expr::Bool(true)));
            }
            if symbol == "false" {
                return Ok(Some(Expr::Bool(false)));
            }
            let name = normalize_identifier(symbol, form.line, "symbol")?;
            Ok(builder.resolve_local_expr(&name))
        }
        SchemeNode::List(items) => lower_scheme_direct_list_expr(items, builder),
    }
}

fn lower_scheme_direct_list_expr(
    items: &[SchemeForm],
    builder: &LocalIrBuilder,
) -> Result<Option<Expr>, ParseError> {
    let Some(head) = items.first().and_then(|item| item.as_symbol()) else {
        return Ok(None);
    };
    let args = &items[1..];

    match head {
        "+" => lower_scheme_direct_fold(args, builder, Expr::Add, fold_int_add),
        "*" => lower_scheme_direct_fold(args, builder, Expr::Mul, fold_int_mul),
        "-" => {
            if args.is_empty() {
                return Ok(None);
            }
            if args.len() == 1 {
                let Some(inner) = lower_scheme_direct_expr(&args[0], builder)? else {
                    return Ok(None);
                };
                return Ok(Some(Expr::Neg(Box::new(inner))));
            }
            lower_scheme_direct_fold(args, builder, Expr::Sub, fold_int_sub)
        }
        "/" => lower_scheme_direct_fold(args, builder, Expr::Div, fold_int_div),
        "modulo" | "remainder" => lower_scheme_direct_binary(args, builder, Expr::Mod),
        "=" => lower_scheme_direct_compare_fold(args, builder, Expr::Eq),
        "/=" => {
            let Some(eq) = lower_scheme_direct_compare_fold(args, builder, Expr::Eq)? else {
                return Ok(None);
            };
            Ok(Some(Expr::Not(Box::new(eq))))
        }
        "<" => lower_scheme_direct_compare_fold(args, builder, Expr::Lt),
        ">" => lower_scheme_direct_compare_fold(args, builder, Expr::Gt),
        "<=" => {
            let Some(gt) = lower_scheme_direct_compare_fold(args, builder, Expr::Gt)? else {
                return Ok(None);
            };
            Ok(Some(Expr::Not(Box::new(gt))))
        }
        ">=" => {
            let Some(lt) = lower_scheme_direct_compare_fold(args, builder, Expr::Lt)? else {
                return Ok(None);
            };
            Ok(Some(Expr::Not(Box::new(lt))))
        }
        "and" => {
            if args.is_empty() {
                return Ok(Some(Expr::Bool(true)));
            }
            let mut it = args.iter();
            let Some(first_form) = it.next() else {
                return Ok(Some(Expr::Bool(true)));
            };
            let Some(mut expr) = lower_scheme_direct_expr(first_form, builder)? else {
                return Ok(None);
            };
            for arg in it {
                let Some(rhs) = lower_scheme_direct_expr(arg, builder)? else {
                    return Ok(None);
                };
                expr = Expr::And(Box::new(expr), Box::new(rhs));
            }
            Ok(Some(expr))
        }
        "or" => {
            if args.is_empty() {
                return Ok(Some(Expr::Bool(false)));
            }
            let mut it = args.iter();
            let Some(first_form) = it.next() else {
                return Ok(Some(Expr::Bool(false)));
            };
            let Some(mut expr) = lower_scheme_direct_expr(first_form, builder)? else {
                return Ok(None);
            };
            for arg in it {
                let Some(rhs) = lower_scheme_direct_expr(arg, builder)? else {
                    return Ok(None);
                };
                expr = Expr::Or(Box::new(expr), Box::new(rhs));
            }
            Ok(Some(expr))
        }
        "not" => {
            if args.len() != 1 {
                return Ok(None);
            }
            let Some(inner) = lower_scheme_direct_expr(&args[0], builder)? else {
                return Ok(None);
            };
            Ok(Some(Expr::Not(Box::new(inner))))
        }
        "if" => {
            if !(2..=3).contains(&args.len()) {
                return Ok(None);
            }
            let Some(condition) = lower_scheme_direct_expr(&args[0], builder)? else {
                return Ok(None);
            };
            let Some(then_expr) = lower_scheme_direct_expr(&args[1], builder)? else {
                return Ok(None);
            };
            let else_expr = if args.len() == 3 {
                let Some(expr) = lower_scheme_direct_expr(&args[2], builder)? else {
                    return Ok(None);
                };
                expr
            } else {
                Expr::Bool(false)
            };
            Ok(Some(Expr::IfElse {
                condition: Box::new(condition),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
            }))
        }
        _ => Ok(None),
    }
}

fn lower_scheme_direct_binary<F>(
    args: &[SchemeForm],
    builder: &LocalIrBuilder,
    build: F,
) -> Result<Option<Expr>, ParseError>
where
    F: Fn(Box<Expr>, Box<Expr>) -> Expr,
{
    if args.len() != 2 {
        return Ok(None);
    }
    let Some(lhs) = lower_scheme_direct_expr(&args[0], builder)? else {
        return Ok(None);
    };
    let Some(rhs) = lower_scheme_direct_expr(&args[1], builder)? else {
        return Ok(None);
    };
    Ok(Some(build(Box::new(lhs), Box::new(rhs))))
}

fn lower_scheme_direct_fold<F>(
    args: &[SchemeForm],
    builder: &LocalIrBuilder,
    build: F,
    eval_int: fn(i64, i64) -> Option<i64>,
) -> Result<Option<Expr>, ParseError>
where
    F: Fn(Box<Expr>, Box<Expr>) -> Expr + Copy,
{
    if args.len() < 2 {
        return Ok(None);
    }

    let mut lowered_exprs = Vec::with_capacity(args.len());
    let mut int_values = Vec::with_capacity(args.len());
    let mut all_int_values = true;

    for arg in args {
        let Some(expr) = lower_scheme_direct_expr(arg, builder)? else {
            return Ok(None);
        };
        if let Expr::Int(value) = &expr {
            int_values.push(*value);
        } else {
            all_int_values = false;
        }
        lowered_exprs.push(expr);
    }

    if all_int_values {
        let mut iter = int_values.into_iter();
        let Some(mut acc) = iter.next() else {
            return Ok(None);
        };
        let mut foldable = true;
        for rhs in iter {
            let Some(next) = eval_int(acc, rhs) else {
                foldable = false;
                break;
            };
            acc = next;
        }
        if foldable {
            return Ok(Some(Expr::Int(acc)));
        }
    }

    let mut iter = lowered_exprs.into_iter();
    let Some(mut expr) = iter.next() else {
        return Ok(None);
    };
    for rhs in iter {
        expr = build(Box::new(expr), Box::new(rhs));
    }
    Ok(Some(expr))
}

fn lower_scheme_direct_compare_fold<F>(
    args: &[SchemeForm],
    builder: &LocalIrBuilder,
    build: F,
) -> Result<Option<Expr>, ParseError>
where
    F: Fn(Box<Expr>, Box<Expr>) -> Expr + Copy,
{
    if args.len() != 2 {
        return Ok(None);
    }
    lower_scheme_direct_binary(args, builder, build)
}

#[derive(Clone, Debug)]
struct SchemeForm {
    line: usize,
    node: SchemeNode,
}

impl SchemeForm {
    fn as_symbol(&self) -> Option<&str> {
        match &self.node {
            SchemeNode::Symbol(value) => Some(value),
            _ => None,
        }
    }

    fn as_list(&self) -> Option<&[SchemeForm]> {
        match &self.node {
            SchemeNode::List(values) => Some(values),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum SchemeNode {
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

            // Line comment: ; ... newline
            if self.current == Some(';') {
                while let Some(ch) = self.current {
                    self.advance();
                    if ch == '\n' {
                        break;
                    }
                }
                continue;
            }

            // Block comment: #| ... |#
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
                    // Not a block comment, restore state
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

        // Character literals: #\a, #\space, #\newline, #\tab
        if let Some(rest) = atom.strip_prefix("#\\") {
            let ch = match rest {
                "space" => ' ',
                "newline" => '\n',
                "tab" => '\t',
                "return" => '\r',
                "nul" | "null" => '\0',
                _ if is_hex_char_literal(rest) => {
                    let bytes = rest.as_bytes();
                    let hi = hex_nibble(bytes[1] as char).ok_or(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: format!("unknown character literal '#\\{rest}'"),
                    })?;
                    let lo = hex_nibble(bytes[2] as char).ok_or(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: format!("unknown character literal '#\\{rest}'"),
                    })?;
                    ((hi << 4) | lo) as char
                }
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

fn is_hex_char_literal(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() == 3
        && (bytes[0] == b'x' || bytes[0] == b'X')
        && hex_nibble(bytes[1] as char).is_some()
        && hex_nibble(bytes[2] as char).is_some()
}

fn hex_nibble(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some((ch as u8) - b'0'),
        'a'..='f' => Some((ch as u8) - b'a' + 10),
        'A'..='F' => Some((ch as u8) - b'A' + 10),
        _ => None,
    }
}

fn parse_number_atom(atom: &str) -> Option<TokenKind> {
    if atom.is_empty() {
        return None;
    }

    let body = atom.strip_prefix('-').unwrap_or(atom);
    if body.is_empty() || !body.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }

    // Check for float
    if body.contains('.') {
        return atom.parse::<f64>().ok().map(TokenKind::Float);
    }

    if body.chars().all(|ch| ch.is_ascii_digit()) {
        return atom.parse::<i64>().ok().map(TokenKind::Int);
    }

    None
}

struct SchemeParser {
    tokens: Vec<Token>,
    pos: usize,
}

impl SchemeParser {
    fn new(source: &str) -> Result<Self, ParseError> {
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

    fn parse_program(&mut self) -> Result<Vec<SchemeForm>, ParseError> {
        let mut forms = Vec::new();
        while !self.check_eof() {
            forms.push(self.parse_form()?);
        }
        Ok(forms)
    }

    fn parse_form(&mut self) -> Result<SchemeForm, ParseError> {
        // Handle #; datum comment: skip one entire form
        while self.check_datum_comment() {
            self.advance(); // skip the #; symbol token
            if self.check_eof() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current().line,
                    message: "expected form after #;".to_string(),
                });
            }
            self.parse_form()?; // parse and discard
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

#[inline]
fn fold_int_add(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_add(rhs))
}

#[inline]
fn fold_int_sub(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_sub(rhs))
}

#[inline]
fn fold_int_mul(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_mul(rhs))
}

#[inline]
fn fold_int_div(lhs: i64, rhs: i64) -> Option<i64> {
    if rhs == 0 {
        return None;
    }
    Some(lhs.wrapping_div(rhs))
}

fn normalize_identifier(name: &str, line: usize, context: &str) -> Result<String, ParseError> {
    if name.is_empty() {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("{context} cannot be empty"),
        });
    }

    let mut out = String::new();
    for ch in name.chars() {
        let mapped = if ch == '-' { '_' } else { ch };
        out.push(mapped);
    }

    let mut chars = out.chars();
    let Some(first) = chars.next() else {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("{context} cannot be empty"),
        });
    };

    if !is_ident_start(first) || !chars.all(is_ident_continue) {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("unsupported identifier '{name}' in {context}"),
        });
    }

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

fn is_reserved_identifier(name: &str) -> bool {
    matches!(
        name,
        "fn" | "let" | "for" | "if" | "else" | "while" | "break" | "continue" | "true" | "false"
    )
}
