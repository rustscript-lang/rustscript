mod availability;
mod liveness;

use super::ParseError;
use super::ir::FrontendIr;

pub(super) fn enforce_local_availability(
    ir: FrontendIr,
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
) -> Result<FrontendIr, ParseError> {
    availability::enforce_local_availability(ir, clear_dead_locals, enable_local_move_semantics)
}
