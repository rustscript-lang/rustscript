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
        JitAttempt, JitConfig, JitMetrics, JitNyiDoc, JitNyiReason, JitSnapshot, JitTrace,
        JitTraceTerminal, TraceJitEngine,
    };
}
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "runtime")]
pub mod vm;
#[cfg(feature = "runtime")]
pub mod vmbc;

pub use assembler::{AsmParseError, Assembler, AssemblerError, BytecodeBuilder, assemble};
#[cfg(feature = "runtime")]
pub use builtins::runtime::print::{PrintHostFunction, PrintlnHostFunction, format_value};
pub use builtins::{
    BuiltinFunction, BuiltinNamespaceMemberSpec, BuiltinNamespaceSpec, CallableDef, CallableParam,
    CallableParamType, CallableSignature, LanguageBuiltinSpec, builtin_namespace_specs,
    callable_signatures_for_builtin_namespace_member, default_host_callables, is_builtin_namespace,
    language_builtin_specs, resolve_builtin_namespace_call,
};
pub use bytecode::{HostImport, OpCode, Program, TypeMap, Value, ValueType};
pub fn builtin_call_index(name: &str) -> Option<u16> {
    use builtins::BuiltinFunction;

    match name {
        "len" => Some(BuiltinFunction::Len.call_index()),
        "slice" => Some(BuiltinFunction::Slice.call_index()),
        "concat" => Some(BuiltinFunction::Concat.call_index()),
        "get" => Some(BuiltinFunction::Get.call_index()),
        "has" => Some(BuiltinFunction::Has.call_index()),
        "set" => Some(BuiltinFunction::Set.call_index()),
        "keys" => Some(BuiltinFunction::Keys.call_index()),
        "string_contains" => Some(BuiltinFunction::StringContains.call_index()),
        "string_replace_literal" => Some(BuiltinFunction::StringReplaceLiteral.call_index()),
        "string_lower_ascii" => Some(BuiltinFunction::StringLowerAscii.call_index()),
        "string_split_literal" => Some(BuiltinFunction::StringSplitLiteral.call_index()),
        _ => BuiltinFunction::from_namespaced_name(name).map(|builtin| builtin.call_index()),
    }
}
pub use compiler::diagnostics::{render_compile_error, render_source_error};
pub use compiler::source_map::{LineSpanMapping, LoweredSource, SourceId, SourceMap, Span};
pub use compiler::{
    AssignmentKind, ClosureExpr, CompileError, CompileSourceFileOptions, CompiledProgram,
    CompiledReplProgram, Compiler, Expr, FormatError, FrontendImportSyntax, FrontendIr,
    FunctionDecl, ImportClause, InferredLocalTypeHint, LocalIrBuilder, LocalSlot, ModuleImport,
    NamedImport, ParseError, ParserDialect, ReplLocalBinding, ReplLocalState, SharedParserOptions,
    SourceError, SourceFlavor, SourcePathError, SourcePlugin, Stmt, UnknownInferredLocal,
    collect_inferred_local_type_hints, collect_inferred_local_type_hints_at_path_with_options,
    collect_inferred_local_type_hints_with_options, compile_source,
    compile_source_at_path_with_flavor_and_options, compile_source_file,
    compile_source_file_with_options, compile_source_for_repl, compile_source_for_repl_with_locals,
    compile_source_for_repl_with_state, compile_source_with_flavor,
    compile_source_with_flavor_and_options, format_source, format_source_with_flavor,
    format_source_with_flavor_and_options, lint_trailing_function_return_semicolons,
    lint_unknown_inferred_local_types, lint_unknown_inferred_local_types_at_path_with_options,
    lint_unknown_inferred_local_types_with_options, lint_unknown_type_annotations,
    parse_source_with_dialect,
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
    JitAttempt, JitConfig, JitMetrics, JitNyiDoc, JitNyiReason, JitSnapshot, JitTrace,
    JitTraceTerminal, TraceJitEngine,
};
#[cfg(feature = "runtime")]
pub use vm::diagnostics::render_vm_error;
#[cfg(feature = "runtime")]
pub use vm::{
    AotArtifactError, CallOutcome, CallReturn, EpochCheckpoint, EpochHandle, FuelCheckpoint,
    HostArgsFunction, HostAsyncBridge, HostBindingPlan, HostFunction, HostFunctionRegistry,
    HostOpId, HostStackFunction, StaticHostArgsFunction, StaticHostFunction,
    StaticHostStackFunction, Store, Vm, VmError, VmResult, VmStatus, VmYieldReason,
};
#[cfg(feature = "runtime")]
pub use vmbc::{
    DisassembleOptions, ValidationError, WireError, decode_program, disassemble_program,
    disassemble_program_with_options, disassemble_vmbc, disassemble_vmbc_with_options,
    encode_program, infer_local_count, validate_program,
};
