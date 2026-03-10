use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use self::source_map::{SourceMap, Span};
use crate::assembler::{Assembler, AssemblerError};
use crate::builtins::BuiltinFunction;
use crate::{HostImport, Program, TypeMap, Value, ValueType};

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
mod driver;
mod frontends;
pub mod ir;
mod lifetime;
mod linker;
mod opt;
mod parser;
mod source_loader;
use self::driver::normalize_import_spec;
pub use self::driver::{
    compile_source, compile_source_at_path_with_flavor_and_options, compile_source_file,
    compile_source_file_with_options, compile_source_for_repl, compile_source_for_repl_with_locals,
    compile_source_with_flavor, compile_source_with_flavor_and_options,
    lint_trailing_function_return_semicolons,
};
pub mod source_map;

use linker::merge_units;

pub use ir::{
    ClosureExpr, Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, MatchPattern,
    MatchTypePattern, Stmt,
};

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

    fn has_module_overrides(&self) -> bool {
        !self.module_path_overrides.is_empty() || !self.module_source_overrides.is_empty()
    }
}

pub struct Compiler {
    assembler: Assembler,
    next_label_id: u32,
    loop_stack: Vec<LoopContext>,
    function_impls: HashMap<u16, FunctionImpl>,
    host_import_return_types: HashMap<u16, opt::BoundType>,
    host_import_signatures: HashMap<u16, opt::HostCallableSignature>,
    call_index_remap: HashMap<u16, u16>,
    inline_call_stack: Vec<u16>,
    callable_bindings: HashMap<LocalSlot, CallableBinding>,
    enable_local_move_semantics: bool,
    type_state: opt::LocalTypeState,
    type_map: TypeMap,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CaptureBindingMode {
    Copy,
    Borrow,
    BorrowMut,
    Move,
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
            host_import_return_types: HashMap::new(),
            host_import_signatures: HashMap::new(),
            call_index_remap: HashMap::new(),
            inline_call_stack: Vec::new(),
            callable_bindings: HashMap::new(),
            enable_local_move_semantics: false,
            type_state: opt::LocalTypeState::default(),
            type_map: TypeMap::default(),
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

    pub(crate) fn set_host_import_return_types(
        &mut self,
        host_import_return_types: HashMap<u16, opt::BoundType>,
    ) {
        self.host_import_return_types = host_import_return_types;
    }

    pub(crate) fn set_host_import_signatures(
        &mut self,
        host_import_signatures: HashMap<u16, opt::HostCallableSignature>,
    ) {
        self.host_import_signatures = host_import_signatures;
    }

    pub fn set_call_index_remap(&mut self, call_index_remap: HashMap<u16, u16>) {
        self.call_index_remap = call_index_remap;
    }

    pub fn set_enable_local_move_semantics(&mut self, enable_local_move_semantics: bool) {
        self.enable_local_move_semantics = enable_local_move_semantics;
    }

    pub(crate) fn set_type_inference(&mut self, type_info: opt::TypeInferenceResult) {
        self.type_map.local_types = type_info.local_types;
    }

    pub fn compile_program(mut self, stmts: &[Stmt]) -> Result<Program, CompileError> {
        self.compile_stmts(stmts)?;
        self.assembler.ret();
        let mut program = self
            .assembler
            .finish_program()
            .map_err(CompileError::Assembler)?;
        program.type_map = Some(self.type_map);
        Ok(program)
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
            Stmt::FuncDecl { index, line, .. } => {
                self.assembler.mark_line(*line);
                self.bind_function_decl_captures(*index)?;
            }
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
                let type_state_snapshot = self.type_state.clone();
                self.assembler.mark_line(*line);
                let else_label = self.fresh_label("else");
                let end_label = self.fresh_label("endif");
                self.compile_scalar_expr(condition)?;
                self.assembler.brfalse_label(&else_label);
                self.compile_stmts(then_branch)?;
                let then_type_state = self.type_state.clone();
                self.assembler.br_label(&end_label);
                self.assembler
                    .label(&else_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot.clone();
                self.type_state = type_state_snapshot.clone();
                self.compile_stmts(else_branch)?;
                let else_type_state = self.type_state.clone();
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.callable_bindings = callable_snapshot;
                self.type_state
                    .merge_from_branches(&then_type_state, &else_type_state);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                let loop_entry_type_state = self.type_state.clone();
                self.assembler.mark_line(*line);
                self.compile_stmt(init)?;
                let start_label = self.fresh_label("for_start");
                let continue_label = self.fresh_label("for_continue");
                let end_label = self.fresh_label("for_end");
                self.assembler
                    .label(&start_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_scalar_expr(condition)?;
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
                self.type_state = self
                    .simulate_stmt_type_state(std::slice::from_ref(stmt), &loop_entry_type_state);
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
                let callable_snapshot = self.callable_bindings.clone();
                let loop_entry_type_state = self.type_state.clone();
                self.assembler.mark_line(*line);
                let start_label = self.fresh_label("while_start");
                let end_label = self.fresh_label("while_end");
                self.assembler
                    .label(&start_label)
                    .map_err(CompileError::Assembler)?;
                self.compile_scalar_expr(condition)?;
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
                self.type_state = self
                    .simulate_stmt_type_state(std::slice::from_ref(stmt), &loop_entry_type_state);
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
            Stmt::Drop { index, line } => {
                self.assembler.mark_line(*line);
                self.assign_expr_to_slot(*index, &Expr::Null)?;
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
                self.assembler.push_const(Value::string(value.clone()));
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
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                if is_definitely_string_expr(lhs) {
                    self.compile_scalar_expr(lhs)?;
                    self.compile_string_concat_operand(rhs)?;
                    self.record_operand_types(ValueType::String, ValueType::String);
                    self.assembler.add();
                    return Ok(());
                }
                if is_definitely_string_expr(rhs) {
                    self.compile_string_concat_operand(lhs)?;
                    self.compile_scalar_expr(rhs)?;
                    self.record_operand_types(ValueType::String, ValueType::String);
                    self.assembler.add();
                    return Ok(());
                }
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.add();
            }
            Expr::Sub(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.sub();
            }
            Expr::Mul(lhs, rhs) => {
                if let Expr::Int(value) = rhs.as_ref()
                    && let Some(shift) = shift_amount_for_power_of_two(*value)
                {
                    self.compile_scalar_expr(lhs)?;
                    self.assembler.push_const(Value::Int(shift as i64));
                    self.assembler.shl();
                } else if let Expr::Int(value) = lhs.as_ref()
                    && let Some(shift) = shift_amount_for_power_of_two(*value)
                {
                    self.compile_scalar_expr(rhs)?;
                    self.assembler.push_const(Value::Int(shift as i64));
                    self.assembler.shl();
                } else {
                    let lhs_ty = self.value_type_of_expr(lhs);
                    let rhs_ty = self.value_type_of_expr(rhs);
                    self.compile_scalar_expr(lhs)?;
                    self.compile_scalar_expr(rhs)?;
                    self.record_operand_types(lhs_ty, rhs_ty);
                    self.assembler.mul();
                }
            }
            Expr::Div(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.div();
            }
            Expr::Mod(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.modulo();
            }
            Expr::Neg(inner) => {
                let inner_ty = self.value_type_of_expr(inner);
                self.compile_scalar_expr(inner)?;
                self.record_unary_operand_type(inner_ty);
                self.assembler.neg();
            }
            Expr::Not(inner) => {
                self.compile_scalar_expr(inner)?;
                self.assembler.not();
            }
            Expr::ToOwned(inner) => {
                self.compile_scalar_expr(inner)?;
            }
            Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
                self.compile_scalar_expr(inner)?;
            }
            Expr::And(lhs, rhs) => {
                self.compile_short_circuit_and(lhs, rhs)?;
            }
            Expr::Or(lhs, rhs) => {
                self.compile_short_circuit_or(lhs, rhs)?;
            }
            Expr::Eq(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.ceq();
            }
            Expr::Lt(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.clt();
            }
            Expr::Gt(lhs, rhs) => {
                let lhs_ty = self.value_type_of_expr(lhs);
                let rhs_ty = self.value_type_of_expr(rhs);
                self.compile_scalar_expr(lhs)?;
                self.compile_scalar_expr(rhs)?;
                self.record_operand_types(lhs_ty, rhs_ty);
                self.assembler.cgt();
            }
            Expr::Var(index) => {
                if self.callable_bindings.contains_key(index) {
                    return Err(CompileError::CallableUsedAsValue);
                }
                self.emit_copy_ldloc(*index)?;
            }
            Expr::MoveVar(index) => {
                if self.callable_bindings.contains_key(index) {
                    return Err(CompileError::CallableUsedAsValue);
                }
                self.emit_move_ldloc(*index)?;
                self.type_state.set(*index, opt::BoundType::Null);
            }
            Expr::MoveField { root, key } => {
                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::string(key.clone()));
                self.assembler.call(BuiltinFunction::Get.call_index(), 2);

                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::string(key.clone()));
                self.assembler.push_const(Value::Null);
                self.assembler.call(BuiltinFunction::Set.call_index(), 3);
                self.emit_stloc(*root)?;
            }
            Expr::MoveIndex { root, index } => {
                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::Int(*index));
                self.assembler.call(BuiltinFunction::Get.call_index(), 2);

                self.emit_copy_ldloc(*root)?;
                self.assembler.push_const(Value::Int(*index));
                self.assembler.push_const(Value::Null);
                self.assembler.call(BuiltinFunction::Set.call_index(), 3);
                self.emit_stloc(*root)?;
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.compile_scalar_expr(condition)?;
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
                self.compile_scalar_expr(value)?;
                self.emit_stloc(*value_slot)?;
                let end_label = self.fresh_label("match_end");
                for (pattern, arm_expr) in arms {
                    let next_label = self.fresh_label("match_next");
                    self.compile_match_pattern_condition(*value_slot, pattern)?;
                    self.assembler.brfalse_label(&next_label);
                    self.compile_scalar_expr(arm_expr)?;
                    self.emit_stloc(*result_slot)?;
                    self.assembler.br_label(&end_label);
                    self.assembler
                        .label(&next_label)
                        .map_err(CompileError::Assembler)?;
                }
                self.compile_scalar_expr(default)?;
                self.emit_stloc(*result_slot)?;
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.emit_copy_ldloc(*result_slot)?;
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
            let capture_mode = self.closure_capture_mode_for_slot(closure, *captured_slot);
            self.bind_capture_copy(*source_index, *captured_slot, capture_mode)?;
        }
        Ok(())
    }

    fn bind_function_decl_captures(&mut self, index: u16) -> Result<(), CompileError> {
        let Some(function_impl) = self.function_impls.get(&index).cloned() else {
            return Ok(());
        };
        for (source_index, captured_slot) in &function_impl.capture_copies {
            let capture_mode = self.function_capture_mode_for_slot(&function_impl, *captured_slot);
            self.bind_capture_copy(*source_index, *captured_slot, capture_mode)?;
        }
        Ok(())
    }

    fn bind_capture_copy(
        &mut self,
        source_index: LocalSlot,
        captured_slot: LocalSlot,
        capture_mode: CaptureBindingMode,
    ) -> Result<(), CompileError> {
        let captured_type = self.type_state.get(source_index);
        if self.enable_local_move_semantics && capture_mode == CaptureBindingMode::Move {
            self.emit_move_ldloc(source_index)?;
            self.type_state.set(source_index, opt::BoundType::Null);
        } else {
            self.emit_copy_ldloc(source_index)?;
        }
        self.emit_stloc(captured_slot)?;
        self.type_state.set(captured_slot, captured_type);
        Ok(())
    }

    fn function_capture_mode_for_slot(
        &self,
        function_impl: &FunctionImpl,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        let mut mode = CaptureBindingMode::Copy;
        let mut seen = false;
        self.capture_mode_for_stmts(
            &function_impl.body_stmts,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        self.capture_mode_for_expr(
            &function_impl.body_expr,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        if seen { mode } else { CaptureBindingMode::Move }
    }

    fn closure_capture_mode_for_slot(
        &self,
        closure: &ClosureExpr,
        captured_slot: LocalSlot,
    ) -> CaptureBindingMode {
        let mut mode = CaptureBindingMode::Copy;
        let mut seen = false;
        self.capture_mode_for_expr(
            &closure.body,
            captured_slot,
            CaptureBindingMode::Move,
            &mut mode,
            &mut seen,
        );
        if seen { mode } else { CaptureBindingMode::Move }
    }

    fn capture_mode_for_stmts(
        &self,
        stmts: &[Stmt],
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        for stmt in stmts {
            self.capture_mode_for_stmt(stmt, captured_slot, context, mode, seen);
        }
    }

    fn capture_mode_for_stmt(
        &self,
        stmt: &Stmt,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        match stmt {
            Stmt::Noop { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::Drop { index, .. } => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
            }
            Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
            Stmt::ClosureLet { closure, .. } => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Stmt::Expr { expr, .. } => {
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(then_branch, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(else_branch, captured_slot, context, mode, seen);
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                ..
            } => {
                self.capture_mode_for_stmt(init, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmt(post, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(body, captured_slot, context, mode, seen);
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_stmts(body, captured_slot, context, mode, seen);
            }
        }
    }

    fn capture_mode_for_expr(
        &self,
        expr: &Expr,
        captured_slot: LocalSlot,
        context: CaptureBindingMode,
        mode: &mut CaptureBindingMode,
        seen: &mut bool,
    ) {
        match expr {
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::FunctionRef(_) => {}
            Expr::Var(index) => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
            }
            Expr::MoveVar(index) => {
                if *index == captured_slot {
                    *seen = true;
                    *mode = CaptureBindingMode::Move;
                }
            }
            Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
                if *root == captured_slot {
                    *seen = true;
                    *mode = CaptureBindingMode::Move;
                }
            }
            Expr::Call(_, args) | Expr::LocalCall(_, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, mode, seen);
                }
            }
            Expr::Closure(closure) => {
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.capture_mode_for_expr(arg, captured_slot, context, mode, seen);
                }
                for (nested_source_slot, nested_captured_slot) in &closure.capture_copies {
                    if *nested_source_slot == captured_slot {
                        self.capture_mode_for_expr(
                            &closure.body,
                            *nested_captured_slot,
                            CaptureBindingMode::Move,
                            mode,
                            seen,
                        );
                    }
                }
                self.capture_mode_for_expr(&closure.body, captured_slot, context, mode, seen);
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
                self.capture_mode_for_expr(lhs, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(rhs, captured_slot, context, mode, seen);
            }
            Expr::Neg(inner) | Expr::Not(inner) => {
                self.capture_mode_for_expr(inner, captured_slot, context, mode, seen);
            }
            Expr::ToOwned(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Copy,
                    mode,
                    seen,
                );
            }
            Expr::Borrow(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::Borrow,
                    mode,
                    seen,
                );
            }
            Expr::BorrowMut(inner) => {
                self.capture_mode_for_expr(
                    inner,
                    captured_slot,
                    CaptureBindingMode::BorrowMut,
                    mode,
                    seen,
                );
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                self.capture_mode_for_expr(condition, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(then_expr, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(else_expr, captured_slot, context, mode, seen);
            }
            Expr::Match {
                value_slot,
                result_slot,
                value,
                arms,
                default,
            } => {
                if *value_slot == captured_slot || *result_slot == captured_slot {
                    *seen = true;
                    *mode = (*mode).max(context);
                }
                self.capture_mode_for_expr(value, captured_slot, context, mode, seen);
                for (_, arm_expr) in arms {
                    self.capture_mode_for_expr(arm_expr, captured_slot, context, mode, seen);
                }
                self.capture_mode_for_expr(default, captured_slot, context, mode, seen);
            }
            Expr::Block { stmts, expr } => {
                self.capture_mode_for_stmts(stmts, captured_slot, context, mode, seen);
                self.capture_mode_for_expr(expr, captured_slot, context, mode, seen);
            }
        }
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
            self.callable_bindings.insert(slot, callable.clone());
            match callable {
                CallableBinding::Closure(closure) => self.type_state.bind_closure(slot, &closure),
                CallableBinding::Function(index) => self.type_state.bind_function(slot, index),
            }
            return Ok(());
        }
        let ty = self.infer_bound_type(expr);
        self.callable_bindings.remove(&slot);
        self.compile_scalar_expr(expr)?;
        self.emit_stloc(slot)?;
        self.type_state.set(slot, ty);
        Ok(())
    }

    fn compile_scalar_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        self.compile_expr(expr)
    }

    fn compile_short_circuit_and(&mut self, lhs: &Expr, rhs: &Expr) -> Result<(), CompileError> {
        let false_label = self.fresh_label("and_false");
        let end_label = self.fresh_label("and_end");
        self.compile_scalar_expr(lhs)?;
        self.assembler.brfalse_label(&false_label);
        self.compile_scalar_expr(rhs)?;
        self.assembler.br_label(&end_label);
        self.assembler
            .label(&false_label)
            .map_err(CompileError::Assembler)?;
        self.assembler.push_const(Value::Bool(false));
        self.assembler
            .label(&end_label)
            .map_err(CompileError::Assembler)?;
        Ok(())
    }

    fn compile_short_circuit_or(&mut self, lhs: &Expr, rhs: &Expr) -> Result<(), CompileError> {
        let rhs_label = self.fresh_label("or_rhs");
        let end_label = self.fresh_label("or_end");
        self.compile_scalar_expr(lhs)?;
        self.assembler.brfalse_label(&rhs_label);
        self.assembler.push_const(Value::Bool(true));
        self.assembler.br_label(&end_label);
        self.assembler
            .label(&rhs_label)
            .map_err(CompileError::Assembler)?;
        self.compile_scalar_expr(rhs)?;
        self.assembler
            .label(&end_label)
            .map_err(CompileError::Assembler)?;
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
        let frame_slots = collect_function_frame_slots(function_impl);
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
        result?;
        self.emit_inline_frame_clears(&frame_slots)?;
        Ok(())
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
        let frame_slots = collect_closure_frame_slots(closure);
        let callable_snapshot = self.callable_bindings.clone();
        for (arg, slot) in args.iter().zip(closure.param_slots.iter()) {
            self.assign_expr_to_slot(*slot, arg)?;
        }
        let result = self.compile_expr(&closure.body);
        self.callable_bindings = callable_snapshot;
        result?;
        self.emit_inline_frame_clears(&frame_slots)?;
        Ok(())
    }

    fn compile_direct_call(&mut self, index: u16, args: &[Expr]) -> Result<(), CompileError> {
        for arg in args {
            self.compile_scalar_expr(arg)?;
        }
        let argc = u8::try_from(args.len()).map_err(|_| CompileError::CallArityOverflow)?;
        if let Some(builtin) = BuiltinFunction::from_call_index(index) {
            debug_assert!(builtin.accepts_arity(argc));
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
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::Int(*v));
                self.assembler.ceq();
            }
            MatchPattern::String(v) => {
                self.emit_copy_ldloc(value_slot)?;
                self.assembler.push_const(Value::string(v.clone()));
                self.assembler.ceq();
            }
            MatchPattern::Null => {
                self.emit_copy_ldloc(value_slot)?;
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
        self.emit_copy_ldloc(value_slot)?;
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler
            .push_const(Value::string(expected.to_string()));
        self.assembler.ceq();
        Ok(())
    }

    fn infer_bound_type(&self, expr: &Expr) -> opt::BoundType {
        opt::infer_expr_type_with_function_impls_and_imports(
            expr,
            &self.type_state,
            &self.function_impls,
            &self.host_import_return_types,
            &self.host_import_signatures,
        )
    }

    fn simulate_stmt_type_state(
        &self,
        stmts: &[Stmt],
        initial_state: &opt::LocalTypeState,
    ) -> opt::LocalTypeState {
        let mut state = initial_state.clone();
        opt::apply_stmts_with_function_impls_and_imports(
            stmts,
            &mut state,
            &self.function_impls,
            &self.host_import_return_types,
            &self.host_import_signatures,
        );
        state
    }

    fn value_type_of_expr(&self, expr: &Expr) -> ValueType {
        ValueType::from(self.infer_bound_type(expr))
    }

    fn record_operand_types(&mut self, lhs: ValueType, rhs: ValueType) {
        if lhs == ValueType::Unknown || rhs == ValueType::Unknown {
            return;
        }
        self.type_map
            .operand_types
            .insert(self.assembler.position() as usize, (lhs, rhs));
    }

    fn record_unary_operand_type(&mut self, operand: ValueType) {
        if operand == ValueType::Unknown {
            return;
        }
        self.type_map.operand_types.insert(
            self.assembler.position() as usize,
            (operand, ValueType::Unknown),
        );
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let label = format!("{prefix}_{}", self.next_label_id);
        self.next_label_id += 1;
        label
    }

    fn emit_move_ldloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.assembler.ldloc(local_slot_operand(slot)?);
        Ok(())
    }

    fn emit_copy_ldloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.emit_move_ldloc(slot)?;
        self.assembler.dup();
        self.emit_stloc(slot)
    }

    fn emit_stloc(&mut self, slot: LocalSlot) -> Result<(), CompileError> {
        self.assembler.stloc(local_slot_operand(slot)?);
        Ok(())
    }

    fn emit_inline_frame_clears(&mut self, slots: &[LocalSlot]) -> Result<(), CompileError> {
        for slot in slots {
            self.assembler.push_const(Value::Null);
            self.emit_stloc(*slot)?;
            self.type_state.set(*slot, opt::BoundType::Null);
        }
        Ok(())
    }

    fn compile_string_concat_operand(&mut self, expr: &Expr) -> Result<(), CompileError> {
        if let Some(value) = eval_const_int_expr(expr) {
            self.assembler.push_const(Value::string(value.to_string()));
            return Ok(());
        }

        self.compile_scalar_expr(expr)?;
        self.lower_number_to_string_for_concat_top();
        Ok(())
    }

    fn lower_number_to_string_for_concat_top(&mut self) {
        let not_int_label = self.fresh_label("concat_not_int");
        let not_float_label = self.fresh_label("concat_not_float");
        let done_label = self.fresh_label("concat_value_done");

        self.assembler.dup();
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler.push_const(Value::string("int"));
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
        self.assembler.push_const(Value::string("float"));
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

fn collect_function_frame_slots(function_impl: &FunctionImpl) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    for slot in &function_impl.param_slots {
        slots.insert(*slot);
    }
    for stmt in &function_impl.body_stmts {
        collect_stmt_slot_footprint(stmt, &mut slots);
    }
    collect_expr_slot_footprint(&function_impl.body_expr, &mut slots);
    for (_, captured_slot) in &function_impl.capture_copies {
        slots.remove(captured_slot);
    }
    let mut out = slots.into_iter().collect::<Vec<_>>();
    out.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
    out
}

fn collect_closure_frame_slots(closure: &ClosureExpr) -> Vec<LocalSlot> {
    let mut slots = BTreeSet::new();
    for slot in &closure.param_slots {
        slots.insert(*slot);
    }
    collect_expr_slot_footprint(&closure.body, &mut slots);
    for (_, captured_slot) in &closure.capture_copies {
        slots.remove(captured_slot);
    }
    let mut out = slots.into_iter().collect::<Vec<_>>();
    out.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
    out
}

fn collect_stmt_slot_footprint(stmt: &Stmt, slots: &mut BTreeSet<LocalSlot>) {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, .. } => {
            slots.insert(*index);
        }
        Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
            slots.insert(*index);
            collect_expr_slot_footprint(expr, slots);
        }
        Stmt::ClosureLet { closure, .. } => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Stmt::Expr { expr, .. } => collect_expr_slot_footprint(expr, slots),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_expr_slot_footprint(condition, slots);
            for stmt in then_branch {
                collect_stmt_slot_footprint(stmt, slots);
            }
            for stmt in else_branch {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_stmt_slot_footprint(init, slots);
            collect_expr_slot_footprint(condition, slots);
            collect_stmt_slot_footprint(post, slots);
            for stmt in body {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_expr_slot_footprint(condition, slots);
            for stmt in body {
                collect_stmt_slot_footprint(stmt, slots);
            }
        }
    }
}

fn collect_expr_slot_footprint(expr: &Expr, slots: &mut BTreeSet<LocalSlot>) {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::FunctionRef(_) => {}
        Expr::Var(index) | Expr::MoveVar(index) => {
            slots.insert(*index);
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            slots.insert(*root);
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
        }
        Expr::LocalCall(index, args) => {
            slots.insert(*index);
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
        }
        Expr::Closure(closure) => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            collect_expr_slot_footprint(&closure.body, slots);
        }
        Expr::ClosureCall(closure, args) => {
            for slot in &closure.param_slots {
                slots.insert(*slot);
            }
            for (source_slot, captured_slot) in &closure.capture_copies {
                slots.insert(*source_slot);
                slots.insert(*captured_slot);
            }
            for arg in args {
                collect_expr_slot_footprint(arg, slots);
            }
            collect_expr_slot_footprint(&closure.body, slots);
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
            collect_expr_slot_footprint(lhs, slots);
            collect_expr_slot_footprint(rhs, slots);
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => collect_expr_slot_footprint(inner, slots),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_expr_slot_footprint(condition, slots);
            collect_expr_slot_footprint(then_expr, slots);
            collect_expr_slot_footprint(else_expr, slots);
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            slots.insert(*value_slot);
            slots.insert(*result_slot);
            collect_expr_slot_footprint(value, slots);
            for (_, arm_expr) in arms {
                collect_expr_slot_footprint(arm_expr, slots);
            }
            collect_expr_slot_footprint(default, slots);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                collect_stmt_slot_footprint(stmt, slots);
            }
            collect_expr_slot_footprint(expr, slots);
        }
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
