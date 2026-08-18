use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::builtins::BuiltinFunction;

use super::{
    ParseError, SourceError, SourcePathError,
    ir::{Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt, StructDecl},
    modules::{ModuleId, SymbolId},
};

pub(super) struct ParsedUnit {
    pub(super) parsed: FrontendIr,
    /// Deterministic scope identity for the unit's local bindings at the flat
    /// bytecode boundary. `None` for the root unit (which keeps bare names);
    /// otherwise a mangled full-module identity computed by the loader, never
    /// a bare file stem (milestone 4).
    pub(super) scope_identity: Option<String>,
    pub(super) source_name: String,
    /// Semantic module identity assigned by the module graph during discovery.
    /// Consumed by milestone 4+ symbol resolution; carried on the unit so the
    /// link between parsed IR and graph node survives the merge pipeline.
    /// (The loader resolves call sites with it before merge; the flat merge
    /// itself keys on [`SymbolId`].)
    #[allow(dead_code)]
    pub(super) module: ModuleId,
    /// Graph `SourceId` of the unit's module (milestone 5). Every span the
    /// unit's IR carries references this id in the compilation-wide
    /// [`SourceMap`](crate::compiler::source_map::SourceMap), so merged
    /// diagnostics always render from the owning source.
    #[allow(dead_code)]
    pub(super) source_id: u32,
}

/// Deterministic flat-boundary scope identity for a non-root module's local
/// bindings.
///
/// Encodes the full canonical module identity (never a bare file stem) plus
/// the compiler-owned [`ModuleId`], so same-stem modules in different
/// directories and same-named locals across independent modules never collide
/// at the flat boundary.
pub(super) fn module_scope_prefix(identity: &Path, module: ModuleId) -> String {
    let sanitized: String = identity
        .to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{sanitized}__m{}", module.0)
}

/// Deterministic flat name for a module function whose source name is already
/// claimed by another flat entry. The mangling encodes the compiler-owned
/// module identity, so it is stable across compilations and never depends on
/// discovery-order-dependent string synthesis.
fn deterministic_flat_name(name: &str, symbol: SymbolId) -> String {
    format!("{}__m{}", name, symbol.module.0)
}

pub(super) fn merge_units(units: Vec<ParsedUnit>) -> Result<FrontendIr, SourcePathError> {
    let mut merged_stmts = Vec::new();
    let mut merged_stmt_sources = Vec::new();
    let mut merged_local_bindings = Vec::new();
    let mut merged_struct_schemas = HashMap::<String, StructDecl>::new();
    let mut merged_unknown_type_spans = Vec::new();
    let mut merged_functions = Vec::new();
    let mut merged_function_impls = HashMap::<u16, FunctionImpl>::new();
    let mut merged_function_sources = HashMap::<u16, String>::new();

    // Milestone 4 flat identity maps.
    //
    // Module functions (declarations with implementations) are merged by
    // compiler-owned `SymbolId`, so same-named declarations in independent
    // modules each get their own flat entry. Host imports (declarations
    // without implementations) keep name-keyed deduplication: their names are
    // the runtime binding surface (`program.imports`, `Vm::bind_function`),
    // so the legacy merge semantics apply verbatim.
    let mut flat_index_by_symbol = HashMap::<SymbolId, u16>::new();
    let mut host_index_by_name = HashMap::<String, u16>::new();
    // Every flat name claimed so far. Module functions that collide are
    // deterministically mangled with their module identity; host imports are
    // deduplicated by name before ever reaching this set.
    let mut claimed_flat_names = HashSet::<String>::new();

    let mut local_base = 0usize;

    for unit in units {
        let source_name = unit.source_name.clone();
        // Fingerprint-bound per-flat-function host candidates cannot be
        // merged yet: flat function indices are remapped during merge, so an
        // index-keyed catalog would no longer line up. Preserve only the
        // legacy `None` path and fail loudly rather than silently dropping a
        // catalog-authoritative candidate set. Index-remapping integration is
        // a follow-up scope.
        if unit.parsed.host_api_metadata.is_some() {
            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: "host API candidate metadata cannot be merged in this scope; flat index remapping is not integrated yet"
                    .to_string(),
            })));
        }
        let function_map = register_unit_functions(
            &unit,
            &mut merged_functions,
            &mut flat_index_by_symbol,
            &mut host_index_by_name,
            &mut claimed_flat_names,
        )?;
        let unit_local_base = local_base;
        let unit_local_count = unit.parsed.locals;

        let mut remapped_stmts = unit.parsed.stmts;
        for stmt in &mut remapped_stmts {
            remap_stmt_indices(stmt, unit_local_base, &function_map, &flat_index_by_symbol)?;
        }
        merged_stmt_sources.extend(std::iter::repeat_n(
            Some(source_name.clone()),
            remapped_stmts.len(),
        ));
        merged_stmts.extend(remapped_stmts);

        for (name, index) in unit.parsed.local_bindings {
            let remapped_index = remap_local_index(index, unit_local_base)?;
            let scoped_name = if let Some(identity) = &unit.scope_identity {
                format!("{identity}::{name}")
            } else {
                name
            };
            merged_local_bindings.push((scoped_name, remapped_index));
        }

        for (name, schema) in unit.parsed.struct_schemas {
            if let Some(existing) = merged_struct_schemas.get(&name) {
                if existing != &schema {
                    return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                        span: None,
                        code: None,
                        line: 1,
                        message: format!(
                            "struct schema '{}' declared with conflicting definitions across imported modules",
                            name
                        ),
                    })));
                }
                continue;
            }
            merged_struct_schemas.insert(name, schema);
        }
        merged_unknown_type_spans.extend(unit.parsed.unknown_type_spans);

        for (unit_index, mut function_impl) in unit.parsed.function_impls {
            let merged_index = function_map.get(&unit_index).copied().ok_or_else(|| {
                SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: "function implementation remap failed while merging imported modules"
                        .to_string(),
                }))
            })?;
            for param_slot in &mut function_impl.param_slots {
                *param_slot = remap_local_index(*param_slot, unit_local_base)?;
            }
            for (source_slot, captured_slot) in &mut function_impl.capture_copies {
                *source_slot = remap_local_index(*source_slot, unit_local_base)?;
                *captured_slot = remap_local_index(*captured_slot, unit_local_base)?;
            }
            for stmt in &mut function_impl.body_stmts {
                remap_stmt_indices(stmt, unit_local_base, &function_map, &flat_index_by_symbol)?;
            }
            remap_expr_indices(
                &mut function_impl.body_expr,
                unit_local_base,
                &function_map,
                &flat_index_by_symbol,
            )?;
            if merged_function_impls
                .insert(merged_index, function_impl)
                .is_some()
            {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: "duplicate RSS function implementation while merging imported modules"
                        .to_string(),
                })));
            }
            merged_function_sources.insert(merged_index, source_name.clone());
        }

        local_base = local_base.checked_add(unit_local_count).ok_or_else(|| {
            SourcePathError::Source(SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: "local count overflow while merging imported modules".to_string(),
            }))
        })?;
        if local_base > (LocalSlot::MAX as usize + 1) {
            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: "too many locals across imported modules".to_string(),
            })));
        }
    }

    Ok(FrontendIr {
        stmts: merged_stmts,
        locals: local_base,
        local_bindings: merged_local_bindings,
        struct_schemas: merged_struct_schemas,
        unknown_type_spans: merged_unknown_type_spans,
        functions: merged_functions,
        function_impls: merged_function_impls,
        stmt_sources: merged_stmt_sources,
        function_sources: merged_function_sources,
        use_declarations: Vec::new(),
        // The loader resolves every implicit extern before merge; any name
        // that survived would indicate a loader bug, so the merged IR never
        // carries them.
        implicit_extern_names: Vec::new(),
        // No fingerprint-bound candidates survive merge in this scope; any
        // unit carrying metadata is rejected above rather than discarded.
        host_api_metadata: None,
    })
}

fn next_flat_index(merged_functions: &[FunctionDecl]) -> Result<u16, SourcePathError> {
    u16::try_from(merged_functions.len()).map_err(|_| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "too many functions across imported modules".to_string(),
        }))
    })
}

/// Register one unit's declarations in the flat function table and return the
/// unit-index → flat-index map.
///
/// Synthetic prelude declarations (symbol-less, mirroring imported bindings)
/// never become flat entries: the loader already resolved their call sites to
/// [`Expr::ModuleCall`] with the target's [`SymbolId`].
fn register_unit_functions(
    unit: &ParsedUnit,
    merged_functions: &mut Vec<FunctionDecl>,
    flat_index_by_symbol: &mut HashMap<SymbolId, u16>,
    host_index_by_name: &mut HashMap<String, u16>,
    claimed_flat_names: &mut HashSet<String>,
) -> Result<HashMap<u16, u16>, SourcePathError> {
    let mut map = HashMap::new();

    for func in &unit.parsed.functions {
        let Some(symbol) = func.symbol else {
            // Synthetic prelude/stub declaration; resolved by the loader.
            continue;
        };
        if let Some(&existing) = flat_index_by_symbol.get(&symbol) {
            map.insert(func.index, existing);
            continue;
        }
        let has_impl = unit.parsed.function_impls.contains_key(&func.index);
        let flat = if !has_impl {
            // Host import: name-keyed deduplication preserves the legacy
            // merge semantics and the runtime name-binding surface.
            if let Some(&existing) = host_index_by_name.get(&func.name) {
                merge_host_import_metadata(&mut merged_functions[existing as usize], func)?;
                flat_index_by_symbol.insert(symbol, existing);
                map.insert(func.index, existing);
                continue;
            }
            let flat = next_flat_index(merged_functions)?;
            merged_functions.push(FunctionDecl {
                name: func.name.clone(),
                arity: func.arity,
                index: flat,
                args: func.args.clone(),
                arg_schemas: func.arg_schemas.clone(),
                return_schema: func.return_schema.clone(),
                type_params: func.type_params.clone(),
                exported: func.exported,
                return_type: func.return_type,
                symbol: Some(symbol),
            });
            host_index_by_name.insert(func.name.clone(), flat);
            claimed_flat_names.insert(func.name.clone());
            flat
        } else {
            // Module function: one flat entry per symbol; the source name is
            // kept unless another flat entry already claimed it, in which
            // case it is deterministically mangled with the module identity.
            let flat = next_flat_index(merged_functions)?;
            let flat_name = if claimed_flat_names.insert(func.name.clone()) {
                func.name.clone()
            } else {
                deterministic_flat_name(&func.name, symbol)
            };
            merged_functions.push(FunctionDecl {
                name: flat_name,
                arity: func.arity,
                index: flat,
                args: func.args.clone(),
                arg_schemas: func.arg_schemas.clone(),
                return_schema: func.return_schema.clone(),
                type_params: func.type_params.clone(),
                exported: func.exported,
                return_type: func.return_type,
                symbol: Some(symbol),
            });
            flat
        };
        flat_index_by_symbol.insert(symbol, flat);
        map.insert(func.index, flat);
    }

    Ok(map)
}

/// Replicate the legacy name-merge metadata rules for host imports that are
/// declared by more than one unit: arity conflicts are errors, `Unknown`
/// return types are refined, and schemas/type parameters merge.
fn merge_host_import_metadata(
    existing: &mut FunctionDecl,
    func: &FunctionDecl,
) -> Result<(), SourcePathError> {
    if existing.arity != func.arity {
        return Err(SourcePathError::Source(SourceError::Parse(ParseError {
            span: None,
            code: None,
            line: 1,
            message: format!(
                "function '{}' declared with conflicting arity {} vs {}",
                func.name, existing.arity, func.arity
            ),
        })));
    }
    if existing.return_type != func.return_type {
        match (existing.return_type, func.return_type) {
            (crate::ValueType::Unknown, known) => existing.return_type = known,
            (known, crate::ValueType::Unknown) => existing.return_type = known,
            (lhs, rhs) => {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: format!(
                        "function '{}' declared with conflicting return type {} vs {}",
                        func.name,
                        value_type_name(lhs),
                        value_type_name(rhs)
                    ),
                })));
            }
        }
    }
    if existing.return_schema != func.return_schema {
        match (&existing.return_schema, &func.return_schema) {
            (None, Some(schema)) => existing.return_schema = Some(schema.clone()),
            (Some(_), None) => {}
            (Some(lhs), Some(rhs)) if lhs == rhs => {}
            _ => {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: format!(
                        "function '{}' declared with conflicting return schemas across imported modules",
                        func.name
                    ),
                })));
            }
        }
    }
    if existing.type_params != func.type_params {
        if existing.type_params.is_empty() {
            existing.type_params = func.type_params.clone();
        } else if !func.type_params.is_empty() {
            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!(
                    "function '{}' declared with conflicting type parameters across imported modules",
                    func.name
                ),
            })));
        }
    }
    if existing.arg_schemas != func.arg_schemas {
        if existing.arg_schemas.iter().all(Option::is_none) {
            existing.arg_schemas = func.arg_schemas.clone();
        } else if !func.arg_schemas.iter().all(Option::is_none) {
            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: format!(
                    "function '{}' declared with conflicting parameter schemas across imported modules",
                    func.name
                ),
            })));
        }
    }
    if function_args_are_placeholders(&existing.args) && !function_args_are_placeholders(&func.args)
    {
        existing.args = func.args.clone();
    }
    existing.exported = existing.exported || func.exported;
    Ok(())
}

fn function_args_are_placeholders(args: &[String]) -> bool {
    args.iter()
        .enumerate()
        .all(|(index, arg)| arg == &format!("arg{index}"))
}

fn value_type_name(ty: crate::ValueType) -> &'static str {
    match ty {
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

fn remap_local_index(index: LocalSlot, local_base: usize) -> Result<LocalSlot, SourcePathError> {
    let remapped = (index as usize).checked_add(local_base).ok_or_else(|| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "local index overflow while merging imported modules".to_string(),
        }))
    })?;
    LocalSlot::try_from(remapped).map_err(|_| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "local index overflow while merging imported modules".to_string(),
        }))
    })
}

fn remap_stmt_indices(
    stmt: &mut Stmt,
    local_base: usize,
    function_map: &HashMap<u16, u16>,
    flat_index_by_symbol: &HashMap<SymbolId, u16>,
) -> Result<(), SourcePathError> {
    match stmt {
        Stmt::Noop { .. } => {}
        Stmt::Let { index, expr, .. } => {
            *index = remap_local_index(*index, local_base)?;
            remap_expr_indices(expr, local_base, function_map, flat_index_by_symbol)?;
        }
        Stmt::Assign { index, expr, .. } => {
            *index = remap_local_index(*index, local_base)?;
            remap_expr_indices(expr, local_base, function_map, flat_index_by_symbol)?;
        }
        Stmt::ClosureLet { closure, .. } => {
            for (source_index, captured_slot) in &mut closure.capture_copies {
                *source_index = remap_local_index(*source_index, local_base)?;
                *captured_slot = remap_local_index(*captured_slot, local_base)?;
            }
            remap_expr_indices(
                &mut closure.body,
                local_base,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Stmt::FuncDecl {
            index, has_impl, ..
        } => {
            // Implementation-less declarations (import prelude stubs, extern
            // prototypes) never enter the flat table and codegen ignores
            // their index; only declarations with implementations are
            // remapped to their symbol-owned flat entry.
            if *has_impl {
                *index = function_map.get(index).copied().ok_or_else(|| {
                    SourcePathError::Source(SourceError::Parse(ParseError {
                        span: None,
                        code: None,
                        line: 1,
                        message: "function index remap failed while merging imported modules"
                            .to_string(),
                    }))
                })?;
            }
        }
        Stmt::Expr { expr, .. } => {
            remap_expr_indices(expr, local_base, function_map, flat_index_by_symbol)?;
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            remap_expr_indices(condition, local_base, function_map, flat_index_by_symbol)?;
            for stmt in then_branch {
                remap_stmt_indices(stmt, local_base, function_map, flat_index_by_symbol)?;
            }
            for stmt in else_branch {
                remap_stmt_indices(stmt, local_base, function_map, flat_index_by_symbol)?;
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            remap_stmt_indices(init, local_base, function_map, flat_index_by_symbol)?;
            remap_expr_indices(condition, local_base, function_map, flat_index_by_symbol)?;
            remap_stmt_indices(post, local_base, function_map, flat_index_by_symbol)?;
            for stmt in body {
                remap_stmt_indices(stmt, local_base, function_map, flat_index_by_symbol)?;
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            remap_expr_indices(condition, local_base, function_map, flat_index_by_symbol)?;
            for stmt in body {
                remap_stmt_indices(stmt, local_base, function_map, flat_index_by_symbol)?;
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
        Stmt::Drop { index, .. } => {
            *index = remap_local_index(*index, local_base)?;
        }
    }
    Ok(())
}

fn remap_expr_indices(
    expr: &mut Expr,
    local_base: usize,
    function_map: &HashMap<u16, u16>,
    flat_index_by_symbol: &HashMap<SymbolId, u16>,
) -> Result<(), SourcePathError> {
    match expr {
        Expr::Null
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Bytes(_)
        | Expr::String(_) => {}
        Expr::FunctionRef(index, _) => {
            if let Some(remapped_index) = function_map.get(index).copied() {
                *index = remapped_index;
            } else if BuiltinFunction::from_call_index(*index).is_none() {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: "function index remap failed while merging imported modules"
                        .to_string(),
                })));
            }
        }
        Expr::ModuleFunctionRef(symbol, _) => {
            let flat = flat_index_by_symbol.get(symbol).copied().ok_or_else(|| {
                SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message:
                        "resolved module function value target is missing from the merged function table"
                            .to_string(),
                }))
            })?;
            *expr = Expr::FunctionRef(flat, std::mem::take(&mut expr_type_args(expr)));
        }
        Expr::UnresolvedFunctionRef { .. } => {
            // The loader resolves every function-value reference before
            // merge; reaching the merge means resolution missed a site.
            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                span: None,
                code: None,
                line: 1,
                message: "unresolved function value reference reached the module merge".to_string(),
            })));
        }
        Expr::Call(index, _, args) => {
            if let Some(remapped_index) = function_map.get(index).copied() {
                *index = remapped_index;
            } else if BuiltinFunction::from_call_index(*index).is_none() {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: "function index remap failed while merging imported modules"
                        .to_string(),
                })));
            }
            for arg in args {
                remap_expr_indices(arg, local_base, function_map, flat_index_by_symbol)?;
            }
        }
        Expr::ModuleCall(symbol, type_args, args) => {
            for arg in args.iter_mut() {
                remap_expr_indices(arg, local_base, function_map, flat_index_by_symbol)?;
            }
            let flat = flat_index_by_symbol.get(symbol).copied().ok_or_else(|| {
                SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message:
                        "resolved module call target is missing from the merged function table"
                            .to_string(),
                }))
            })?;
            *expr = Expr::Call(flat, std::mem::take(type_args), std::mem::take(args));
        }
        Expr::OptionalGet {
            container,
            key,
            container_slot,
            key_slot,
        } => {
            *container_slot = remap_local_index(*container_slot, local_base)?;
            *key_slot = remap_local_index(*key_slot, local_base)?;
            remap_expr_indices(container, local_base, function_map, flat_index_by_symbol)?;
            remap_expr_indices(key, local_base, function_map, flat_index_by_symbol)?;
        }
        Expr::OptionUnwrapOr {
            value,
            value_slot,
            fallback,
        } => {
            *value_slot = remap_local_index(*value_slot, local_base)?;
            remap_expr_indices(value, local_base, function_map, flat_index_by_symbol)?;
            remap_expr_indices(fallback, local_base, function_map, flat_index_by_symbol)?;
        }
        Expr::LocalCall(index, _, args) => {
            *index = remap_local_index(*index, local_base)?;
            for arg in args {
                remap_expr_indices(arg, local_base, function_map, flat_index_by_symbol)?;
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
            remap_expr_indices(
                &mut closure.body,
                local_base,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Expr::ClosureCall(closure, args) => {
            for param_slot in &mut closure.param_slots {
                *param_slot = remap_local_index(*param_slot, local_base)?;
            }
            for (source_index, captured_slot) in &mut closure.capture_copies {
                *source_index = remap_local_index(*source_index, local_base)?;
                *captured_slot = remap_local_index(*captured_slot, local_base)?;
            }
            remap_expr_indices(
                &mut closure.body,
                local_base,
                function_map,
                flat_index_by_symbol,
            )?;
            for arg in args {
                remap_expr_indices(arg, local_base, function_map, flat_index_by_symbol)?;
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
            remap_expr_indices(lhs, local_base, function_map, flat_index_by_symbol)?;
            remap_expr_indices(rhs, local_base, function_map, flat_index_by_symbol)?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            remap_expr_indices(inner, local_base, function_map, flat_index_by_symbol)?;
        }
        Expr::Var(index) | Expr::MoveVar(index) => {
            *index = remap_local_index(*index, local_base)?;
        }
        Expr::MoveField { root, .. } | Expr::MoveIndex { root, .. } => {
            *root = remap_local_index(*root, local_base)?;
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            remap_expr_indices(condition, local_base, function_map, flat_index_by_symbol)?;
            remap_expr_indices(then_expr, local_base, function_map, flat_index_by_symbol)?;
            remap_expr_indices(else_expr, local_base, function_map, flat_index_by_symbol)?;
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
            remap_expr_indices(value, local_base, function_map, flat_index_by_symbol)?;
            for (pattern, arm_expr) in arms {
                if let crate::compiler::ir::MatchPattern::SomeBinding(binding_slot) = pattern {
                    *binding_slot = remap_local_index(*binding_slot, local_base)?;
                }
                remap_expr_indices(arm_expr, local_base, function_map, flat_index_by_symbol)?;
            }
            remap_expr_indices(default, local_base, function_map, flat_index_by_symbol)?;
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                remap_stmt_indices(stmt, local_base, function_map, flat_index_by_symbol)?;
            }
            remap_expr_indices(expr, local_base, function_map, flat_index_by_symbol)?;
        }
    }
    Ok(())
}

/// Borrow the type arguments of a resolved function-value node.
///
/// Only used while converting a [`Expr::ModuleFunctionRef`] into a plain
/// [`Expr::FunctionRef`] in place.
fn expr_type_args(expr: &mut Expr) -> Vec<super::ir::TypeSchema> {
    match expr {
        Expr::ModuleFunctionRef(_, type_args) => std::mem::take(type_args),
        _ => Vec::new(),
    }
}
