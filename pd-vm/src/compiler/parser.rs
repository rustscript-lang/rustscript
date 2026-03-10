mod expressions;
mod lexer;
mod lint;
mod statements;

use std::collections::{HashMap, HashSet};

use rt_format::{NoNamedArguments, ParsedFormat};

use crate::ValueType;
use crate::builtins::{
    BuiltinFunction, builtin_namespace_hint, default_host_callable, is_builtin_namespace,
    resolve_builtin_namespace_call,
};
use crate::compiler::source_map::{SourceId, Span};

use self::lexer::{Lexer, ParserFormatArg, Token, TokenKind, is_ident_continue, is_ident_start};
use super::{
    ParseError, ReplLocalBinding, STDLIB_PRINT_ARITY, STDLIB_PRINT_NAME,
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

fn abi_value_type_to_value_type(value: edge_abi::AbiValueType) -> ValueType {
    match value {
        edge_abi::AbiValueType::Unknown => ValueType::Unknown,
        edge_abi::AbiValueType::Null => ValueType::Null,
        edge_abi::AbiValueType::Int => ValueType::Int,
        edge_abi::AbiValueType::Float => ValueType::Float,
        edge_abi::AbiValueType::Bool => ValueType::Bool,
        edge_abi::AbiValueType::String => ValueType::String,
        edge_abi::AbiValueType::Array => ValueType::Array,
        edge_abi::AbiValueType::Map => ValueType::Map,
    }
}

fn known_host_return_type(name: &str) -> ValueType {
    edge_abi::function_by_name(name)
        .map(|function| abi_value_type_to_value_type(function.return_type))
        .unwrap_or(ValueType::Unknown)
}

fn known_host_accepts_arity(name: &str, arity: u8) -> bool {
    if let Some(function) = edge_abi::function_by_name(name) {
        return function.param_types.len() == usize::from(arity);
    }
    default_host_callable(name).is_some_and(|callable| {
        let required = callable
            .signature
            .params
            .iter()
            .take_while(|param| !param.optional)
            .count();
        required <= usize::from(arity) && usize::from(arity) <= callable.signature.params.len()
    })
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

pub(super) fn lint_trailing_function_return_semicolons(
    source: &str,
    source_id: SourceId,
    dialect: &'static dyn ParserDialect,
) -> Result<Vec<ParseError>, ParseError> {
    lint::lint_trailing_function_return_semicolons(source, source_id, dialect)
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

    pub(super) fn new_with_predeclared_locals(
        source: &str,
        source_id: SourceId,
        allow_implicit_externs: bool,
        allow_implicit_semicolons: bool,
        enforce_mutable_bindings: bool,
        dialect: &'static dyn ParserDialect,
        predeclared_locals: &[ReplLocalBinding],
    ) -> Result<Self, ParseError> {
        let mut parser = Self::new(
            source,
            source_id,
            allow_implicit_externs,
            allow_implicit_semicolons,
            enforce_mutable_bindings,
            dialect,
        )?;
        for binding in predeclared_locals {
            parser.predeclare_local(binding)?;
        }
        Ok(parser)
    }

    pub(super) fn parse_program(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::Eof) {
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
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
            return_type: ValueType::Unknown,
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
            return_type: ValueType::Unknown,
        };
        self.functions.insert(name.to_string(), decl.clone());
        self.function_list.push(decl.clone());
        Ok(decl)
    }

    fn define_host_function(&mut self, name: &str, arity: u8) -> Result<FunctionDecl, ParseError> {
        if let Some(existing) = self.functions.get(name) {
            if existing.arity != arity && !known_host_accepts_arity(name, arity) {
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
            return_type: known_host_return_type(name),
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

    fn predeclare_local(&mut self, binding: &ReplLocalBinding) -> Result<(), ParseError> {
        if self.locals.contains_key(&binding.name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!("duplicate repl local '{}'", binding.name),
            });
        }
        let index = self.allocate_hidden_local()?;
        self.locals.insert(binding.name.clone(), index);
        self.set_local_slot_mutable(index, binding.mutable);
        Ok(())
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

    fn match_return_type_arrow(&mut self) -> bool {
        if self.check(&TokenKind::Minus) && self.check_kind_at(self.pos + 1, &TokenKind::Greater) {
            self.pos += 2;
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

    fn starts_trailing_expr_block_statement(&self) -> bool {
        if self.check(&TokenKind::Pub)
            || self.check(&TokenKind::Use)
            || (self.dialect.allow_import_stmt() && self.check(&TokenKind::Import))
            || self.check(&TokenKind::Fn)
            || self.check(&TokenKind::Let)
            || self.check(&TokenKind::For)
            || self.check(&TokenKind::While)
            || self.check(&TokenKind::Break)
            || self.check(&TokenKind::Continue)
            || (self.dialect.allow_return_stmt() && self.check_ident_literal("return"))
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

    pub(super) fn local_bindings_with_mutability(&self) -> Vec<ReplLocalBinding> {
        let mut locals = self
            .locals
            .iter()
            .map(|(name, index)| ReplLocalBinding {
                name: name.clone(),
                mutable: self.is_local_slot_mutable(*index),
            })
            .collect::<Vec<_>>();
        locals.sort_by_key(|binding| self.locals.get(&binding.name).copied().unwrap_or(0));
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
