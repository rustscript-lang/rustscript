//! Frame-local lifetime analysis.
//!
//! # Same-frame interference
//!
//! Locals that are simultaneously live inside one execution frame share a
//! single interference domain: the coloring pass must give them distinct
//! relative slot numbers. This applies to the root body and to each named
//! function body independently — argument evaluation and values used after
//! a call keep the caller's slots live across the call.
//!
//! # Cross-frame reuse
//!
//! Every script invocation allocates its own runtime frame with a fresh
//! `local_base` (see `docs/callable-runtime.md`). A statically resolved
//! named call (`Expr::Call`) therefore contributes only caller-side
//! argument uses to the caller live set; the callee body's locals are
//! analyzed inside the callee frame and never union into the caller.
//! Locals from different frames may reuse the same relative slot numbers —
//! the runtime frame bases already separate them, so cross-frame live
//! ranges need no interference edges.
//!
//! # Conservative dynamic paths
//!
//! Dynamic targets keep their pre-frame conservatism on purpose:
//! `Expr::LocalCall` marks the whole live set because the invoked slot can
//! hold an inline closure whose captures are not visible from the call
//! expression, and closure bodies contribute their transitive footprint so
//! captured slots stay live for the duration of the call.

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

pub(super) use availability::allocate_local_slots;
