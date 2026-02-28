use std::collections::{HashMap, HashSet};

use crate::builtins::BuiltinFunction;

use super::{
    ClosureExpr, Expr, FunctionDecl, FunctionImpl, MatchPattern, ParseError, STDLIB_PRINT_ARITY,
    STDLIB_PRINT_NAME, Stmt,
};

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Ident(String),
    Int(i64),
    String(String),
    True,
    False,
    Pub,
    Use,
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
    Greater,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    line: usize,
}

struct Lexer<'a> {
    chars: std::str::Chars<'a>,
    current: Option<char>,
    line: usize,
}

impl<'a> Lexer<'a> {
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
        self.skip_whitespace_and_comments()?;
        let line = self.line;
        let Some(ch) = self.current else {
            return Ok(Token {
                kind: TokenKind::Eof,
                line,
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
            '|' => {
                self.advance();
                TokenKind::Pipe
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
                TokenKind::Less
            }
            '>' => {
                self.advance();
                TokenKind::Greater
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
            '"' => {
                let value = self.consume_string()?;
                TokenKind::String(value)
            }
            c if c.is_ascii_digit() => {
                let value = self.consume_number()?;
                TokenKind::Int(value)
            }
            c if is_ident_start(c) => {
                let ident = self.consume_ident();
                match ident.as_str() {
                    "pub" => TokenKind::Pub,
                    "use" => TokenKind::Use,
                    "as" => TokenKind::As,
                    "fn" => TokenKind::Fn,
                    "let" => TokenKind::Let,
                    "for" => TokenKind::For,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "match" => TokenKind::Match,
                    "while" => TokenKind::While,
                    "break" => TokenKind::Break,
                    "continue" => TokenKind::Continue,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    _ => TokenKind::Ident(ident),
                }
            }
            other => {
                return Err(ParseError {
                    line,
                    message: format!("unexpected character '{other}'"),
                });
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

    fn consume_number(&mut self) -> Result<i64, ParseError> {
        let line = self.line;
        let mut text = String::new();
        while let Some(ch) = self.current {
            if ch.is_ascii_digit() {
                text.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        text.parse::<i64>().map_err(|_| ParseError {
            line,
            message: format!("invalid number '{text}'"),
        })
    }

    fn consume_string(&mut self) -> Result<String, ParseError> {
        let line = self.line;
        if self.current != Some('"') {
            return Err(ParseError {
                line,
                message: "string literal must start with '\"'".to_string(),
            });
        }
        self.advance();

        let mut out = String::new();
        loop {
            let Some(ch) = self.current else {
                return Err(ParseError {
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

pub(super) struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    locals: HashMap<String, u8>,
    next_local: u8,
    functions: HashMap<String, FunctionDecl>,
    function_list: Vec<FunctionDecl>,
    function_impls: HashMap<u16, FunctionImpl>,
    next_function: u16,
    closure_bindings: HashMap<String, ClosureExpr>,
    closure_scopes: Vec<HashMap<String, u8>>,
    closure_capture_contexts: Vec<ClosureCaptureContext>,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
    loop_depth: usize,
    vm_namespace_aliases: HashSet<String>,
    vm_named_imports: HashMap<String, String>,
    vm_wildcard_import: bool,
}

struct ClosureCaptureContext {
    by_name: HashMap<String, u8>,
    capture_copies: Vec<(u8, u8)>,
}

impl Parser {
    pub(super) fn new(
        source: &str,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
    ) -> Result<Self, ParseError> {
        let mut lexer = Lexer::new(source);
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
            closure_bindings: HashMap::new(),
            closure_scopes: Vec::new(),
            closure_capture_contexts: Vec::new(),
            allow_implicit_externs,
            allow_implicit_semicolons,
            loop_depth: 0,
            vm_namespace_aliases: HashSet::new(),
            vm_named_imports: HashMap::new(),
            vm_wildcard_import: false,
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
                line: self.current_line(),
                message: "expected 'fn' after 'pub'".to_string(),
            });
        }
        if self.match_kind(&TokenKind::Use) {
            return self.parse_use_stmt();
        }
        if self.match_kind(&TokenKind::Fn) {
            return self.parse_fn_decl(false);
        }
        if self.match_kind(&TokenKind::Let) {
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
        if namespace != "vm" {
            return Err(ParseError {
                line: self.current_line(),
                message: format!(
                    "unsupported use namespace '{namespace}'; only 'vm' host namespace is supported here"
                ),
            });
        }

        if self.match_kind(&TokenKind::Semicolon) {
            self.vm_namespace_aliases.insert("vm".to_string());
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::As) {
            let alias = self.expect_ident("expected namespace alias after 'as'")?;
            self.expect(&TokenKind::Semicolon, "expected ';' after use alias")?;
            self.vm_namespace_aliases.insert(alias);
            return Ok(Stmt::Noop { line });
        }

        if !self.match_path_separator() {
            return Err(ParseError {
                line: self.current_line(),
                message: "expected ';', 'as <alias>', or '::{...}' after 'use vm'".to_string(),
            });
        }

        if self.match_kind(&TokenKind::Star) {
            self.vm_wildcard_import = true;
            self.expect(&TokenKind::Semicolon, "expected ';' after 'use vm::*'")?;
            return Ok(Stmt::Noop { line });
        }

        self.expect(&TokenKind::LBrace, "expected '{' after 'use vm::'")?;
        if self.match_kind(&TokenKind::Star) {
            self.vm_wildcard_import = true;
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
            if let Some(existing) = self.vm_named_imports.get(&local)
                && existing != &imported
            {
                return Err(ParseError {
                    line: self.current_line(),
                    message: format!(
                        "host import alias '{local}' already maps to '{existing}', cannot remap to '{imported}'"
                    ),
                });
            }
            self.vm_named_imports.insert(local, imported);

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
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        if self.functions.contains_key(&name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!("duplicate function '{name}'"),
            });
        }
        if self.locals.contains_key(&name) || self.closure_bindings.contains_key(&name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
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
                        line: parser.current_line(),
                        message: "unexpected end of input in function body".to_string(),
                    });
                }
                body_stmts.push(parser.parse_stmt()?);
            }

            let Some(last_stmt) = body_stmts.pop() else {
                return Err(ParseError {
                    line: parser.current_line(),
                    message: "function body must end with an expression statement".to_string(),
                });
            };
            let body_expr = if let Stmt::Expr { expr, .. } = last_stmt {
                expr
            } else {
                return Err(ParseError {
                    line: parser.current_line(),
                    message: "function body must end with an expression statement".to_string(),
                });
            };

            if body_stmts
                .iter()
                .any(|stmt| matches!(stmt, Stmt::FuncDecl { .. }))
            {
                return Err(ParseError {
                    line: parser.current_line(),
                    message: "nested function declarations are not supported".to_string(),
                });
            }

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
        let (body_stmts, body_expr) = parse_body(self)?;
        let capture_context = self
            .closure_capture_contexts
            .pop()
            .ok_or_else(|| ParseError {
                line: self.current_line(),
                message: "internal function capture state error".to_string(),
            })?;
        self.closure_scopes.pop();
        if !capture_context.capture_copies.is_empty() {
            return Err(ParseError {
                line: self.current_line(),
                message: "RustScript function definitions cannot capture outer locals".to_string(),
            });
        }
        Ok(FunctionImpl {
            param_slots,
            body_stmts,
            body_expr,
        })
    }

    fn parse_let_with_terminator(&mut self, expect_terminator: bool) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let name = self.expect_ident("expected identifier after 'let'")?;
        self.expect(&TokenKind::Equal, "expected '=' after identifier")?;
        let expr = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after let")?;
        }

        if let Expr::Closure(closure) = expr {
            if self.locals.contains_key(&name)
                || self.functions.contains_key(&name)
                || self.closure_bindings.contains_key(&name)
            {
                return Err(ParseError {
                    line: self.current_line(),
                    message: format!("name '{name}' already used"),
                });
            }
            self.closure_bindings.insert(name, closure.clone());
            return Ok(Stmt::ClosureLet { line, closure });
        }

        if self.closure_bindings.contains_key(&name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!(
                    "cannot rebind closure '{name}' as a value variable in this compiler subset"
                ),
            });
        }

        if !self.closure_scopes.is_empty() {
            if let Some(index) = self
                .closure_scopes
                .last()
                .and_then(|scope| scope.get(&name))
                .copied()
            {
                return Ok(Stmt::Let { index, expr, line });
            }
            let index = self.allocate_hidden_local()?;
            if let Some(scope) = self.closure_scopes.last_mut() {
                scope.insert(name, index);
            }
            return Ok(Stmt::Let { index, expr, line });
        }

        let index = self.get_or_assign_local(&name)?;
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

        if self.closure_bindings.contains_key(&name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!("cannot assign to closure '{name}'"),
            });
        }

        let index = self.get_local(&name)?;
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
        self.parse_comparison()
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
            } else if self.match_kind(&TokenKind::Greater) {
                let rhs = self.parse_term()?;
                expr = Expr::Gt(Box::new(expr), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(expr)
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
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::Minus) {
            let inner = self.parse_unary()?;
            Ok(Expr::Neg(Box::new(inner)))
        } else if self.match_kind(&TokenKind::Bang) {
            let inner = self.parse_unary()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
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
        if let Some(value) = self.match_int() {
            return Ok(Expr::Int(value));
        }
        if let Some(value) = self.match_string() {
            return Ok(Expr::String(value));
        }
        if self.match_kind(&TokenKind::Pipe) {
            return self.parse_closure_literal();
        }
        if let Some(name) = self.match_ident() {
            if self.match_path_separator() {
                let member = self.expect_ident("expected function name after '::'")?;
                self.expect(
                    &TokenKind::LParen,
                    "expected '(' after namespaced function name",
                )?;
                let args = self.parse_call_args()?;
                if let Some(builtin) = self.resolve_builtin_namespace_call(&name, &member) {
                    let expr = self.build_builtin_call_expr(builtin, args)?;
                    return Ok(expr);
                }
                let host_name =
                    self.resolve_vm_namespace_call_target(&name, &member)
                        .ok_or_else(|| ParseError {
                            line: self.current_line(),
                            message: format!(
                                "unknown namespace call '{}::{}'; supported namespaces are io:: (builtins) and vm:: (host imports via 'use vm;' or 'use vm as <alias>;')",
                                name, member
                            ),
                        })?
                        .to_string();
                let expr = self.build_vm_host_call_expr(&host_name, args)?;
                return Ok(expr);
            }

            let mut expr = if self.match_kind(&TokenKind::LParen) {
                let args = self.parse_call_args()?;
                if let Some(closure) = self.closure_bindings.get(&name).cloned() {
                    if closure.param_slots.len() != args.len() {
                        return Err(ParseError {
                            line: self.current_line(),
                            message: format!(
                                "closure '{name}' expects {} arguments",
                                closure.param_slots.len()
                            ),
                        });
                    }
                    Expr::ClosureCall(closure, args)
                } else if self.functions.contains_key(&name) {
                    let decl = self.resolve_function_for_call(&name, args.len())?;
                    Expr::Call(decl.index, args)
                } else if let Some(builtin) = BuiltinFunction::from_name(&name) {
                    let arg_arity = u8::try_from(args.len()).map_err(|_| ParseError {
                        line: self.current_line(),
                        message: "function arity too large".to_string(),
                    })?;
                    if arg_arity != builtin.arity() {
                        return Err(ParseError {
                            line: self.current_line(),
                            message: format!(
                                "function '{}' expects {} arguments",
                                builtin.name(),
                                builtin.arity()
                            ),
                        });
                    }
                    if builtin == BuiltinFunction::Concat {
                        let mut args = args.into_iter();
                        let lhs = args.next().ok_or_else(|| ParseError {
                            line: self.current_line(),
                            message: "concat expects two arguments".to_string(),
                        })?;
                        let rhs = args.next().ok_or_else(|| ParseError {
                            line: self.current_line(),
                            message: "concat expects two arguments".to_string(),
                        })?;
                        Expr::Add(Box::new(lhs), Box::new(rhs))
                    } else {
                        Expr::Call(builtin.call_index(), args)
                    }
                } else if let Some(host_name) = self.resolve_vm_direct_call_target(&name) {
                    let host_name = host_name.to_string();
                    self.build_vm_host_call_expr(&host_name, args)?
                } else {
                    let decl = self.resolve_function_for_call(&name, args.len())?;
                    Expr::Call(decl.index, args)
                }
            } else {
                if self.closure_bindings.contains_key(&name) {
                    return Err(ParseError {
                        line: self.current_line(),
                        message: format!("closure '{name}' must be called with '(...)'"),
                    });
                }
                let index = self.get_local(&name)?;
                Expr::Var(index)
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
                    line: self.current_line(),
                    message: "if expression branch must end with an expression".to_string(),
                });
            };
            if let Stmt::Expr { expr, .. } = last_stmt {
                expr
            } else {
                return Err(ParseError {
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
                    line: self.current_line(),
                    message: "expected ',' or '}' after match arm".to_string(),
                });
            }
        }
        self.expect(&TokenKind::RBrace, "expected '}' after match expression")?;

        let default = default.ok_or_else(|| ParseError {
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
        if let Some(value) = self.match_int() {
            return Ok(Some(MatchPattern::Int(value)));
        }
        if let Some(value) = self.match_string() {
            return Ok(Some(MatchPattern::String(value)));
        }
        if let Some(name) = self.match_ident()
            && name == "_"
        {
            return Ok(None);
        }
        Err(ParseError {
            line: self.current_line(),
            message: "match patterns currently support only int/string literals and '_'"
                .to_string(),
        })
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
                let member = self.expect_ident("expected member name after '.'")?;
                expr = self.build_builtin_call_expr(
                    BuiltinFunction::Get,
                    vec![expr, Expr::String(member)],
                )?;
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
                let member = self.expect_ident("expected member name after '?.'")?;
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
        let container_slot = self.allocate_hidden_local()?;
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
        self.bind_hidden_local_expr(container_slot, container, slice_len)
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
        container_slot: u8,
        key_slot: u8,
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
        container_slot: u8,
        key_slot: u8,
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
        value_slot: u8,
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

        enum BraceLiteralKind {
            Array,
            Map,
        }

        let mut kind: Option<BraceLiteralKind> = None;
        let mut out_array = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
        let mut out_map = self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new())?;

        loop {
            let is_map_entry = self.check_map_entry_start();
            match kind {
                None => {
                    kind = Some(if is_map_entry {
                        BraceLiteralKind::Map
                    } else {
                        BraceLiteralKind::Array
                    });
                }
                Some(BraceLiteralKind::Array) if is_map_entry => {
                    return Err(ParseError {
                        line: self.current_line(),
                        message: "cannot mix map entries into array literal".to_string(),
                    });
                }
                Some(BraceLiteralKind::Map) if !is_map_entry => {
                    return Err(ParseError {
                        line: self.current_line(),
                        message: "cannot mix array entries into map literal".to_string(),
                    });
                }
                _ => {}
            }

            match kind {
                Some(BraceLiteralKind::Array) => {
                    let value = self.parse_expr()?;
                    out_array = self.build_builtin_call_expr(
                        BuiltinFunction::ArrayPush,
                        vec![out_array, value],
                    )?;
                }
                Some(BraceLiteralKind::Map) => {
                    let key = self.parse_map_key_literal()?;
                    if !(self.match_kind(&TokenKind::Colon) || self.match_kind(&TokenKind::Equal)) {
                        return Err(ParseError {
                            line: self.current_line(),
                            message: "expected ':' or '=' after map key".to_string(),
                        });
                    }
                    let value = self.parse_expr()?;
                    out_map = self
                        .build_builtin_call_expr(BuiltinFunction::Set, vec![out_map, key, value])?;
                }
                None => unreachable!(),
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
        match kind {
            Some(BraceLiteralKind::Array) => Ok(out_array),
            Some(BraceLiteralKind::Map) | None => Ok(out_map),
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
        if self.match_kind(&TokenKind::True) {
            return Ok(Expr::Bool(true));
        }
        if self.match_kind(&TokenKind::False) {
            return Ok(Expr::Bool(false));
        }
        Err(ParseError {
            line: self.current_line(),
            message: "map keys must be identifier/string/int/bool literals".to_string(),
        })
    }

    fn check_map_entry_start(&self) -> bool {
        let Some(current) = self.tokens.get(self.pos) else {
            return false;
        };
        let Some(next) = self.tokens.get(self.pos + 1) else {
            return false;
        };
        let is_key = matches!(
            current.kind,
            TokenKind::Ident(_)
                | TokenKind::String(_)
                | TokenKind::Int(_)
                | TokenKind::True
                | TokenKind::False
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
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        if arity != builtin.arity() {
            return Err(ParseError {
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

    fn build_vm_host_call_expr(
        &mut self,
        host_name: &str,
        args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        let decl = self.define_vm_host_function(host_name, arity)?;
        Ok(Expr::Call(decl.index, args))
    }

    fn resolve_vm_direct_call_target<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        if let Some(mapped) = self.vm_named_imports.get(name) {
            return Some(mapped.as_str());
        }
        if self.vm_wildcard_import {
            return Some(name);
        }
        None
    }

    fn resolve_vm_namespace_call_target<'a>(
        &'a self,
        namespace: &str,
        member: &'a str,
    ) -> Option<&'a str> {
        if self.vm_namespace_aliases.contains(namespace) {
            Some(member)
        } else {
            None
        }
    }

    fn resolve_builtin_namespace_call(
        &self,
        namespace: &str,
        member: &str,
    ) -> Option<BuiltinFunction> {
        match namespace {
            "io" => match member {
                "open" => Some(BuiltinFunction::IoOpen),
                "popen" => Some(BuiltinFunction::IoPopen),
                "read_all" => Some(BuiltinFunction::IoReadAll),
                "read_line" => Some(BuiltinFunction::IoReadLine),
                "write" => Some(BuiltinFunction::IoWrite),
                "flush" => Some(BuiltinFunction::IoFlush),
                "close" => Some(BuiltinFunction::IoClose),
                "exists" => Some(BuiltinFunction::IoExists),
                _ => None,
            },
            _ => None,
        }
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

    fn parse_closure_literal(&mut self) -> Result<Expr, ParseError> {
        let mut param_slots = Vec::new();
        let mut param_scope = HashMap::new();
        if !self.check(&TokenKind::Pipe) {
            loop {
                let param_name = self.expect_ident("expected closure parameter name")?;
                if param_scope.contains_key(&param_name) {
                    return Err(ParseError {
                        line: self.current_line(),
                        message: format!("duplicate closure parameter '{param_name}'"),
                    });
                }
                let slot = self.allocate_hidden_local()?;
                param_scope.insert(param_name, slot);
                param_slots.push(slot);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::Pipe, "expected '|' after closure parameters")?;
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
            })
        }
    }

    fn get_local(&mut self, name: &str) -> Result<u8, ParseError> {
        for scope in self.closure_scopes.iter().rev() {
            if let Some(&index) = scope.get(name) {
                return Ok(index);
            }
        }
        if let Some(source_index) = self.locals.get(name).copied() {
            if let Some(capture_idx) = self.closure_capture_contexts.len().checked_sub(1) {
                if let Some(&captured_slot) =
                    self.closure_capture_contexts[capture_idx].by_name.get(name)
                {
                    return Ok(captured_slot);
                }
                let captured_slot = self.allocate_hidden_local()?;
                self.closure_capture_contexts[capture_idx]
                    .by_name
                    .insert(name.to_string(), captured_slot);
                self.closure_capture_contexts[capture_idx]
                    .capture_copies
                    .push((source_index, captured_slot));
                return Ok(captured_slot);
            }
            return Ok(source_index);
        }
        Err(ParseError {
            line: self.current_line(),
            message: format!("unknown local '{name}'"),
        })
    }

    fn resolve_function_for_call(
        &mut self,
        name: &str,
        arg_count: usize,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(decl) = self.functions.get(name).cloned() {
            if decl.arity as usize != arg_count {
                return Err(ParseError {
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", decl.arity),
                });
            }
            return Ok(decl);
        }

        if name == STDLIB_PRINT_NAME {
            let arg_arity = u8::try_from(arg_count).map_err(|_| ParseError {
                line: self.current_line(),
                message: "function arity too large".to_string(),
            })?;
            if arg_arity != STDLIB_PRINT_ARITY {
                return Err(ParseError {
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
                line: self.current_line(),
                message: "function arity too large".to_string(),
            })?;
            return self.define_external_function(name, arity);
        }

        Err(ParseError {
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
        if self.locals.contains_key(name) || self.closure_bindings.contains_key(name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
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
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", existing.arity),
                });
            }
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) || self.closure_bindings.contains_key(name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
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

    fn define_vm_host_function(
        &mut self,
        name: &str,
        arity: u8,
    ) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            if existing.arity != arity {
                return Err(ParseError {
                    line: self.current_line(),
                    message: format!("function '{name}' expects {} arguments", existing.arity),
                });
            }
            return Ok(existing.clone());
        }
        if self.locals.contains_key(name) || self.closure_bindings.contains_key(name) {
            return Err(ParseError {
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
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

    fn get_or_assign_local(&mut self, name: &str) -> Result<u8, ParseError> {
        if let Some(&index) = self.locals.get(name) {
            return Ok(index);
        }
        let index = self.allocate_hidden_local()?;
        self.locals.insert(name.to_string(), index);
        Ok(index)
    }

    fn allocate_hidden_local(&mut self) -> Result<u8, ParseError> {
        let index = self.next_local;
        self.next_local = self.next_local.checked_add(1).ok_or(ParseError {
            line: self.current_line(),
            message: "local index overflow".to_string(),
        })?;
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

    pub(super) fn local_bindings(&self) -> Vec<(String, u8)> {
        let mut locals: Vec<(String, u8)> = self
            .locals
            .iter()
            .map(|(name, index)| (name.clone(), *index))
            .collect();
        locals.sort_by_key(|(_, index)| *index);
        locals
    }
}
