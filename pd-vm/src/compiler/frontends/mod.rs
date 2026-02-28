use std::collections::HashMap;

mod javascript;
mod lua;
mod rustscript;
mod scheme;

use super::{FunctionDecl, FunctionImpl, ParseError, SourceFlavor, Stmt, parser::Parser};

pub(super) struct FrontendOutput {
    pub(super) stmts: Vec<Stmt>,
    pub(super) locals: usize,
    pub(super) local_bindings: Vec<(String, u8)>,
    pub(super) functions: Vec<FunctionDecl>,
    pub(super) function_impls: HashMap<u16, FunctionImpl>,
}

trait FrontendCompiler {
    fn parse(&self, source: &str) -> Result<FrontendOutput, ParseError>;
}

struct RustScriptCompiler;
struct JavaScriptCompiler;
struct LuaCompiler;
struct SchemeCompiler;

pub(super) fn parse_source(
    source: &str,
    flavor: SourceFlavor,
) -> Result<FrontendOutput, ParseError> {
    let frontend: &dyn FrontendCompiler = match flavor {
        SourceFlavor::RustScript => &RustScriptCompiler,
        SourceFlavor::JavaScript => &JavaScriptCompiler,
        SourceFlavor::Lua => &LuaCompiler,
        SourceFlavor::Scheme => &SchemeCompiler,
    };
    frontend.parse(source)
}

pub(super) fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

pub(super) fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

impl FrontendCompiler for RustScriptCompiler {
    fn parse(&self, source: &str) -> Result<FrontendOutput, ParseError> {
        let lowered = rustscript::lower(source);
        parse_with_parser(&lowered, false, false)
    }
}

impl FrontendCompiler for JavaScriptCompiler {
    fn parse(&self, source: &str) -> Result<FrontendOutput, ParseError> {
        let lowered = javascript::lower(source)?;
        parse_with_parser(&lowered, false, true)
    }
}

impl FrontendCompiler for LuaCompiler {
    fn parse(&self, source: &str) -> Result<FrontendOutput, ParseError> {
        let lowered = lua::lower(source)?;
        parse_with_parser(&lowered, false, false)
    }
}

impl FrontendCompiler for SchemeCompiler {
    fn parse(&self, source: &str) -> Result<FrontendOutput, ParseError> {
        let lowered = scheme::lower(source)?;
        parse_with_parser(&lowered, false, false)
    }
}

fn parse_with_parser(
    source: &str,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
) -> Result<FrontendOutput, ParseError> {
    let mut parser = Parser::new(source, allow_implicit_externs, allow_implicit_semicolons)?;
    let stmts = parser.parse_program()?;
    Ok(FrontendOutput {
        stmts,
        locals: parser.local_count(),
        local_bindings: parser.local_bindings(),
        functions: parser.function_decls(),
        function_impls: parser.function_impls(),
    })
}
