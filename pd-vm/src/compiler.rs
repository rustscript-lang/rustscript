use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    BreakOutsideLoop,
    ContinueOutsideLoop,
    InlineFunctionRecursion(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
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

mod frontends;
mod linker;
mod parser;
mod source_loader;

use linker::{MergedFrontendOutput, merge_units};

#[derive(Clone, Debug)]
pub struct ClosureExpr {
    pub param_slots: Vec<u8>,
    pub capture_copies: Vec<(u8, u8)>,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug)]
pub enum MatchPattern {
    Int(i64),
    String(String),
}

#[derive(Clone, Debug)]
pub enum Expr {
    Null,
    Int(i64),
    Bool(bool),
    String(String),
    Call(u16, Vec<Expr>),
    Closure(ClosureExpr),
    ClosureCall(ClosureExpr, Vec<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Var(u8),
    IfElse {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    Match {
        value_slot: u8,
        result_slot: u8,
        value: Box<Expr>,
        arms: Vec<(MatchPattern, Expr)>,
        default: Box<Expr>,
    },
    Block {
        stmts: Vec<Stmt>,
        expr: Box<Expr>,
    },
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Noop {
        line: u32,
    },
    Let {
        index: u8,
        expr: Expr,
        line: u32,
    },
    Assign {
        index: u8,
        expr: Expr,
        line: u32,
    },
    ClosureLet {
        line: u32,
        closure: ClosureExpr,
    },
    FuncDecl {
        name: String,
        arity: u8,
        args: Vec<String>,
        exported: bool,
        line: u32,
    },
    Expr {
        expr: Expr,
        line: u32,
    },
    IfElse {
        condition: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Vec<Stmt>,
        line: u32,
    },
    For {
        init: Box<Stmt>,
        condition: Expr,
        post: Box<Stmt>,
        body: Vec<Stmt>,
        line: u32,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    Break {
        line: u32,
    },
    Continue {
        line: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDecl {
    pub name: String,
    pub arity: u8,
    pub index: u16,
    pub args: Vec<String>,
    pub exported: bool,
}

#[derive(Clone, Debug)]
pub struct FunctionImpl {
    pub param_slots: Vec<u8>,
    pub body_stmts: Vec<Stmt>,
    pub body_expr: Expr,
}

pub struct CompiledProgram {
    pub program: Program,
    pub locals: usize,
    pub functions: Vec<FunctionDecl>,
}

impl CompiledProgram {
    #[cfg(feature = "runtime")]
    pub fn into_vm(self) -> Vm {
        Vm::with_locals(self.program, self.locals)
    }
}

fn compile_parsed_output(parsed: MergedFrontendOutput) -> Result<CompiledProgram, SourceError> {
    let MergedFrontendOutput {
        source,
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
        compiler.add_local_debug(name, index);
    }
    let mut program = compiler
        .compile_program(&stmts)
        .map_err(SourceError::Compile)?;
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

pub fn compile_source_with_flavor(
    source: &str,
    flavor: SourceFlavor,
) -> Result<CompiledProgram, SourceError> {
    let parsed = frontends::parse_source(source, flavor).map_err(SourceError::Parse)?;
    compile_parsed_output(MergedFrontendOutput {
        source: source.to_string(),
        stmts: parsed.stmts,
        locals: parsed.locals,
        local_bindings: parsed.local_bindings,
        functions: parsed.functions,
        function_impls: parsed.function_impls,
    })
}

pub fn compile_source_file(path: impl AsRef<Path>) -> Result<CompiledProgram, SourcePathError> {
    let path = path.as_ref();
    let flavor = SourceFlavor::from_path(path)?;
    let source_raw = std::fs::read_to_string(path)?;
    let (root_source, units) =
        source_loader::load_units_for_source_file(path, flavor, &source_raw)?;
    let merged = merge_units(root_source, units)?;
    compile_parsed_output(merged).map_err(SourcePathError::Source)
}

pub struct Compiler {
    assembler: Assembler,
    next_label_id: u32,
    loop_stack: Vec<LoopContext>,
    function_impls: HashMap<u16, FunctionImpl>,
    call_index_remap: HashMap<u16, u16>,
    inline_call_stack: Vec<u16>,
}

struct LoopContext {
    continue_label: String,
    break_label: String,
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
        }
    }

    pub fn set_source(&mut self, source: String) {
        self.assembler.set_source(source);
    }

    pub fn add_function_debug(&mut self, func: &FunctionDecl) {
        self.assembler
            .add_function(func.name.clone(), func.args.clone());
    }

    pub fn add_local_debug(&mut self, name: String, index: u8) {
        self.assembler.add_local(name, index);
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
                self.compile_expr(expr)?;
                self.assembler.stloc(*index);
            }
            Stmt::Assign { index, expr, line } => {
                self.assembler.mark_line(*line);
                self.compile_expr(expr)?;
                self.assembler.stloc(*index);
            }
            Stmt::ClosureLet { line, closure } => {
                self.assembler.mark_line(*line);
                for (source_index, captured_slot) in &closure.capture_copies {
                    self.assembler.ldloc(*source_index);
                    self.assembler.stloc(*captured_slot);
                }
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
                self.compile_stmts(else_branch)?;
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
            } => {
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
            }
            Stmt::While {
                condition,
                body,
                line,
            } => {
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
            Expr::Bool(value) => {
                self.assembler.push_const(Value::Bool(*value));
            }
            Expr::String(value) => {
                self.assembler.push_const(Value::String(value.clone()));
            }
            Expr::Call(index, args) => {
                if let Some(function_impl) = self.function_impls.get(index).cloned() {
                    for (arg, slot) in args.iter().zip(function_impl.param_slots.iter()) {
                        self.compile_expr(arg)?;
                        self.assembler.stloc(*slot);
                    }
                    if self.inline_call_stack.contains(index) {
                        return Err(CompileError::InlineFunctionRecursion(format!(
                            "recursive RustScript function call detected for function index {}",
                            index
                        )));
                    }
                    self.inline_call_stack.push(*index);
                    let result = (|| -> Result<(), CompileError> {
                        self.compile_stmts(&function_impl.body_stmts)?;
                        self.compile_expr(&function_impl.body_expr)
                    })();
                    self.inline_call_stack.pop();
                    result?;
                    return Ok(());
                }
                for arg in args {
                    self.compile_expr(arg)?;
                }
                let argc = u8::try_from(args.len()).map_err(|_| CompileError::CallArityOverflow)?;
                if let Some(builtin) = BuiltinFunction::from_call_index(*index) {
                    debug_assert_eq!(argc, builtin.arity());
                    self.assembler.call(*index, argc);
                    return Ok(());
                }
                let remapped_index = self.call_index_remap.get(index).copied().unwrap_or(*index);
                self.assembler.call(remapped_index, argc);
            }
            Expr::Closure(_) => {
                return Err(CompileError::ClosureUsedAsValue);
            }
            Expr::ClosureCall(closure, args) => {
                for (arg, slot) in args.iter().zip(closure.param_slots.iter()) {
                    self.compile_expr(arg)?;
                    self.assembler.stloc(*slot);
                }
                self.compile_expr(&closure.body)?;
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
            Expr::Neg(inner) => {
                self.compile_expr(inner)?;
                self.assembler.neg();
            }
            Expr::Not(inner) => {
                self.compile_expr(inner)?;
                self.assembler.push_const(Value::Bool(false));
                self.assembler.ceq();
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
                self.assembler.ldloc(*index);
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
                self.assembler.stloc(*value_slot);
                let end_label = self.fresh_label("match_end");
                for (pattern, arm_expr) in arms {
                    let next_label = self.fresh_label("match_next");
                    self.assembler.ldloc(*value_slot);
                    match pattern {
                        MatchPattern::Int(v) => {
                            self.assembler.push_const(Value::Int(*v));
                        }
                        MatchPattern::String(v) => {
                            self.assembler.push_const(Value::String(v.clone()));
                        }
                    }
                    self.assembler.ceq();
                    self.assembler.brfalse_label(&next_label);
                    self.compile_expr(arm_expr)?;
                    self.assembler.stloc(*result_slot);
                    self.assembler.br_label(&end_label);
                    self.assembler
                        .label(&next_label)
                        .map_err(CompileError::Assembler)?;
                }
                self.compile_expr(default)?;
                self.assembler.stloc(*result_slot);
                self.assembler
                    .label(&end_label)
                    .map_err(CompileError::Assembler)?;
                self.assembler.ldloc(*result_slot);
            }
            Expr::Block { stmts, expr } => {
                self.compile_stmts(stmts)?;
                self.compile_expr(expr)?;
            }
        }
        Ok(())
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let label = format!("{prefix}_{}", self.next_label_id);
        self.next_label_id += 1;
        label
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
        self.assembler.call(BuiltinFunction::ToString.call_index(), 1);
        self.assembler.br_label(&done_label);

        self.assembler
            .label(&not_int_label)
            .expect("compiler-generated label should be valid");
        self.assembler.dup();
        self.assembler.call(BuiltinFunction::TypeOf.call_index(), 1);
        self.assembler.push_const(Value::String("float".to_string()));
        self.assembler.ceq();
        self.assembler.brfalse_label(&not_float_label);
        self.assembler.call(BuiltinFunction::ToString.call_index(), 1);
        self.assembler.br_label(&done_label);

        self.assembler
            .label(&not_float_label)
            .expect("compiler-generated label should be valid");
        self.assembler
            .label(&done_label)
            .expect("compiler-generated label should be valid");
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
