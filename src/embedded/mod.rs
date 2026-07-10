//! Minimal `no_std + alloc` RustScript bytecode runtime.
//!
//! The implementation is intentionally independent from compiler, CLI, JIT,
//! debugger, and operating-system integrations.

mod error;
mod program;
mod value;
mod vmbc;

pub use error::WireError;
pub use program::{HostImport, OpCode, Program, ValueType};
pub use value::Value;
pub use vmbc::decode_program;
