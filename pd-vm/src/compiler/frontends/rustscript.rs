use super::super::ParseError;
use super::super::parser::ParserDialect;
use crate::compiler::source_map::LoweredSource;

struct RustScriptDialect;

impl ParserDialect for RustScriptDialect {
    fn allow_let_mut_binding(&self) -> bool {
        true
    }

    fn allow_macro_calls(&self) -> bool {
        true
    }

    fn allow_plus_equal_operator(&self) -> bool {
        true
    }

    fn allow_for_in_loop(&self) -> bool {
        true
    }
}

static RUSTSCRIPT_DIALECT: RustScriptDialect = RustScriptDialect;

pub(super) fn parser_dialect() -> &'static dyn ParserDialect {
    &RUSTSCRIPT_DIALECT
}

pub(super) fn lower(source: &str) -> Result<LoweredSource, ParseError> {
    Ok(LoweredSource::identity(source.to_string()))
}
