#![no_std]

//! Minimal `no_std + alloc` RustScript bytecode runtime.
//!
//! The implementation is intentionally independent from compiler, CLI, JIT,
//! debugger, and operating-system integrations.

extern crate alloc;

mod error;
mod generated_builtin_ids;
mod host;
mod program;
mod value;
mod vm;
mod vmbc;

pub use error::{HostImportBindingError, VmError, WireError};
pub use host::{HostBinding, HostDispatcher, HostError, HostFunction};
pub use program::{
    CallablePrototype, CallableTarget, CaptureBindingMode, ExportedCallable, FunctionRegion,
    HostApiFingerprint, HostImport, HostImportParam, HostImportSchema, HostParamPassing, OpCode,
    Program, ResourceTypeKey, RootCallableBinding, ScriptFunction, TypeSchema, ValueType,
};
pub use value::{CallableEnvironment, CallableKind, CallableValue, Value};
pub use vm::{DEFAULT_MAX_SCRIPT_CALL_DEPTH, Vm, VmResult, VmStatus};
pub use vmbc::decode_program;

pub(crate) use host::resolve_host_functions;
