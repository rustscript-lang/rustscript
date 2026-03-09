mod builtins;

pub mod assembler;
pub mod bytecode;
pub mod compiler;
pub mod debug_info;
#[cfg(feature = "runtime")]
pub mod debugger;
#[cfg(feature = "runtime")]
pub mod jit {
    pub use crate::vm::jit::{
        JitAttempt, JitConfig, JitNyiDoc, JitNyiReason, JitSnapshot, JitTrace, JitTraceTerminal,
        TraceJitEngine, TraceStep,
    };
}
#[cfg(feature = "runtime")]
pub mod vm;
#[cfg(feature = "runtime")]
pub mod vmbc;

pub use assembler::{AsmParseError, Assembler, AssemblerError, BytecodeBuilder, assemble};
pub use builtins::{BuiltinNamespaceMemberSpec, BuiltinNamespaceSpec, builtin_namespace_specs};
pub use bytecode::{HostImport, OpCode, Program, Value};
pub use compiler::diagnostics::render_source_error;
pub use compiler::source_map::{LineSpanMapping, LoweredSource, SourceId, SourceMap, Span};
pub use compiler::{
    CompileError, CompileSourceFileOptions, CompiledProgram, Compiler, Expr, FunctionDecl,
    ParseError, SourceError, SourceFlavor, SourcePathError, Stmt, compile_source,
    compile_source_at_path_with_flavor_and_options, compile_source_file,
    compile_source_file_with_options, compile_source_for_repl, compile_source_with_flavor,
    compile_source_with_flavor_and_options,
};
pub use debug_info::{ArgInfo, DebugFunction, DebugInfo, LineInfo, LocalInfo};
#[cfg(feature = "runtime")]
pub use debugger::{
    DebugCommandBridge, DebugCommandBridgeError, DebugCommandBridgeResponse,
    DebugCommandBridgeStatus, Debugger, StepMode, VmRecording, VmRecordingError, VmRecordingFrame,
    VmRecordingReplayResponse, VmRecordingReplayState, replay_recording_stdio,
    run_recording_replay_command,
};
#[cfg(feature = "runtime")]
pub use jit::{
    JitAttempt, JitConfig, JitNyiDoc, JitNyiReason, JitSnapshot, JitTrace, JitTraceTerminal,
    TraceJitEngine,
};
#[cfg(feature = "runtime")]
pub use vm::builtins_impl::print::{PrintHostFunction, PrintlnHostFunction, format_value};
#[cfg(feature = "runtime")]
pub use vm::diagnostics::render_vm_error;
#[cfg(feature = "runtime")]
pub use vm::{
    CallOutcome, EpochCheckpoint, EpochHandle, FuelCheckpoint, HostAsyncBridge, HostBindingPlan,
    HostFunction, HostFunctionRegistry, HostOpId, StaticHostFunction, Store, Vm, VmError, VmResult,
    VmStatus, VmYieldReason,
};
#[cfg(feature = "runtime")]
pub use vmbc::{
    DisassembleOptions, ValidationError, WireError, decode_program, disassemble_program,
    disassemble_program_with_options, disassemble_vmbc, disassemble_vmbc_with_options,
    encode_program, infer_local_count, validate_program,
};
