use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::ParseError;
use super::{is_ident_continue, is_ident_start};

static GENSYM_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn gensym(prefix: &str) -> String {
    let id = GENSYM_COUNTER
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("scheme gensym counter exhausted");
    format!("__{prefix}_{id}")
}

fn wrap_let_expr(name: &str, value: &str, body: &str) -> String {
    format!("if true => {{ let {name} = {value}; {body} }} else => {{ false }}")
}

fn wrap_statement_sequence(stmts: Vec<String>, expr: String) -> String {
    let mut out = expr;
    for stmt in stmts.into_iter().rev() {
        out = format!("if true => {{ {stmt} {out} }} else => {{ false }}");
    }
    out
}

pub(super) fn lower(source: &str) -> Result<String, ParseError> {
    let mut parser = SchemeParser::new(source)?;
    let forms = parser.parse_program()?;

    let mut out = Vec::new();
    for form in &forms {
        lower_stmt(form, 0, &mut out)?;
    }

    Ok(out.join("\n"))
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
                s if s.chars().count() == 1 => s.chars().next().unwrap(),
                _ => {
                    return Err(ParseError {
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

fn lower_stmt(form: &SchemeForm, indent: usize, out: &mut Vec<String>) -> Result<(), ParseError> {
    if let Some(items) = form.as_list()
        && let Some(head) = items.first().and_then(|item| item.as_symbol())
    {
        let args = &items[1..];
        match head {
            "define" => return lower_define_stmt(args, form.line, indent, out),
            "set!" => return lower_set_stmt(args, form.line, indent, out),
            "if" => return lower_if_stmt(args, form.line, indent, out),
            "when" => return lower_when_unless_stmt(args, form.line, indent, out, false),
            "unless" => return lower_when_unless_stmt(args, form.line, indent, out, true),
            "cond" => return lower_cond_stmt(args, form.line, indent, out),
            "case" => return lower_case_stmt(args, form.line, indent, out),
            "while" => return lower_while_stmt(args, form.line, indent, out),
            "do" => return lower_do_stmt(args, form.line, indent, out),
            "for" => return lower_for_stmt(args, form.line, indent, out),
            "break" => return lower_break_stmt(args, form.line, indent, out),
            "continue" => return lower_continue_stmt(args, form.line, indent, out),
            "import" | "require" => return lower_import_require_stmt(args, form.line, indent, out),
            "vector-set!" | "hash-set!" => {
                return lower_index_set_stmt(head, args, form.line, indent, out);
            }
            "begin" => return lower_begin_stmt(args, indent, out),
            "declare" => return lower_declare_stmt(args, form.line, indent, out),
            "display" | "write" => return lower_display_stmt(args, form.line, indent, out),
            "newline" => return lower_newline_stmt(args, form.line, indent, out),
            "for-each" => return lower_for_each_stmt(args, form.line, indent, out),
            _ => {}
        }
    }

    let expr = lower_expr(form)?;
    push_line(out, indent, &format!("{expr};"));
    Ok(())
}

fn lower_define_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "define expects a target and value".to_string(),
        });
    }

    match &args[0].node {
        SchemeNode::Symbol(name_raw) => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "variable define expects exactly one value".to_string(),
                });
            }
            let name = normalize_identifier(name_raw, line, "define target")?;
            let value = lower_expr(&args[1])?;
            push_line(out, indent, &format!("let {name} = {value};"));
            Ok(())
        }
        SchemeNode::List(signature) => {
            if signature.is_empty() {
                return Err(ParseError {
                    line,
                    message: "function define requires a name".to_string(),
                });
            }
            if args.len() < 2 {
                return Err(ParseError {
                    line,
                    message: "function define expects at least one body expression".to_string(),
                });
            }
            let name_raw = signature[0].as_symbol().ok_or(ParseError {
                line,
                message: "function define name must be a symbol".to_string(),
            })?;
            let name = normalize_identifier(name_raw, line, "function name")?;

            let mut params = Vec::new();
            for param in &signature[1..] {
                let param_raw = param.as_symbol().ok_or(ParseError {
                    line: param.line,
                    message: "function parameter must be a symbol".to_string(),
                })?;
                params.push(normalize_identifier(
                    param_raw,
                    param.line,
                    "function parameter",
                )?);
            }
            let body = lower_body_exprs(&args[1..], line)?;
            push_line(
                out,
                indent,
                &format!("let {name} = |{}| {body};", params.join(", ")),
            );
            Ok(())
        }
        _ => Err(ParseError {
            line,
            message: "define target must be a symbol or parameter list".to_string(),
        }),
    }
}

fn lower_set_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "set! expects exactly two arguments".to_string(),
        });
    }

    let name_raw = args[0].as_symbol().ok_or(ParseError {
        line: args[0].line,
        message: "set! target must be a symbol".to_string(),
    })?;
    let name = normalize_identifier(name_raw, args[0].line, "set! target")?;
    let value = lower_expr(&args[1])?;
    push_line(out, indent, &format!("{name} = {value};"));
    Ok(())
}

fn lower_if_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(ParseError {
            line,
            message: "if expects (if condition then [else])".to_string(),
        });
    }

    let condition = lower_expr(&args[0])?;
    push_line(out, indent, &format!("if {condition} {{"));
    lower_branch_body(&args[1], indent + 1, out)?;
    push_line(out, indent, "}");

    if args.len() == 3 {
        push_line(out, indent, "else {");
        lower_branch_body(&args[2], indent + 1, out)?;
        push_line(out, indent, "}");
    }

    Ok(())
}

fn lower_when_unless_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
    negate: bool,
) -> Result<(), ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: format!(
                "{} expects a condition and at least one body form",
                if negate { "unless" } else { "when" }
            ),
        });
    }
    let condition = lower_expr(&args[0])?;
    let cond_text = if negate {
        format!("!({condition})")
    } else {
        condition
    };
    push_line(out, indent, &format!("if {cond_text} {{"));
    for stmt in &args[1..] {
        lower_stmt(stmt, indent + 1, out)?;
    }
    push_line(out, indent, "}");
    Ok(())
}

fn lower_cond_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.is_empty() {
        return Err(ParseError {
            line,
            message: "cond expects at least one clause".to_string(),
        });
    }

    let mut first = true;
    for clause_form in args {
        let clause = clause_form.as_list().ok_or(ParseError {
            line: clause_form.line,
            message: "each cond clause must be a list".to_string(),
        })?;
        if clause.is_empty() {
            return Err(ParseError {
                line: clause_form.line,
                message: "cond clause must not be empty".to_string(),
            });
        }

        let is_else = clause[0].as_symbol() == Some("else");
        if is_else {
            if clause.len() < 2 {
                return Err(ParseError {
                    line: clause_form.line,
                    message: "cond else clause must have at least one body form".to_string(),
                });
            }
            if first {
                push_line(out, indent, "{");
            } else {
                push_line(out, indent, "else {");
            }
            for stmt in &clause[1..] {
                lower_stmt(stmt, indent + 1, out)?;
            }
            push_line(out, indent, "}");
            return Ok(());
        }

        if clause.len() < 2 {
            return Err(ParseError {
                line: clause_form.line,
                message: "cond clause must have a test and at least one body form".to_string(),
            });
        }

        let condition = lower_expr(&clause[0])?;
        if first {
            push_line(out, indent, &format!("if {condition} {{"));
            first = false;
        } else {
            push_line(out, indent, &format!("else if {condition} {{"));
        }
        for stmt in &clause[1..] {
            lower_stmt(stmt, indent + 1, out)?;
        }
        push_line(out, indent, "}");
    }

    Ok(())
}

fn lower_case_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "case expects a key expression and at least one clause".to_string(),
        });
    }

    let key_var = gensym("case");
    let key_expr = lower_expr(&args[0])?;
    push_line(out, indent, &format!("let {key_var} = {key_expr};"));

    let mut first = true;
    for clause_form in &args[1..] {
        let clause = clause_form.as_list().ok_or(ParseError {
            line: clause_form.line,
            message: "each case clause must be a list".to_string(),
        })?;
        if clause.is_empty() {
            return Err(ParseError {
                line: clause_form.line,
                message: "case clause must not be empty".to_string(),
            });
        }

        let is_else = clause[0].as_symbol() == Some("else");
        if is_else {
            if clause.len() < 2 {
                return Err(ParseError {
                    line: clause_form.line,
                    message: "case else clause must have at least one body form".to_string(),
                });
            }
            if first {
                push_line(out, indent, "{");
            } else {
                push_line(out, indent, "else {");
            }
            for stmt in &clause[1..] {
                lower_stmt(stmt, indent + 1, out)?;
            }
            push_line(out, indent, "}");
            return Ok(());
        }

        let datums = clause[0].as_list().ok_or(ParseError {
            line: clause[0].line,
            message: "case datum list must be a list".to_string(),
        })?;
        if clause.len() < 2 {
            return Err(ParseError {
                line: clause_form.line,
                message: "case clause must have datums and at least one body form".to_string(),
            });
        }

        let mut conditions = Vec::new();
        for datum in datums {
            let datum_expr = lower_expr(datum)?;
            conditions.push(format!("({key_var} == {datum_expr})"));
        }
        let combined = if conditions.len() == 1 {
            conditions.into_iter().next().unwrap()
        } else {
            // (a || b || c) — but we don't have || operator, so chain with if-expressions
            // Use nested: if a => { true } else if b => { true } else => { false }
            lower_or_chain_text(&conditions)
        };

        if first {
            push_line(out, indent, &format!("if {combined} {{"));
            first = false;
        } else {
            push_line(out, indent, &format!("else if {combined} {{"));
        }
        for stmt in &clause[1..] {
            lower_stmt(stmt, indent + 1, out)?;
        }
        push_line(out, indent, "}");
    }

    Ok(())
}

fn lower_display_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() != 1 {
        return Err(ParseError {
            line,
            message: "display/write expects exactly one argument".to_string(),
        });
    }
    let value = lower_expr(&args[0])?;
    push_line(out, indent, &format!("print({value});"));
    Ok(())
}

fn lower_newline_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if !args.is_empty() {
        return Err(ParseError {
            line,
            message: "newline does not accept arguments".to_string(),
        });
    }
    push_line(out, indent, "print(\"\\n\");");
    Ok(())
}

fn lower_for_each_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "for-each expects (for-each proc list)".to_string(),
        });
    }
    let func = lower_expr(&args[0])?;
    let list = lower_expr(&args[1])?;
    let idx = gensym("fe_i");
    let vec = gensym("fe_v");
    let callable = if func.trim_start().starts_with('|') {
        let f = gensym("fe_f");
        push_line(out, indent, &format!("let {f} = {func};"));
        f
    } else {
        func
    };
    push_line(out, indent, &format!("let {vec} = {list};"));
    push_line(
        out,
        indent,
        &format!("for (let {idx} = 0; {idx} < len({vec}); {idx} = {idx} + 1) {{"),
    );
    push_line(out, indent + 1, &format!("{callable}(({vec})[{idx}]);"));
    push_line(out, indent, "}");
    Ok(())
}

/// Helper: produce an or-chain of boolean text expressions using nested if-expressions.
/// No `||` in the target language, so we use `if a => { true } else if b => { true } else => { false }`.
fn lower_or_chain_text(conditions: &[String]) -> String {
    if conditions.is_empty() {
        return "false".to_string();
    }
    if conditions.len() == 1 {
        return conditions[0].clone();
    }
    let mut result = "false".to_string();
    for cond in conditions.iter().rev() {
        result = format!("if {cond} => {{ true }} else => {{ {result} }}");
    }
    result
}

fn lower_while_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.is_empty() {
        return Err(ParseError {
            line,
            message: "while expects (while condition body...)".to_string(),
        });
    }

    let condition = lower_expr(&args[0])?;
    push_line(out, indent, &format!("while {condition} {{"));
    for stmt in &args[1..] {
        lower_stmt(stmt, indent + 1, out)?;
    }
    push_line(out, indent, "}");
    Ok(())
}

fn lower_do_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "do expects (do ((name init [step]) ...) (test expr...) body...)".to_string(),
        });
    }

    let bindings = args[0].as_list().ok_or(ParseError {
        line: args[0].line,
        message: "do bindings must be a list".to_string(),
    })?;
    let mut binding_names = HashSet::new();
    let mut step_updates = Vec::new();

    for (index, binding_form) in bindings.iter().enumerate() {
        let binding = binding_form.as_list().ok_or(ParseError {
            line: binding_form.line,
            message: "each do binding must be a list".to_string(),
        })?;
        if binding.len() < 2 || binding.len() > 3 {
            return Err(ParseError {
                line: binding_form.line,
                message: "do binding must be (name init [step])".to_string(),
            });
        }

        let name_raw = binding[0].as_symbol().ok_or(ParseError {
            line: binding[0].line,
            message: "do binding name must be a symbol".to_string(),
        })?;
        let name = normalize_identifier(name_raw, binding[0].line, "do binding name")?;
        if !binding_names.insert(name.clone()) {
            return Err(ParseError {
                line: binding[0].line,
                message: format!("duplicate do binding '{name_raw}'"),
            });
        }

        let init = lower_expr(&binding[1])?;
        push_line(out, indent, &format!("let {name} = {init};"));

        if binding.len() == 3 {
            let step = lower_expr(&binding[2])?;
            let temp = format!("__do_step_{}_{}", line, index);
            step_updates.push((name, temp, step));
        }
    }

    let test_clause = args[1].as_list().ok_or(ParseError {
        line: args[1].line,
        message: "do test clause must be a list".to_string(),
    })?;
    if test_clause.is_empty() {
        return Err(ParseError {
            line: args[1].line,
            message: "do test clause must start with a test expression".to_string(),
        });
    }

    let test_expr = lower_expr(&test_clause[0])?;
    let result_exprs = &test_clause[1..];

    push_line(out, indent, "while true {");
    push_line(out, indent + 1, &format!("if {test_expr} {{"));
    if let Some((last, prefix)) = result_exprs.split_last() {
        for (index, expr) in prefix.iter().enumerate() {
            let lowered = lower_expr(expr)?;
            let temp = format!("__do_result_{}_{}", line, index);
            push_line(out, indent + 2, &format!("let {temp} = {lowered};"));
        }
        let lowered_last = lower_expr(last)?;
        push_line(out, indent + 2, &format!("{lowered_last};"));
    }
    push_line(out, indent + 2, "break;");
    push_line(out, indent + 1, "}");

    for stmt in &args[2..] {
        lower_stmt(stmt, indent + 1, out)?;
    }

    for (_, temp, step) in &step_updates {
        push_line(out, indent + 1, &format!("let {temp} = {step};"));
    }
    for (name, temp, _) in &step_updates {
        push_line(out, indent + 1, &format!("{name} = {temp};"));
    }
    push_line(out, indent, "}");
    Ok(())
}

fn lower_for_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "for expects (for (name start end [step]) body...)".to_string(),
        });
    }

    let header = args[0].as_list().ok_or(ParseError {
        line: args[0].line,
        message: "for header must be a list".to_string(),
    })?;
    if header.len() < 3 || header.len() > 4 {
        return Err(ParseError {
            line: args[0].line,
            message: "for header must be (name start end [step])".to_string(),
        });
    }

    let name_raw = header[0].as_symbol().ok_or(ParseError {
        line: header[0].line,
        message: "for loop variable must be a symbol".to_string(),
    })?;
    let name = normalize_identifier(name_raw, header[0].line, "for loop variable")?;
    let start = lower_expr(&header[1])?;
    let end = lower_expr(&header[2])?;
    let step = if header.len() == 4 {
        lower_expr(&header[3])?
    } else {
        "1".to_string()
    };

    push_line(
        out,
        indent,
        &format!("for (let {name} = {start}; {name} < ({end}); {name} = {name} + ({step})) {{"),
    );
    for stmt in &args[1..] {
        lower_stmt(stmt, indent + 1, out)?;
    }
    push_line(out, indent, "}");
    Ok(())
}

fn lower_break_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if !args.is_empty() {
        return Err(ParseError {
            line,
            message: "break does not accept arguments".to_string(),
        });
    }
    push_line(out, indent, "break;");
    Ok(())
}

fn lower_continue_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if !args.is_empty() {
        return Err(ParseError {
            line,
            message: "continue does not accept arguments".to_string(),
        });
    }
    push_line(out, indent, "continue;");
    Ok(())
}

fn lower_index_set_stmt(
    head: &str,
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() != 3 {
        return Err(ParseError {
            line,
            message: format!("{head} expects exactly three arguments"),
        });
    }
    let target_raw = args[0].as_symbol().ok_or(ParseError {
        line: args[0].line,
        message: format!("{head} target must be a symbol"),
    })?;
    let target = normalize_identifier(target_raw, args[0].line, &format!("{head} target"))?;
    let key = if head == "hash-set!" {
        lower_hash_key_expr(&args[1])?
    } else {
        lower_expr(&args[1])?
    };
    let value = lower_expr(&args[2])?;
    push_line(out, indent, &format!("{target}[{key}] = {value};"));
    Ok(())
}

fn lower_begin_stmt(
    args: &[SchemeForm],
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    for stmt in args {
        lower_stmt(stmt, indent, out)?;
    }
    Ok(())
}

fn lower_declare_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if args.len() != 1 {
        return Err(ParseError {
            line,
            message: "declare expects exactly one signature list".to_string(),
        });
    }

    let signature = args[0].as_list().ok_or(ParseError {
        line: args[0].line,
        message: "declare signature must be a list".to_string(),
    })?;
    if signature.is_empty() {
        return Err(ParseError {
            line: args[0].line,
            message: "declare signature cannot be empty".to_string(),
        });
    }

    let name_raw = signature[0].as_symbol().ok_or(ParseError {
        line: signature[0].line,
        message: "declare function name must be a symbol".to_string(),
    })?;
    let name = normalize_identifier(name_raw, signature[0].line, "declare function name")?;

    let mut params = Vec::new();
    for param in &signature[1..] {
        let param_raw = param.as_symbol().ok_or(ParseError {
            line: param.line,
            message: "declare parameter must be a symbol".to_string(),
        })?;
        params.push(normalize_identifier(
            param_raw,
            param.line,
            "declare parameter",
        )?);
    }

    push_line(out, indent, &format!("fn {name}({});", params.join(", ")));
    Ok(())
}

fn lower_import_require_stmt(
    args: &[SchemeForm],
    line: usize,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    let mut emitted_any = false;
    let mut emitted_vm_wildcard = false;

    for arg in args {
        let Some(clause) = arg.as_list() else {
            if let SchemeNode::String(spec) = &arg.node
                && spec == "vm"
            {
                if !emitted_vm_wildcard {
                    push_line(out, indent, "use vm::*;");
                    emitted_vm_wildcard = true;
                }
                emitted_any = true;
            }
            continue;
        };
        if clause.is_empty() {
            continue;
        }
        let Some(head) = clause[0].as_symbol() else {
            continue;
        };
        match head {
            "only" | "only-in" => {
                if clause.len() < 3 {
                    return Err(ParseError {
                        line,
                        message: format!("{head} import requires module path and bindings"),
                    });
                }
                let Some(module_spec) = clause[1].as_symbol().or_else(|| {
                    if let SchemeNode::String(spec) = &clause[1].node {
                        Some(spec.as_str())
                    } else {
                        None
                    }
                }) else {
                    return Err(ParseError {
                        line: clause[1].line,
                        message: format!("{head} module path must be a symbol or string"),
                    });
                };
                if module_spec != "vm" {
                    continue;
                }
                if !emitted_vm_wildcard {
                    push_line(out, indent, "use vm::*;");
                    emitted_vm_wildcard = true;
                }
                let mut bindings = Vec::new();
                for binding in &clause[2..] {
                    if let Some(symbol) = binding.as_symbol() {
                        let name = normalize_identifier(symbol, binding.line, "vm import binding")?;
                        bindings.push(name);
                        continue;
                    }
                    let Some(pair) = binding.as_list() else {
                        return Err(ParseError {
                            line: binding.line,
                            message: "vm import binding must be a symbol or (imported local)"
                                .to_string(),
                        });
                    };
                    if pair.len() != 2 {
                        return Err(ParseError {
                            line: binding.line,
                            message: "vm import rename must be (imported local)".to_string(),
                        });
                    }
                    let imported = pair[0].as_symbol().ok_or(ParseError {
                        line: pair[0].line,
                        message: "vm import rename source must be a symbol".to_string(),
                    })?;
                    let local = pair[1].as_symbol().ok_or(ParseError {
                        line: pair[1].line,
                        message: "vm import rename target must be a symbol".to_string(),
                    })?;
                    let imported =
                        normalize_identifier(imported, pair[0].line, "vm import source")?;
                    let local = normalize_identifier(local, pair[1].line, "vm import target")?;
                    if imported == local {
                        bindings.push(imported);
                    } else {
                        bindings.push(format!("{imported} as {local}"));
                    }
                }
                if !bindings.is_empty() {
                    push_line(
                        out,
                        indent,
                        &format!("use vm::{{{}}};", bindings.join(", ")),
                    );
                }
                emitted_any = true;
            }
            "prefix" | "prefix-in" => {
                if clause.len() < 3 {
                    return Err(ParseError {
                        line,
                        message: format!("{head} import requires module path and prefix"),
                    });
                }
                let module_candidate = if head == "prefix" {
                    &clause[1]
                } else {
                    &clause[2]
                };
                let module_spec = module_candidate.as_symbol().or({
                    if let SchemeNode::String(spec) = &module_candidate.node {
                        Some(spec.as_str())
                    } else {
                        None
                    }
                });
                if module_spec != Some("vm") {
                    continue;
                }
                if !emitted_vm_wildcard {
                    push_line(out, indent, "use vm::*;");
                    emitted_vm_wildcard = true;
                }
                emitted_any = true;
            }
            _ => {}
        }
    }

    if !emitted_any {
        push_line(out, indent, "");
    }
    Ok(())
}

fn lower_branch_body(
    form: &SchemeForm,
    indent: usize,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    if let Some(items) = form.as_list()
        && let Some("begin") = items.first().and_then(|item| item.as_symbol())
    {
        for stmt in &items[1..] {
            lower_stmt(stmt, indent, out)?;
        }
        return Ok(());
    }

    lower_stmt(form, indent, out)
}

fn lower_expr(form: &SchemeForm) -> Result<String, ParseError> {
    match &form.node {
        SchemeNode::Int(value) => Ok(value.to_string()),
        SchemeNode::Float(value) => {
            let s = value.to_string();
            // Ensure it has a decimal point for the target parser
            if s.contains('.') {
                Ok(s)
            } else {
                Ok(format!("{s}.0"))
            }
        }
        SchemeNode::Bool(value) => Ok(value.to_string()),
        SchemeNode::Char(ch) => {
            // Lower char to its integer code point
            Ok((*ch as u32).to_string())
        }
        SchemeNode::String(value) => Ok(render_string(value)),
        SchemeNode::Symbol(name) => {
            if name == "true" {
                return Ok("true".to_string());
            }
            if name == "false" {
                return Ok("false".to_string());
            }
            if name == "null" || name == "nil" {
                // RustScript surface syntax has no null literal; optional access on an empty map yields null.
                return Ok("({})?.__null".to_string());
            }
            if let Some(chain) = lower_optional_chain_symbol(name, form.line)? {
                return Ok(chain);
            }
            normalize_identifier(name, form.line, "symbol")
        }
        SchemeNode::List(items) => lower_list_expr(items, form.line),
    }
}

fn lower_optional_chain_symbol(name: &str, line: usize) -> Result<Option<String>, ParseError> {
    if !name.contains("?.") {
        return Ok(None);
    }

    let parts: Vec<&str> = name.split("?.").collect();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return Err(ParseError {
            line,
            message: format!("invalid optional chain symbol '{name}'"),
        });
    }

    let root = normalize_identifier(parts[0], line, "optional chain root")?;
    let mut out = root;
    for member in &parts[1..] {
        if !is_valid_member_ident(member) {
            return Err(ParseError {
                line,
                message: format!("invalid optional chain member '{member}' in '{name}'"),
            });
        }
        out.push_str("?.");
        out.push_str(member);
    }
    Ok(Some(out))
}

fn is_valid_member_ident(member: &str) -> bool {
    let mut chars = member.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
}

fn lower_list_expr(items: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if items.is_empty() {
        return Err(ParseError {
            line,
            message: "cannot lower empty list expression".to_string(),
        });
    }

    let head = items[0].as_symbol().ok_or(ParseError {
        line: items[0].line,
        message: "list head must be a symbol".to_string(),
    })?;
    let args = &items[1..];

    match head {
        // Arithmetic
        "+" => {
            if args.is_empty() {
                return Ok("0".to_string());
            }
            if args.len() == 1 {
                return lower_expr(&args[0]);
            }
            fold_infix_expr(args, "+", line, 2, "+ expects at least two arguments")
        }
        "*" => {
            if args.is_empty() {
                return Ok("1".to_string());
            }
            if args.len() == 1 {
                return lower_expr(&args[0]);
            }
            fold_infix_expr(args, "*", line, 2, "* expects at least two arguments")
        }
        "/" => fold_infix_expr(args, "/", line, 2, "/ expects at least two arguments"),
        "-" => {
            if args.is_empty() {
                return Err(ParseError {
                    line,
                    message: "- expects at least one argument".to_string(),
                });
            }
            if args.len() == 1 {
                return Ok(format!("-({})", lower_expr(&args[0])?));
            }
            fold_infix_expr(args, "-", line, 2, "- expects at least one argument")
        }
        "modulo" => lower_modulo_expr(args, line),
        "remainder" => lower_remainder_expr(args, line),
        "quotient" => lower_binary_expr(args, "/", line, "quotient expects exactly two arguments"),
        "abs" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "abs expects exactly one argument".to_string(),
                });
            }
            let val = lower_expr(&args[0])?;
            let t = gensym("abs");
            Ok(wrap_let_expr(
                &t,
                &val,
                &format!("if {t} < 0 => {{ (0 - {t}) }} else => {{ {t} }}"),
            ))
        }
        "min" => {
            if args.len() < 2 {
                return Err(ParseError {
                    line,
                    message: "min expects at least two arguments".to_string(),
                });
            }
            let mut expr = lower_expr(&args[0])?;
            for arg in &args[1..] {
                let rhs = lower_expr(arg)?;
                let a = gensym("min_a");
                let b = gensym("min_b");
                let inner = wrap_let_expr(
                    &b,
                    &rhs,
                    &format!("if {a} < {b} => {{ {a} }} else => {{ {b} }}"),
                );
                expr = wrap_let_expr(&a, &expr, &inner);
            }
            Ok(expr)
        }
        "max" => {
            if args.len() < 2 {
                return Err(ParseError {
                    line,
                    message: "max expects at least two arguments".to_string(),
                });
            }
            let mut expr = lower_expr(&args[0])?;
            for arg in &args[1..] {
                let rhs = lower_expr(arg)?;
                let a = gensym("max_a");
                let b = gensym("max_b");
                let inner = wrap_let_expr(
                    &b,
                    &rhs,
                    &format!("if {a} > {b} => {{ {a} }} else => {{ {b} }}"),
                );
                expr = wrap_let_expr(&a, &expr, &inner);
            }
            Ok(expr)
        }

        // Comparison
        "=" => lower_binary_expr(args, "==", line, "= expects exactly two arguments"),
        "/=" => lower_binary_expr(args, "!=", line, "/= expects exactly two arguments"),
        "<" => lower_binary_expr(args, "<", line, "< expects exactly two arguments"),
        ">" => lower_binary_expr(args, ">", line, "> expects exactly two arguments"),
        "<=" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "<= expects exactly two arguments".to_string(),
                });
            }
            let lhs = lower_expr(&args[0])?;
            let rhs = lower_expr(&args[1])?;
            Ok(format!("!(({lhs}) > ({rhs}))"))
        }
        ">=" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: ">= expects exactly two arguments".to_string(),
                });
            }
            let lhs = lower_expr(&args[0])?;
            let rhs = lower_expr(&args[1])?;
            Ok(format!("!(({lhs}) < ({rhs}))"))
        }

        // Boolean
        "not" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "not expects exactly one argument".to_string(),
                });
            }
            let value = lower_expr(&args[0])?;
            Ok(format!("!({value})"))
        }
        "and" => lower_and_expr(args),
        "or" => lower_or_expr(args),

        // Type predicates
        "null?" => lower_type_check(args, line, "null"),
        "number?" | "integer?" => lower_type_check(args, line, "int"),
        "string?" => lower_type_check(args, line, "string"),
        "boolean?" => lower_type_check(args, line, "bool"),
        "vector?" | "list?" => lower_type_check(args, line, "array"),
        "pair?" => lower_type_check(args, line, "array"), // Lists are represented as arrays
        "procedure?" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "procedure? expects exactly one argument".to_string(),
                });
            }
            // In our VM, closures don't have a separate type — always return false
            Ok("false".to_string())
        }
        "symbol?" => {
            // No symbol type in the VM
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "symbol? expects exactly one argument".to_string(),
                });
            }
            Ok("false".to_string())
        }

        // Numeric predicates
        "zero?" => lower_predicate_expr(args, line, |x| format!("({x}) == 0")),
        "positive?" => lower_predicate_expr(args, line, |x| format!("({x}) > 0")),
        "negative?" => lower_predicate_expr(args, line, |x| format!("({x}) < 0")),
        "even?" => lower_predicate_expr(args, line, |x| format!("(({x}) - (({x}) / 2) * 2) == 0")),
        "odd?" => lower_predicate_expr(args, line, |x| format!("(({x}) - (({x}) / 2) * 2) != 0")),

        // Equality
        "eq?" | "eqv?" | "equal?" => lower_binary_expr(
            args,
            "==",
            line,
            &format!("{head} expects exactly two arguments"),
        ),

        // Lists/Pairs (represented as arrays)
        "list" => {
            let mut rendered = Vec::new();
            for arg in args {
                rendered.push(lower_expr(arg)?);
            }
            Ok(format!("[{}]", rendered.join(", ")))
        }
        "cons" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "cons expects exactly two arguments".to_string(),
                });
            }
            let car = lower_expr(&args[0])?;
            let cdr = lower_expr(&args[1])?;
            // cons creates a new array with car prepended to cdr (if cdr is array) or [car, cdr]
            let t = gensym("cons_cdr");
            Ok(wrap_let_expr(
                &t,
                &cdr,
                &format!(
                    "if type_of({t}) == \"array\" => {{ concat([{car}], {t}) }} else => {{ [{car}, {t}] }}"
                ),
            ))
        }
        "car" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "car expects exactly one argument".to_string(),
                });
            }
            let list = lower_expr(&args[0])?;
            Ok(format!("({list})[0]"))
        }
        "cdr" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "cdr expects exactly one argument".to_string(),
                });
            }
            let list = lower_expr(&args[0])?;
            Ok(format!("({list})[1:]"))
        }
        "cadr" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "cadr expects exactly one argument".to_string(),
                });
            }
            let list = lower_expr(&args[0])?;
            Ok(format!("({list})[1]"))
        }
        "caddr" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "caddr expects exactly one argument".to_string(),
                });
            }
            let list = lower_expr(&args[0])?;
            Ok(format!("({list})[2]"))
        }
        "length" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "length expects exactly one argument".to_string(),
                });
            }
            let list = lower_expr(&args[0])?;
            Ok(format!("len({list})"))
        }
        "append" => {
            if args.is_empty() {
                return Ok("[]".to_string());
            }
            if args.len() == 1 {
                return lower_expr(&args[0]);
            }
            let mut expr = lower_expr(&args[0])?;
            for arg in &args[1..] {
                let rhs = lower_expr(arg)?;
                expr = format!("concat({expr}, {rhs})");
            }
            Ok(expr)
        }
        "reverse" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "reverse expects exactly one argument".to_string(),
                });
            }
            let list = lower_expr(&args[0])?;
            let v = gensym("rev_v");
            let i = gensym("rev_i");
            let r = gensym("rev_r");
            let body = wrap_let_expr(
                &r,
                "[]",
                &format!(
                    "for (let {i} = len({v}) - 1; !({i} < 0); {i} = {i} - 1) {{ {r} = array_push({r}, ({v})[{i}]); }} {r}"
                ),
            );
            Ok(wrap_let_expr(&v, &list, &body))
        }

        // Higher-order
        "map" => lower_map_expr(args, line),
        "filter" => lower_filter_expr(args, line),
        "apply" => lower_apply_expr(args, line),

        // Strings
        "string-append" => {
            if args.is_empty() {
                return Ok("\"\"".to_string());
            }
            if args.len() == 1 {
                return lower_expr(&args[0]);
            }
            let mut expr = lower_expr(&args[0])?;
            for arg in &args[1..] {
                let rhs = lower_expr(arg)?;
                expr = format!("({expr} + {rhs})");
            }
            Ok(expr)
        }
        "string-length" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "string-length expects exactly one argument".to_string(),
                });
            }
            let s = lower_expr(&args[0])?;
            Ok(format!("len({s})"))
        }
        "string-ref" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "string-ref expects exactly two arguments".to_string(),
                });
            }
            let s = lower_expr(&args[0])?;
            let i = lower_expr(&args[1])?;
            Ok(format!("({s})[{i}]"))
        }
        "substring" => {
            if args.len() == 3 {
                let s = lower_expr(&args[0])?;
                let start = lower_expr(&args[1])?;
                let end = lower_expr(&args[2])?;
                Ok(format!("({s})[{start}:{end}]"))
            } else if args.len() == 2 {
                let s = lower_expr(&args[0])?;
                let start = lower_expr(&args[1])?;
                Ok(format!("({s})[{start}:]"))
            } else {
                Err(ParseError {
                    line,
                    message: "substring expects 2 or 3 arguments".to_string(),
                })
            }
        }
        "number->string" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "number->string expects exactly one argument".to_string(),
                });
            }
            let n = lower_expr(&args[0])?;
            Ok(format!("__to_string({n})"))
        }
        "string->number" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "string->number expects exactly one argument".to_string(),
                });
            }
            // No parse builtin — return 0 as a placeholder
            Ok("0".to_string())
        }

        // Collections
        "vector" => {
            let mut rendered = Vec::new();
            for arg in args {
                rendered.push(lower_expr(arg)?);
            }
            Ok(format!("[{}]", rendered.join(", ")))
        }
        "hash" => {
            let mut rendered = Vec::new();
            for entry in args {
                let pair = entry.as_list().ok_or(ParseError {
                    line: entry.line,
                    message: "hash entries must be two-item lists".to_string(),
                })?;
                if pair.len() != 2 {
                    return Err(ParseError {
                        line: entry.line,
                        message: "hash entries must contain exactly key and value".to_string(),
                    });
                }
                let key = lower_hash_key_expr(&pair[0])?;
                let value = lower_expr(&pair[1])?;
                rendered.push(format!("{key}: {value}"));
            }
            Ok(format!("{{{}}}", rendered.join(", ")))
        }
        "vector-ref" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "vector-ref expects exactly two arguments".to_string(),
                });
            }
            let container = lower_expr(&args[0])?;
            let key = lower_expr(&args[1])?;
            Ok(format!("({container})[{key}]"))
        }
        "hash-ref" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "hash-ref expects exactly two arguments".to_string(),
                });
            }
            let container = lower_expr(&args[0])?;
            let key = lower_hash_key_expr(&args[1])?;
            Ok(format!("({container})[{key}]"))
        }
        "slice-range" => {
            if args.len() != 3 {
                return Err(ParseError {
                    line,
                    message: "slice-range expects exactly three arguments".to_string(),
                });
            }
            let container = lower_expr(&args[0])?;
            let start = lower_expr(&args[1])?;
            let end = lower_expr(&args[2])?;
            Ok(format!("({container})[{start}:{end}]"))
        }
        "slice-to" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "slice-to expects exactly two arguments".to_string(),
                });
            }
            let container = lower_expr(&args[0])?;
            let end = lower_expr(&args[1])?;
            Ok(format!("({container})[:{end}]"))
        }
        "slice-from" => {
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "slice-from expects exactly two arguments".to_string(),
                });
            }
            let container = lower_expr(&args[0])?;
            let start = lower_expr(&args[1])?;
            Ok(format!("({container})[{start}:]"))
        }

        // Special forms (as expressions)
        "quote" => {
            if args.len() != 1 {
                return Err(ParseError {
                    line,
                    message: "quote expects exactly one argument".to_string(),
                });
            }
            lower_quote_expr(&args[0])
        }
        "if" => lower_if_expr(args, line),
        "let" => lower_let_expr(args, line, false, false),
        "let*" => lower_let_expr(args, line, true, false),
        "letrec" => lower_let_expr(args, line, false, true),
        "lambda" => lower_lambda_expr(args, line),

        // Statement-only forms
        "while" | "do" | "for" | "define" | "set!" | "declare" | "break" | "continue" | "begin"
        | "vector-set!" | "hash-set!" | "when" | "unless" | "cond" | "case" | "display"
        | "write" | "newline" | "for-each" => Err(ParseError {
            line,
            message: format!("special form '{head}' is only valid in statement position"),
        }),

        // Function calls
        _ => {
            let callee = if let Some(vm_path) = head.strip_prefix("vm.") {
                let mut segments = Vec::new();
                for segment in vm_path.split('.') {
                    if !is_valid_member_ident(segment) {
                        return Err(ParseError {
                            line,
                            message: format!(
                                "invalid vm namespace segment '{segment}' in '{head}'"
                            ),
                        });
                    }
                    segments.push(segment);
                }
                if segments.is_empty() {
                    return Err(ParseError {
                        line,
                        message: "vm namespace call requires at least one member".to_string(),
                    });
                }
                format!("vm::{}", segments.join("::"))
            } else {
                let callee_head = if let Some((_, member)) = head.split_once('.') {
                    member
                } else {
                    head
                };
                normalize_identifier(callee_head, items[0].line, "call target")?
            };
            let mut rendered = Vec::new();
            for arg in args {
                rendered.push(lower_expr(arg)?);
            }
            Ok(format!("{callee}({})", rendered.join(", ")))
        }
    }
}

fn lower_hash_key_expr(form: &SchemeForm) -> Result<String, ParseError> {
    if let Some(symbol) = form.as_symbol() {
        return Ok(render_string(symbol));
    }
    lower_expr(form)
}

fn lower_lambda_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "lambda expects (lambda (params...) body...)".to_string(),
        });
    }

    let params_list = args[0].as_list().ok_or(ParseError {
        line: args[0].line,
        message: "lambda parameters must be a list".to_string(),
    })?;

    let mut params = Vec::new();
    for param in params_list {
        let name_raw = param.as_symbol().ok_or(ParseError {
            line: param.line,
            message: "lambda parameter must be a symbol".to_string(),
        })?;
        params.push(normalize_identifier(
            name_raw,
            param.line,
            "lambda parameter",
        )?);
    }

    let body = lower_body_exprs(&args[1..], line)?;
    Ok(format!("|{}| {body}", params.join(", ")))
}

fn fold_infix_expr(
    args: &[SchemeForm],
    op: &str,
    line: usize,
    min_arity: usize,
    arity_message: &str,
) -> Result<String, ParseError> {
    if args.len() < min_arity {
        return Err(ParseError {
            line,
            message: arity_message.to_string(),
        });
    }

    let mut expr = lower_expr(&args[0])?;
    for arg in &args[1..] {
        let rhs = lower_expr(arg)?;
        expr = format!("({expr} {op} {rhs})");
    }
    Ok(expr)
}

fn lower_binary_expr(
    args: &[SchemeForm],
    op: &str,
    line: usize,
    message: &str,
) -> Result<String, ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: message.to_string(),
        });
    }
    let lhs = lower_expr(&args[0])?;
    let rhs = lower_expr(&args[1])?;
    Ok(format!("({lhs} {op} {rhs})"))
}

fn push_line(out: &mut Vec<String>, indent: usize, line: &str) {
    out.push(format!("{}{}", "    ".repeat(indent), line));
}

fn render_string(value: &str) -> String {
    let mut out = String::new();
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Helper to lower multiple body expressions into a block expression
fn lower_body_exprs(exprs: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if exprs.is_empty() {
        return Err(ParseError {
            line,
            message: "body must have at least one expression".to_string(),
        });
    }
    if exprs.len() == 1 {
        return lower_expr(&exprs[0]);
    }
    // Multiple expressions: wrap in a block with all but last as statements
    let (last, prefix) = exprs.split_last().unwrap();
    let mut stmts = Vec::new();
    for (idx, expr) in prefix.iter().enumerate() {
        let lowered = lower_expr(expr)?;
        let temp = gensym(&format!("body_{}", idx));
        stmts.push(format!("let {temp} = {lowered};"));
    }
    let last_expr = lower_expr(last)?;
    Ok(wrap_statement_sequence(stmts, last_expr))
}

fn lower_modulo_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "modulo expects exactly two arguments".to_string(),
        });
    }
    let a = lower_expr(&args[0])?;
    let b = lower_expr(&args[1])?;
    // Use native modulo operator
    Ok(format!("(({a}) % ({b}))"))
}

fn lower_remainder_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "remainder expects exactly two arguments".to_string(),
        });
    }
    let a = lower_expr(&args[0])?;
    let b = lower_expr(&args[1])?;
    // Use native modulo operator (same as modulo for integers)
    Ok(format!("(({a}) % ({b}))"))
}

fn lower_and_expr(args: &[SchemeForm]) -> Result<String, ParseError> {
    if args.is_empty() {
        return Ok("true".to_string());
    }
    if args.len() == 1 {
        return lower_expr(&args[0]);
    }
    // Short-circuit: if a => { b } else => { false }
    let first = lower_expr(&args[0])?;
    let rest = lower_and_expr(&args[1..])?;
    Ok(format!("if {first} => {{ {rest} }} else => {{ false }}"))
}

fn lower_or_expr(args: &[SchemeForm]) -> Result<String, ParseError> {
    if args.is_empty() {
        return Ok("false".to_string());
    }
    if args.len() == 1 {
        return lower_expr(&args[0]);
    }
    // Short-circuit: use temp to avoid re-evaluation
    let first = lower_expr(&args[0])?;
    let rest = lower_or_expr(&args[1..])?;
    let t = gensym("or");
    Ok(wrap_let_expr(
        &t,
        &first,
        &format!("if {t} => {{ {t} }} else => {{ {rest} }}"),
    ))
}

fn lower_if_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(ParseError {
            line,
            message: "if expression expects (if condition then [else])".to_string(),
        });
    }
    let cond = lower_expr(&args[0])?;
    let then_expr = lower_body_exprs(&args[1..2], line)?;
    let else_expr = if args.len() == 3 {
        lower_body_exprs(&args[2..3], line)?
    } else {
        "false".to_string()
    };
    Ok(format!(
        "if {cond} => {{ {then_expr} }} else => {{ {else_expr} }}"
    ))
}

fn lower_let_expr(
    args: &[SchemeForm],
    line: usize,
    sequential: bool,
    letrec: bool,
) -> Result<String, ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "let expression expects bindings and at least one body form".to_string(),
        });
    }

    // Check if this is a named let: (let name ((var val) ...) body...)
    if let Some(name_sym) = args[0].as_symbol() {
        // Named let
        if args.len() < 3 {
            return Err(ParseError {
                line,
                message: "named let expects name, bindings, and body".to_string(),
            });
        }
        return lower_named_let_expr(name_sym, &args[1], &args[2..], line);
    }

    let bindings = args[0].as_list().ok_or(ParseError {
        line: args[0].line,
        message: "let bindings must be a list".to_string(),
    })?;

    if bindings.is_empty() {
        return lower_body_exprs(&args[1..], line);
    }

    let mut stmts = Vec::new();

    if letrec {
        // letrec: lower recursive lambdas to function declarations.
        let mut deferred = Vec::<(String, String)>::new();
        for binding in bindings {
            let pair = binding.as_list().ok_or(ParseError {
                line: binding.line,
                message: "let binding must be a list".to_string(),
            })?;
            if pair.len() != 2 {
                return Err(ParseError {
                    line: binding.line,
                    message: "let binding must be (name value)".to_string(),
                });
            }
            let name_raw = pair[0].as_symbol().ok_or(ParseError {
                line: pair[0].line,
                message: "let binding name must be a symbol".to_string(),
            })?;
            let name = normalize_identifier(name_raw, pair[0].line, "let binding")?;
            if let Some(function_stmt) = lower_letrec_lambda_binding(&name, &pair[1], binding.line)? {
                stmts.push(function_stmt);
            } else {
                stmts.push(format!("let {name} = false;"));
                let value = lower_expr(&pair[1])?;
                deferred.push((name, value));
            }
        }
        for (name, value) in deferred {
            stmts.push(format!("{name} = {value};"));
        }
    } else if sequential {
        // let*: sequential
        for binding in bindings {
            let pair = binding.as_list().ok_or(ParseError {
                line: binding.line,
                message: "let binding must be a list".to_string(),
            })?;
            if pair.len() != 2 {
                return Err(ParseError {
                    line: binding.line,
                    message: "let binding must be (name value)".to_string(),
                });
            }
            let name_raw = pair[0].as_symbol().ok_or(ParseError {
                line: pair[0].line,
                message: "let binding name must be a symbol".to_string(),
            })?;
            let name = normalize_identifier(name_raw, pair[0].line, "let binding")?;
            let value = lower_expr(&pair[1])?;
            stmts.push(format!("let {name} = {value};"));
        }
    } else {
        // let: parallel (use temp vars)
        let mut temps = Vec::new();
        for (idx, binding) in bindings.iter().enumerate() {
            let pair = binding.as_list().ok_or(ParseError {
                line: binding.line,
                message: "let binding must be a list".to_string(),
            })?;
            if pair.len() != 2 {
                return Err(ParseError {
                    line: binding.line,
                    message: "let binding must be (name value)".to_string(),
                });
            }
            let value = lower_expr(&pair[1])?;
            let temp = gensym(&format!("let_{}", idx));
            stmts.push(format!("let {temp} = {value};"));
            temps.push((pair[0].clone(), temp));
        }
        for (name_form, temp) in temps {
            let name_raw = name_form.as_symbol().unwrap();
            let name = normalize_identifier(name_raw, name_form.line, "let binding")?;
            stmts.push(format!("let {name} = {temp};"));
        }
    }

    let body = lower_body_exprs(&args[1..], line)?;
    Ok(wrap_statement_sequence(stmts, body))
}

fn lower_named_let_expr(
    name: &str,
    bindings_form: &SchemeForm,
    body: &[SchemeForm],
    line: usize,
) -> Result<String, ParseError> {
    let bindings = bindings_form.as_list().ok_or(ParseError {
        line: bindings_form.line,
        message: "named let bindings must be a list".to_string(),
    })?;

    let mut params = Vec::new();
    let mut init_vals = Vec::new();
    for binding in bindings {
        let pair = binding.as_list().ok_or(ParseError {
            line: binding.line,
            message: "named let binding must be a list".to_string(),
        })?;
        if pair.len() != 2 {
            return Err(ParseError {
                line: binding.line,
                message: "named let binding must be (name value)".to_string(),
            });
        }
        let name_raw = pair[0].as_symbol().ok_or(ParseError {
            line: pair[0].line,
            message: "named let binding name must be a symbol".to_string(),
        })?;
        params.push(normalize_identifier(
            name_raw,
            pair[0].line,
            "named let param",
        )?);
        init_vals.push(lower_expr(&pair[1])?);
    }

    let func_name = normalize_identifier(name, line, "named let name")?;
    let body_expr = lower_body_exprs(body, line)?;
    let function_stmt = format!("fn {func_name}({}) = {body_expr};", params.join(", "));

    // fn func_name(params...) = body; func_name(init_vals...)
    let call = format!("{}({})", func_name, init_vals.join(", "));
    Ok(wrap_statement_sequence(
        vec![function_stmt],
        call,
    ))
}

fn lower_letrec_lambda_binding(
    name: &str,
    value: &SchemeForm,
    line: usize,
) -> Result<Option<String>, ParseError> {
    let Some(items) = value.as_list() else {
        return Ok(None);
    };
    let Some("lambda") = items.first().and_then(|item| item.as_symbol()) else {
        return Ok(None);
    };
    if items.len() < 3 {
        return Err(ParseError {
            line,
            message: "lambda expects (lambda (params...) body...)".to_string(),
        });
    }
    let params_list = items[1].as_list().ok_or(ParseError {
        line: items[1].line,
        message: "lambda parameters must be a list".to_string(),
    })?;
    let mut params = Vec::new();
    for param in params_list {
        let raw = param.as_symbol().ok_or(ParseError {
            line: param.line,
            message: "lambda parameter must be a symbol".to_string(),
        })?;
        params.push(normalize_identifier(raw, param.line, "lambda parameter")?);
    }
    let body_expr = lower_body_exprs(&items[2..], line)?;
    Ok(Some(format!(
        "fn {name}({}) = {body_expr};",
        params.join(", ")
    )))
}

fn lower_quote_expr(form: &SchemeForm) -> Result<String, ParseError> {
    match &form.node {
        SchemeNode::Int(v) => Ok(v.to_string()),
        SchemeNode::Float(v) => Ok(v.to_string()),
        SchemeNode::Bool(v) => Ok(v.to_string()),
        SchemeNode::Char(ch) => Ok((*ch as u32).to_string()),
        SchemeNode::String(s) => Ok(render_string(s)),
        SchemeNode::Symbol(s) => Ok(render_string(s)),
        SchemeNode::List(items) => {
            let mut quoted = Vec::new();
            for item in items {
                quoted.push(lower_quote_expr(item)?);
            }
            Ok(format!("[{}]", quoted.join(", ")))
        }
    }
}

fn lower_type_check(
    args: &[SchemeForm],
    line: usize,
    expected_type: &str,
) -> Result<String, ParseError> {
    if args.len() != 1 {
        return Err(ParseError {
            line,
            message: "type predicate expects exactly one argument".to_string(),
        });
    }
    let val = lower_expr(&args[0])?;
    Ok(format!("type_of({val}) == \"{}\"", expected_type))
}

fn lower_predicate_expr<F>(
    args: &[SchemeForm],
    line: usize,
    predicate_fn: F,
) -> Result<String, ParseError>
where
    F: FnOnce(String) -> String,
{
    if args.len() != 1 {
        return Err(ParseError {
            line,
            message: "predicate expects exactly one argument".to_string(),
        });
    }
    let val = lower_expr(&args[0])?;
    Ok(predicate_fn(val))
}

fn lower_map_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "map expects (map proc list)".to_string(),
        });
    }
    let func = lower_expr(&args[0])?;
    let list = lower_expr(&args[1])?;
    let map_func = gensym("map_f");
    let v = gensym("map_v");
    let i = gensym("map_i");
    let r = gensym("map_r");
    let callable = if func.trim_start().starts_with('|') {
        map_func.as_str()
    } else {
        func.as_str()
    };
    let map_body = wrap_let_expr(
        &r,
        "[]",
        &format!(
            "for (let {i} = 0; {i} < len({v}); {i} = {i} + 1) {{ {r} = array_push({r}, {callable}(({v})[{i}])); }} {r}"
        ),
    );
    let list_body = wrap_let_expr(&v, &list, &map_body);
    if func.trim_start().starts_with('|') {
        Ok(wrap_let_expr(&map_func, &func, &list_body))
    } else {
        Ok(list_body)
    }
}

fn lower_filter_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "filter expects (filter pred list)".to_string(),
        });
    }
    let pred = lower_expr(&args[0])?;
    let list = lower_expr(&args[1])?;
    let filter_pred = gensym("filt_p");
    let v = gensym("filt_v");
    let i = gensym("filt_i");
    let r = gensym("filt_r");
    let x = gensym("filt_x");
    let callable = if pred.trim_start().starts_with('|') {
        filter_pred.as_str()
    } else {
        pred.as_str()
    };
    let filter_body = wrap_let_expr(
        &r,
        "[]",
        &format!(
            "for (let {i} = 0; {i} < len({v}); {i} = {i} + 1) {{ let {x} = ({v})[{i}]; if {callable}({x}) {{ {r} = array_push({r}, {x}); }} }} {r}"
        ),
    );
    let list_body = wrap_let_expr(&v, &list, &filter_body);
    if pred.trim_start().starts_with('|') {
        Ok(wrap_let_expr(&filter_pred, &pred, &list_body))
    } else {
        Ok(list_body)
    }
}

fn lower_apply_expr(args: &[SchemeForm], line: usize) -> Result<String, ParseError> {
    if args.len() < 2 {
        return Err(ParseError {
            line,
            message: "apply expects at least a function and argument list".to_string(),
        });
    }
    // apply func arg1 arg2 ... arglist
    // We don't have true varargs or spread — approximate by requiring exactly 2 args
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "apply in this subset expects exactly (apply func arglist)".to_string(),
        });
    }
    let func = lower_expr(&args[0])?;
    let arglist = lower_expr(&args[1])?;
    // Can't actually spread — just call with the whole list (won't work as intended)
    // This is a limitation of the lowering approach
    Ok(format!("{func}({arglist})"))
}

fn normalize_identifier(name: &str, line: usize, context: &str) -> Result<String, ParseError> {
    if name.is_empty() {
        return Err(ParseError {
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
            line,
            message: format!("{context} cannot be empty"),
        });
    };

    if !is_ident_start(first) || !chars.all(is_ident_continue) {
        return Err(ParseError {
            line,
            message: format!("unsupported identifier '{name}' in {context}"),
        });
    }

    if is_reserved_identifier(&out) {
        return Err(ParseError {
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

#[cfg(test)]
mod tests {
    use super::super::parse_with_parser;
    use super::*;

    fn with_line_numbers(source: &str) -> String {
        source
            .lines()
            .enumerate()
            .map(|(idx, line)| format!("{:>4}: {}", idx + 1, line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn lower_example_complex_parses() {
        let source = include_str!("../../../examples/example_complex.scm");
        let lowered = lower(source).expect("scheme lowering should succeed");
        if let Err(err) = parse_with_parser(&lowered, false, false) {
            panic!(
                "lowered source should parse: {err}\n---- lowered ----\n{}",
                with_line_numbers(&lowered)
            );
        }
    }
}
