use std::collections::HashMap;
use std::path::{Path, PathBuf};

use self::source_map::{SourceMap, Span};
use crate::assembler::{Assembler, AssemblerError};
use crate::builtins::BuiltinFunction;
#[cfg(feature = "runtime")]
use crate::vm::Vm;
use crate::{HostImport, Program, Value};

#[derive(Debug)]
pub enum CompileError {
    Assembler(AssemblerError),
    CallArityOverflow,
    ClosureUsedAsValue,
    CallableUsedAsValue,
    NonCallableLocal(LocalSlot),
    LocalSlotOverflow(LocalSlot),
    CallableArityMismatch { expected: usize, got: usize },
    BreakOutsideLoop,
    ContinueOutsideLoop,
    InlineFunctionRecursion(String),
}

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

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Parse(err) => write!(f, "{err}"),
            SourceError::Compile(err) => write!(f, "compile error: {err:?}"),
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

impl std::fmt::Display for SourcePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

    fn from_path(path: &Path) -> Result<Self, SourcePathError> {
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .ok_or(SourcePathError::MissingExtension)?;
        SourceFlavor::from_extension(ext)
            .ok_or_else(|| SourcePathError::UnsupportedExtension(ext.to_string()))
    }
}

const STDLIB_PRINT_NAME: &str = "print";
const STDLIB_PRINT_ARITY: u8 = 1;

pub mod diagnostics;
mod frontends;
pub mod ir;
mod lifetime;
mod linker;
mod opt;
mod parser;
mod source_loader;
pub mod source_map;

use linker::merge_units;

pub use ir::{
    ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern,
    MatchTypePattern, Stmt,
};

pub struct CompiledProgram {
    pub program: Program,
    pub locals: usize,
    pub functions: Vec<FunctionDecl>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LocalDebugRange {
    declared_line: Option<u32>,
    last_line: Option<u32>,
}

impl CompiledProgram {
    #[cfg(feature = "runtime")]
    pub fn into_vm(self) -> Vm {
        Vm::new(self.program)
    }
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
        self.module_source_overrides.insert(key, module_source.into());
    }

    pub fn module_override_path(&self, import_spec: &str) -> Option<&Path> {
        let key = normalize_import_spec(import_spec.to_string());
        self.module_path_overrides.get(&key).map(PathBuf::as_path)
    }

    pub fn module_override_source(&self, import_spec: &str) -> Option<&str> {
        let key = normalize_import_spec(import_spec.to_string());
        self.module_source_overrides.get(&key).map(String::as_str)
    }

    fn has_module_overrides(&self) -> bool {
        !self.module_path_overrides.is_empty() || !self.module_source_overrides.is_empty()
    }
}

fn normalize_import_spec(spec: String) -> String {
    spec.trim().replace('\\', "/")
}

#[derive(Clone, Copy, Debug)]
struct CompileBehavior {
    clear_dead_locals: bool,
}

impl CompileBehavior {
    const DEFAULT: Self = Self {
        clear_dead_locals: true,
    };
    const REPL: Self = Self {
        clear_dead_locals: false,
    };
}

fn compile_parsed_output(
    source: String,
    parsed: FrontendIr,
    behavior: CompileBehavior,
) -> Result<CompiledProgram, SourceError> {
    let local_debug_ranges = collect_named_local_debug_ranges(&parsed);
    let parsed = opt::legalize_builtins_and_bind_types(parsed);
    let parsed = lifetime::enforce_local_availability(parsed, behavior.clear_dead_locals)
        .map_err(SourceError::Parse)?;
    let FrontendIr {
        stmts,
        locals,
        local_bindings,
        functions,
        function_impls,
    } = parsed;

    let mut runtime_import_functions: Vec<FunctionDecl> = functions
        .iter()
        .filter(|func| !function_impls.contains_key(&func.index))
        .cloned()
        .collect();
    let mut call_index_remap = HashMap::<u16, u16>::new();
    for (next_index, func) in runtime_import_functions.iter_mut().enumerate() {
        let next_index = u16::try_from(next_index).map_err(|_| {
            SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: "too many host imports after RSS function inlining".to_string(),
            })
        })?;
        call_index_remap.insert(func.index, next_index);
        func.index = next_index;
    }
    let visible_runtime_import_functions = runtime_import_functions
        .iter()
        .filter(|func| !is_compiler_primitive_import(&func.name))
        .cloned()
        .collect::<Vec<_>>();

    let mut compiler = Compiler::new();
    compiler.set_source(source);
    compiler.set_function_impls(function_impls);
    compiler.set_call_index_remap(call_index_remap);
    for func in &functions {
        compiler.add_function_debug(func);
    }
    for (name, index) in local_bindings {
        let range = local_debug_ranges.get(&name).copied().unwrap_or_default();
        compiler
            .add_local_debug(name, index, range.declared_line, range.last_line)
            .map_err(SourceError::Compile)?;
    }
    let mut program = compiler
        .compile_program(&stmts)
        .map_err(SourceError::Compile)?;
    program.local_count = locals;
    program.imports = runtime_import_functions
        .iter()
        .map(|func| HostImport {
            name: func.name.clone(),
            arity: func.arity,
        })
        .collect();
    Ok(CompiledProgram {
        program,
        locals,
        functions: visible_runtime_import_functions,
    })
}

fn is_compiler_primitive_import(name: &str) -> bool {
    name.starts_with("__prim_")
}

pub fn compile_source(source: &str) -> Result<CompiledProgram, SourceError> {
    compile_source_with_flavor(source, SourceFlavor::RustScript)
}

pub fn compile_source_for_repl(source: &str) -> Result<CompiledProgram, SourceError> {
    compile_source_with_flavor_and_behavior(source, SourceFlavor::RustScript, CompileBehavior::REPL)
}

pub fn compile_source_with_flavor(
    source: &str,
    flavor: SourceFlavor,
) -> Result<CompiledProgram, SourceError> {
    compile_source_with_flavor_and_behavior(source, flavor, CompileBehavior::DEFAULT)
}

pub fn compile_source_with_flavor_and_options(
    source: &str,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    let source_owned = source.to_string();
    run_with_compiler_stack(move || {
        compile_source_with_flavor_and_options_impl(&source_owned, flavor, &options)
    })
}

fn compile_source_with_flavor_and_behavior(
    source: &str,
    flavor: SourceFlavor,
    behavior: CompileBehavior,
) -> Result<CompiledProgram, SourceError> {
    let owned_source = source.to_string();
    run_with_compiler_stack(move || {
        compile_source_with_flavor_impl(&owned_source, flavor, behavior)
    })
}

fn compile_source_with_flavor_impl(
    source: &str,
    flavor: SourceFlavor,
    behavior: CompileBehavior,
) -> Result<CompiledProgram, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    let parsed = frontends::parse_source(source, flavor).map_err(|err| {
        SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
    })?;
    match compile_parsed_output(source.to_string(), parsed, behavior) {
        Err(SourceError::Parse(err)) => Err(SourceError::Parse(
            err.with_line_span_from_source(&source_map, source_id),
        )),
        other => other,
    }
}

fn compile_source_with_flavor_and_options_impl(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    if !options.has_module_overrides() {
        return compile_source_with_flavor_impl(source, flavor, CompileBehavior::DEFAULT)
            .map_err(SourcePathError::Source);
    }

    let path = virtual_inmemory_entry_path(flavor);
    let (_root_parse_source, units) =
        source_loader::load_units_for_source_file(&path, flavor, source, options)?;
    let merged = merge_units(units)?;
    compile_parsed_output(source.to_string(), merged, CompileBehavior::DEFAULT)
        .map_err(SourcePathError::Source)
}

fn virtual_inmemory_entry_path(flavor: SourceFlavor) -> PathBuf {
    let ext = match flavor {
        SourceFlavor::RustScript => "rss",
        SourceFlavor::JavaScript => "js",
        SourceFlavor::Lua => "lua",
        SourceFlavor::Scheme => "scm",
    };
    PathBuf::from("__pd_vm_inmemory__").join(format!("main.{ext}"))
}

pub fn compile_source_file(path: impl AsRef<Path>) -> Result<CompiledProgram, SourcePathError> {
    compile_source_file_with_options(path, CompileSourceFileOptions::default())
}

pub fn compile_source_file_with_options(
    path: impl AsRef<Path>,
    options: CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    let path = path.as_ref().to_path_buf();
    run_with_compiler_stack(move || compile_source_file_impl(&path, &options))
}

fn compile_source_file_impl(
    path: &Path,
    options: &CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    let flavor = SourceFlavor::from_path(path)?;
    let source_raw = std::fs::read_to_string(path)?;
    let (_root_parse_source, units) =
        source_loader::load_units_for_source_file(path, flavor, &source_raw, options)?;
    let merged = merge_units(units)?;
    compile_parsed_output(source_raw, merged, CompileBehavior::DEFAULT)
        .map_err(SourcePathError::Source)
}

fn run_with_compiler_stack<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    #[cfg(target_arch = "wasm32")]
    {
        f()
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        const COMPILER_STACK_SIZE: usize = 32 * 1024 * 1024;
        let handle = std::thread::Builder::new()
            .name("pd-vm-compile".to_string())
            .stack_size(COMPILER_STACK_SIZE)
            .spawn(f)
            .expect("failed to spawn compiler thread");
        match handle.join() {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

pub struct Compiler {
    assembler: Assembler,
    next_label_id: u32,
    loop_stack: Vec<LoopContext>,
    function_impls: HashMap<u16, FunctionImpl>,
    call_index_remap: HashMap<u16, u16>,
    inline_call_stack: Vec<u16>,
    callable_bindings: HashMap<LocalSlot, CallableBinding>,
}

struct LoopContext {
    continue_label: String,
    break_label: String,
}

#[derive(Clone)]
enum CallableBinding {
    Closure(ClosureExpr),
    Function(u16),
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

impl Compiler {
    pub fn new() -> Self {
        Self {
            assembler: Assembler::new(),
            next_label_id: 0,
            loop_stack: Vec::new(),
            function_impls: HashMap::new(),
            call_index_remap: HashMap::new(),
            inline_call_stack: Vec::new(),
            callable_bindings: HashMap::new(),
        }
    }

    pub fn set_source(&mut self, source: String) {
        self.assembler.set_source(source);
    }

    pub fn add_function_debug(&mut self, func: &FunctionDecl) {
        self.assembler
            .add_function(func.name.clone(), func.args.clone());
    }

    pub fn add_local_debug(
        &mut self,
        name: String,
        index: LocalSlot,
        declared_line: Option<u32>,
        last_line: Option<u32>,
    ) -> Result<(), CompileError> {
        self.assembler.add_local_with_range(
            name,
            local_slot_operand(index)?,
            declared_line,
            last_line,
        );
        Ok(())
    }

    pub fn set_function_impls(&mut self, function_impls: HashMap<u16, FunctionImpl>) {
        self.function_impls = function_impls;
    }

    pub fn set_call_index_remap(&mut self, call_index_remap: HashMap<u16, u16>) {
        self.call_index_remap = call_index_remap;
    }

    pub fn compile_program(mut self, stmts: &[Stmt]) -> Result<Program, CompileError> {
        self.compile_stmts(stmts)?;
        self.assembler.ret();
        self.assembler
            .finish_program()
            .map_err(CompileError::Assembler)
    }

    fn compile_stmts(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            self.compile_stmt(stmt)?;
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        match stmt {
            Stmt::Noop { line } => {
                self.assembler.mark_line(*line);
            }
            Stmt::Let { index, expr, line } => {
                self.assembler.mark_line(*line);
                self.assign_expr_to_slot(*index, expr)?;
            }
            Stmt::Assign { index, expr, line } => {
                self.assembler.mark_line(*line);
                self.assign_expr_to_slot(*index, expr)?;
            }
            Stmt::ClosureLet { line, closure } => {
                self.assembler.mark_line(*line);
                self.bind_closure_captures(closure)?;
            }
            Stmt::FuncDecl { .. } => {}
            Stmt::Expr { expr, line } => {
                self.assembler.mark_line(*line);
                self.compile_expr(expr)?;
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                self.assembler.mark_line(*line);
                let else_label = self.fresh_label("else");
                let end_label = self.fresh_label("endif");
                self.compile_expr(condition)?;
                self.assembler.brfalse_label(&else_label);
                self.compile_stmts(then_branch)?;
                self.assembler.br_label(&end_label);
                self.assembler
                    .label(&else_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot.clone();
                self.compile_stmts(else_branch)?;
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                self.assembler.mark_line(*line);
                self.compile_stmt(init)?;
                let start_label = self.fresh_label("for_start");
                let continue_label = self.fresh_label("for_continue");
                let end_label = self.fresh_label("for_end");
                self.assembler
                    .label(&start_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_expr(condition)?;
                self.assembler.brfalse_label(&end_label);
                self.loop_stack.push(LoopContext {
                    continue_label: continue_label.clone(),
                    break_label: end_label.clone(),
                });
                self.compile_stmts(body)?;
                self.loop_stack.pop();
                self.assembler
                    .label(&continue_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_stmt(post)?;
                self.assembler.br_label(&start_label);
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                self.assembler.mark_line(*line);
                let start_label = self.fresh_label("while_start");
                let end_label = self.fresh_label("while_end");
                self.assembler
                    .label(&start_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_expr(condition)?;
                self.assembler.brfalse_label(&end_label);
                self.loop_stack.push(LoopContext {
                    continue_label: start_label.clone(),
                    break_label: end_label.clone(),
                });
                self.compile_stmts(body)?;
                self.loop_stack.pop();
                self.assembler.br_label(&start_label);
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
            }
            Stmt::Break { line } => {
                self.assembler.mark_line(*line);
                let loop_ctx = self
                    .loop_stack
                    .last()
                    .ok_or(CompileError::BreakOutsideLoop)?;
                self.assembler.br_label(&loop_ctx.break_label);
            }
            Stmt::Continue { line } => {
                self.assembler.mark_line(*line);
                let loop_ctx = self
                    .loop_stack
                    .last()
                    .ok_or(CompileError::ContinueOutsideLoop)?;
                self.assembler.br_label(&loop_ctx.continue_label);
            }
        }
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        match expr {
            Expr::Null => {
                self.assembler.push_const(Value::Null);
            }
            Expr::Int(value) => {
                self.assembler.push_const(Value::Int(*value));
            }
            Expr::Float(value) => {
                self.assembler.push_const(Value::Float(*value));
            }
            Expr::Bool(value) => {
                self.assembler.push_const(Value::Bool(*value));
            }
            Expr::String(value) => {
                self.assembler.push_const(Value::String(value.clone()));
            }
            Expr::FunctionRef(_) => {
                return Err(CompileError::CallableUsedAsValue);
            }
            Expr::Call(index, args) => {
                self.compile_function_call(*index, args)?;
            }
            Expr::Closure(_) => {
                return Err(CompileError::CallableUsedAsValue);
            }
            Expr::ClosureCall(closure, args) => {
                self.compile_inline_closure_call(closure, args)?;
            }
            Expr::LocalCall(index, args) => {
                let callable = self
                    .callable_bindings
                    .get(index)
                    .cloned()
                    .ok_or(CompileError::NonCallableLocal(*index))?;
                self.compile_callable_call(callable, args)?;
            }
            Expr::Add(lhs, rhs) => {
                if is_definitely_string_expr(lhs) {
                    self.compile_expr(lhs)?;
                    self.compile_string_concat_operand(rhs)?;
                    self.assembler.add();
                    return Ok(());
                }
                if is_definitely_string_expr(rhs) {
                    self.compile_string_concat_operand(lhs)?;
                    self.compile_expr(rhs)?;
                    self.assembler.add();
                    return Ok(());
                }
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.add();
            }
            Expr::Sub(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.sub();
            }
            Expr::Mul(lhs, rhs) => {
                if let Expr::Int(value) = rhs.as_ref()
                    && let Some(shift) = shift_amount_for_power_of_two(*value)
                {
                    self.compile_expr(lhs)?;
                    self.assembler.push_const(Value::Int(shift as i64));
                    self.assembler.shl();
                } else if let Expr::Int(value) = lhs.as_ref()
                    && let Some(shift) = shift_amount_for_power_of_two(*value)
                {
                    self.compile_expr(rhs)?;
                    self.assembler.push_const(Value::Int(shift as i64));
                    self.assembler.shl();
                } else {
                    self.compile_expr(lhs)?;
                    self.compile_expr(rhs)?;
                    self.assembler.mul();
                }
            }
            Expr::Div(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.div();
            }
            Expr::Mod(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.modulo();
            }
            Expr::Neg(inner) => {
                self.compile_expr(inner)?;
                self.assembler.neg();
            }
            Expr::Not(inner) => {
                self.compile_expr(inner)?;
                self.assembler.push_const(Value::Bool(false));
                self.assembler.ceq();
            }
            Expr::ToOwned(inner) => {
                self.compile_expr(inner)?;
            }
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.compile_expr(inner)?;
            }
            Expr::And(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.and();
            }
            Expr::Or(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.or();
            }
            Expr::Eq(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.ceq();
            }
            Expr::Lt(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.clt();
            }
            Expr::Gt(lhs, rhs) => {
                self.compile_expr(lhs)?;
                self.compile_expr(rhs)?;
                self.assembler.cgt();
            }
            Expr::Var(index) => {
                if self.callable_bindings.contains_key(index) {
                    return Err(CompileError::CallableUsedAsValue);
                }
                self.emit_ldloc(*index)?;
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.compile_expr(condition)?;
                let else_label = self.fresh_label("if_else");
                let end_label = self.fresh_label("if_end");
                self.assembler.brfalse_label(&else_label);
                self.compile_expr(then_expr)?;
                self.assembler.br_label(&end_label);
                self.assembler
                    .label(&else_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_expr(else_expr)?;
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                self.compile_expr(value)?;
                self.emit_stloc(*value_slot)?;
                let end_label = self.fresh_label("match_end");
                for (pattern, arm_expr) in arms {
                    let next_label = self.fresh_label("match_next");
                    self.compile_match_pattern_condition(*value_slot, pattern)?;
                    self.assembler.brfalse_label(&next_label);
                    self.compile_expr(arm_expr)?;
                    self.emit_stloc(*result_slot)?;
                    self.assembler.br_label(&end_label);
                    self.assembler
                        .label(&next_label)
                        .map_err(CompileError::Assembler)?;
                }
                self.compile_expr(default)?;
                self.emit_stloc(*result_slot)?;
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.emit_ldloc(*result_slot)?;
            }
            Expr::Block { stmts, expr } => {
                self.compile_stmts(stmts)?;
                self.compile_expr(expr)?;
            }
        }
        Ok(())
    }

    fn bind_closure_captures(&mut self, closure: &ClosureExpr) -> Result<(), CompileError> {
        for (source_index, captured_slot) in &closure.capture_copies {
            self.emit_ldloc(*source_index)?;
            self.emit_stloc(*captured_slot)?;
        }
        Ok(())
    }

    fn callable_binding_from_expr(
        &mut self,
        expr: &Expr,
    ) -> Result<Option<CallableBinding>, CompileError> {
        match expr {
            Expr::Closure(closure) => {
                self.bind_closure_captures(closure)?;
                Ok(Some(CallableBinding::Closure(closure.clone())))
            }
            Expr::FunctionRef(index) => Ok(Some(CallableBinding::Function(*index))),
            Expr::Var(index) => Ok(self.callable_bindings.get(index).cloned()),
            _ => Ok(None),
        }
    }

    fn assign_expr_to_slot(&mut self, slot: LocalSlot, expr: &Expr) -> Result<(), CompileError> {
        if let Some(callable) = self.callable_binding_from_expr(expr)? {
            self.callable_bindings.insert(slot, callable);
            return Ok(());
        }
        self.callable_bindings.remove(&slot);
        self.compile_expr(expr)?;
        self.emit_stloc(slot)?;
        Ok(())
    }

    fn compile_function_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        if let Some(function_impl) = self.function_impls.get(&index).cloned() {
            return self.compile_inline_function_call(index, &function_impl, args);
        }
        self.compile_direct_call(index, args)
    }

    fn compile_inline_function_call(
        &mut self,
        index: u16,
        function_impl: &FunctionImpl,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        if function_impl.param_slots.len() != args.len() {
            return Err(CompileError::CallableArityMismatch {
                expected: function_impl.param_slots.len(),
                got: args.len(),
            });
        }
        let callable_snapshot = self.callable_bindings.clone();
        for (arg, slot) in args.iter().zip(function_impl.param_slots.iter()) {
            self.assign_expr_to_slot(*slot, arg)?;
        }
        if self.inline_call_stack.contains(&index) {
            self.callable_bindings = callable_snapshot;
            return Err(CompileError::InlineFunctionRecursion(format!(
                "recursive RustScript function call detected for function index {}",
                index
            )));
        }
        self.inline_call_stack.push(index);
        let result = (|| -> Result<(), CompileError> {
            self.compile_stmts(&function_impl.body_stmts)?;
            self.compile_expr(&function_impl.body_expr)
        })();
        self.inline_call_stack.pop();
        self.callable_bindings = callable_snapshot;
        result
    }

    fn compile_inline_closure_call(
        &mut self,
        closure: &ClosureExpr,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        if closure.param_slots.len() != args.len() {
            return Err(CompileError::CallableArityMismatch {
                expected: closure.param_slots.len(),
                got: args.len(),
            });
        }
        let callable_snapshot = self.callable_bindings.clone();
        for (arg, slot) in args.iter().zip(closure.param_slots.iter()) {
            self.assign_expr_to_slot(*slot, arg)?;
        }
        let result = self.compile_expr(&closure.body);
        self.callable_bindings = callable_snapshot;
        result
    }

    fn compile_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        for arg in args {
            self.compile_expr(arg)?;
        }
        let argc = u8::try_from(args.len()).map_err(|_| CompileError::CallArityOverflow)?;
        if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            debug_assert_eq!(argc, builtin.arity());
            self.assembler.call(index, argc);
            return Ok(());
        }
        let remapped_index = self.call_index_remap.get(&index).copied().unwrap_or(index);
        self.assembler.call(remapped_index, argc);
        Ok(())
    }

    fn compile_callable_call(
        &mut self,
        callable: CallableBinding,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        match callable {
            CallableBinding::Closure(closure) => self.compile_inline_closure_call(&closure, args),
            CallableBinding::Function(index) => self.compile_function_call(index, args),
        }
    }

    fn compile_match_pattern_condition(
        &mut self,
        value_slot: LocalSlot,
        pattern: &MatchPattern,
    ) -> Result<(), CompileError> {
        match pattern {
            MatchPattern::Int(v) => {
                self.emit_ldloc(value_slot)?;
                self.assembler.push_const(Value::Int(*v));
                self.assembler.ceq();
            }
            MatchPattern::String(v) => {
                self.emit_ldloc(value_slot)?;
                self.assembler.push_const(Value::String(v.clone()));
                self.assembler.ceq();
            }
            MatchPattern::Null => {
                self.emit_ldloc(value_slot)?;
                self.assembler.push_const(Value::Null);
                self.assembler.ceq();
            }
            MatchPattern::Type(type_pattern) => {
                self.compile_match_type_pattern_condition(value_slot, type_pattern)?;
            }
        }
        Ok(())
    }

    fn compile_match_type_pattern_condition(
        &mut self,
        value_slot: LocalSlot,
        type_pattern: &MatchTypePattern,
    ) -> Result<(), CompileError> {
        match type_pattern {
            MatchTypePattern::Int => self.compile_type_name_equals(value_slot, "int")?,
            MatchTypePattern::Float => self.compile_type_name_equals(value_slot, "float")?,
            MatchTypePattern::Bool => self.compile_type_name_equals(value_slot, "bool")?,
            MatchTypePattern::String => self.compile_type_name_equals(value_slot, "string")?,
            MatchTypePattern::Array => self.compile_type_name_equals(value_slot, "array")?,
            MatchTypePattern::Map => self.compile_type_name_equals(value_slot, "map")?,
            MatchTypePattern::Number => {
                let number_fallback_label = self.fresh_label("match_type_number_fallback");
                let number_end_label = self.fresh_label("match_type_number_end");

                self.compile_type_name_equals(value_slot, "int")?;
                self.assembler.brfalse_label(&number_fallback_label);
                self.assembler.push_const(Value::Bool(true));
                self.assembler.br_label(&number_end_label);
                self.assembler
                    .label(&number_fallback_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_type_name_equals(value_slot, "float")?;
                self.assembler
                    .label(&number_end_label)
                    .map_err(CompileError::Assembler)?;
            }
        }
        Ok(())
    }

    fn compile_type_name_equals(
        &mut self,
        value_slot: LocalSlot,
        expected: &str,
    ) -> Result<(), CompileError> {
        self.emit_ldloc(value_slot)?;
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler
            .push_const(Value::String(expected.to_string()));
        self.assembler.ceq();
        Ok(())
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let label = format!("{prefix}_{}", self.next_label_id);
        self.next_label_id += 1;
        label
    }

    fn emit_ldloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.assembler.ldloc(local_slot_operand(slot)?);
        Ok(())
    }

    fn emit_stloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.assembler.stloc(local_slot_operand(slot)?);
        Ok(())
    }

    fn compile_string_concat_operand(&mut self, expr: &Expr) -> Result<(), CompileError> {
        if let Some(value) = eval_const_int_expr(expr) {
            self.assembler.push_const(Value::String(value.to_string()));
            return Ok(());
        }

        self.compile_expr(expr)?;
        self.lower_number_to_string_for_concat_top();
        Ok(())
    }

    fn lower_number_to_string_for_concat_top(&mut self) {
        let not_int_label = self.fresh_label("concat_not_int");
        let not_float_label = self.fresh_label("concat_not_float");
        let done_label = self.fresh_label("concat_value_done");

        self.assembler.dup();
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler.push_const(Value::String("int".to_string()));
        self.assembler.ceq();
        self.assembler.brfalse_label(&not_int_label);
        self.assembler
            .call(BuiltinFunction::ToString.call_index(), 1);
        self.assembler.br_label(&done_label);

        self.assembler
            .label(&not_int_label)
            .expect("compiler-generated label should be valid");
        self.assembler.dup();
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler
            .push_const(Value::String("float".to_string()));
        self.assembler.ceq();
        self.assembler.brfalse_label(&not_float_label);
        self.assembler
            .call(BuiltinFunction::ToString.call_index(), 1);
        self.assembler.br_label(&done_label);

        self.assembler
            .label(&not_float_label)
            .expect("compiler-generated label should be valid");
        self.assembler
            .label(&done_label)
            .expect("compiler-generated label should be valid");
    }
}

fn local_slot_operand(index: LocalSlot) -> Result<u8, CompileError> {
    u8::try_from(index).map_err(|_| CompileError::LocalSlotOverflow(index))
}

fn collect_named_local_debug_ranges(parsed: &FrontendIr) -> HashMap<String, LocalDebugRange> {
    let slot_ranges = collect_local_debug_ranges(&parsed.stmts, &parsed.function_impls);
    let mut named_ranges = HashMap::<String, LocalDebugRange>::new();
    for (name, slot) in &parsed.local_bindings {
        let Some(range) = slot_ranges.get(slot).copied() else {
            continue;
        };
        let entry = named_ranges.entry(name.clone()).or_default();
        entry.declared_line = merge_min_debug_line(entry.declared_line, range.declared_line);
        entry.last_line = merge_max_debug_line(entry.last_line, range.last_line);
    }
    named_ranges
}

fn collect_local_debug_ranges(
    stmts: &[Stmt],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<LocalSlot, LocalDebugRange> {
    let mut ranges = HashMap::<LocalSlot, LocalDebugRange>::new();
    for stmt in stmts {
        record_stmt_local_debug_ranges(stmt, &mut ranges);
    }
    for function_impl in function_impls.values() {
        for stmt in &function_impl.body_stmts {
            record_stmt_local_debug_ranges(stmt, &mut ranges);
        }
        let fallback_line = function_impl
            .body_stmts
            .last()
            .map(stmt_source_line)
            .unwrap_or(1);
        record_expr_local_debug_ranges(&function_impl.body_expr, fallback_line, &mut ranges);
    }
    ranges
}

fn record_stmt_local_debug_ranges(stmt: &Stmt, ranges: &mut HashMap<LocalSlot, LocalDebugRange>) {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Let { index, expr, line } => {
            note_local_decl(ranges, *index, *line);
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::Assign { index, expr, line } => {
            note_local_use(ranges, *index, *line);
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::ClosureLet { line, closure } => {
            for (source_slot, captured_slot) in &closure.capture_copies {
                note_local_use(ranges, *source_slot, *line);
                note_local_use(ranges, *captured_slot, *line);
            }
            record_expr_local_debug_ranges(&closure.body, *line, ranges);
        }
        Stmt::Expr { expr, line } => {
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        } => {
            record_expr_local_debug_ranges(condition, *line, ranges);
            for nested in then_branch {
                record_stmt_local_debug_ranges(nested, ranges);
            }
            for nested in else_branch {
                record_stmt_local_debug_ranges(nested, ranges);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            line,
        } => {
            record_stmt_local_debug_ranges(init, ranges);
            record_expr_local_debug_ranges(condition, *line, ranges);
            record_stmt_local_debug_ranges(post, ranges);
            for nested in body {
                record_stmt_local_debug_ranges(nested, ranges);
            }
        }
        Stmt::While {
            condition,
            body,
            line,
        } => {
            record_expr_local_debug_ranges(condition, *line, ranges);
            for nested in body {
                record_stmt_local_debug_ranges(nested, ranges);
            }
        }
    }
}

fn record_expr_local_debug_ranges(
    expr: &Expr,
    line: u32,
    ranges: &mut HashMap<LocalSlot, LocalDebugRange>,
) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_) => {}
        Expr::Var(index) => {
            note_local_use(ranges, *index, line);
        }
        Expr::Call(_, args) => {
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
        }
        Expr::LocalCall(index, args) => {
            note_local_use(ranges, *index, line);
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
        }
        Expr::Closure(closure) => {
            for (source_slot, captured_slot) in &closure.capture_copies {
                note_local_use(ranges, *source_slot, line);
                note_local_use(ranges, *captured_slot, line);
            }
            record_expr_local_debug_ranges(&closure.body, line, ranges);
        }
        Expr::ClosureCall(closure, args) => {
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                note_local_use(ranges, *source_slot, line);
                note_local_use(ranges, *captured_slot, line);
            }
            record_expr_local_debug_ranges(&closure.body, line, ranges);
        }
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs) => {
            record_expr_local_debug_ranges(lhs, line, ranges);
            record_expr_local_debug_ranges(rhs, line, ranges);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            record_expr_local_debug_ranges(inner, line, ranges);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            record_expr_local_debug_ranges(condition, line, ranges);
            record_expr_local_debug_ranges(then_expr, line, ranges);
            record_expr_local_debug_ranges(else_expr, line, ranges);
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            note_local_use(ranges, *value_slot, line);
            note_local_use(ranges, *result_slot, line);
            record_expr_local_debug_ranges(value, line, ranges);
            for (_, arm_expr) in arms {
                record_expr_local_debug_ranges(arm_expr, line, ranges);
            }
            record_expr_local_debug_ranges(default, line, ranges);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                record_stmt_local_debug_ranges(stmt, ranges);
            }
            record_expr_local_debug_ranges(expr, line, ranges);
        }
    }
}

fn note_local_decl(ranges: &mut HashMap<LocalSlot, LocalDebugRange>, slot: LocalSlot, line: u32) {
    let entry = ranges.entry(slot).or_default();
    entry.declared_line = Some(
        entry
            .declared_line
            .map_or(line, |current| current.min(line)),
    );
    entry.last_line = Some(entry.last_line.map_or(line, |current| current.max(line)));
}

fn note_local_use(ranges: &mut HashMap<LocalSlot, LocalDebugRange>, slot: LocalSlot, line: u32) {
    let entry = ranges.entry(slot).or_default();
    entry.last_line = Some(entry.last_line.map_or(line, |current| current.max(line)));
}

fn merge_min_debug_line(current: Option<u32>, incoming: Option<u32>) -> Option<u32> {
    match (current, incoming) {
        (Some(lhs), Some(rhs)) => Some(lhs.min(rhs)),
        (Some(lhs), None) => Some(lhs),
        (None, Some(rhs)) => Some(rhs),
        (None, None) => None,
    }
}

fn merge_max_debug_line(current: Option<u32>, incoming: Option<u32>) -> Option<u32> {
    match (current, incoming) {
        (Some(lhs), Some(rhs)) => Some(lhs.max(rhs)),
        (Some(lhs), None) => Some(lhs),
        (None, Some(rhs)) => Some(rhs),
        (None, None) => None,
    }
}

fn stmt_source_line(stmt: &Stmt) -> u32 {
    match stmt {
        Stmt::Noop { line }
        | Stmt::Let { line, .. }
        | Stmt::Assign { line, .. }
        | Stmt::ClosureLet { line, .. }
        | Stmt::FuncDecl { line, .. }
        | Stmt::Expr { line, .. }
        | Stmt::IfElse { line, .. }
        | Stmt::For { line, .. }
        | Stmt::While { line, .. }
        | Stmt::Break { line }
        | Stmt::Continue { line } => *line,
    }
}

fn shift_amount_for_power_of_two(value: i64) -> Option<u32> {
    if value <= 0 {
        return None;
    }
    let as_u64 = value as u64;
    if !as_u64.is_power_of_two() {
        return None;
    }
    Some(as_u64.trailing_zeros())
}

fn is_definitely_string_expr(expr: &Expr) -> bool {
    match expr {
        Expr::String(_) => true,
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            is_definitely_string_expr(inner)
        }
        Expr::Add(lhs, rhs) => {
            (is_definitely_string_expr(lhs) && is_definitely_string_expr(rhs))
                || (is_definitely_string_expr(lhs) && eval_const_int_expr(rhs).is_some())
                || (eval_const_int_expr(lhs).is_some() && is_definitely_string_expr(rhs))
        }
        _ => false,
    }
}

fn eval_const_int_expr(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Int(value) => Some(*value),
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            eval_const_int_expr(inner)
        }
        Expr::Neg(inner) => eval_const_int_expr(inner)?.checked_neg(),
        Expr::Add(lhs, rhs) => eval_const_int_expr(lhs)?.checked_add(eval_const_int_expr(rhs)?),
        Expr::Sub(lhs, rhs) => eval_const_int_expr(lhs)?.checked_sub(eval_const_int_expr(rhs)?),
        Expr::Mul(lhs, rhs) => eval_const_int_expr(lhs)?.checked_mul(eval_const_int_expr(rhs)?),
        Expr::Div(lhs, rhs) => {
            let rhs = eval_const_int_expr(rhs)?;
            if rhs == 0 {
                return None;
            }
            eval_const_int_expr(lhs)?.checked_div(rhs)
        }
        _ => None,
    }
}
