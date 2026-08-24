mod builtins;

pub mod assembler;
pub mod bytecode;
pub mod compiler;
pub mod debug_info;
#[cfg(feature = "runtime")]
pub mod debugger;
pub mod host_api;
#[cfg(feature = "runtime")]
pub mod jit {
    pub use crate::vm::jit::{
        JitAttempt, JitCallSiteProfile, JitConfig, JitExitProfile, JitMetrics, JitNyiDoc,
        JitNyiReason, JitSnapshot, JitTrace, JitTraceTerminal, TraceJitEngine,
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
#[cfg(feature = "runtime")]
pub use builtins::runtime::{
    BorrowVmValue, FromVmValue, HostCallResult, IntoHostCallOutcome, TakeVmValue, arg, borrow_arg,
    return_one, take_arg,
};
#[cfg(feature = "http-client")]
pub use builtins::runtime::{
    HttpConfig, HttpHostExt, http_host_catalog, register_http_builtin_module,
    register_http_builtin_module_from_catalog,
};
#[cfg(feature = "runtime")]
pub use builtins::runtime::{
    IoExtension, IoHostExt, IoPolicy, io_host_catalog, register_io_builtin_module,
    register_io_builtin_module_from_catalog, standard_composition, standard_host_catalog,
    standard_host_catalog_fingerprint,
};
#[cfg(feature = "sqlite")]
pub use builtins::runtime::{
    SqliteExtension, SqliteHostExt, SqliteLimits, SqlitePolicy, register_sqlite_builtin_module,
    register_sqlite_builtin_module_from_catalog, sqlite_host_catalog,
};
pub use builtins::{
    BUILTIN_CATALOG, BuiltinFunction, BuiltinNamespaceMemberSpec, BuiltinNamespaceSpec,
    CallableDef, CallableParam, CallableParamType, CallableSignature, CallableType, HostExecution,
    LanguageBuiltinSpec, builtin_namespace_specs, callable_signatures_for_builtin_namespace_member,
    default_host_callables, is_builtin_namespace, language_builtin_specs,
    resolve_builtin_namespace_call,
};
pub use bytecode::{
    CallableEnvironment, CallableKind, CallablePrototype, CallableTarget, CallableValue,
    CaptureBindingMode, ExportedCallable, FunctionRegion, HostImport, HostImportParam,
    HostImportSchema, NamedStructSchema, OpCode, Program, RootCallableBinding, ScriptFunction,
    TypeMap, Value, ValueType, VmMap,
};
pub fn builtin_call_index(name: &str) -> Option<u16> {
    use builtins::BuiltinFunction;

    BuiltinFunction::from_source_name(name).map(|builtin| builtin.call_index())
}
#[cfg(feature = "runtime")]
pub use builtins::runtime::error::{RuntimeError, RuntimeErrorCode, RuntimeResult};
pub use compiler::diagnostics::{
    render_compile_error, render_source_error, render_source_path_error,
};
pub use compiler::source_map::{
    ByteSegment, ByteSpanMapping, LineSpanMapping, LoweredSource, LoweringBuilder, SourceId,
    SourceMap, Span,
};
pub use compiler::{
    AssignmentKind, ClosureExpr, CompileError, CompileSourceFileOptions, CompiledProgram,
    CompiledReplProgram, Compiler, CompletionItemKind, DeclSymbol, Definition, ExportEntry, Expr,
    FormatError, FrontendImportSyntax, FrontendIr, FunctionDecl, ImportClause, ImportTargetKind,
    ImportedBinding, InferredLocalTypeHint, LocalIrBuilder, LocalSlot, ModuleGraph, ModuleId,
    ModuleImport, ModuleNode, NamedImport, ParseError, ParserDialect, ReplLocalBinding,
    ReplLocalState, ResolvedImport, SemanticCompletion, SemanticDiagnostic, SemanticModel,
    SharedParserOptions, SourceError, SourceFlavor, SourcePathError, SourcePlugin, SourcePosition,
    Stmt, SymbolId, UnknownInferredLocal, UseDecl, UsePathSegment,
    analyze_source_from_string_with_options, collect_inferred_local_type_hints,
    collect_inferred_local_type_hints_at_path_with_options,
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
pub use compiler::{HostCallResolveError, HostCallResolver, ResolvedHostCall, ResolvedHostParam};
pub use debug_info::{ArgInfo, DebugFunction, DebugInfo, LineInfo, LocalInfo};
#[cfg(feature = "runtime")]
pub use debugger::{
    DebugCommandBridge, DebugCommandBridgeError, DebugCommandBridgeResponse,
    DebugCommandBridgeStatus, Debugger, StepMode, VmRecording, VmRecordingError, VmRecordingFrame,
    VmRecordingReplayResponse, VmRecordingReplayState, replay_recording_stdio,
    run_recording_replay_command,
};
pub use host_api::{
    FunctionNameError, HostApiBuilder, HostApiCatalog, HostApiCatalogError, HostApiFingerprint,
    HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema, ResourceTypeKey,
    ResourceTypeKeyError, ResourceTypeSchema,
};
#[cfg(feature = "runtime")]
pub use host_extension::{
    HostExtension, HostModuleState, catalog_import_schemas,
    validate_catalog_import_schemas_with_fingerprints,
};
#[cfg(feature = "runtime")]
pub use jit::{
    JitAttempt, JitCallSiteProfile, JitConfig, JitExitProfile, JitMetrics, JitNyiDoc, JitNyiReason,
    JitSnapshot, JitTrace, JitTraceTerminal, TraceJitEngine,
};
#[cfg(feature = "runtime")]
pub use vm::diagnostics::render_vm_error;
#[cfg(feature = "runtime")]
pub use vm::{
    AotArtifactError, BeginResetOutcome, CallOutcome, CallReturn, CancellationReason,
    CapabilityProfile, CapabilityProfileBuilder, CaptureAsyncHostContext, CloseProgress,
    DEFAULT_MAX_SCRIPT_CALL_DEPTH, EpochCheckpoint, EpochHandle, FuelCheckpoint, HostArgsFunction,
    HostAsyncBridge, HostBindingPlan, HostContext, HostContextError, HostContextErrorKind,
    HostContextResult, HostFunction, HostFunctionRegistry, HostFuture, HostFutureOutput,
    HostImportBindingError, HostModule, HostOpId, HostResource, HostStackFunction, IntoScriptValue,
    Invocation, InvocationError, InvocationItem, InvocationPoll, QueuedScriptInvocation, Resource,
    ResourceAccessFrame, ResourceAccessMode, ResourceAccessRequest, ResourceError,
    ResourceErrorCode, ResourceHandle, ResourceMut, ResourceOwned, ResourceOwnership, ResourceRef,
    ResourceTable, ScriptArgs, ScriptCallback, ScriptResult, StandardSurfaceComposition,
    StaticHostArgsFunction, StaticHostFunction, StaticHostStackFunction, Store, Vm, VmError,
    VmResetError, VmResetState, VmResult, VmStatus, VmYieldReason, execution_scope, host_extension,
    operation, resource,
};

#[cfg(feature = "runtime")]
pub use vmbc::{
    DisassembleOptions, ValidationError, WireError, decode_program, disassemble_program,
    disassemble_program_with_options, disassemble_vmbc, disassemble_vmbc_with_options,
    encode_program, infer_local_count, validate_program,
};
