#![allow(dead_code)]

mod artifact;
pub(crate) mod cfg;
pub(crate) mod compile;
pub(crate) mod ir;
mod runtime;
pub(crate) mod ssa;

pub use artifact::AotArtifactError;
pub(crate) use compile::CompiledProgram;
