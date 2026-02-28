use std::collections::HashMap;
use std::path::Path;

use crate::builtins::BuiltinFunction;

use super::{
    Expr, FunctionDecl, FunctionImpl, ParseError, SourceError, SourcePathError, Stmt, frontends,
};

pub(super) struct ParsedUnit {
    pub(super) parsed: frontends::FrontendOutput,
    pub(super) scope_prefix: Option<String>,
}

pub(super) struct MergedFrontendOutput {
    pub(super) source: String,
    pub(super) stmts: Vec<Stmt>,
    pub(super) locals: usize,
    pub(super) local_bindings: Vec<(String, u8)>,
    pub(super) functions: Vec<FunctionDecl>,
    pub(super) function_impls: HashMap<u16, FunctionImpl>,
}

pub(super) fn sanitize_scope_prefix(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("module")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn merge_units(
    root_source: String,
    units: Vec<ParsedUnit>,
) -> Result<MergedFrontendOutput, SourcePathError> {
    let mut merged_stmts = Vec::new();
    let mut merged_local_bindings = Vec::new();
    let mut merged_functions = Vec::new();
    let mut merged_function_impls = HashMap::<u16, FunctionImpl>::new();
    let mut function_index_by_name = HashMap::<String, u16>::new();
    let mut local_base = 0usize;

    for unit in units {
        let function_map = remap_functions(
            &unit.parsed.functions,
            &mut merged_functions,
            &mut function_index_by_name,
        )?;
        let unit_local_base = local_base;
        let unit_local_count = unit.parsed.locals;

        let mut remapped_stmts = unit.parsed.stmts;
        for stmt in &mut remapped_stmts {
            remap_stmt_indices(stmt, unit_local_base, &function_map)?;
        }
        merged_stmts.extend(remapped_stmts);

        for (name, index) in unit.parsed.local_bindings {
            let remapped_index = remap_local_index(index, unit_local_base)?;
            let scoped_name = if let Some(prefix) = &unit.scope_prefix {
                format!("{prefix}::{name}")
            } else {
                name
            };
            merged_local_bindings.push((scoped_name, remapped_index));
        }

        for (unit_index, mut function_impl) in unit.parsed.function_impls {
            let merged_index = function_map.get(&unit_index).copied().ok_or_else(|| {
                SourcePathError::Source(SourceError::Parse(ParseError {
                    line: 1,
                    message: "function implementation remap failed while merging imported modules"
                        .to_string(),
                }))
            })?;
            for param_slot in &mut function_impl.param_slots {
                *param_slot = remap_local_index(*param_slot, unit_local_base)?;
            }
            for stmt in &mut function_impl.body_stmts {
                remap_stmt_indices(stmt, unit_local_base, &function_map)?;
            }
            remap_expr_indices(&mut function_impl.body_expr, unit_local_base, &function_map)?;
            if merged_function_impls
                .insert(merged_index, function_impl)
                .is_some()
            {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    line: 1,
                    message: "duplicate RSS function implementation while merging imported modules"
                        .to_string(),
                })));
            }
        }

        local_base = local_base.checked_add(unit_local_count).ok_or_else(|| {
            SourcePathError::Source(SourceError::Parse(ParseError {
                line: 1,
                message: "local count overflow while merging imported modules".to_string(),
            }))
        })?;
        if local_base > (u8::MAX as usize) {
            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                line: 1,
                message: "too many locals across imported modules".to_string(),
            })));
        }
    }

    Ok(MergedFrontendOutput {
        source: root_source,
        stmts: merged_stmts,
        locals: local_base,
        local_bindings: merged_local_bindings,
        functions: merged_functions,
        function_impls: merged_function_impls,
    })
}

fn remap_functions(
    unit_functions: &[FunctionDecl],
    merged_functions: &mut Vec<FunctionDecl>,
    function_index_by_name: &mut HashMap<String, u16>,
) -> Result<HashMap<u16, u16>, SourcePathError> {
    let mut map = HashMap::new();

    for func in unit_functions {
        let merged_index = if let Some(existing_index) = function_index_by_name.get(&func.name) {
            let existing = &mut merged_functions[*existing_index as usize];
            if existing.arity != func.arity {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    line: 1,
                    message: format!(
                        "function '{}' declared with conflicting arity {} vs {}",
                        func.name, existing.arity, func.arity
                    ),
                })));
            }
            existing.exported = existing.exported || func.exported;
            *existing_index
        } else {
            let next_index = u16::try_from(merged_functions.len()).map_err(|_| {
                SourcePathError::Source(SourceError::Parse(ParseError {
                    line: 1,
                    message: "too many functions across imported modules".to_string(),
                }))
            })?;
            merged_functions.push(FunctionDecl {
                name: func.name.clone(),
                arity: func.arity,
                index: next_index,
                args: func.args.clone(),
                exported: func.exported,
            });
            function_index_by_name.insert(func.name.clone(), next_index);
            next_index
        };
        map.insert(func.index, merged_index);
    }

    Ok(map)
}

fn remap_local_index(index: u8, local_base: usize) -> Result<u8, SourcePathError> {
    let remapped = (index as usize).checked_add(local_base).ok_or_else(|| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            line: 1,
            message: "local index overflow while merging imported modules".to_string(),
        }))
    })?;
    u8::try_from(remapped).map_err(|_| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            line: 1,
            message: "local index overflow while merging imported modules".to_string(),
        }))
    })
}

fn remap_stmt_indices(
    stmt: &mut Stmt,
    local_base: usize,
    function_map: &HashMap<u16, u16>,
) -> Result<(), SourcePathError> {
    match stmt {
        Stmt::Noop { .. } => {}
        Stmt::Let { index, expr, .. } => {
            *index = remap_local_index(*index, local_base)?;
            remap_expr_indices(expr, local_base, function_map)?;
        }
        Stmt::Assign { index, expr, .. } => {
            *index = remap_local_index(*index, local_base)?;
            remap_expr_indices(expr, local_base, function_map)?;
        }
        Stmt::ClosureLet { closure, .. } => {
            for (source_index, captured_slot) in &mut closure.capture_copies {
                *source_index = remap_local_index(*source_index, local_base)?;
                *captured_slot = remap_local_index(*captured_slot, local_base)?;
            }
            remap_expr_indices(&mut closure.body, local_base, function_map)?;
        }
        Stmt::FuncDecl { .. } => {}
        Stmt::Expr { expr, .. } => {
            remap_expr_indices(expr, local_base, function_map)?;
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            remap_expr_indices(condition, local_base, function_map)?;
            for stmt in then_branch {
                remap_stmt_indices(stmt, local_base, function_map)?;
            }
            for stmt in else_branch {
                remap_stmt_indices(stmt, local_base, function_map)?;
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            remap_stmt_indices(init, local_base, function_map)?;
            remap_expr_indices(condition, local_base, function_map)?;
            remap_stmt_indices(post, local_base, function_map)?;
            for stmt in body {
                remap_stmt_indices(stmt, local_base, function_map)?;
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            remap_expr_indices(condition, local_base, function_map)?;
            for stmt in body {
                remap_stmt_indices(stmt, local_base, function_map)?;
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
    }
    Ok(())
}

fn remap_expr_indices(
    expr: &mut Expr,
    local_base: usize,
    function_map: &HashMap<u16, u16>,
) -> Result<(), SourcePathError> {
    match expr {
        Expr::Null | Expr::Int(_) | Expr::Bool(_) | Expr::String(_) => {}
        Expr::Call(index, args) => {
            if let Some(remapped_index) = function_map.get(index).copied() {
                *index = remapped_index;
            } else if BuiltinFunction::from_call_index(*index).is_none() {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    line: 1,
                    message: "function index remap failed while merging imported modules"
                        .to_string(),
                })));
            }
            for arg in args {
                remap_expr_indices(arg, local_base, function_map)?;
            }
        }
        Expr::Closure(closure) => {
            for param_slot in &mut closure.param_slots {
                *param_slot = remap_local_index(*param_slot, local_base)?;
            }
            for (source_index, captured_slot) in &mut closure.capture_copies {
                *source_index = remap_local_index(*source_index, local_base)?;
                *captured_slot = remap_local_index(*captured_slot, local_base)?;
            }
            remap_expr_indices(&mut closure.body, local_base, function_map)?;
        }
        Expr::ClosureCall(closure, args) => {
            for param_slot in &mut closure.param_slots {
                *param_slot = remap_local_index(*param_slot, local_base)?;
            }
            for (source_index, captured_slot) in &mut closure.capture_copies {
                *source_index = remap_local_index(*source_index, local_base)?;
                *captured_slot = remap_local_index(*captured_slot, local_base)?;
            }
            remap_expr_indices(&mut closure.body, local_base, function_map)?;
            for arg in args {
                remap_expr_indices(arg, local_base, function_map)?;
            }
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
            remap_expr_indices(lhs, local_base, function_map)?;
            remap_expr_indices(rhs, local_base, function_map)?;
        }
        Expr::Neg(inner) | Expr::Not(inner) => {
            remap_expr_indices(inner, local_base, function_map)?;
        }
        Expr::Var(index) => {
            *index = remap_local_index(*index, local_base)?;
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            remap_expr_indices(condition, local_base, function_map)?;
            remap_expr_indices(then_expr, local_base, function_map)?;
            remap_expr_indices(else_expr, local_base, function_map)?;
        }
        Expr::Match {
            value_slot,
            result_slot,
            value,
            arms,
            default,
        } => {
            *value_slot = remap_local_index(*value_slot, local_base)?;
            *result_slot = remap_local_index(*result_slot, local_base)?;
            remap_expr_indices(value, local_base, function_map)?;
            for (_, arm_expr) in arms {
                remap_expr_indices(arm_expr, local_base, function_map)?;
            }
            remap_expr_indices(default, local_base, function_map)?;
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                remap_stmt_indices(stmt, local_base, function_map)?;
            }
            remap_expr_indices(expr, local_base, function_map)?;
        }
    }
    Ok(())
}
