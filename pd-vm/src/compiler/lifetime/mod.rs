mod availability;
mod liveness;

use super::ParseError;
use super::ir::{FrontendIr, LocalSlot};

// This module is the entry point for the lifetime pass. `availability` owns the
// top-level transformation and depends on the lower-level liveness machinery.
pub(super) fn enforce_local_availability_with_entry_locals(
    ir: FrontendIr,
    entry_definite_locals: &[LocalSlot],
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
) -> Result<FrontendIr, ParseError> {
    // Only the REPL uses non-empty entry locals; regular compilation starts from an
    // empty top-level environment.
    availability::enforce_local_availability(
        ir,
        entry_definite_locals,
        clear_dead_locals,
        enable_local_move_semantics,
    )
}
