mod cursor;
mod expressions;
mod lexer;
mod lint;
mod statements;
mod symbols;

use std::collections::{HashMap, HashSet};

use rt_format::{NoNamedArguments, ParsedFormat};

use crate::ValueType;
use crate::builtins::{
    BuiltinFunction, builtin_namespace_hint, default_host_callable, is_builtin_namespace,
    resolve_builtin_namespace_call,
};
use crate::compiler::source_map::{SourceId, Span};

use self::lexer::{Lexer, ParserFormatArg, Token, TokenKind, is_ident_continue, is_ident_start};
use self::symbols::is_virtual_host_namespace_spec;
use super::{
    ParseError, ReplLocalBinding, STDLIB_PRINT_ARITY, STDLIB_PRINT_NAME,
    ir::{
        ClosureExpr, Expr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern,
        MatchTypePattern, Stmt, TypeSchema,
    },
};

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
    struct_schemas: HashMap<String, TypeSchema>,
    schema_reference_sites: Vec<(String, usize, Span)>,
    unknown_type_spans: Vec<Span>,
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
            struct_schemas: HashMap::new(),
            schema_reference_sites: Vec::new(),
            unknown_type_spans: Vec::new(),
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
        self.validate_schema_reference_sites()?;
        Ok(stmts)
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

    pub(super) fn struct_schemas(&self) -> HashMap<String, TypeSchema> {
        self.struct_schemas.clone()
    }

    pub(super) fn unknown_type_spans(&self) -> Vec<Span> {
        self.unknown_type_spans.clone()
    }

    fn validate_schema_reference_sites(&self) -> Result<(), ParseError> {
        for (name, line, span) in &self.schema_reference_sites {
            if self.struct_schemas.contains_key(name) {
                continue;
            }
            return Err(ParseError {
                span: Some(*span),
                code: None,
                line: *line,
                message: format!("unknown struct schema '{name}'"),
            });
        }
        Ok(())
    }
}
