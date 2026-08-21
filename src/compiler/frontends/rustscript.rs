use super::super::ParseError;
use super::super::parser::ParserDialect;
use crate::compiler::source_map::{LoweredSource, LoweringBuilder};

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

/// Lower RustScript source before parsing.
///
/// The current frontend performs no textual transformation: the source is
/// copied verbatim through [`LoweringBuilder`], which records the exact
/// byte-for-byte mapping from lowered text back to the original source. Any
/// future RustScript construct that needs rewriting (macro expansion,
/// syntax normalization) appends copy/insert operations through the same
/// builder so parser provenance spans keep mapping to exact original slices.
pub(super) fn lower(source: &str) -> Result<LoweredSource, ParseError> {
    let mut builder = LoweringBuilder::new(source);
    builder.copy_rest();
    Ok(builder.finish())
}
