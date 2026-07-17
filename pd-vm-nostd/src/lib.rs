#![no_std]

//! Minimal `no_std + alloc` RustScript bytecode runtime.
//!
//! The implementation is intentionally independent from compiler, CLI, JIT,
//! debugger, and operating-system integrations.

extern crate alloc;

mod error;
mod host;
mod program;
mod value;
mod vm;
mod vmbc;

pub use error::{VmError, WireError};
pub use host::{HostBinding, HostDispatcher, HostError, HostFunction};
pub use program::{
    CallablePrototype, CallableTarget, CaptureBindingMode, FunctionRegion, HostImport, OpCode,
    Program, RootCallableBinding, ScriptFunction, ValueType,
};
pub use value::{CallableEnvironment, CallableKind, CallableValue, Value};
pub use vm::{Vm, VmResult, VmStatus};
pub use vmbc::decode_program;

pub(crate) use host::resolve_host_functions;
