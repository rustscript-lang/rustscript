use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::Program;
use crate::assembler::AssemblerError;
#[cfg(feature = "runtime")]
use crate::vm::Vm;

mod codegen;
pub mod diagnostics;
mod frontends;
pub mod ir;
mod lifetime;
mod linker;
mod parser;
mod pipeline;
mod source_loader;
pub mod source_map;
mod typing;

use self::source_map::{SourceMap, Span};

pub use self::codegen::Compiler;
pub use self::ir::{
    ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern,
    MatchTypePattern, Stmt,
};
pub use self::pipeline::{
    compile_source, compile_source_at_path_with_flavor_and_options, compile_source_file,
    compile_source_file_with_options, compile_source_for_repl, compile_source_for_repl_with_locals,
    compile_source_with_flavor, compile_source_with_flavor_and_options,
    lint_trailing_function_return_semicolons,
};

#[derive(Debug)]
pub enum CompileError {
    Assembler(AssemblerError),
    CallArityOverflow,
    ClosureUsedAsValue,
    CallableUsedAsValue,
    NonCallableLocal(LocalSlot),
    LocalSlotOverflow(LocalSlot),
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
    FunctionParameterTypeConflict {
        line: Option<u32>,
        source_name: Option<String>,
        detail: String,
    },
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
            CompileError::FunctionParameterTypeConflict { line, .. } => {
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
            | CompileError::FunctionParameterTypeConflict { source_name, .. } => {
                source_name.as_deref()
            }
            _ => None,
        }
    }

    pub fn diagnostic_message(&self) -> String {
        match self {
            CompileError::Assembler(err) => err.to_string(),
            CompileError::CallArityOverflow => {
                "call arity exceeds the supported bytecode encoding".to_string()
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
            CompileError::FunctionParameterTypeConflict { detail, .. } => detail.clone(),
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
    ImportCycle(PathBuf),
    NonRustScriptModule(PathBuf),
    ImportWithoutParent(PathBuf),
    InvalidImportSyntax {
        path: PathBuf,
        line: usize,
        message: String,
    },
    Source(SourceError),
}

impl fmt::Display for SourcePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourcePathError::Io(err) => write!(f, "{err}"),
            SourcePathError::MissingExtension => write!(f, "source file must have an extension"),
            SourcePathError::UnsupportedExtension(ext) => write!(
                f,
                "unsupported source extension '.{ext}', expected .rss, .js, .lua, or .scm"
            ),
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
    Scheme,
}

impl SourceFlavor {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rss" => Some(Self::RustScript),
            "js" => Some(Self::JavaScript),
            "lua" => Some(Self::Lua),
            "scm" => Some(Self::Scheme),
            _ => None,
        }
    }

    pub(crate) fn from_path(path: &Path) -> Result<Self, SourcePathError> {
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .ok_or(SourcePathError::MissingExtension)?;
        SourceFlavor::from_extension(ext)
            .ok_or_else(|| SourcePathError::UnsupportedExtension(ext.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplLocalBinding {
    pub name: String,
    pub mutable: bool,
}

pub struct CompiledProgram {
    pub program: Program,
    pub locals: usize,
    pub functions: Vec<FunctionDecl>,
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

#[derive(Clone, Debug, Default)]
pub struct CompileSourceFileOptions {
    module_path_overrides: HashMap<String, PathBuf>,
    module_source_overrides: HashMap<String, String>,
}

impl CompileSourceFileOptions {
    pub fn new() -> Self {
        Self::default()
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
