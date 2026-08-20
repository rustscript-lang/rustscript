use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Program;
use crate::assembler::AssemblerError;
#[cfg(feature = "runtime")]
use crate::vm::Vm;

mod codegen;
pub mod diagnostics;
mod format;
mod frontends;
mod host_call_resolve;
mod host_conversion;
pub mod ir;
mod lifetime;
mod linker;
mod materialization;
mod modules;
mod parser;
mod pipeline;
mod semantic_model;
mod source_loader;
pub mod source_map;
mod typing;

#[cfg(test)]
use self::materialization::CallableUseObservation;
use self::source_map::{SourceMap, Span};

pub use self::codegen::Compiler;
pub use self::format::{
    FormatError, format_source, format_source_with_flavor, format_source_with_flavor_and_options,
};
pub use self::frontends::parse_source_with_dialect;
pub use self::host_call_resolve::{HostCallResolveError, HostCallResolver};
pub use self::ir::{
    AssignmentKind, ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, FunctionParam,
    LocalIrBuilder, LocalSlot, MatchPattern, MatchTypePattern, ResolvedHostCall, ResolvedHostParam,
    Stmt, StructDecl, TypeSchema,
};
pub use self::modules::{
    DeclSymbol, ExportEntry, ImportTargetKind, ImportedBinding, ModuleGraph, ModuleId, ModuleNode,
    ResolvedImport, SymbolId, UseDecl, UsePathSegment,
};
pub use self::parser::ParserDialect;
pub use self::pipeline::{
    InferredLocalTypeHint, UnknownInferredLocal, collect_inferred_local_type_hints,
    collect_inferred_local_type_hints_at_path_with_options,
    collect_inferred_local_type_hints_with_options, compile_source,
    compile_source_at_path_with_flavor_and_options, compile_source_file,
    compile_source_file_with_options, compile_source_for_repl, compile_source_for_repl_with_locals,
    compile_source_for_repl_with_state, compile_source_with_flavor,
    compile_source_with_flavor_and_options, lint_trailing_function_return_semicolons,
    lint_unknown_inferred_local_types, lint_unknown_inferred_local_types_at_path_with_options,
    lint_unknown_inferred_local_types_with_options, lint_unknown_type_annotations,
};
pub use self::semantic_model::{
    CompletionItemKind, Definition, SemanticCompletion, SemanticDiagnostic, SemanticModel,
    SourcePosition,
};
pub use self::source_loader::{FrontendImportSyntax, ImportClause, ModuleImport, NamedImport};

#[derive(Debug)]
pub enum CompileError {
    Assembler(AssemblerError),
    CallArityOverflow,
    HostImportOverflow,
    ClosureUsedAsValue,
    CallableUsedAsValue,
    NonCallableLocal(LocalSlot),
    LocalSlotOverflow(LocalSlot),
    /// The aggregate frame-local count (data slots plus materialized callable
    /// slots) exceeds what the short bytecode operands can address. Carries
    /// the real counts so the diagnostic is actionable instead of a sentinel.
    FrameLocalLimitExceeded {
        data_slots: usize,
        callable_slots: usize,
        total_slots: usize,
        max_slots: usize,
    },
    CallableArityMismatch {
        expected: usize,
        got: usize,
    },
    BreakOutsideLoop,
    ContinueOutsideLoop,
    InlineFunctionRecursion(String),
    IfElseBranchTypeMismatch {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    CallableArgumentTypeMismatch {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    BinaryOperandTypeMismatch {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    InvalidFieldAccess {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    FunctionParameterTypeConflict {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    StrictTypingRequired {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    /// Catalog host-call overload resolution failed at a call site. Carries
    /// the optional call-site line and source name plus a diagnostic detail
    /// describing the failed overload selection.
    HostCallResolve {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
    /// Internal error: a symbol-resolved module call or function value
    /// survived unit merge and reached codegen, where flat function indices
    /// are the only valid call targets.
    UnresolvedModuleCall,
}

impl CompileError {
    pub fn line(&self) -> Option<usize> {
        match self {
            CompileError::IfElseBranchTypeMismatch { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }
            CompileError::CallableArgumentTypeMismatch { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }
            CompileError::BinaryOperandTypeMismatch { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }
            CompileError::InvalidFieldAccess { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }
            CompileError::FunctionParameterTypeConflict { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }
            CompileError::StrictTypingRequired { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }
            CompileError::HostCallResolve { line, .. } => {
                line.and_then(|value| usize::try_from(value).ok())
            }

            _ => None,
        }
    }

    pub fn source_name(&self) -> Option<&str> {
        match self {
            CompileError::IfElseBranchTypeMismatch { source_name, .. }
            | CompileError::CallableArgumentTypeMismatch { source_name, .. }
            | CompileError::BinaryOperandTypeMismatch { source_name, .. }
            | CompileError::InvalidFieldAccess { source_name, .. }
            | CompileError::FunctionParameterTypeConflict { source_name, .. }
            | CompileError::StrictTypingRequired { source_name, .. }
            | CompileError::HostCallResolve { source_name, .. } => source_name.as_deref(),
            _ => None,
        }
    }

    pub fn diagnostic_message(&self) -> String {
        match self {
            CompileError::Assembler(err) => err.to_string(),
            CompileError::CallArityOverflow => {
                "call arity exceeds the supported bytecode encoding".to_string()
            }
            CompileError::HostImportOverflow => {
                "host import count exceeds the supported bytecode encoding".to_string()
            }
            CompileError::ClosureUsedAsValue => {
                "closures cannot be used as plain values".to_string()
            }
            CompileError::CallableUsedAsValue => {
                "callables cannot be used as plain values".to_string()
            }
            CompileError::NonCallableLocal(slot) => format!("local slot {slot} is not callable"),
            CompileError::LocalSlotOverflow(slot) => {
                format!("local slot {slot} exceeds the supported bytecode encoding")
            }
            CompileError::FrameLocalLimitExceeded {
                data_slots,
                callable_slots,
                total_slots,
                max_slots,
            } => format!(
                "frame requires {total_slots} local slots ({data_slots} data + {callable_slots} callable); short bytecode supports {max_slots}"
            ),
            CompileError::CallableArityMismatch { expected, got } => {
                format!("callable arity mismatch: expected {expected}, got {got}")
            }
            CompileError::BreakOutsideLoop => "break used outside of a loop".to_string(),
            CompileError::ContinueOutsideLoop => "continue used outside of a loop".to_string(),
            CompileError::InlineFunctionRecursion(name) => {
                format!("inline function recursion detected in '{name}'")
            }
            CompileError::IfElseBranchTypeMismatch { detail, .. } => detail.clone(),
            CompileError::CallableArgumentTypeMismatch { detail, .. } => detail.clone(),
            CompileError::BinaryOperandTypeMismatch { detail, .. } => detail.clone(),
            CompileError::InvalidFieldAccess { detail, .. } => detail.clone(),
            CompileError::FunctionParameterTypeConflict { detail, .. } => detail.clone(),
            CompileError::StrictTypingRequired { detail, .. } => detail.clone(),
            CompileError::HostCallResolve { detail, .. } => detail.clone(),
            CompileError::UnresolvedModuleCall => {
                "internal compiler error: unresolved module call reached codegen".to_string()
            }
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.diagnostic_message())
    }
}

impl std::error::Error for CompileError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
    pub span: Option<Span>,
    pub code: Option<String>,
}

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            line: 1,
            message: message.into(),
            span: None,
            code: None,
        }
    }

    pub fn at_line(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
            span: None,
            code: None,
        }
    }

    pub fn at_span(span: Span, message: impl Into<String>) -> Self {
        Self {
            line: 1,
            message: message.into(),
            span: Some(span),
            code: None,
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_line_span_from_source(mut self, source_map: &SourceMap, source_id: u32) -> Self {
        if self.span.is_some() {
            return self;
        }
        if let Some(span) = source_map.line_span(source_id, self.line) {
            self.span = Some(span);
        }
        self
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(span) = self.span {
            write!(
                f,
                "{} (source {} [{}..{}])",
                self.message, span.source_id, span.lo, span.hi
            )
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum SourceError {
    Parse(ParseError),
    Compile(CompileError),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::Parse(err) => write!(f, "{err}"),
            SourceError::Compile(err) => write!(f, "compile error: {err}"),
        }
    }
}

impl std::error::Error for SourceError {}

#[derive(Debug)]
pub enum SourcePathError {
    Io(std::io::Error),
    MissingExtension,
    UnsupportedExtension(String),
    MissingFrontendPlugin(SourceFlavor),
    ImportCycle(PathBuf),
    NonRustScriptModule(PathBuf),
    ImportWithoutParent(PathBuf),
    InvalidImportSyntax {
        path: PathBuf,
        line: usize,
        message: String,
    },
    Source(SourceError),
    /// A source error plus the compilation-wide [`SourceMap`] that resolves
    /// every span it carries (milestone 5). Produced by the module-loading
    /// compile entry points; spans reference the semantic module graph's
    /// `SourceId` space, so rendering against this map always reads from the
    /// owning source. `Display` delegates to the inner error.
    SourceWithMap {
        error: SourceError,
        sources: SourceMap,
    },
}

impl SourcePathError {
    /// The compilation-wide source map carried with this error, if any.
    pub fn sources(&self) -> Option<&SourceMap> {
        match self {
            SourcePathError::SourceWithMap { sources, .. } => Some(sources),
            _ => None,
        }
    }
}

impl fmt::Display for SourcePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourcePathError::Io(err) => write!(f, "{err}"),
            SourcePathError::MissingExtension => write!(f, "source file must have an extension"),
            SourcePathError::UnsupportedExtension(ext) => write!(
                f,
                "unsupported source extension '.{ext}', expected .rss, .js, or .lua"
            ),
            SourcePathError::MissingFrontendPlugin(flavor) => {
                write!(f, "no frontend plugin registered for {flavor:?} source")
            }
            SourcePathError::ImportCycle(path) => {
                write!(f, "import cycle detected at '{}'", path.display())
            }
            SourcePathError::NonRustScriptModule(path) => {
                write!(f, "module '{}' must use .rss extension", path.display())
            }
            SourcePathError::ImportWithoutParent(path) => write!(
                f,
                "cannot resolve import from '{}': missing parent directory",
                path.display()
            ),
            SourcePathError::InvalidImportSyntax {
                path,
                line,
                message,
            } => write!(
                f,
                "invalid import syntax in '{}' at line {}: {}",
                path.display(),
                line,
                message
            ),
            SourcePathError::Source(err) => write!(f, "{err}"),
            SourcePathError::SourceWithMap { error, .. } => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SourcePathError {}

impl From<std::io::Error> for SourcePathError {
    fn from(value: std::io::Error) -> Self {
        SourcePathError::Io(value)
    }
}

impl From<SourceError> for SourcePathError {
    fn from(value: SourceError) -> Self {
        SourcePathError::Source(value)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SourceFlavor {
    RustScript,
    JavaScript,
    Lua,
}

pub trait SourcePlugin: Sync {
    fn flavor(&self) -> SourceFlavor;

    fn extensions(&self) -> &'static [&'static str];

    fn import_syntax(&self) -> FrontendImportSyntax;

    fn parse_source(&self, source: &str) -> Result<FrontendIr, ParseError>;

    fn parser_dialect(&self) -> Option<&'static dyn ParserDialect> {
        None
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
    ) -> Result<Vec<ModuleImport>, SourcePathError> {
        Ok(Vec::new())
    }

    fn strip_import_directives(&self, source: &str) -> String {
        source.to_string()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SharedParserOptions {
    pub source_id: u32,
    pub allow_implicit_externs: bool,
    pub allow_implicit_semicolons: bool,
    pub enforce_mutable_bindings: bool,
    /// Import-scan mode: used by the source loader's discovery parse. The
    /// parser tolerates calls to not-yet-declared imported functions
    /// (`allow_implicit_externs`) and records host aliases for multi-segment
    /// file-module paths so namespace calls parse during the scan; the
    /// resulting IR is discarded after `use` declarations are extracted.
    pub import_scan_mode: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypingMode {
    DynamicHints,
    StrictRustScript,
}

impl TypingMode {
    pub(crate) fn for_flavor(flavor: SourceFlavor) -> Self {
        match flavor {
            SourceFlavor::RustScript => Self::StrictRustScript,
            SourceFlavor::JavaScript | SourceFlavor::Lua => Self::DynamicHints,
        }
    }

    pub(crate) fn is_strict(self) -> bool {
        matches!(self, Self::StrictRustScript)
    }
}

impl SourceFlavor {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rss" => Some(Self::RustScript),
            "js" => Some(Self::JavaScript),
            "lua" => Some(Self::Lua),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, SourcePathError> {
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .ok_or(SourcePathError::MissingExtension)?;
        SourceFlavor::from_extension(ext)
            .ok_or_else(|| SourcePathError::UnsupportedExtension(ext.to_string()))
    }

    pub(crate) fn from_path_with_options(
        path: &Path,
        options: &CompileSourceFileOptions,
    ) -> Result<Self, SourcePathError> {
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .ok_or(SourcePathError::MissingExtension)?;
        if let Some(plugin) = options.source_plugin_for_extension(ext) {
            return Ok(plugin.flavor());
        }
        SourceFlavor::from_extension(ext)
            .ok_or_else(|| SourcePathError::UnsupportedExtension(ext.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplLocalBinding {
    pub name: String,
    pub mutable: bool,
    pub schema: Option<TypeSchema>,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplLocalState {
    pub binding: ReplLocalBinding,
    pub moved: bool,
}

pub struct CompiledProgram {
    pub program: Program,
    pub locals: usize,
    pub functions: Vec<FunctionDecl>,
    /// Milestone-5 callable-use classification observed through the
    /// production pipeline, keyed by resolved flat function index and
    /// sorted by index. Test-only observation compiled into the crate's
    /// unit-test builds only; never part of the public API.
    #[cfg(test)]
    pub(crate) callable_use_facts: Vec<CallableUseObservation>,
}

impl CompiledProgram {
    #[cfg(feature = "runtime")]
    pub fn into_vm(self) -> Vm {
        Vm::new(self.program)
    }
}

pub struct CompiledReplProgram {
    pub compiled: CompiledProgram,
    pub bindings: Vec<ReplLocalBinding>,
}

#[derive(Clone, Default)]
pub struct CompileSourceFileOptions {
    module_path_overrides: HashMap<String, PathBuf>,
    module_source_overrides: HashMap<String, String>,
    source_plugins: Vec<&'static dyn SourcePlugin>,
    host_api_catalog: Option<Arc<crate::host_api::HostApiCatalog>>,
}

impl fmt::Debug for CompileSourceFileOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("CompileSourceFileOptions");
        debug
            .field("module_path_overrides", &self.module_path_overrides)
            .field("module_source_overrides", &self.module_source_overrides)
            .field("source_plugin_count", &self.source_plugins.len());
        match &self.host_api_catalog {
            Some(catalog) => {
                debug.field("host_api_catalog_present", &true);
                debug.field("host_api_catalog_fingerprint", &Some(catalog.fingerprint()));
            }
            None => {
                debug.field("host_api_catalog_present", &false);
                debug.field(
                    "host_api_catalog_fingerprint",
                    &Option::<crate::host_api::HostApiFingerprint>::None,
                );
            }
        }
        debug.finish()
    }
}

impl CompileSourceFileOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_host_api_catalog(mut self, catalog: Arc<crate::host_api::HostApiCatalog>) -> Self {
        self.set_host_api_catalog(catalog);
        self
    }

    pub fn set_host_api_catalog(&mut self, catalog: Arc<crate::host_api::HostApiCatalog>) {
        self.host_api_catalog = Some(catalog);
    }

    pub fn host_api_catalog(&self) -> Option<&Arc<crate::host_api::HostApiCatalog>> {
        self.host_api_catalog.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn has_host_api_catalog(&self) -> bool {
        self.host_api_catalog.is_some()
    }

    pub fn with_module_override_path(
        mut self,
        import_spec: impl Into<String>,
        module_path: impl Into<PathBuf>,
    ) -> Self {
        self.set_module_override_path(import_spec, module_path);
        self
    }

    pub fn set_module_override_path(
        &mut self,
        import_spec: impl Into<String>,
        module_path: impl Into<PathBuf>,
    ) {
        let key = normalize_import_spec(import_spec.into());
        self.module_path_overrides.insert(key, module_path.into());
    }

    pub fn with_module_override_source(
        mut self,
        import_spec: impl Into<String>,
        module_source: impl Into<String>,
    ) -> Self {
        self.set_module_override_source(import_spec, module_source);
        self
    }

    pub fn set_module_override_source(
        &mut self,
        import_spec: impl Into<String>,
        module_source: impl Into<String>,
    ) {
        let key = normalize_import_spec(import_spec.into());
        self.module_source_overrides
            .insert(key, module_source.into());
    }

    pub fn with_source_plugin(mut self, plugin: &'static dyn SourcePlugin) -> Self {
        self.add_source_plugin(plugin);
        self
    }

    pub fn add_source_plugin(&mut self, plugin: &'static dyn SourcePlugin) {
        self.source_plugins.push(plugin);
    }

    pub fn module_override_path(&self, import_spec: &str) -> Option<&Path> {
        let key = normalize_import_spec(import_spec.to_string());
        self.module_path_overrides.get(&key).map(PathBuf::as_path)
    }

    pub fn module_override_source(&self, import_spec: &str) -> Option<&str> {
        let key = normalize_import_spec(import_spec.to_string());
        self.module_source_overrides.get(&key).map(String::as_str)
    }

    pub(crate) fn has_module_overrides(&self) -> bool {
        !self.module_path_overrides.is_empty() || !self.module_source_overrides.is_empty()
    }

    pub(crate) fn has_source_plugins(&self) -> bool {
        !self.source_plugins.is_empty()
    }

    pub(crate) fn source_plugin_for_flavor(
        &self,
        flavor: SourceFlavor,
    ) -> Option<&'static dyn SourcePlugin> {
        self.source_plugins
            .iter()
            .copied()
            .find(|plugin| plugin.flavor() == flavor)
    }

    pub(crate) fn source_plugin_for_extension(
        &self,
        ext: &str,
    ) -> Option<&'static dyn SourcePlugin> {
        self.source_plugins.iter().copied().find(|plugin| {
            plugin
                .extensions()
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
    }
}

const STDLIB_PRINT_NAME: &str = "print";
const STDLIB_PRINT_ARITY: u8 = 1;

fn normalize_import_spec(spec: String) -> String {
    normalize_import_key(spec.trim())
}

fn normalize_import_key(spec: &str) -> String {
    let normalized = spec.replace('\\', "/");
    let (prefix, remainder) = split_windows_prefix(&normalized);
    let absolute = remainder.starts_with('/');
    let mut segments = Vec::<&str>::new();

    for segment in remainder.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            match segments.last().copied() {
                Some(existing) if existing != ".." => {
                    segments.pop();
                }
                _ if !absolute => segments.push(".."),
                _ => {}
            }
            continue;
        }
        segments.push(segment);
    }

    let mut out = String::new();
    out.push_str(prefix);
    if absolute {
        out.push('/');
    }
    out.push_str(&segments.join("/"));
    out
}

fn split_windows_prefix(input: &str) -> (&str, &str) {
    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        (&input[..2], &input[2..])
    } else {
        ("", input)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::HostApiCatalog;
    use crate::host_api::{HostApiBuilder, HostFunctionSchema, HostParamSchema, HostTypeSchema};

    use super::{CompileError, CompileSourceFileOptions};

    fn test_catalog() -> Arc<HostApiCatalog> {
        let mut builder = HostApiBuilder::new();
        let mut f = HostFunctionSchema::with_return(
            "unambiguous_unique_marker_fn",
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        );
        f.description = "TOP-SECRET-OPTION-DEBUG-DOC".to_string();
        builder.function(f);
        Arc::new(builder.build().expect("test catalog must be valid"))
    }

    #[test]
    fn default_has_no_host_api_catalog() {
        let options = CompileSourceFileOptions::default();
        assert!(options.host_api_catalog().is_none());
        assert!(!options.has_host_api_catalog());
    }

    #[test]
    fn setter_stores_same_catalog() {
        let catalog = test_catalog();
        let mut options = CompileSourceFileOptions::default();
        options.set_host_api_catalog(Arc::clone(&catalog));
        let stored = options.host_api_catalog().expect("set catalog present");
        assert!(Arc::ptr_eq(&catalog, stored));
        assert!(options.has_host_api_catalog());
    }

    #[test]
    fn builder_pointer_is_same_catalog() {
        let catalog = test_catalog();
        let options =
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
        let stored = options.host_api_catalog().expect("builder catalog present");
        assert!(Arc::ptr_eq(&catalog, stored));
    }

    #[test]
    fn clone_shares_same_catalog() {
        let options = CompileSourceFileOptions::default().with_host_api_catalog(test_catalog());
        let cloned = options.clone();
        let original = options.host_api_catalog().expect("original present");
        let cloned_catalog = cloned.host_api_catalog().expect("clone present");
        assert!(Arc::ptr_eq(original, cloned_catalog));
    }

    #[test]
    fn debug_reveals_presence_and_fingerprint_only() {
        let options = CompileSourceFileOptions::default().with_host_api_catalog(test_catalog());
        let debug = format!("{:?}", options);
        let fp_debug = format!(
            "{:?}",
            options.host_api_catalog().expect("present").fingerprint()
        );
        assert!(debug.contains("host_api_catalog_present"));
        assert!(debug.contains("host_api_catalog_fingerprint"));
        assert!(debug.contains(&fp_debug));
        assert!(!debug.contains("unambiguous_unique_marker_fn"));
        assert!(!debug.contains("TOP-SECRET-OPTION-DEBUG-DOC"));

        let defaults = CompileSourceFileOptions::default();
        let default_debug = format!("{:?}", defaults);
        assert!(default_debug.contains("host_api_catalog_present: false"));
        assert!(default_debug.contains("host_api_catalog_fingerprint: None"));
    }

    #[test]
    fn host_call_resolve_accessors() {
        let with_meta = CompileError::HostCallResolve {
            line: Some(42),
            source_name: Some("main.rss".to_string()),
            detail: "no overload of 'fetch' matches (Int)".to_string(),
        };
        assert_eq!(with_meta.line(), Some(42));
        assert_eq!(with_meta.source_name(), Some("main.rss"));
        assert_eq!(
            with_meta.diagnostic_message(),
            "no overload of 'fetch' matches (Int)"
        );

        let without_meta = CompileError::HostCallResolve {
            line: None,
            source_name: None,
            detail: "catalog resolution failed".to_string(),
        };
        assert_eq!(without_meta.line(), None);
        assert_eq!(without_meta.source_name(), None);
        assert_eq!(
            without_meta.diagnostic_message(),
            "catalog resolution failed"
        );
    }
}
