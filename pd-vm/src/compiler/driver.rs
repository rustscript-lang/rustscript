use super::*;
#[cfg(feature = "runtime")]
use crate::vm::Vm;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LocalDebugRange {
    pub(super) declared_line: Option<u32>,
    pub(super) last_line: Option<u32>,
}

impl CompiledProgram {
    #[cfg(feature = "runtime")]
    pub fn into_vm(self) -> Vm {
        Vm::new(self.program)
    }
}

pub(super) fn normalize_import_spec(spec: String) -> String {
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
        Stmt::Drop { index, line } => {
            note_local_use(ranges, *index, *line);
        }
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
        Expr::Var(index) | Expr::MoveVar(index) => {
            note_local_use(ranges, *index, line);
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            note_local_use(ranges, *root, line);
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
        | Stmt::Continue { line }
        | Stmt::Drop { line, .. } => *line,
    }
}

fn is_compiler_primitive_import(name: &str) -> bool {
    name.starts_with("__prim_")
}

fn compile_parsed_output(
    source: String,
    parsed: FrontendIr,
    behavior: CompileBehavior,
    enable_local_move_semantics: bool,
) -> Result<CompiledProgram, SourceError> {
    compile_parsed_output_with_entry_locals(
        source,
        parsed,
        &[],
        behavior,
        enable_local_move_semantics,
    )
}

fn compile_parsed_output_with_entry_locals(
    source: String,
    parsed: FrontendIr,
    entry_definite_locals: &[LocalSlot],
    behavior: CompileBehavior,
    enable_local_move_semantics: bool,
) -> Result<CompiledProgram, SourceError> {
    // Normal compilation passes no entry locals. The REPL uses this hook to treat
    // carried-over locals from prior entries as already available at snippet start.
    let local_debug_ranges = collect_named_local_debug_ranges(&parsed);
    let parsed = opt::legalize_builtins_and_bind_types(parsed);
    opt::validate_if_else_type_consistency(&parsed).map_err(SourceError::Compile)?;
    let parsed = lifetime::enforce_local_availability_with_entry_locals(
        parsed,
        entry_definite_locals,
        behavior.clear_dead_locals,
        enable_local_move_semantics,
    )
    .map_err(SourceError::Parse)?;
    let type_info = opt::infer_types(&parsed);
    let FrontendIr {
        stmts,
        locals,
        local_bindings,
        functions,
        function_impls,
        ..
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
    let host_import_return_types = functions
        .iter()
        .filter(|func| !function_impls.contains_key(&func.index))
        .map(|func| (func.index, opt::BoundType::from(func.return_type)))
        .collect::<HashMap<_, _>>();
    let host_import_signatures = opt::build_host_import_signatures(&functions, &function_impls);

    let mut compiler = Compiler::new();
    compiler.set_type_inference(type_info);
    compiler.set_source(source);
    compiler.set_function_impls(function_impls);
    compiler.set_host_import_return_types(host_import_return_types);
    compiler.set_host_import_signatures(host_import_signatures);
    compiler.set_call_index_remap(call_index_remap);
    compiler.set_enable_local_move_semantics(enable_local_move_semantics);
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
            return_type: func.return_type,
        })
        .collect();
    Ok(CompiledProgram {
        program,
        locals,
        functions: visible_runtime_import_functions,
    })
}

pub fn compile_source(source: &str) -> Result<CompiledProgram, SourceError> {
    compile_source_with_flavor(source, SourceFlavor::RustScript)
}

pub fn lint_trailing_function_return_semicolons(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<ParseError>, ParseError> {
    let Some(dialect) = frontends::parser_dialect_for_flavor(flavor) else {
        return Ok(Vec::new());
    };
    parser::lint_trailing_function_return_semicolons(source, 0, dialect)
}

pub fn compile_source_for_repl(source: &str) -> Result<CompiledProgram, SourceError> {
    compile_source_for_repl_with_locals(source, &[]).map(|compiled| compiled.compiled)
}

pub fn compile_source_for_repl_with_locals(
    source: &str,
    predefined_locals: &[ReplLocalBinding],
) -> Result<CompiledReplProgram, SourceError> {
    let source_owned = source.to_string();
    let predefined_locals = predefined_locals.to_vec();
    run_with_compiler_stack(move || {
        compile_source_for_repl_with_locals_impl(&source_owned, &predefined_locals)
    })
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

pub fn compile_source_at_path_with_flavor_and_options(
    path: impl AsRef<Path>,
    source: &str,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    let path = path.as_ref().to_path_buf();
    let source_owned = source.to_string();
    run_with_compiler_stack(move || {
        compile_source_at_path_with_flavor_and_options_impl(&path, &source_owned, flavor, &options)
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

fn compile_source_for_repl_with_locals_impl(
    source: &str,
    predefined_locals: &[ReplLocalBinding],
) -> Result<CompiledReplProgram, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    // REPL parsing/compiler entry state is separate from normal program compilation so
    // persisted locals do not leak into the generic frontend or IR surface.
    let parsed =
        frontends::parse_rustscript_repl_source(source, predefined_locals).map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
        })?;
    let compiled = match compile_parsed_output_with_entry_locals(
        source.to_string(),
        parsed.ir,
        &parsed.entry_definite_locals,
        CompileBehavior::REPL,
        true,
    ) {
        Err(SourceError::Parse(err)) => Err(SourceError::Parse(
            err.with_line_span_from_source(&source_map, source_id),
        )),
        other => other,
    }?;
    Ok(CompiledReplProgram {
        compiled,
        bindings: parsed.bindings,
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
    match compile_parsed_output(
        source.to_string(),
        parsed,
        behavior,
        matches!(flavor, SourceFlavor::RustScript),
    ) {
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
    compile_parsed_output(
        source.to_string(),
        merged,
        CompileBehavior::DEFAULT,
        matches!(flavor, SourceFlavor::RustScript),
    )
    .map_err(SourcePathError::Source)
}

fn compile_source_at_path_with_flavor_and_options_impl(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    let (_root_parse_source, units) =
        source_loader::load_units_for_source_file(path, flavor, source, options)?;
    let merged = merge_units(units)?;
    compile_parsed_output(
        source.to_string(),
        merged,
        CompileBehavior::DEFAULT,
        matches!(flavor, SourceFlavor::RustScript),
    )
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
    compile_parsed_output(
        source_raw,
        merged,
        CompileBehavior::DEFAULT,
        matches!(flavor, SourceFlavor::RustScript),
    )
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
