mod availability;
mod liveness;

use super::ParseError;
use super::ir::{FrontendIr, LocalSlot};

pub(crate) use availability::{closure_capture_binding_mode, function_capture_binding_mode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EntryLocalAvailability {
    pub slot: LocalSlot,
    pub copyable: bool,
    pub movable: bool,
    pub moved: bool,
}

// This module is the entry point for the lifetime pass. `availability` owns the
// top-level transformation and depends on the lower-level liveness machinery.
pub(super) fn enforce_local_availability_with_entry_locals(
    ir: FrontendIr,
    entry_locals: &[EntryLocalAvailability],
    clear_dead_locals: bool,
    enable_local_move_semantics: bool,
) -> Result<FrontendIr, ParseError> {
    // Only the REPL uses non-empty entry locals; regular compilation starts from an
    // empty top-level environment.
    availability::enforce_local_availability(
        ir,
        entry_locals,
        clear_dead_locals,
        enable_local_move_semantics,
    )
}
