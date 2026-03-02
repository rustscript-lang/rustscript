use super::super::ParseError;
use super::super::ir::FrontendIr;

pub(super) fn lower_to_ir(source: &str) -> Result<FrontendIr, ParseError> {
    // JavaScript now lowers directly through the shared parser in JS mode.
    // No RustScript text rewriting layer is used.
    super::parse_with_parser(source, 0, false, true, true)
}
