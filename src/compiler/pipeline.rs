use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::HostImport;

use super::ReplLocalState;
use super::codegen::Compiler;
use super::frontends;
use super::ir::{Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt, TypeSchema};
use super::linker::{ParsedUnit, merge_units};
use super::modules::ModuleGraph;
use super::source_loader::load_units_for_source_file;
use super::source_map::SourceMap;
use super::{
    CompileError, CompileSourceFileOptions, CompiledProgram, CompiledReplProgram, ParseError,
    ReplLocalBinding, SourceError, SourceFlavor, SourcePathError, TypingMode, lifetime,
    materialization, parser, typing,
};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LocalDebugRange {
    pub(super) declared_line: Option<u32>,
    pub(super) last_line: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownInferredLocal {
    pub name: String,
    pub line: usize,
    pub span: Option<crate::compiler::source_map::Span>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferredLocalTypeHint {
    pub name: String,
    pub inferred_type: String,
    pub declared_line: Option<u32>,
    pub last_line: Option<u32>,
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
        let body_expr_line = if function_impl.body_expr_line > 0 {
            function_impl.body_expr_line
        } else {
            fallback_line
        };
        record_expr_local_debug_ranges(&function_impl.body_expr, body_expr_line, &mut ranges);
    }
    ranges
}

fn record_stmt_local_debug_ranges(stmt: &Stmt, ranges: &mut HashMap<LocalSlot, LocalDebugRange>) {
    match stmt {
        Stmt::Noop { .. } | Stmt::FuncDecl { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, line } => {
            note_local_use(ranges, *index, *line);
        }
        Stmt::Let {
            index, expr, line, ..
        } => {
            note_local_decl(ranges, *index, *line);
            record_expr_local_debug_ranges(expr, *line, ranges);
        }
        Stmt::Assign {
            index, expr, line, ..
        } => {
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
        | Expr::Bytes(_)
        | Expr::String(_)
        | Expr::FunctionRef(..)
        | Expr::ModuleFunctionRef(..)
        | Expr::UnresolvedFunctionRef { .. } => {}
        Expr::Var(index) | Expr::MoveVar(index) => {
            note_local_use(ranges, *index, line);
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            note_local_use(ranges, *root, line);
        }
        Expr::OptionalGet {
            container,
            key,
            container_slot,
            key_slot,
        } => {
            note_local_use(ranges, *container_slot, line);
            note_local_use(ranges, *key_slot, line);
            record_expr_local_debug_ranges(container, line, ranges);
            record_expr_local_debug_ranges(key, line, ranges);
        }
        Expr::OptionUnwrapOr {
            value,
            value_slot,
            fallback,
        } => {
            note_local_use(ranges, *value_slot, line);
            record_expr_local_debug_ranges(value, line, ranges);
            record_expr_local_debug_ranges(fallback, line, ranges);
        }
        Expr::Call(_, _, args) | Expr::ModuleCall(_, _, args) => {
            for arg in args {
                record_expr_local_debug_ranges(arg, line, ranges);
            }
        }
        Expr::LocalCall(index, _, args) => {
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
            for (pattern, arm_expr) in arms {
                if let Some(binding_slot) = pattern.binding_slot() {
                    note_local_use(ranges, binding_slot, line);
                }
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
    typing_mode: TypingMode,
    enable_local_move_semantics: bool,
) -> Result<CompiledProgram, SourceError> {
    compile_parsed_output_with_entry_locals(
        source,
        parsed,
        &[],
        &[],
        behavior,
        typing_mode,
        enable_local_move_semantics,
    )
}

fn compile_parsed_output_with_entry_locals(
    source: String,
    parsed: FrontendIr,
    entry_locals: &[lifetime::EntryLocalAvailability],
    entry_local_types: &[typing::EntryLocalType],
    behavior: CompileBehavior,
    typing_mode: TypingMode,
    enable_local_move_semantics: bool,
) -> Result<CompiledProgram, SourceError> {
    // Normal compilation passes no entry locals. The REPL uses this hook to treat
    // carried-over locals from prior entries as already available at snippet start.
    if typing_mode.is_strict() {
        reject_strict_unknown_annotations(&parsed).map_err(SourceError::Parse)?;
    }
    let local_debug_ranges = collect_named_local_debug_ranges(&parsed);
    let parsed = typing::legalize_builtins_and_bind_types(parsed, typing_mode, entry_local_types);
    typing::validate_if_else_type_consistency(&parsed, typing_mode, entry_local_types)
        .map_err(SourceError::Compile)?;
    if typing_mode.is_strict() {
        let strict_type_info = typing::infer_types(&parsed, typing_mode, entry_local_types);
        enforce_strict_rustscript_type_resolution(&parsed, &strict_type_info)
            .map_err(SourceError::Compile)?;
    }
    let parsed = lifetime::enforce_local_availability_with_entry_locals(
        parsed,
        entry_locals,
        behavior.clear_dead_locals,
        enable_local_move_semantics,
    )
    .map_err(SourceError::Parse)?;
    // Classify named callable materialization on the final merged IR
    // (post-lifetime, so capture metadata and rewritten uses are
    // authoritative). Codegen consumes `requires_callable_slot` to omit
    // hidden callable slots for direct-only functions.
    //
    // The classification runs BEFORE local-slot compaction: it tracks
    // named-function values through slot flows, and merged physical slots
    // would collapse distinct flows into one slot, producing spurious
    // dynamic-target facts. Pre-compaction slots are the true frame-relative
    // value identities, so the classification is strictly more precise on
    // the unallocated IR.
    let callable_use_facts = materialization::classify_named_callables(&parsed);
    let parsed = lifetime::allocate_local_slots(parsed).map_err(SourceError::Parse)?;
    let type_info = typing::infer_types(&parsed, typing_mode, entry_local_types);
    let FrontendIr {
        stmts,
        locals,
        local_bindings,
        struct_schemas,
        functions,
        function_impls,
        ..
    } = parsed;
    let function_decls = functions
        .iter()
        .cloned()
        .map(|decl| (decl.index, decl))
        .collect::<HashMap<_, _>>();

    // Milestone-5 observation for the crate's unit tests: capture the
    // classification keyed by the merged flat function identity before the
    // facts move into the Compiler, so tests observe exactly what the
    // compiler received. Compiled into unit-test builds only; never part of
    // the public API.
    #[cfg(test)]
    let mut callable_use_observations = functions
        .iter()
        .filter_map(|decl| {
            callable_use_facts.get(&decl.index).map(|facts| {
                materialization::CallableUseObservation {
                    function_index: decl.index,
                    name: decl.name.clone(),
                    facts: *facts,
                }
            })
        })
        .collect::<Vec<_>>();
    #[cfg(test)]
    callable_use_observations.sort_unstable_by_key(|observation| observation.function_index);

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
        .map(|func| (func.index, typing::BoundType::from(func.return_type)))
        .collect::<HashMap<_, _>>();
    let host_import_signatures = typing::build_host_import_signatures(&functions, &function_impls);

    let mut compiler = Compiler::new();
    compiler.set_type_inference(type_info);
    compiler.set_typing_mode(typing_mode);
    compiler.set_source(source);
    compiler.set_root_local_count(locals);
    compiler.set_function_decls(function_decls);
    compiler.set_function_impls(function_impls);
    compiler.set_callable_use_facts(callable_use_facts);
    compiler.set_struct_schemas(struct_schemas);
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
    program.local_count = program.local_count.max(locals);
    program.imports = runtime_import_functions
        .iter()
        .map(|func| HostImport {
            name: func.name.clone(),
            arity: func.arity,
            return_type: func.return_type,
        })
        .collect();
    let runtime_locals = program.local_count;
    Ok(CompiledProgram {
        program,
        locals: runtime_locals,
        functions: visible_runtime_import_functions,
        #[cfg(test)]
        callable_use_facts: callable_use_observations,
    })
}

#[derive(Clone, Debug)]
struct StrictSlotSite {
    name: String,
    kind: &'static str,
    line: Option<u32>,
    source_name: Option<String>,
}

fn reject_strict_unknown_annotations(parsed: &FrontendIr) -> Result<(), ParseError> {
    let Some(span) = parsed.unknown_type_spans.first().copied() else {
        return Ok(());
    };
    Err(ParseError {
        line: 1,
        message:
            "RustScript requires concrete compile-time types; 'unknown' annotations are not allowed"
                .to_string(),
        span: Some(span),
        code: Some("E_STRICT_UNKNOWN_TYPE".to_string()),
    })
}

fn enforce_strict_rustscript_type_resolution(
    parsed: &FrontendIr,
    type_info: &typing::TypeInferenceResult,
) -> Result<(), CompileError> {
    for schema in parsed.struct_schemas.values() {
        if schema_is_fully_known(&schema.body_schema) {
            continue;
        }
        return Err(CompileError::StrictTypingRequired {
            line: None,
            source_name: None,
            detail: format!(
                "struct '{}' contains non-concrete field types; RustScript requires concrete schemas",
                schema.name
            ),
        });
    }

    let function_decl_lines = collect_function_decl_lines(&parsed.stmts);
    for decl in &parsed.functions {
        if let Some(schema) = decl.return_schema.as_ref()
            && !schema_is_fully_known(schema)
        {
            return Err(CompileError::StrictTypingRequired {
                line: function_decl_lines.get(&decl.index).copied(),
                source_name: parsed.function_sources.get(&decl.index).cloned(),
                detail: format!(
                    "function '{}' uses a non-concrete return schema; RustScript requires concrete return types",
                    decl.name
                ),
            });
        }
    }

    for (slot, site) in collect_strict_slot_sites(parsed) {
        if slot_is_fully_typed(slot, type_info) {
            continue;
        }
        return Err(CompileError::StrictTypingRequired {
            line: site.line,
            source_name: site.source_name,
            detail: format!(
                "{} '{}' does not resolve to a concrete compile-time type in RustScript",
                site.kind, site.name
            ),
        });
    }

    Ok(())
}

fn slot_is_fully_typed(slot: LocalSlot, type_info: &typing::TypeInferenceResult) -> bool {
    let slot_index = usize::from(slot);
    if type_info
        .callable_slots
        .get(slot_index)
        .copied()
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(schema) = type_info
        .local_schemas
        .get(slot_index)
        .and_then(|schema| schema.as_ref())
    {
        return schema_is_fully_known(schema);
    }
    type_info.local_types.get(slot_index).copied() != Some(crate::ValueType::Unknown)
}

fn schema_is_fully_known(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Unknown => false,
        TypeSchema::Null
        | TypeSchema::Int
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::Bool
        | TypeSchema::String
        | TypeSchema::Bytes
        | TypeSchema::GenericParam(_) => true,
        TypeSchema::Optional(inner) => schema_is_fully_known(inner),
        TypeSchema::Named(_, type_args) => type_args.iter().all(schema_is_fully_known),
        TypeSchema::Array(item) | TypeSchema::Map(item) => {
            matches!(item.as_ref(), TypeSchema::Unknown) || schema_is_fully_known(item)
        }
        TypeSchema::ArrayTuple(items) => items.iter().all(schema_is_fully_known),
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            prefix.iter().all(schema_is_fully_known) && schema_is_fully_known(rest)
        }
        TypeSchema::Object(fields) => fields.values().all(schema_is_fully_known),
        TypeSchema::Callable { params, result } => {
            params.iter().all(schema_is_fully_known) && schema_is_fully_known(result)
        }
    }
}

fn collect_strict_slot_sites(parsed: &FrontendIr) -> Vec<(LocalSlot, StrictSlotSite)> {
    let mut sites = Vec::new();
    let local_debug_ranges = collect_local_debug_ranges(&parsed.stmts, &parsed.function_impls);
    let local_source_names = collect_local_source_names(parsed);
    for (name, slot) in &parsed.local_bindings {
        let line = local_debug_ranges
            .get(slot)
            .and_then(|range| range.declared_line);
        sites.push((
            *slot,
            StrictSlotSite {
                name: name.clone(),
                kind: "local",
                line,
                source_name: local_source_names.get(slot).cloned().flatten(),
            },
        ));
    }

    let function_decl_lines = collect_function_decl_lines(&parsed.stmts);
    for decl in &parsed.functions {
        let Some(function_impl) = parsed.function_impls.get(&decl.index) else {
            continue;
        };
        for (name, slot) in decl.args.iter().zip(function_impl.param_slots.iter()) {
            sites.push((
                *slot,
                StrictSlotSite {
                    name: name.clone(),
                    kind: "parameter",
                    line: function_decl_lines.get(&decl.index).copied(),
                    source_name: parsed.function_sources.get(&decl.index).cloned(),
                },
            ));
        }
    }

    sites.sort_by_key(|(slot, _)| *slot);
    sites
}

fn collect_local_source_names(parsed: &FrontendIr) -> HashMap<LocalSlot, Option<String>> {
    let mut out = HashMap::new();
    for (index, stmt) in parsed.stmts.iter().enumerate() {
        let source_name = parsed
            .stmt_sources
            .get(index)
            .and_then(|source| source.as_deref());
        record_local_source_names(std::slice::from_ref(stmt), source_name, &mut out);
    }
    for decl in &parsed.functions {
        let Some(function_impl) = parsed.function_impls.get(&decl.index) else {
            continue;
        };
        let source_name = parsed.function_sources.get(&decl.index).map(String::as_str);
        record_local_source_names(&function_impl.body_stmts, source_name, &mut out);
    }
    out
}

fn record_local_source_names(
    stmts: &[Stmt],
    source_name: Option<&str>,
    out: &mut HashMap<LocalSlot, Option<String>>,
) {
    let source_name = source_name.map(str::to_string);
    for stmt in stmts {
        match stmt {
            Stmt::Let { index, .. } => {
                out.entry(*index).or_insert_with(|| source_name.clone());
            }
            Stmt::IfElse {
                then_branch,
                else_branch,
                ..
            } => {
                record_local_source_names(then_branch, source_name.as_deref(), out);
                record_local_source_names(else_branch, source_name.as_deref(), out);
            }
            Stmt::For {
                init, post, body, ..
            } => {
                record_local_source_names(
                    std::slice::from_ref(init.as_ref()),
                    source_name.as_deref(),
                    out,
                );
                record_local_source_names(
                    std::slice::from_ref(post.as_ref()),
                    source_name.as_deref(),
                    out,
                );
                record_local_source_names(body, source_name.as_deref(), out);
            }
            Stmt::While { body, .. } => {
                record_local_source_names(body, source_name.as_deref(), out);
            }
            Stmt::Noop { .. }
            | Stmt::Assign { .. }
            | Stmt::ClosureLet { .. }
            | Stmt::FuncDecl { .. }
            | Stmt::Expr { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Drop { .. } => {}
        }
    }
}

pub fn compile_source(source: &str) -> Result<CompiledProgram, SourceError> {
    compile_source_with_flavor(source, SourceFlavor::RustScript)
}

pub fn lint_trailing_function_return_semicolons(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<ParseError>, ParseError> {
    let Some(dialect) =
        frontends::parser_dialect_for_flavor(flavor, &CompileSourceFileOptions::default())
    else {
        return Ok(Vec::new());
    };
    parser::lint_trailing_function_return_semicolons(source, 0, dialect)
}

pub fn lint_unknown_type_annotations(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<crate::compiler::source_map::Span>, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    let parsed = frontends::parse_source(source, flavor, &CompileSourceFileOptions::default())
        .map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
        })?;
    Ok(parsed.unknown_type_spans)
}

pub fn lint_unknown_inferred_local_types(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<UnknownInferredLocal>, SourceError> {
    lint_unknown_inferred_local_types_impl(source, flavor)
}

pub fn collect_inferred_local_type_hints(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<InferredLocalTypeHint>, SourceError> {
    collect_inferred_local_type_hints_impl(source, flavor)
}

pub fn collect_inferred_local_type_hints_with_options(
    source: &str,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Result<Vec<InferredLocalTypeHint>, SourcePathError> {
    let source_owned = source.to_string();
    run_with_compiler_stack(move || {
        collect_inferred_local_type_hints_with_options_impl(&source_owned, flavor, &options)
    })
}

pub fn collect_inferred_local_type_hints_at_path_with_options(
    path: impl AsRef<Path>,
    source: &str,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Result<Vec<InferredLocalTypeHint>, SourcePathError> {
    let path = path.as_ref().to_path_buf();
    let source_owned = source.to_string();
    run_with_compiler_stack(move || {
        collect_inferred_local_type_hints_at_path_with_options_impl(
            &path,
            &source_owned,
            flavor,
            &options,
        )
    })
}

pub fn lint_unknown_inferred_local_types_with_options(
    source: &str,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Result<Vec<UnknownInferredLocal>, SourcePathError> {
    let source_owned = source.to_string();
    run_with_compiler_stack(move || {
        lint_unknown_inferred_local_types_with_options_impl(&source_owned, flavor, &options)
    })
}

pub fn lint_unknown_inferred_local_types_at_path_with_options(
    path: impl AsRef<Path>,
    source: &str,
    flavor: SourceFlavor,
    options: CompileSourceFileOptions,
) -> Result<Vec<UnknownInferredLocal>, SourcePathError> {
    let path = path.as_ref().to_path_buf();
    let source_owned = source.to_string();
    run_with_compiler_stack(move || {
        lint_unknown_inferred_local_types_at_path_with_options_impl(
            &path,
            &source_owned,
            flavor,
            &options,
        )
    })
}

fn lint_unknown_inferred_local_types_impl(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<UnknownInferredLocal>, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    let parsed = frontends::parse_source(source, flavor, &CompileSourceFileOptions::default())
        .map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
        })?;
    Ok(collect_unknown_inferred_local_types(
        &source_map,
        source_id,
        parsed,
    ))
}

fn collect_inferred_local_type_hints_impl(
    source: &str,
    flavor: SourceFlavor,
) -> Result<Vec<InferredLocalTypeHint>, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    let parsed = frontends::parse_source(source, flavor, &CompileSourceFileOptions::default())
        .map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
        })?;
    Ok(collect_named_local_type_hints(parsed))
}

fn lint_unknown_inferred_local_types_with_options_impl(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<Vec<UnknownInferredLocal>, SourcePathError> {
    if !options.has_module_overrides() && !options.has_source_plugins() {
        return lint_unknown_inferred_local_types_impl(source, flavor)
            .map_err(SourcePathError::Source);
    }

    let path = virtual_inmemory_entry_path(flavor);
    lint_unknown_inferred_local_types_at_path_with_options_impl(&path, source, flavor, options)
}

fn collect_inferred_local_type_hints_with_options_impl(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<Vec<InferredLocalTypeHint>, SourcePathError> {
    if !options.has_module_overrides() && !options.has_source_plugins() {
        return collect_inferred_local_type_hints_impl(source, flavor)
            .map_err(SourcePathError::Source);
    }

    let path = virtual_inmemory_entry_path(flavor);
    collect_inferred_local_type_hints_at_path_with_options_impl(&path, source, flavor, options)
}

fn lint_unknown_inferred_local_types_at_path_with_options_impl(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<Vec<UnknownInferredLocal>, SourcePathError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source(path.display().to_string(), source.to_string());
    let loaded = load_units_for_source_file(path, flavor, source, options)?;
    let parsed = loaded
        .units
        .into_iter()
        .last()
        .map(|unit| unit.parsed)
        .expect("root parsed unit should always be present");
    Ok(collect_unknown_inferred_local_types(
        &source_map,
        source_id,
        parsed,
    ))
}

fn collect_inferred_local_type_hints_at_path_with_options_impl(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<Vec<InferredLocalTypeHint>, SourcePathError> {
    let loaded = load_units_for_source_file(path, flavor, source, options)?;
    let parsed = loaded
        .units
        .into_iter()
        .last()
        .map(|unit| unit.parsed)
        .expect("root parsed unit should always be present");
    Ok(collect_named_local_type_hints(parsed))
}

fn collect_unknown_inferred_local_types(
    source_map: &SourceMap,
    source_id: u32,
    parsed: FrontendIr,
) -> Vec<UnknownInferredLocal> {
    let local_debug_ranges = collect_local_debug_ranges(&parsed.stmts, &parsed.function_impls);
    let parsed = typing::legalize_builtins_and_bind_types(parsed, TypingMode::DynamicHints, &[]);
    let type_info = typing::infer_types(&parsed, TypingMode::DynamicHints, &[]);

    let mut warnings = Vec::new();
    for (name, slot) in &parsed.local_bindings {
        let Some(range) = local_debug_ranges.get(slot) else {
            continue;
        };
        let Some(line_u32) = range.declared_line else {
            continue;
        };
        let slot_index = usize::from(*slot);
        if type_info
            .callable_slots
            .get(slot_index)
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        if type_info
            .local_schema_labels
            .get(slot_index)
            .and_then(|label| label.as_ref())
            .is_some_and(|label| label != "unknown")
        {
            continue;
        }
        if type_info.local_types.get(slot_index) != Some(&crate::ValueType::Unknown) {
            continue;
        }
        let line = usize::try_from(line_u32).unwrap_or(usize::MAX);
        warnings.push(UnknownInferredLocal {
            name: name.clone(),
            line,
            span: find_local_name_span(source_map, source_id, line, name)
                .or_else(|| source_map.line_span(source_id, line)),
        });
    }
    warnings
}

fn collect_named_local_type_hints(parsed: FrontendIr) -> Vec<InferredLocalTypeHint> {
    let slot_ranges = collect_local_debug_ranges(&parsed.stmts, &parsed.function_impls);
    let function_decl_lines = collect_function_decl_lines(&parsed.stmts);
    let parsed = typing::legalize_builtins_and_bind_types(parsed, TypingMode::DynamicHints, &[]);
    let type_info = typing::infer_types(&parsed, TypingMode::DynamicHints, &[]);

    let mut hints = Vec::new();
    for (name, slot) in &parsed.local_bindings {
        hints.push(InferredLocalTypeHint {
            name: name.clone(),
            inferred_type: inferred_slot_type_name(&type_info, *slot),
            declared_line: slot_ranges.get(slot).and_then(|range| range.declared_line),
            last_line: slot_ranges.get(slot).and_then(|range| range.last_line),
        });
    }

    for decl in &parsed.functions {
        let Some(function_impl) = parsed.function_impls.get(&decl.index) else {
            continue;
        };
        let declared_line = function_decl_lines.get(&decl.index).copied();
        let last_line = function_scope_last_line(function_impl).or(declared_line);
        for (name, slot) in decl.args.iter().zip(function_impl.param_slots.iter()) {
            hints.push(InferredLocalTypeHint {
                name: name.clone(),
                inferred_type: inferred_slot_type_name(&type_info, *slot),
                declared_line: slot_ranges
                    .get(slot)
                    .and_then(|range| range.declared_line)
                    .or(declared_line),
                last_line: slot_ranges
                    .get(slot)
                    .and_then(|range| range.last_line)
                    .or(last_line),
            });
        }
    }

    hints
}

fn inferred_slot_type_name(type_info: &typing::TypeInferenceResult, slot: LocalSlot) -> String {
    let slot_index = usize::from(slot);
    if type_info
        .callable_slots
        .get(slot_index)
        .copied()
        .unwrap_or(false)
    {
        return "function".to_string();
    }
    if let Some(label) = type_info
        .local_schema_labels
        .get(slot_index)
        .and_then(|label| label.as_ref())
        .filter(|label| label.as_str() != "unknown")
    {
        return label.clone();
    }
    value_type_name(
        type_info
            .local_types
            .get(slot_index)
            .copied()
            .unwrap_or(crate::ValueType::Unknown),
    )
    .to_string()
}

fn value_type_name(value: crate::ValueType) -> &'static str {
    match value {
        crate::ValueType::Unknown => "unknown",
        crate::ValueType::Null => "null",
        crate::ValueType::Int => "int",
        crate::ValueType::Float => "float",
        crate::ValueType::Bool => "bool",
        crate::ValueType::String => "string",
        crate::ValueType::Bytes => "bytes",
        crate::ValueType::Array => "array",
        crate::ValueType::Map => "map",
        crate::ValueType::Callable => "callable",
    }
}

fn collect_function_decl_lines(stmts: &[Stmt]) -> HashMap<u16, u32> {
    let mut lines = HashMap::new();
    record_function_decl_lines(stmts, &mut lines);
    lines
}

fn record_function_decl_lines(stmts: &[Stmt], lines: &mut HashMap<u16, u32>) {
    for stmt in stmts {
        match stmt {
            Stmt::FuncDecl { index, line, .. } => {
                lines.insert(*index, *line);
            }
            Stmt::IfElse {
                then_branch,
                else_branch,
                ..
            } => {
                record_function_decl_lines(then_branch, lines);
                record_function_decl_lines(else_branch, lines);
            }
            Stmt::For {
                init, post, body, ..
            } => {
                record_function_decl_lines(std::slice::from_ref(init.as_ref()), lines);
                record_function_decl_lines(std::slice::from_ref(post.as_ref()), lines);
                record_function_decl_lines(body, lines);
            }
            Stmt::While { body, .. } => {
                record_function_decl_lines(body, lines);
            }
            Stmt::Noop { .. }
            | Stmt::Let { .. }
            | Stmt::Assign { .. }
            | Stmt::ClosureLet { .. }
            | Stmt::Expr { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Drop { .. } => {}
        }
    }
}

fn function_scope_last_line(function_impl: &FunctionImpl) -> Option<u32> {
    let stmt_last_line = function_impl.body_stmts.last().map(stmt_source_line);
    match stmt_last_line {
        Some(line) => Some(line.max(function_impl.body_expr_line)),
        None if function_impl.body_expr_line > 0 => Some(function_impl.body_expr_line),
        None => None,
    }
}

fn find_local_name_span(
    source_map: &SourceMap,
    source_id: u32,
    line: usize,
    name: &str,
) -> Option<crate::compiler::source_map::Span> {
    let file = source_map.file(source_id)?;
    let line_range = file.line_span(line)?;
    let line_text = file.line_text(line)?;
    let mut search_start = 0usize;
    while let Some(relative) = line_text[search_start..].find(name) {
        let start = search_start + relative;
        let end = start + name.len();
        let prev_ok = start == 0
            || !line_text[..start]
                .chars()
                .next_back()
                .is_some_and(is_ident_char);
        let next_ok =
            end == line_text.len() || !line_text[end..].chars().next().is_some_and(is_ident_char);
        if prev_ok && next_ok {
            return Some(crate::compiler::source_map::Span::new(
                source_id,
                line_range.start + start,
                line_range.start + end,
            ));
        }
        search_start = end;
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
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
        compile_source_for_repl_with_locals_impl(&source_owned, &predefined_locals, &[])
    })
}

pub fn compile_source_for_repl_with_state(
    source: &str,
    predefined_locals: &[ReplLocalState],
) -> Result<CompiledReplProgram, SourceError> {
    let source_owned = source.to_string();
    let predefined_locals = predefined_locals.to_vec();
    run_with_compiler_stack(move || {
        let bindings = predefined_locals
            .iter()
            .map(|state| state.binding.clone())
            .collect::<Vec<_>>();
        let moved_names = predefined_locals
            .iter()
            .filter(|state| state.moved)
            .map(|state| state.binding.name.clone())
            .collect::<Vec<_>>();
        compile_source_for_repl_with_locals_impl(&source_owned, &bindings, &moved_names)
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
    moved_names: &[String],
) -> Result<CompiledReplProgram, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    // REPL parsing/compiler entry state is separate from normal program compilation so
    // persisted locals do not leak into the generic frontend or IR surface.
    let parsed =
        frontends::parse_rustscript_repl_source(source, predefined_locals).map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
        })?;
    let entry_local_types = build_entry_local_types(&parsed.ir, predefined_locals);
    let entry_availability =
        build_entry_local_availability(&parsed.ir, predefined_locals, moved_names);
    let compiled = match compile_parsed_output_with_entry_locals(
        source.to_string(),
        parsed.ir,
        &entry_availability,
        &entry_local_types,
        CompileBehavior::REPL,
        TypingMode::StrictRustScript,
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

fn build_entry_local_availability(
    parsed: &FrontendIr,
    predefined_locals: &[ReplLocalBinding],
    moved_names: &[String],
) -> Vec<lifetime::EntryLocalAvailability> {
    let predefined_by_name = predefined_locals
        .iter()
        .map(|binding| (binding.name.as_str(), binding))
        .collect::<HashMap<_, _>>();
    parsed
        .local_bindings
        .iter()
        .filter_map(|(name, slot)| {
            let binding = predefined_by_name.get(name.as_str())?;
            let schema = binding
                .schema
                .as_ref()
                .map(|schema| schema.split_optional().0);
            let copyable = matches!(
                schema,
                Some(
                    TypeSchema::Null
                        | TypeSchema::Int
                        | TypeSchema::Float
                        | TypeSchema::Number
                        | TypeSchema::Bool
                )
            );
            let movable = matches!(schema, Some(TypeSchema::String | TypeSchema::Bytes));
            Some(lifetime::EntryLocalAvailability {
                slot: *slot,
                copyable,
                movable,
                moved: moved_names.iter().any(|moved| moved == name),
            })
        })
        .collect()
}

fn build_entry_local_types(
    parsed: &FrontendIr,
    predefined_locals: &[ReplLocalBinding],
) -> Vec<typing::EntryLocalType> {
    let predefined_by_name = predefined_locals
        .iter()
        .map(|binding| (binding.name.as_str(), binding))
        .collect::<HashMap<_, _>>();
    parsed
        .local_bindings
        .iter()
        .filter_map(|(name, slot)| {
            let binding = predefined_by_name.get(name.as_str())?;
            let (schema, schema_optional) = binding
                .schema
                .clone()
                .map(|schema| schema.split_optional())
                .map(|(schema, optional)| (Some(schema), optional))
                .unwrap_or((None, false));
            Some(typing::EntryLocalType {
                slot: *slot,
                schema,
                optional: binding.optional || schema_optional,
            })
        })
        .collect()
}

fn compile_source_with_flavor_impl(
    source: &str,
    flavor: SourceFlavor,
    behavior: CompileBehavior,
) -> Result<CompiledProgram, SourceError> {
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    let parsed = frontends::parse_source(source, flavor, &CompileSourceFileOptions::default())
        .map_err(|err| {
            SourceError::Parse(err.with_line_span_from_source(&source_map, source_id))
        })?;
    match compile_parsed_output(
        source.to_string(),
        parsed,
        behavior,
        TypingMode::for_flavor(flavor),
        matches!(flavor, SourceFlavor::RustScript),
    ) {
        Err(SourceError::Parse(err)) => Err(SourceError::Parse(
            err.with_line_span_from_source(&source_map, source_id),
        )),
        other => other,
    }
}

fn compile_loaded_units(
    source: String,
    units: Vec<ParsedUnit>,
    flavor: SourceFlavor,
    // Carried from the loader for Milestone 2+ (structured imports, symbol
    // resolution); codegen output is unchanged until then.
    _module_graph: ModuleGraph,
    // Compilation-wide source map keyed by the module graph's `SourceId`
    // space (milestone 5). Every span produced during load/merge references
    // this map, so errors are returned with it and render from the owning
    // source.
    sources: SourceMap,
) -> Result<CompiledProgram, SourcePathError> {
    let diagnostic_path = units
        .iter()
        .find(|unit| !unit.parsed.unknown_type_spans.is_empty())
        .map(|unit| PathBuf::from(&unit.source_name));
    let merged = merge_units(units)?;
    compile_parsed_output(
        source,
        merged,
        CompileBehavior::DEFAULT,
        TypingMode::for_flavor(flavor),
        matches!(flavor, SourceFlavor::RustScript),
    )
    .map_err(|error| match (error, diagnostic_path) {
        (SourceError::Parse(mut parse), Some(path))
            if parse.code.as_deref() == Some("E_STRICT_UNKNOWN_TYPE") =>
        {
            parse.message = format!("{}: {}", path.display(), parse.message);
            SourcePathError::SourceWithMap {
                error: SourceError::Parse(parse),
                sources,
            }
        }
        (error, _) => SourcePathError::SourceWithMap { error, sources },
    })
}

fn compile_source_with_flavor_and_options_impl(
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    if !options.has_module_overrides() && !options.has_source_plugins() {
        return compile_source_with_flavor_impl(source, flavor, CompileBehavior::DEFAULT)
            .map_err(SourcePathError::Source);
    }

    let path = virtual_inmemory_entry_path(flavor);
    let loaded = load_units_for_source_file(&path, flavor, source, options)?;
    compile_loaded_units(
        source.to_string(),
        loaded.units,
        flavor,
        loaded.module_graph,
        loaded.sources,
    )
}

fn compile_source_at_path_with_flavor_and_options_impl(
    path: &Path,
    source: &str,
    flavor: SourceFlavor,
    options: &CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    let loaded = load_units_for_source_file(path, flavor, source, options)?;
    compile_loaded_units(
        source.to_string(),
        loaded.units,
        flavor,
        loaded.module_graph,
        loaded.sources,
    )
}

fn virtual_inmemory_entry_path(flavor: SourceFlavor) -> PathBuf {
    let ext = match flavor {
        SourceFlavor::RustScript => "rss",
        SourceFlavor::JavaScript => "js",
        SourceFlavor::Lua => "lua",
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
    let flavor = SourceFlavor::from_path_with_options(path, options)?;
    let source_raw = std::fs::read_to_string(path)?;
    let loaded = load_units_for_source_file(path, flavor, &source_raw, options)?;
    compile_loaded_units(
        source_raw,
        loaded.units,
        flavor,
        loaded.module_graph,
        loaded.sources,
    )
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::vm::Vm;

    use super::*;

    #[test]
    fn production_path_callable_use_facts_observed() {
        // Observe the milestone-5 classification through the real production
        // pipeline (parse -> module merge -> lifetime -> classification ->
        // Compiler) via the crate-internal test observation on
        // CompiledProgram. Facts must be keyed by resolved flat identity
        // and include the flow-aware dynamic-target and runtime-self facts;
        // allocation behavior stays untouched (every named function keeps
        // its prototype and hidden callable slot).
        let source = r#"
            fn direct_helper(x: int) -> int { x + 1 }
            pub fn exported_helper(x: int) -> int { x + 2 }
            fn stored_helper(x: int) -> int { x + 3 }
            fn flow_helper() -> int { 4 }
            fn consume(f) -> int { 1 }
            fn apply(f) -> int { f(1) }
            fn direct_recursive(n: int) -> int {
                if n <= 0 => { 0 } else => { direct_recursive(n - 1) }
            }
            let captured = 42;
            fn read_captured() -> int { captured }
            fn captured_walk(n: int) -> int {
                if n <= 0 => { captured } else => { captured_walk(n - 1) }
            }
            let stored = stored_helper;
            let a = flow_helper;
            let b = a;
            b();
            consume(stored_helper);
            apply(consume);
            direct_helper(1);
            exported_helper(1);
            direct_recursive(3);
            read_captured;
            captured_walk(2);
        "#;
        let compiled = compile_source(source).expect("classification program should compile");
        let observations = &compiled.callable_use_facts;
        let find = |name: &str| {
            observations
                .iter()
                .find(|observation| observation.name == name)
                .unwrap_or_else(|| panic!("observation for '{name}' missing: {observations:#?}"))
                .facts
        };
        assert_eq!(
            observations.len(),
            9,
            "every named script function must carry production-path facts"
        );
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.function_index)
                .collect::<BTreeSet<_>>()
                .len(),
            9,
            "facts must be keyed by distinct resolved flat identities"
        );

        let direct = find("direct_helper");
        assert!(direct.called_directly);
        assert!(!direct.referenced_as_value);
        assert!(!direct.exported);
        assert!(!direct.captures_environment);
        assert!(!direct.dynamic_target_required);
        assert!(!direct.runtime_self_required);
        assert!(!direct.requires_callable_slot());

        let exported = find("exported_helper");
        assert!(exported.called_directly);
        assert!(exported.exported);
        assert!(exported.requires_callable_slot());

        let stored = find("stored_helper");
        assert!(stored.referenced_as_value);
        assert!(
            !stored.dynamic_target_required,
            "passing a function value to a callee that never invokes it must not \
             mark a dynamic target (tracked flow only)"
        );
        assert!(stored.requires_callable_slot());

        let flow = find("flow_helper");
        assert!(flow.referenced_as_value);
        assert!(
            flow.dynamic_target_required,
            "the alias chain `let a = flow_helper; let b = a; b();` must propagate \
             to the dynamic invocation"
        );

        let consume = find("consume");
        assert!(consume.called_directly);
        assert!(
            consume.dynamic_target_required,
            "consume is passed to `apply`, whose parameter is dynamically invoked"
        );

        let recursive = find("direct_recursive");
        assert!(recursive.called_directly);
        assert!(!recursive.captures_environment);
        assert!(
            !recursive.runtime_self_required,
            "non-capturing direct recursion needs no runtime self identity"
        );
        assert!(!recursive.requires_callable_slot());

        let read_captured = find("read_captured");
        assert!(read_captured.captures_environment);
        assert!(!read_captured.runtime_self_required);

        let captured_walk = find("captured_walk");
        assert!(captured_walk.captures_environment);
        assert!(
            captured_walk.runtime_self_required,
            "capturing direct recursion retains the runtime self identity"
        );
        assert!(captured_walk.requires_callable_slot());

        // Milestone 6 lowering: every named function keeps its prototype;
        // direct-only functions (no value reference, export, capture, or
        // dynamic target) keep no hidden callable slot, while the
        // materialized functions retain their runtime self slot.
        assert_eq!(compiled.program.callable_prototypes.len(), 9);
        let self_slots = compiled
            .program
            .callable_prototypes
            .iter()
            .filter(|prototype| prototype.self_slot.is_some())
            .count();
        assert_eq!(
            self_slots, 6,
            "exported, stored, flow, consume, and both capturing functions stay materialized"
        );
        assert_eq!(
            compiled
                .program
                .callable_prototypes
                .iter()
                .filter(|prototype| prototype.self_slot.is_none())
                .count(),
            3,
            "direct_helper, apply, and direct_recursive are direct-only"
        );
        assert_eq!(compiled.program.root_callable_bindings.len(), 4);
        assert!(
            compiled
                .program
                .code
                .contains(&(crate::OpCode::CallScript as u8)),
            "direct-only call sites emit CallScript"
        );

        let mut vm = Vm::new(compiled.program);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, crate::vm::VmStatus::Halted);
    }

    #[test]
    fn production_path_module_merge_facts_follow_flat_indices() {
        // Two modules each declare a private `helper` plus a `pub run` that
        // calls it, merged through the real production pipeline. The
        // classification must attribute facts to distinct resolved flat
        // identities; assertions never parse the merged display names (a
        // mangling policy change must not affect them) and instead check
        // counts, index uniqueness, and the exported-vs-private semantic
        // facts.
        let options = CompileSourceFileOptions::new()
            .with_module_override_source(
                "a/util.rss",
                "pub fn run() { helper(); }\nfn helper() { 11; }\n",
            )
            .with_module_override_source(
                "b/util.rss",
                "pub fn run() { helper(); }\nfn helper() { 22; }\n",
            );
        let source = "use a::util as au;\nuse b::util as bu;\nau::run();\nbu::run();\n";
        let compiled =
            compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options)
                .expect("same-named module helpers should compile");

        let observations = &compiled.callable_use_facts;
        assert_eq!(
            observations.len(),
            4,
            "both modules' run and both same-named helpers must carry facts: {observations:#?}"
        );
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.function_index)
                .collect::<BTreeSet<_>>()
                .len(),
            4,
            "classification must be keyed by distinct resolved flat identities"
        );

        let runs = observations
            .iter()
            .filter(|observation| observation.facts.exported)
            .collect::<Vec<_>>();
        assert_eq!(runs.len(), 2, "both exported runs must survive the merge");
        for run in runs {
            assert!(run.facts.called_directly);
            assert!(run.facts.requires_callable_slot());
        }

        let helpers = observations
            .iter()
            .filter(|observation| !observation.facts.exported)
            .collect::<Vec<_>>();
        assert_eq!(
            helpers.len(),
            2,
            "both same-named private helpers must survive the merge"
        );
        for helper in helpers {
            assert!(
                helper.facts.called_directly,
                "each module's run calls its own same-named helper"
            );
            assert!(!helper.facts.dynamic_target_required);
            assert!(!helper.facts.requires_callable_slot());
        }

        // Milestone 6 allocation: every merged function keeps its prototype;
        // the same-named private helpers are direct-only (no hidden slot),
        // and both exported runs stay materialized and exported.
        assert_eq!(compiled.program.callable_prototypes.len(), 4);
        assert_eq!(
            compiled
                .program
                .callable_prototypes
                .iter()
                .filter(|prototype| prototype.self_slot.is_some())
                .count(),
            2,
            "both exported runs keep their runtime self slot"
        );
        assert_eq!(
            compiled
                .program
                .callable_prototypes
                .iter()
                .filter(|prototype| prototype.self_slot.is_none())
                .count(),
            2,
            "both same-named private helpers are direct-only"
        );
        assert_eq!(compiled.program.root_callable_bindings.len(), 2);
        assert_eq!(compiled.program.exported_callables.len(), 2);

        let mut vm = Vm::new(compiled.program);
        let status = vm.run().expect("vm should run");
        assert_eq!(status, crate::vm::VmStatus::Halted);
    }
}
