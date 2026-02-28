mod javascript;
mod lua;
mod rustscript;
mod scheme;

use super::{ParseError, SourceFlavor, ir::FrontendIr, parser::Parser};

trait FrontendCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError>;
}

struct RustScriptCompiler;
struct JavaScriptCompiler;
struct LuaCompiler;
struct SchemeCompiler;

pub(super) fn parse_source(source: &str, flavor: SourceFlavor) -> Result<FrontendIr, ParseError> {
    let frontend: &dyn FrontendCompiler = match flavor {
        SourceFlavor::RustScript => &RustScriptCompiler,
        SourceFlavor::JavaScript => &JavaScriptCompiler,
        SourceFlavor::Lua => &LuaCompiler,
        SourceFlavor::Scheme => &SchemeCompiler,
    };
    frontend.lower_to_ir(source)
}

pub(super) fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

pub(super) fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

impl FrontendCompiler for RustScriptCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        let lowered = rustscript::lower(source);
        parse_with_parser(&lowered, false, false)
    }
}

impl FrontendCompiler for JavaScriptCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        let lowered = javascript::lower(source)?;
        parse_with_parser(&lowered, false, true)
    }
}

impl FrontendCompiler for LuaCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        let lowered = lua::lower(source)?;
        parse_with_parser(&lowered, false, false)
    }
}

impl FrontendCompiler for SchemeCompiler {
    fn lower_to_ir(&self, source: &str) -> Result<FrontendIr, ParseError> {
        let lowered = scheme::lower(source)?;
        parse_with_parser(&lowered, false, false)
    }
}

fn parse_with_parser(
    source: &str,
    allow_implicit_externs: bool,
    allow_implicit_semicolons: bool,
) -> Result<FrontendIr, ParseError> {
    let mut parser = Parser::new(source, allow_implicit_externs, allow_implicit_semicolons)?;
    let stmts = parser.parse_program()?;
    Ok(FrontendIr {
        stmts,
        locals: parser.local_count(),
        local_bindings: parser.local_bindings(),
        functions: parser.function_decls(),
        function_impls: parser.function_impls(),
    })
}
