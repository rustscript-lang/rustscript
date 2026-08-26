//! Immutable program artifact.
//!
//! [`Program`] is the compiled, immutable unit of
//! execution: bytecode, constants, metadata, import requirements, and
//! binding tables. This module documents its ownership contract for the VM
//! runtime decomposition:
//!
//! - A `Program` is immutable after compilation and binding metadata
//!   construction; sharing one `Program` (e.g. through `Arc<Program>`) is the
//!   only supported way to share code between VMs or instances.
//! - Per-run state (stacks, locals, frames, wait state) never lives in the
//!   program; it lives in the VM's private `Instance` state.
//! - Backend caches derived from the program (decoded instruction data,
//!   operand type hints, AOT/JIT artifacts) live in
//!   the VM's private `Engine` state and are keyed by the program's cache
//!   identity, never owned by a run.
//!
//! Thread safety: `Program` is `Send + Sync` and `Clone`-cheap only through
//! `Arc`; cloning the struct itself duplicates metadata, which is allowed but
//! wasteful. Prefer `Arc<Program>` for sharing.

pub use crate::bytecode::Program;
