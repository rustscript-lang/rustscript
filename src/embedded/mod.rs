//! Minimal `no_std + alloc` RustScript bytecode runtime.
//!
//! The implementation is intentionally independent from compiler, CLI, JIT,
//! debugger, and operating-system integrations.

mod error;
mod host;
mod program;
mod value;
mod vm;
mod vmbc;

pub use error::{VmError, WireError};
pub use host::{HostBinding, HostError, HostFunction};
pub use program::{HostImport, OpCode, Program, ValueType};
pub use value::Value;
pub use vm::{Vm, VmResult, VmStatus};
pub use vmbc::decode_program;

pub(crate) use host::resolve_host_functions;
