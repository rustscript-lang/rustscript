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
        message: "unsupported Lua syntax".to_string(),
    })
}
fn try_lower_direct_subset_to_ir(source: &str) -> Result<Option<FrontendIr>, ParseError> {
    let cleaned_source = remove_lua_comments(source)?;
    let mut builder = LocalIrBuilder::new();
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
#[derive(Clone)]
enum LuaDirectExpr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
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
    Float(f64),
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
    builder: &LocalIrBuilder,
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
fn lower_lua_direct_expr(expr: LuaDirectExpr, builder: &LocalIrBuilder) -> Option<Expr> {
    match expr {
        LuaDirectExpr::Null => Some(Expr::Null),
        LuaDirectExpr::Bool(value) => Some(Expr::Bool(value)),
        LuaDirectExpr::Int(value) => Some(Expr::Int(value)),
        LuaDirectExpr::Float(value) => Some(Expr::Float(value)),
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
                LuaDirectToken::Float(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Float(value))
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
            let mut is_float = false;
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                is_float = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text = std::str::from_utf8(&bytes[start..i]).ok()?;
            if is_float {
                out.push(LuaDirectToken::Float(text.parse::<f64>().ok()?));
            } else {
                out.push(LuaDirectToken::Int(text.parse::<i64>().ok()?));
            }
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
                    match ch {
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'\\' => text.push('\\'),
                        b'"' => text.push('"'),
                        b'\'' => text.push('\''),
                        b'0' => text.push('\0'),
                        b'x' => {
                            if i + 1 >= bytes.len() {
                                return None;
                            }
                            let hi = hex_nibble_byte(bytes[i])?;
                            let lo = hex_nibble_byte(bytes[i + 1])?;
                            text.push(((hi << 4) | lo) as char);
                            i += 2;
                        }
                        other => text.push(other as char),
                    }
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

fn hex_nibble_byte(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
