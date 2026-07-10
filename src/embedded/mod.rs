//! Minimal `no_std + alloc` RustScript bytecode runtime.
//!
//! The implementation is intentionally independent from compiler, CLI, JIT,
//! debugger, and operating-system integrations.

mod error;
mod program;
mod value;
mod vm;
mod vmbc;

pub use error::{VmError, WireError};
pub use program::{HostImport, OpCode, Program, ValueType};
pub use value::Value;
pub use vm::{Vm, VmResult, VmStatus};
pub use vmbc::decode_program;
