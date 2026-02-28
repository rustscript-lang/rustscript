use std::collections::HashSet;

use super::super::ParseError;
use super::{is_ident_continue, is_ident_start};

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
    Bool(bool),
    String(String),
    Symbol(String),
    List(Vec<SchemeForm>),
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    LParen,
    RParen,
    Int(i64),
    Bool(bool),
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

        if atom == "#t" {
            return Ok(TokenKind::Bool(true));
        }
        if atom == "#f" {
            return Ok(TokenKind::Bool(false));
        }

        if let Some(value) = parse_int_atom(&atom) {
            return Ok(TokenKind::Int(value));
        }

        Ok(TokenKind::Symbol(atom))
    }
}

fn is_scheme_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '(' | ')' | ';')
}

fn parse_int_atom(atom: &str) -> Option<i64> {
    if atom.is_empty() {
        return None;
    }

    let digits = if let Some(rest) = atom.strip_prefix('-') {
        rest
    } else {
        atom
    };

    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    atom.parse::<i64>().ok()
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
        let token = self.advance().clone();
        match token.kind {
            TokenKind::LParen => self.parse_list(token.line),
            TokenKind::RParen => Err(ParseError {
                line: token.line,
                message: "unexpected ')'".to_string(),
            }),
            TokenKind::Int(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Int(value),
            }),
            TokenKind::Bool(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Bool(value),
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
            if args.len() != 2 {
                return Err(ParseError {
                    line,
                    message: "function define in this subset expects exactly one expression body"
                        .to_string(),
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
            let body = lower_expr(&args[1])?;
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
            if let SchemeNode::String(spec) = &arg.node && spec == "vm" {
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
                    let imported = normalize_identifier(imported, pair[0].line, "vm import source")?;
                    let local = normalize_identifier(local, pair[1].line, "vm import target")?;
                    if imported == local {
                        bindings.push(imported);
                    } else {
                        bindings.push(format!("{imported} as {local}"));
                    }
                }
                if !bindings.is_empty() {
                    push_line(out, indent, &format!("use vm::{{{}}};", bindings.join(", ")));
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
                let module_spec = module_candidate.as_symbol().or_else(|| {
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
        SchemeNode::Bool(value) => Ok(value.to_string()),
        SchemeNode::String(value) => Ok(render_string(value)),
        SchemeNode::Symbol(name) => {
            if name == "true" {
                return Ok("true".to_string());
            }
            if name == "false" {
                return Ok("false".to_string());
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
        "+" => fold_infix_expr(args, "+", line, 2, "+ expects at least two arguments"),
        "*" => fold_infix_expr(args, "*", line, 2, "* expects at least two arguments"),
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
        "=" => lower_binary_expr(args, "==", line, "= expects exactly two arguments"),
        "/=" => lower_binary_expr(args, "!=", line, "/= expects exactly two arguments"),
        "<" => lower_binary_expr(args, "<", line, "< expects exactly two arguments"),
        ">" => lower_binary_expr(args, ">", line, "> expects exactly two arguments"),
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
        "lambda" => lower_lambda_expr(args, line),
        "if" | "while" | "do" | "for" | "define" | "set!" | "declare" | "break" | "continue"
        | "begin" | "vector-set!" | "hash-set!" => Err(ParseError {
            line,
            message: format!("special form '{head}' is only valid in statement position"),
        }),
        _ => {
            let callee_head = if let Some((_, member)) = head.split_once('.') {
                member
            } else {
                head
            };
            let callee = normalize_identifier(callee_head, items[0].line, "call target")?;
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
    if args.len() != 2 {
        return Err(ParseError {
            line,
            message: "lambda expects (lambda (params...) expr)".to_string(),
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

    let body = lower_expr(&args[1])?;
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
