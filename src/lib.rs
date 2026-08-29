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
#[cfg(all(feature = "runtime", feature = "sqlite", not(target_arch = "wasm32")))]
pub use builtins::runtime::sqlite::{SqliteHostExt, SqliteLimits, SqlitePolicy};
#[cfg(feature = "runtime")]
pub(crate) fn install_default_host_functions(registry: &mut vm::HostFunctionRegistry) {
    builtins::runtime::register_default_host_functions(registry);
}

#[cfg(feature = "runtime")]
pub use builtins::runtime::{
    BorrowVmValue, FromVmValue, HostCallResult, IntoHostCallOutcome, TakeVmValue, arg, borrow_arg,
    return_one, take_arg,
};
#[cfg(all(feature = "runtime", not(target_arch = "wasm32")))]
pub use builtins::runtime::{IoHostExt, IoPolicy};
#[cfg(feature = "runtime")]
pub use builtins::runtime::{
    io_host_catalog, sqlite_host_catalog, standard_composition, standard_host_catalog,
};
pub use builtins::{
    BUILTIN_CATALOG, BuiltinFunction, BuiltinNamespaceMemberSpec, BuiltinNamespaceSpec,
    CallableDef, CallableParam, CallableParamType, CallableSignature, HostExecution,
    LanguageBuiltinSpec, builtin_namespace_specs, callable_signatures_for_builtin_namespace_member,
    default_host_callables, is_builtin_namespace, language_builtin_specs,
    resolve_builtin_namespace_call,
};
pub use bytecode::{
    CallableEnvironment, CallableKind, CallablePrototype, CallableTarget, CallableValue,
    CaptureBindingMode, ExportedCallable, FunctionRegion, HostImport, MAX_FRAME_LOCAL_COUNT,
    OpCode, Program, RootCallableBinding, ScriptFunction, TypeMap, Value, ValueType,
};
pub use host_api::{
    FunctionNameError, HostApiBuilder, HostApiCatalog, HostApiCatalogError, HostApiFingerprint,
    HostFunctionSchema, HostParamPassing, HostParamSchema, HostSchemaValidationError,
    HostTypeSchema, MAX_HOST_CATALOG_FUNCTIONS, MAX_HOST_CATALOG_PARAMETERS,
    MAX_HOST_CATALOG_RESOURCES, MAX_HOST_DESCRIPTION_LEN, MAX_HOST_FUNCTION_NAME_LEN,
    MAX_HOST_PARAMETER_NAME_LEN, MAX_HOST_RESOURCE_KEY_LEN, MAX_HOST_SCHEMA_DEPTH,
    MAX_HOST_SCHEMA_NODES, MAX_HOST_SCHEMA_PROPERTIES, ResourceTypeKey, ResourceTypeKeyError,
    ResourceTypeSchema, validate_host_import_schemas,
};
#[cfg(feature = "runtime")]
pub use vm::runtime::{
    EventLimits, EventPayload, RuntimeContext, RuntimeContextConfig, RuntimeError,
    RuntimeErrorCode, RuntimeResult, STREAM_EMIT_NAME,
};
pub fn builtin_call_index(name: &str) -> Option<u16> {
    use builtins::BuiltinFunction;

    BuiltinFunction::from_source_name(name).map(|builtin| builtin.call_index())
}
pub use compiler::diagnostics::{
    render_compile_error, render_source_error, render_source_path_error,
};
pub use compiler::source_map::{LineSpanMapping, LoweredSource, SourceId, SourceMap, Span};
pub use compiler::{
    AssignmentKind, ClosureExpr, CompileError, CompileSourceFileOptions, CompiledProgram,
    CompiledReplProgram, Compiler, CompletionItemKind, DeclSymbol, Definition, ExportEntry, Expr,
    FormatError, FrontendImportSyntax, FrontendIr, FunctionDecl, ImportClause, ImportTargetKind,
    ImportedBinding, InferredLocalTypeHint, LocalIrBuilder, LocalSlot, ModuleGraph, ModuleId,
    ModuleImport, ModuleNode, NamedImport, ParseError, ParserDialect, ReplLocalBinding,
    ReplLocalState, ResolvedImport, SemanticCompletion, SemanticDiagnostic, SemanticModel,
    SharedParserOptions, SourceError, SourceFlavor, SourcePathError, SourcePlugin, SourcePosition,
    Stmt, SymbolId, TypeSchema, UnknownInferredLocal, UseDecl, UsePathSegment, analyze_source,
    analyze_source_from_string_with_options, analyze_source_with_flavor,
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
    JitAttempt, JitCallSiteProfile, JitConfig, JitExitProfile, JitMetrics, JitNyiDoc, JitNyiReason,
    JitSnapshot, JitTrace, JitTraceTerminal, TraceJitEngine,
};
#[cfg(feature = "runtime")]
pub use vm::diagnostics::render_vm_error;
#[cfg(feature = "runtime")]
pub use vm::resource::{Resource, ResourceHandle, ResourceMut, ResourceOwned, ResourceRef};
#[cfg(feature = "runtime")]
pub use vm::{
    AotArtifactError, CallOutcome, CallReturn, CapabilityProfile, CapabilityProfileBuilder,
    CaptureAsyncHostContext, CatalogRegistrationError, CatalogSchemaSelection,
    DEFAULT_MAX_SCRIPT_CALL_DEPTH, EpochCheckpoint, EpochHandle, FuelCheckpoint, HostArgsFunction,
    HostAsyncBridge, HostAsyncOpTerminal, HostBindingPlan, HostContext, HostContextError,
    HostContextErrorKind, HostContextResult, HostExtension, HostFunction, HostFunctionRegistry,
    HostFuture, HostFutureOutput, HostImportParam, HostImportSchema, HostModule, HostModuleState,
    HostOpId, HostStackFunction, IntoScriptValue, Invocation, InvocationError, InvocationItem,
    InvocationPoll, QueuedScriptInvocation, RegistrySchemaError, ResourceCloseReason, ScriptArgs,
    ScriptCallback, ScriptResult, StandardSurfaceComposition, StaticHostArgsFunction,
    StaticHostFunction, StaticHostStackFunction, Store, Vm, VmError, VmResult, VmStatus,
    VmYieldReason, async_host, catalog_import_schemas, execution_scope, host_context,
    host_extension, operation, register_catalog_function, register_catalog_static_function,
    resource, validate_catalog_import_schemas, validate_catalog_import_schemas_with_fingerprints,
};
#[cfg(feature = "runtime")]
pub use vmbc::{
    DisassembleOptions, ValidationError, WireError, decode_program, disassemble_program,
    disassemble_program_with_options, disassemble_vmbc, disassemble_vmbc_with_options,
    encode_program, infer_local_count, validate_program,
};
