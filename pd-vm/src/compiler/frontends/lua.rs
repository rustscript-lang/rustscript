use super::super::ParseError;
use super::super::ir::{Expr, FrontendIr, Stmt};
use super::{is_ident_continue, is_ident_start};
use crate::compiler::source_map::LoweredSource;
use std::collections::HashMap;
use std::collections::HashSet;

enum LuaBlock {
    If,
    For,
    While,
    Do,
    Repeat,
    FunctionDecl,
}

#[derive(Default)]
struct LuaLoweringContext {
    needs_string_sub_helpers: bool,
    needs_table_len_helper: bool,
    next_temp_id: u32,
}

impl LuaLoweringContext {
    fn fresh_temp(&mut self, prefix: &str) -> String {
        let id = self.next_temp_id;
        self.next_temp_id = self.next_temp_id.saturating_add(1);
        format!("__lua_{prefix}_{id}")
    }
}

pub(super) fn lower_to_ir(source: &str) -> Result<FrontendIr, ParseError> {
    if let Some(ir) = try_lower_direct_subset_to_ir(source)? {
        return Ok(ir);
    }
    let lowered = lower(source)?;
    super::parse_lowered_with_mapping(source, lowered, false, false)
}

fn try_lower_direct_subset_to_ir(source: &str) -> Result<Option<FrontendIr>, ParseError> {
    let cleaned_source = remove_lua_comments(source)?;
    let mut builder = LuaDirectIrBuilder::new();
    let mut root_stmts = Vec::<Stmt>::new();
    let mut block_stack = Vec::<LuaDirectBlock>::new();

    for (index, raw_line) in cleaned_source.lines().enumerate() {
        let line_no = index + 1;
        let line_u32 = u32::try_from(line_no).unwrap_or(u32::MAX);
        let trimmed = raw_line.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("local function ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("for ")
            || trimmed == "repeat"
            || trimmed.starts_with("until ")
            || trimmed.starts_with("elseif ")
            || trimmed == "::continue::"
            || trimmed == "goto continue"
            || trimmed.starts_with("return ")
            || trimmed.contains('?')
            || trimmed.contains(':')
            || trimmed.contains('#')
            || trimmed.contains('{')
            || trimmed.contains('[')
            || trimmed.contains("..")
        {
            return Ok(None);
        }

        if lower_lua_vm_require_line(trimmed).is_some() || is_lua_require_line(trimmed) {
            return Ok(None);
        }

        if let Some(rest) = trimmed.strip_prefix("if ")
            && let Some(condition_raw) = rest.strip_suffix(" then")
        {
            let condition = parse_lua_direct_expr(condition_raw, &builder)?;
            let Some(condition) = condition else {
                return Ok(None);
            };
            block_stack.push(LuaDirectBlock::If {
                condition,
                then_branch: Vec::new(),
                else_branch: Vec::new(),
                in_else: false,
                line: line_u32,
            });
            continue;
        }

        if trimmed == "else" {
            let Some(LuaDirectBlock::If { in_else, .. }) = block_stack.last_mut() else {
                return Ok(None);
            };
            *in_else = true;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("while ")
            && let Some(condition_raw) = rest.strip_suffix(" do")
        {
            let condition = parse_lua_direct_expr(condition_raw, &builder)?;
            let Some(condition) = condition else {
                return Ok(None);
            };
            block_stack.push(LuaDirectBlock::While {
                condition,
                body: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        if trimmed == "do" {
            block_stack.push(LuaDirectBlock::Do {
                body: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        if trimmed == "end" {
            let Some(block) = block_stack.pop() else {
                return Ok(None);
            };
            let stmt = match block {
                LuaDirectBlock::If {
                    condition,
                    then_branch,
                    else_branch,
                    line,
                    ..
                } => Stmt::IfElse {
                    condition,
                    then_branch,
                    else_branch,
                    line,
                },
                LuaDirectBlock::While {
                    condition,
                    body,
                    line,
                } => Stmt::While {
                    condition,
                    body,
                    line,
                },
                LuaDirectBlock::Do { body, line } => Stmt::IfElse {
                    condition: Expr::Bool(true),
                    then_branch: body,
                    else_branch: Vec::new(),
                    line,
                },
            };
            emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
            continue;
        }

        if trimmed == "break" {
            emit_lua_direct_stmt(
                Stmt::Break { line: line_u32 },
                &mut root_stmts,
                &mut block_stack,
            );
            continue;
        }

        if trimmed == "continue" {
            emit_lua_direct_stmt(
                Stmt::Continue { line: line_u32 },
                &mut root_stmts,
                &mut block_stack,
            );
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("local ") {
            let Some((name_raw, expr_raw)) = rest.split_once('=') else {
                return Ok(None);
            };
            let name = name_raw.trim();
            if !is_valid_lua_ident(name) {
                return Ok(None);
            }
            let expr = parse_lua_direct_expr(expr_raw.trim(), &builder)?;
            let Some(expr) = expr else {
                return Ok(None);
            };
            let stmt = builder.lower_local(name, expr, line_u32)?;
            emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
            continue;
        }

        if let Some((lhs, rhs)) = trimmed.split_once('=')
            && is_valid_lua_ident(lhs.trim())
            && !lhs.contains('!')
            && !lhs.contains('<')
            && !lhs.contains('>')
        {
            let expr = parse_lua_direct_expr(rhs.trim(), &builder)?;
            let Some(expr) = expr else {
                return Ok(None);
            };
            let stmt = builder.lower_assign(lhs.trim(), expr, line_u32)?;
            emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
            continue;
        }

        let expr = parse_lua_direct_expr(trimmed, &builder)?;
        let Some(expr) = expr else {
            return Ok(None);
        };
        emit_lua_direct_stmt(
            Stmt::Expr {
                expr,
                line: line_u32,
            },
            &mut root_stmts,
            &mut block_stack,
        );
    }

    if !block_stack.is_empty() {
        return Ok(None);
    }

    Ok(Some(builder.finish(root_stmts)))
}

enum LuaDirectBlock {
    If {
        condition: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Vec<Stmt>,
        in_else: bool,
        line: u32,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    Do {
        body: Vec<Stmt>,
        line: u32,
    },
}

fn emit_lua_direct_stmt(stmt: Stmt, root: &mut Vec<Stmt>, blocks: &mut [LuaDirectBlock]) {
    let Some(current) = blocks.last_mut() else {
        root.push(stmt);
        return;
    };
    match current {
        LuaDirectBlock::If {
            then_branch,
            else_branch,
            in_else,
            ..
        } => {
            if *in_else {
                else_branch.push(stmt);
            } else {
                then_branch.push(stmt);
            }
        }
        LuaDirectBlock::While { body, .. } | LuaDirectBlock::Do { body, .. } => body.push(stmt),
    }
}

struct LuaDirectIrBuilder {
    locals: HashMap<String, u8>,
    next_local: u8,
}

impl LuaDirectIrBuilder {
    fn new() -> Self {
        Self {
            locals: HashMap::new(),
            next_local: 0,
        }
    }

    fn lower_local(&mut self, name: &str, expr: Expr, line: u32) -> Result<Stmt, ParseError> {
        let index = if let Some(index) = self.locals.get(name).copied() {
            index
        } else {
            let index = self.alloc_local()?;
            self.locals.insert(name.to_string(), index);
            index
        };
        Ok(Stmt::Let { index, expr, line })
    }

    fn lower_assign(&self, name: &str, expr: Expr, line: u32) -> Result<Stmt, ParseError> {
        let Some(index) = self.locals.get(name).copied() else {
            return Err(ParseError {
                span: None,
                code: None,
                line: line as usize,
                message: format!("unknown local '{name}'"),
            });
        };
        Ok(Stmt::Assign { index, expr, line })
    }

    fn resolve_local_expr(&self, name: &str) -> Option<Expr> {
        self.locals.get(name).copied().map(Expr::Var)
    }

    fn finish(self, stmts: Vec<Stmt>) -> FrontendIr {
        let mut local_bindings = self.locals.into_iter().collect::<Vec<(String, u8)>>();
        local_bindings.sort_by_key(|(_, index)| *index);
        FrontendIr {
            stmts,
            locals: self.next_local as usize,
            local_bindings,
            functions: Vec::new(),
            function_impls: HashMap::new(),
        }
    }

    fn alloc_local(&mut self) -> Result<u8, ParseError> {
        let index = self.next_local;
        self.next_local = self.next_local.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "local index overflow".to_string(),
        })?;
        Ok(index)
    }
}

#[derive(Clone)]
enum LuaDirectExpr {
    Null,
    Bool(bool),
    Int(i64),
    String(String),
    Var(String),
    Add(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Sub(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Mul(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Div(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Mod(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Eq(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Ne(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Lt(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Gt(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Le(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Ge(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    And(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Or(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Neg(Box<LuaDirectExpr>),
    Not(Box<LuaDirectExpr>),
}

#[derive(Clone)]
enum LuaDirectToken {
    Int(i64),
    String(String),
    Bool(bool),
    Null,
    Ident(String),
    LParen,
    RParen,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Less,
    Greater,
    LessEq,
    GreaterEq,
    And,
    Or,
    Not,
}

fn parse_lua_direct_expr(
    input: &str,
    builder: &LuaDirectIrBuilder,
) -> Result<Option<Expr>, ParseError> {
    let Some(tokens) = tokenize_lua_direct_expr(input) else {
        return Ok(None);
    };
    let mut parser = LuaDirectExprParser { tokens, pos: 0 };
    let Some(expr) = parser.parse_or() else {
        return Ok(None);
    };
    if parser.pos != parser.tokens.len() {
        return Ok(None);
    }
    Ok(lower_lua_direct_expr(expr, builder))
}

fn lower_lua_direct_expr(expr: LuaDirectExpr, builder: &LuaDirectIrBuilder) -> Option<Expr> {
    match expr {
        LuaDirectExpr::Null => Some(Expr::Null),
        LuaDirectExpr::Bool(value) => Some(Expr::Bool(value)),
        LuaDirectExpr::Int(value) => Some(Expr::Int(value)),
        LuaDirectExpr::String(value) => Some(Expr::String(value)),
        LuaDirectExpr::Var(name) => builder.resolve_local_expr(&name),
        LuaDirectExpr::Add(lhs, rhs) => Some(Expr::Add(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Sub(lhs, rhs) => Some(Expr::Sub(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Mul(lhs, rhs) => Some(Expr::Mul(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Div(lhs, rhs) => Some(Expr::Div(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Mod(lhs, rhs) => Some(Expr::Mod(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Eq(lhs, rhs) => Some(Expr::Eq(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Ne(lhs, rhs) => Some(Expr::Not(Box::new(Expr::Eq(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )))),
        LuaDirectExpr::Lt(lhs, rhs) => Some(Expr::Lt(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Gt(lhs, rhs) => Some(Expr::Gt(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Le(lhs, rhs) => Some(Expr::Not(Box::new(Expr::Gt(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )))),
        LuaDirectExpr::Ge(lhs, rhs) => Some(Expr::Not(Box::new(Expr::Lt(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )))),
        LuaDirectExpr::And(lhs, rhs) => Some(Expr::And(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Or(lhs, rhs) => Some(Expr::Or(
            Box::new(lower_lua_direct_expr(*lhs, builder)?),
            Box::new(lower_lua_direct_expr(*rhs, builder)?),
        )),
        LuaDirectExpr::Neg(inner) => {
            Some(Expr::Neg(Box::new(lower_lua_direct_expr(*inner, builder)?)))
        }
        LuaDirectExpr::Not(inner) => {
            Some(Expr::Not(Box::new(lower_lua_direct_expr(*inner, builder)?)))
        }
    }
}

struct LuaDirectExprParser {
    tokens: Vec<LuaDirectToken>,
    pos: usize,
}

impl LuaDirectExprParser {
    fn parse_or(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_and()?;
        while self.match_token(|token| matches!(token, LuaDirectToken::Or)) {
            expr = LuaDirectExpr::Or(Box::new(expr), Box::new(self.parse_and()?));
        }
        Some(expr)
    }

    fn parse_and(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_equality()?;
        while self.match_token(|token| matches!(token, LuaDirectToken::And)) {
            expr = LuaDirectExpr::And(Box::new(expr), Box::new(self.parse_equality()?));
        }
        Some(expr)
    }

    fn parse_equality(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_relational()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::EqEq)) {
                expr = LuaDirectExpr::Eq(Box::new(expr), Box::new(self.parse_relational()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::NotEq)) {
                expr = LuaDirectExpr::Ne(Box::new(expr), Box::new(self.parse_relational()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_relational(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_add()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::Less)) {
                expr = LuaDirectExpr::Lt(Box::new(expr), Box::new(self.parse_add()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Greater)) {
                expr = LuaDirectExpr::Gt(Box::new(expr), Box::new(self.parse_add()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::LessEq)) {
                expr = LuaDirectExpr::Le(Box::new(expr), Box::new(self.parse_add()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::GreaterEq)) {
                expr = LuaDirectExpr::Ge(Box::new(expr), Box::new(self.parse_add()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_add(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_mul()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::Plus)) {
                expr = LuaDirectExpr::Add(Box::new(expr), Box::new(self.parse_mul()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Minus)) {
                expr = LuaDirectExpr::Sub(Box::new(expr), Box::new(self.parse_mul()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_mul(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_unary()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::Star)) {
                expr = LuaDirectExpr::Mul(Box::new(expr), Box::new(self.parse_unary()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Slash)) {
                expr = LuaDirectExpr::Div(Box::new(expr), Box::new(self.parse_unary()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Percent)) {
                expr = LuaDirectExpr::Mod(Box::new(expr), Box::new(self.parse_unary()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_unary(&mut self) -> Option<LuaDirectExpr> {
        if self.match_token(|token| matches!(token, LuaDirectToken::Not)) {
            return Some(LuaDirectExpr::Not(Box::new(self.parse_unary()?)));
        }
        if self.match_token(|token| matches!(token, LuaDirectToken::Minus)) {
            return Some(LuaDirectExpr::Neg(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Option<LuaDirectExpr> {
        if let Some(token) = self.peek().cloned() {
            match token {
                LuaDirectToken::Int(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Int(value))
                }
                LuaDirectToken::String(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::String(value))
                }
                LuaDirectToken::Bool(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Bool(value))
                }
                LuaDirectToken::Null => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Null)
                }
                LuaDirectToken::Ident(value) => {
                    self.pos += 1;
                    if matches!(self.peek(), Some(LuaDirectToken::LParen)) {
                        return None;
                    }
                    Some(LuaDirectExpr::Var(value))
                }
                LuaDirectToken::LParen => {
                    self.pos += 1;
                    let expr = self.parse_or()?;
                    if !self.match_token(|token| matches!(token, LuaDirectToken::RParen)) {
                        return None;
                    }
                    Some(expr)
                }
                _ => None,
            }
        } else {
            None
        }
    }

    fn peek(&self) -> Option<&LuaDirectToken> {
        self.tokens.get(self.pos)
    }

    fn match_token<F>(&mut self, predicate: F) -> bool
    where
        F: Fn(&LuaDirectToken) -> bool,
    {
        if self.peek().is_some_and(predicate) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

fn tokenize_lua_direct_expr(input: &str) -> Option<Vec<LuaDirectToken>> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let value = std::str::from_utf8(&bytes[start..i])
                .ok()?
                .parse::<i64>()
                .ok()?;
            out.push(LuaDirectToken::Int(value));
            continue;
        }
        if b == b'"' || b == b'\'' {
            let quote = b;
            i += 1;
            let mut text = String::new();
            let mut escaped = false;
            while i < bytes.len() {
                let ch = bytes[i];
                i += 1;
                if escaped {
                    let mapped = match ch {
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'\\' => '\\',
                        b'"' => '"',
                        b'\'' => '\'',
                        other => other as char,
                    };
                    text.push(mapped);
                    escaped = false;
                    continue;
                }
                if ch == b'\\' {
                    escaped = true;
                    continue;
                }
                if ch == quote {
                    break;
                }
                text.push(ch as char);
            }
            if escaped {
                return None;
            }
            out.push(LuaDirectToken::String(text));
            continue;
        }
        if is_ident_start(b as char) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = std::str::from_utf8(&bytes[start..i]).ok()?;
            match ident {
                "true" => out.push(LuaDirectToken::Bool(true)),
                "false" => out.push(LuaDirectToken::Bool(false)),
                "nil" => out.push(LuaDirectToken::Null),
                "and" => out.push(LuaDirectToken::And),
                "or" => out.push(LuaDirectToken::Or),
                "not" => out.push(LuaDirectToken::Not),
                _ => out.push(LuaDirectToken::Ident(ident.to_string())),
            }
            continue;
        }
        match b {
            b'(' => {
                out.push(LuaDirectToken::LParen);
                i += 1;
            }
            b')' => {
                out.push(LuaDirectToken::RParen);
                i += 1;
            }
            b'+' => {
                out.push(LuaDirectToken::Plus);
                i += 1;
            }
            b'-' => {
                out.push(LuaDirectToken::Minus);
                i += 1;
            }
            b'*' => {
                out.push(LuaDirectToken::Star);
                i += 1;
            }
            b'/' => {
                out.push(LuaDirectToken::Slash);
                i += 1;
            }
            b'%' => {
                out.push(LuaDirectToken::Percent);
                i += 1;
            }
            b'=' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::EqEq);
                i += 2;
            }
            b'~' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::NotEq);
                i += 2;
            }
            b'<' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::LessEq);
                i += 2;
            }
            b'>' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::GreaterEq);
                i += 2;
            }
            b'<' => {
                out.push(LuaDirectToken::Less);
                i += 1;
            }
            b'>' => {
                out.push(LuaDirectToken::Greater);
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

pub(super) fn lower(source: &str) -> Result<LoweredSource, ParseError> {
    let cleaned_source = remove_lua_comments(source)?;
    let mut out = Vec::new();
    let mut blocks = Vec::new();
    let mut lowering_context = LuaLoweringContext::default();
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

        if let Some(rest) = trimmed.strip_prefix("local function ") {
            let signature = rest.trim().trim_end_matches(';').trim();
            if !signature.ends_with(')') {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua local function declaration must end with ')'".to_string(),
                });
            }
            out.push(format!("fn {signature} {{"));
            blocks.push(LuaBlock::FunctionDecl);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("local ") {
            out.push(format!(
                "let {};",
                rewrite_lua_expr(
                    rest.trim().trim_end_matches(';').trim(),
                    &vm_namespace_aliases,
                    &mut lowering_context,
                    line_no
                )?,
            ));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("function ") {
            let signature = rest.trim().trim_end_matches(';').trim();
            if !signature.ends_with(')') {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua function declaration must end with ')'".to_string(),
                });
            }
            if trimmed.ends_with(';') {
                out.push(format!("fn {signature};"));
            } else {
                out.push(format!("fn {signature} {{"));
                blocks.push(LuaBlock::FunctionDecl);
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("if ")
            && let Some(condition) = rest.strip_suffix(" then")
        {
            out.push(format!(
                "if {} {{",
                rewrite_lua_expr(
                    condition.trim(),
                    &vm_namespace_aliases,
                    &mut lowering_context,
                    line_no
                )?
            ));
            blocks.push(LuaBlock::If);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("while ")
            && let Some(condition) = rest.strip_suffix(" do")
        {
            out.push(format!(
                "while {} {{",
                rewrite_lua_expr(
                    condition.trim(),
                    &vm_namespace_aliases,
                    &mut lowering_context,
                    line_no
                )?
            ));
            blocks.push(LuaBlock::While);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("for ")
            && let Some(header) = rest.strip_suffix(" do")
        {
            if let Some(generic) = parse_lua_generic_for_header(header) {
                match generic.kind {
                    LuaIteratorKind::Ipairs => {
                        let iterable = rewrite_lua_expr(
                            generic.iterable.trim(),
                            &vm_namespace_aliases,
                            &mut lowering_context,
                            line_no,
                        )?;
                        lowering_context.needs_table_len_helper = true;
                        let table_temp = lowering_context.fresh_temp("ipairs_table");
                        out.push(format!("let {table_temp} = {iterable};"));
                        if let Some(value_name) = generic.value_name {
                            out.push(format!(
                                "for (let {} = 0; {} < __lua_len({table_temp}); {} = {} + 1) {{",
                                generic.key_name,
                                generic.key_name,
                                generic.key_name,
                                generic.key_name
                            ));
                            out.push(format!(
                                "let {value_name} = ({table_temp})[{}];",
                                generic.key_name
                            ));
                        } else {
                            out.push(format!(
                                "for (let {} = 0; {} < __lua_len({table_temp}); {} = {} + 1) {{",
                                generic.key_name,
                                generic.key_name,
                                generic.key_name,
                                generic.key_name
                            ));
                        }
                        blocks.push(LuaBlock::For);
                        continue;
                    }
                    LuaIteratorKind::Pairs => {
                        let iterable = rewrite_lua_expr(
                            generic.iterable.trim(),
                            &vm_namespace_aliases,
                            &mut lowering_context,
                            line_no,
                        )?;
                        let table_temp = lowering_context.fresh_temp("pairs_table");
                        let keys_temp = lowering_context.fresh_temp("pairs_keys");
                        let iter_temp = lowering_context.fresh_temp("pairs_i");
                        out.push(format!("let {table_temp} = {iterable};"));
                        out.push(format!("let {keys_temp} = ({table_temp}).keys;"));
                        out.push(format!(
                            "for (let {iter_temp} = 0; {iter_temp} < ({keys_temp}).length; {iter_temp} = {iter_temp} + 1) {{"
                        ));
                        out.push(format!(
                            "let {} = ({keys_temp})[{iter_temp}];",
                            generic.key_name
                        ));
                        if let Some(value_name) = generic.value_name {
                            out.push(format!(
                                "let {value_name} = ({table_temp})[{}];",
                                generic.key_name
                            ));
                        }
                        blocks.push(LuaBlock::For);
                        continue;
                    }
                }
            }

            let eq_index = header.find('=').ok_or(ParseError {
                span: None,
                code: None,
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
                    span: None,
                    code: None,
                    line: line_no,
                    message: "invalid lua for loop variable".to_string(),
                });
            }
            let rhs = header[eq_index + 1..].trim();
            let parts = split_top_level_csv(rhs);
            if parts.len() < 2 || parts.len() > 3 {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua numeric for loop must be 'for name = start, end [, step] do'"
                        .to_string(),
                });
            }
            let start_expr = rewrite_lua_expr(
                parts[0].trim(),
                &vm_namespace_aliases,
                &mut lowering_context,
                line_no,
            )?;
            let end_expr = rewrite_lua_expr(
                parts[1].trim(),
                &vm_namespace_aliases,
                &mut lowering_context,
                line_no,
            )?;
            let step_expr = rewrite_lua_expr(
                parts.get(2).map(|s| s.trim()).unwrap_or("1"),
                &vm_namespace_aliases,
                &mut lowering_context,
                line_no,
            )?;
            let end_temp = lowering_context.fresh_temp("for_end");
            let step_temp = lowering_context.fresh_temp("for_step");
            out.push(format!("let {end_temp} = {end_expr};"));
            out.push(format!("let {step_temp} = {step_expr};"));
            out.push(format!(
                "for (let {name} = {start_expr}; ((({step_temp}) > 0) && ({name} < (({end_temp}) + 1))) || ((({step_temp}) < 0) && ({name} > (({end_temp}) - 1))); {name} = {name} + ({step_temp})) {{"
            ));
            blocks.push(LuaBlock::For);
            continue;
        }

        if trimmed == "do" {
            out.push("if true {".to_string());
            blocks.push(LuaBlock::Do);
            continue;
        }

        if trimmed == "repeat" {
            out.push("while true {".to_string());
            blocks.push(LuaBlock::Repeat);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("until ") {
            if !matches!(blocks.last(), Some(LuaBlock::Repeat)) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua 'until' without matching 'repeat'".to_string(),
                });
            }
            let condition = rewrite_lua_expr(
                rest.trim().trim_end_matches(';').trim(),
                &vm_namespace_aliases,
                &mut lowering_context,
                line_no,
            )?;
            out.push(format!("if {condition} {{ break; }}"));
            let _ = blocks.pop();
            out.push("}".to_string());
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("elseif ")
            && let Some(condition) = rest.strip_suffix(" then")
        {
            if !matches!(blocks.last(), Some(LuaBlock::If)) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua 'elseif' without matching 'if'".to_string(),
                });
            }
            out.push(format!(
                "}} else if {} {{",
                rewrite_lua_expr(
                    condition.trim(),
                    &vm_namespace_aliases,
                    &mut lowering_context,
                    line_no
                )?
            ));
            continue;
        }

        if trimmed == "else" {
            if !matches!(blocks.last(), Some(LuaBlock::If)) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua 'else' without matching 'if'".to_string(),
                });
            }
            out.push("} else {".to_string());
            continue;
        }

        if trimmed == "end" {
            let block = blocks.pop().ok_or(ParseError {
                span: None,
                code: None,
                line: line_no,
                message: "lua 'end' without matching block".to_string(),
            })?;
            match block {
                LuaBlock::Repeat => {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: line_no,
                        message: "lua 'repeat' block must be closed with 'until'".to_string(),
                    });
                }
                LuaBlock::FunctionDecl
                | LuaBlock::If
                | LuaBlock::For
                | LuaBlock::While
                | LuaBlock::Do => out.push("}".to_string()),
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
                    &vm_namespace_aliases,
                    &mut lowering_context,
                    line_no
                )?,
            ));
            continue;
        }

        out.push(format!(
            "{};",
            rewrite_lua_expr(
                trimmed.trim_end_matches(';'),
                &vm_namespace_aliases,
                &mut lowering_context,
                line_no
            )?
        ));
    }

    if !blocks.is_empty() {
        return Err(ParseError {
            span: None,
            code: None,
            line: source.lines().count().max(1),
            message: "unterminated lua block: expected 'end'".to_string(),
        });
    }

    let helper_lines = emit_lua_helpers(&lowering_context);
    if !helper_lines.is_empty() {
        let mut combined = helper_lines;
        combined.extend(out);
        out = combined;
    }

    Ok(LoweredSource::identity(out.join("\n")))
}

#[derive(Copy, Clone)]
enum LuaIteratorKind {
    Pairs,
    Ipairs,
}

struct LuaGenericFor {
    key_name: String,
    value_name: Option<String>,
    iterable: String,
    kind: LuaIteratorKind,
}

fn parse_lua_generic_for_header(header: &str) -> Option<LuaGenericFor> {
    let (vars_raw, iter_raw) = split_once_top_level_keyword(header, "in")?;
    let vars = split_top_level_csv(vars_raw)
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if vars.is_empty() || vars.len() > 2 {
        return None;
    }
    if !vars.iter().all(|name| is_valid_lua_ident(name)) {
        return None;
    }

    let (kind, iterable) = parse_lua_iterator_call(iter_raw.trim())?;
    Some(LuaGenericFor {
        key_name: vars[0].clone(),
        value_name: vars.get(1).cloned(),
        iterable,
        kind,
    })
}

fn parse_lua_iterator_call(input: &str) -> Option<(LuaIteratorKind, String)> {
    let (kind, call_head) = if let Some(rest) = input.strip_prefix("pairs") {
        (LuaIteratorKind::Pairs, rest)
    } else if let Some(rest) = input.strip_prefix("ipairs") {
        (LuaIteratorKind::Ipairs, rest)
    } else {
        return None;
    };
    let call_head = call_head.trim_start();
    if !call_head.starts_with('(') {
        return None;
    }
    let bytes = call_head.as_bytes();
    let mut i = 0usize;
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
        if b == b'"' || b == b'\'' {
            in_string = Some(b);
            escaped = false;
            i += 1;
            continue;
        }
        if b == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            depth = depth.saturating_sub(1);
            i += 1;
            if depth == 0 {
                let remainder = call_head[i..].trim();
                if !remainder.is_empty() {
                    return None;
                }
                let iterable = call_head[1..i - 1].trim();
                if iterable.is_empty() {
                    return None;
                }
                return Some((kind, iterable.to_string()));
            }
            continue;
        }
        i += 1;
    }
    None
}

fn split_once_top_level_keyword<'a>(input: &'a str, keyword: &str) -> Option<(&'a str, &'a str)> {
    let bytes = input.as_bytes();
    let keyword_bytes = keyword.as_bytes();
    let mut i = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
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

        if b == b'"' || b == b'\'' {
            in_string = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        match b {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && i + keyword_bytes.len() <= bytes.len()
            && &bytes[i..i + keyword_bytes.len()] == keyword_bytes
        {
            let prev_is_boundary =
                i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b',';
            let next = i + keyword_bytes.len();
            let next_is_boundary =
                next >= bytes.len() || bytes[next].is_ascii_whitespace() || bytes[next] == b',';
            if prev_is_boundary && next_is_boundary {
                return Some((input[..i].trim_end(), input[next..].trim_start()));
            }
        }

        i += 1;
    }
    None
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
                        span: None,
                        code: None,
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
        span: None,
        code: None,
        line: line_no,
        message: "lua function literal missing ')'".to_string(),
    })?;
    let params = after_keyword[1..close_index].trim();

    let body_and_end = after_keyword[close_index + 1..].trim();
    let body_raw = body_and_end.strip_suffix("end").ok_or(ParseError {
        span: None,
        code: None,
        line: line_no,
        message: "lua function literal must end with 'end'".to_string(),
    })?;
    let body_raw = body_raw.trim();
    if !body_raw.starts_with("return") {
        return Err(ParseError {
            span: None,
            code: None,
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
            span: None,
            code: None,
            line: line_no,
            message: "lua function literal must use 'return <expr>'".to_string(),
        });
    }
    let body = after_return.trim().trim_end_matches(';').trim();
    if body.is_empty() {
        return Err(ParseError {
            span: None,
            code: None,
            line: line_no,
            message: "lua function literal return expression cannot be empty".to_string(),
        });
    }

    if params.is_empty() {
        Ok(format!("{prefix}| | {body}"))
    } else {
        Ok(format!("{prefix}|{params}| {body}"))
    }
}

fn rewrite_lua_expr(
    expr: &str,
    vm_namespace_aliases: &HashSet<String>,
    lowering_context: &mut LuaLoweringContext,
    line_no: usize,
) -> Result<String, ParseError> {
    let method_rewritten = rewrite_lua_method_calls(expr, lowering_context, line_no)?;
    let length_rewritten =
        rewrite_lua_length_operator(&method_rewritten, lowering_context, line_no)?;
    Ok(rewrite_lua_expr_tokens(
        &length_rewritten,
        vm_namespace_aliases,
    ))
}

fn rewrite_lua_length_operator(
    expr: &str,
    lowering_context: &mut LuaLoweringContext,
    line_no: usize,
) -> Result<String, ParseError> {
    let bytes = expr.as_bytes();
    let mut out = String::with_capacity(expr.len());
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
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

        if b == b'"' || b == b'\'' {
            out.push(b as char);
            string_delim = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if b != b'#' {
            out.push(b as char);
            i += 1;
            continue;
        }

        let operand_start = skip_inline_whitespace(bytes, i + 1);
        if operand_start >= bytes.len() {
            return Err(ParseError {
                span: None,
                code: None,
                line: line_no,
                message: "lua length operator '#' missing operand".to_string(),
            });
        }
        let operand_end = parse_lua_length_operand_end(expr, operand_start, line_no)?;
        lowering_context.needs_table_len_helper = true;
        out.push_str("__lua_len(");
        out.push_str(&expr[operand_start..operand_end]);
        out.push(')');
        i = operand_end;
    }

    Ok(out)
}

fn parse_lua_length_operand_end(
    input: &str,
    start: usize,
    line_no: usize,
) -> Result<usize, ParseError> {
    let bytes = input.as_bytes();
    let first = bytes[start];
    if first == b'(' {
        return parse_balanced_segment(input, start, b'(', b')', line_no);
    }
    if first == b'[' {
        return parse_balanced_segment(input, start, b'[', b']', line_no);
    }
    if first == b'{' {
        return parse_balanced_segment(input, start, b'{', b'}', line_no);
    }
    if first == b'"' || first == b'\'' {
        return parse_lua_string_end(input, start, line_no);
    }
    if !is_ident_start(first as char) {
        return Err(ParseError {
            span: None,
            code: None,
            line: line_no,
            message: "unsupported operand for lua length operator '#'".to_string(),
        });
    }

    let mut cursor = start + 1;
    while cursor < bytes.len() && is_ident_continue(bytes[cursor] as char) {
        cursor += 1;
    }

    loop {
        let ws = skip_inline_whitespace(bytes, cursor);
        if ws >= bytes.len() {
            return Ok(cursor);
        }
        if bytes[ws] == b'.' {
            let member_start = skip_inline_whitespace(bytes, ws + 1);
            if member_start >= bytes.len() || !is_ident_start(bytes[member_start] as char) {
                return Ok(cursor);
            }
            let mut member_end = member_start + 1;
            while member_end < bytes.len() && is_ident_continue(bytes[member_end] as char) {
                member_end += 1;
            }
            cursor = member_end;
            continue;
        }
        if bytes[ws] == b'?' {
            let dot = skip_inline_whitespace(bytes, ws + 1);
            if dot >= bytes.len() || bytes[dot] != b'.' {
                return Ok(cursor);
            }
            let target_start = skip_inline_whitespace(bytes, dot + 1);
            if target_start >= bytes.len() {
                return Ok(cursor);
            }
            if bytes[target_start] == b'[' {
                cursor = parse_balanced_segment(input, target_start, b'[', b']', line_no)?;
                continue;
            }
            if !is_ident_start(bytes[target_start] as char) {
                return Ok(cursor);
            }
            let mut target_end = target_start + 1;
            while target_end < bytes.len() && is_ident_continue(bytes[target_end] as char) {
                target_end += 1;
            }
            cursor = target_end;
            continue;
        }
        if bytes[ws] == b'[' {
            cursor = parse_balanced_segment(input, ws, b'[', b']', line_no)?;
            continue;
        }
        if bytes[ws] == b'(' {
            cursor = parse_balanced_segment(input, ws, b'(', b')', line_no)?;
            continue;
        }
        return Ok(cursor);
    }
}

fn parse_balanced_segment(
    input: &str,
    start: usize,
    open: u8,
    close: u8,
    line_no: usize,
) -> Result<usize, ParseError> {
    let bytes = input.as_bytes();
    if start >= bytes.len() || bytes[start] != open {
        return Err(ParseError {
            span: None,
            code: None,
            line: line_no,
            message: "malformed lua expression while parsing '#' operand".to_string(),
        });
    }

    let mut i = start;
    let mut depth = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(delim) = string_delim {
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

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
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
                return Ok(i);
            }
            continue;
        }

        i += 1;
    }

    Err(ParseError {
        span: None,
        code: None,
        line: line_no,
        message: "unterminated lua expression while parsing '#' operand".to_string(),
    })
}

fn parse_lua_string_end(input: &str, start: usize, line_no: usize) -> Result<usize, ParseError> {
    let bytes = input.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == quote {
            return Ok(i + 1);
        }
        i += 1;
    }
    Err(ParseError {
        span: None,
        code: None,
        line: line_no,
        message: "unterminated lua string while parsing '#' operand".to_string(),
    })
}

fn rewrite_lua_method_calls(
    expr: &str,
    lowering_context: &mut LuaLoweringContext,
    line_no: usize,
) -> Result<String, ParseError> {
    let bytes = expr.as_bytes();
    let mut out = String::with_capacity(expr.len());
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
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

        if b == b'"' || b == b'\'' {
            out.push(b as char);
            string_delim = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if !is_ident_start(b as char) {
            out.push(b as char);
            i += 1;
            continue;
        }

        let receiver_start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i] as char) {
            i += 1;
        }
        let receiver = &expr[receiver_start..i];
        let mut cursor = skip_inline_whitespace(bytes, i);
        if cursor >= bytes.len() || bytes[cursor] != b':' {
            out.push_str(receiver);
            continue;
        }
        cursor += 1;
        cursor = skip_inline_whitespace(bytes, cursor);
        if cursor >= bytes.len() || !is_ident_start(bytes[cursor] as char) {
            out.push_str(receiver);
            out.push(':');
            i = cursor;
            continue;
        }
        let method_start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_ident_continue(bytes[cursor] as char) {
            cursor += 1;
        }
        let method = &expr[method_start..cursor];
        cursor = skip_inline_whitespace(bytes, cursor);
        if cursor >= bytes.len() || bytes[cursor] != b'(' {
            out.push_str(receiver);
            out.push(':');
            out.push_str(method);
            i = cursor;
            continue;
        }

        let (args_raw, next_index) = parse_balanced_call_args(expr, cursor, line_no)?;
        let rewritten =
            rewrite_lua_method_invocation(receiver, method, &args_raw, lowering_context, line_no)?;
        out.push_str(&rewritten);
        i = next_index;
    }

    Ok(out)
}

fn rewrite_lua_method_invocation(
    receiver: &str,
    method: &str,
    args_raw: &str,
    lowering_context: &mut LuaLoweringContext,
    line_no: usize,
) -> Result<String, ParseError> {
    let mut args = Vec::new();
    for arg in split_top_level_csv(args_raw) {
        args.push(rewrite_lua_method_calls(
            arg.trim(),
            lowering_context,
            line_no,
        )?);
    }

    let rewritten = match method {
        "len" => {
            if !args.is_empty() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua string method ':len' expects no arguments".to_string(),
                });
            }
            format!("({receiver}).length")
        }
        "sub" => match args.as_slice() {
            [start] => {
                lowering_context.needs_string_sub_helpers = true;
                format!("__lua_string_sub_from({receiver}, {start})")
            }
            [start, end] => {
                lowering_context.needs_string_sub_helpers = true;
                format!("__lua_string_sub_range({receiver}, {start}, {end})")
            }
            _ => {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "lua string method ':sub' expects 1 or 2 arguments".to_string(),
                });
            }
        },
        "find" | "match" | "gsub" => {
            return Err(ParseError {
                span: None,
                code: None,
                line: line_no,
                message: format!(
                    "lua string method ':{method}' (Lua pattern API) is not supported in this subset yet"
                ),
            });
        }
        _ => {
            if args.is_empty() {
                format!("{method}({receiver})")
            } else {
                format!("{method}({receiver}, {})", args.join(", "))
            }
        }
    };
    Ok(rewritten)
}

fn parse_balanced_call_args(
    input: &str,
    open_paren_index: usize,
    line_no: usize,
) -> Result<(String, usize), ParseError> {
    let bytes = input.as_bytes();
    let mut i = open_paren_index;
    let mut depth = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(delim) = string_delim {
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

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
            escaped = false;
            i += 1;
            continue;
        }

        if b == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            if depth == 0 {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: line_no,
                    message: "malformed lua method call argument list".to_string(),
                });
            }
            depth -= 1;
            if depth == 0 {
                let args = input[open_paren_index + 1..i].to_string();
                return Ok((args, i + 1));
            }
            i += 1;
            continue;
        }
        i += 1;
    }

    Err(ParseError {
        span: None,
        code: None,
        line: line_no,
        message: "unterminated lua method call argument list".to_string(),
    })
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

fn rewrite_lua_expr_tokens(expr: &str, vm_namespace_aliases: &HashSet<String>) -> String {
    let bytes = expr.as_bytes();
    let mut out = String::with_capacity(expr.len());
    let mut i = 0usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(delim) = string_delim {
            if escaped {
                out.push(b as char);
                escaped = false;
            } else if b == b'\\' {
                out.push('\\');
                escaped = true;
            } else if b == delim {
                out.push('"');
                string_delim = None;
            } else if delim == b'\'' && b == b'"' {
                out.push_str("\\\"");
            } else {
                out.push(b as char);
            }
            i += 1;
            continue;
        }

        if b == b'"' || b == b'\'' {
            out.push('"');
            string_delim = Some(b);
            i += 1;
            continue;
        }

        if b == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b'.' {
            out.push('+');
            i += 2;
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
                        segments.push(expr[member_start..k].to_string());
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
                        out.push_str(ident);
                        out.push_str("::");
                        out.push_str(&segments.join("::"));
                        i = k;
                        continue;
                    }
                }
            }

            if ident == "not" {
                out.push('!');
            } else if ident == "and" {
                out.push_str("&&");
            } else if ident == "or" {
                out.push_str("||");
            } else if ident == "nil" {
                out.push_str("null");
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

const LUA_STRING_SUB_HELPERS: &str = r#"fn __lua_string_norm_start_index(total_len, raw_index) {
    let normalized = 0;
    if raw_index < 0 {
        normalized = total_len + raw_index;
    } else {
        normalized = raw_index - 1;
    }
    if normalized < 0 {
        normalized = 0;
    }
    if normalized > total_len {
        normalized = total_len;
    }
    normalized;
}

fn __lua_string_norm_end_exclusive(total_len, raw_index) {
    let normalized = 0;
    if raw_index < 0 {
        normalized = total_len + raw_index + 1;
    } else {
        normalized = raw_index;
    }
    if normalized < 0 {
        normalized = 0;
    }
    if normalized > total_len {
        normalized = total_len;
    }
    normalized;
}

fn __lua_string_sub_from(value, start_raw) {
    let total_len = (value).length;
    let start = __lua_string_norm_start_index(total_len, start_raw);
    (value)[start:];
}

fn __lua_string_sub_range(value, start_raw, end_raw) {
    let total_len = (value).length;
    let start = __lua_string_norm_start_index(total_len, start_raw);
    let end_exclusive = __lua_string_norm_end_exclusive(total_len, end_raw);
    (value)[start:end_exclusive];
}"#;

const LUA_TABLE_LEN_HELPER: &str = r#"fn __lua_has_key(container, key) {
    let available_keys = (container).keys;
    let found = false;
    let i = 0;
    while i < (available_keys).length {
        if (available_keys)[i] == key {
            found = true;
            i = (available_keys).length;
        } else {
            i = i + 1;
        }
    }
    found;
}

fn __lua_len(value) {
    let out = 0;
    let ty = type(value);
    if ty == "map" {
        let count = 0;
        while __lua_has_key(value, count) {
            count = count + 1;
        }
        out = count;
    } else if ty == "array" {
        out = (value).length;
    } else {
        out = (value).length;
    }
    out;
}"#;

fn emit_lua_helpers(lowering_context: &LuaLoweringContext) -> Vec<String> {
    let mut helper_lines = Vec::new();
    if lowering_context.needs_table_len_helper {
        helper_lines.extend(LUA_TABLE_LEN_HELPER.lines().map(str::to_string));
        helper_lines.push(String::new());
    }
    if lowering_context.needs_string_sub_helpers {
        helper_lines.extend(LUA_STRING_SUB_HELPERS.lines().map(str::to_string));
        helper_lines.push(String::new());
    }
    helper_lines
}

fn split_top_level_csv(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut string_delim: Option<char> = None;
    let mut escaped = false;

    for ch in input.chars() {
        if let Some(delim) = string_delim {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delim {
                string_delim = None;
            }
            continue;
        }

        match ch {
            '"' | '\'' => {
                string_delim = Some(ch);
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
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(ch);
            }
            '{' => {
                brace_depth += 1;
                current.push(ch);
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
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
    let mut string_delim: Option<u8> = None;
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

        if let Some(delim) = string_delim {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                string_delim = None;
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

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
            out.push(b as char);
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
            span: None,
            code: None,
            line,
            message: "unterminated lua block comment".to_string(),
        });
    }
    Ok(out)
}
