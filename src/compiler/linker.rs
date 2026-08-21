use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::builtins::BuiltinFunction;

use super::{
    ParseError, SourceError, SourcePathError,
    ir::{
        CatalogVisibility, Expr, FrontendIr, FunctionDecl, FunctionDeclSite, FunctionImpl,
        FunctionRefSite, FunctionRefTarget, HostApiIrMetadata, LocalDeclSite, LocalRefSite,
        LocalSlot, ModuleNamespaceAlias, ParsedCallSite, ParsedCallTarget, ParsedLexicalScope,
        ParsedSemanticIndex, ScopeId, SemanticNodeId, Stmt, StructDecl, StructDeclSite,
    },
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
    // Fingerprint-bound host candidate catalog carried by the merged IR. Held
    // as `None` until the first supplied unit that carries catalog metadata;
    // the final value mirrors the uniform metadata-presence state across every
    // supplied unit, including zero-function units (see
    // `merge_host_api_metadata_for_unit`).
    let mut merged_host_api_metadata: Option<HostApiIrMetadata> = None;
    // Set when a supplied unit without catalog metadata has been merged.
    // A later supplied unit that *does* carry metadata is a split
    // catalog/no-catalog compilation and is rejected.
    let mut rejected_missing_metadata = false;

    // Milestone 4 flat identity maps.
    //
    // Module functions (declarations with implementations) are merged by
    // compiler-owned `SymbolId`, so same-named declarations in independent
    // modules each get their own flat entry. Host imports (declarations
    // without implementations) are deduplicated by `(name, arity)`: the same
    // name at the same arity merges into one flat *candidate-set identity*
    // carrying the compiler's full discovery-order candidate list. This flat
    // identity is a linker-stage dedup key only — it is not a runtime binding.
    // Later typing resolves each call site to the exact `HostFunctionSchema`
    // for the chosen candidate (passing modes, return type, and any referenced
    // resource schemas), and the VMBC `HostImport`/runtime registry bind that
    // resolved schema identity. The same name at a different arity remains a
    // distinct flat identity with its own entry and independent candidate set.
    let mut flat_index_by_symbol = HashMap::<SymbolId, u16>::new();
    let mut host_index_by_arity = HashMap::<(String, u8), u16>::new();
    // Every flat name claimed so far. Module functions that collide are
    // deterministically mangled with their module identity; host imports
    // are deduplicated by `(name, arity)` before ever reaching this set.
    let mut claimed_flat_names = HashSet::<String>::new();

    let mut local_base = 0usize;

    // Merged parser provenance carrier. Every unit parsed in module mode
    // carries a `Some` parsed semantic index whose ids start at zero; the
    // merge rebases each unit's [`SemanticNodeId`] and [`ScopeId`] by the
    // running totals so the merged index stays collision-free, and remaps
    // local slots and function indices exactly like the IR statements it
    // describes. Units without provenance (REPL fixtures, test IR) simply
    // contribute nothing; the merged carrier is `Some` iff at least one
    // supplied unit carried one.
    let mut merged_parsed_index: Option<ParsedSemanticIndex> = None;
    // Merged catalog visibility. Alias maps are merged deterministically in
    // unit order with deduplication; a conflicting alias (same alias name
    // mapping to different canonical targets across units) is a typed error.
    let mut merged_catalog_visibility: Option<CatalogVisibility> = None;
    // Merged lexer token stream: the concatenation of every unit's tokens in
    // unit order. Token spans carry their owning source ids, so no rebasing
    // is required.
    let mut merged_lexer_tokens: Vec<crate::compiler::ir::LexerToken> = Vec::new();

    for unit in units {
        let source_name = unit.source_name.clone();
        let function_map = register_unit_functions(
            &unit,
            &mut merged_functions,
            &mut flat_index_by_symbol,
            &mut host_index_by_arity,
            &mut claimed_flat_names,
        )?;
        merge_host_api_metadata_for_unit(
            &unit,
            &source_name,
            &function_map,
            &mut merged_host_api_metadata,
            &mut rejected_missing_metadata,
        )?;
        let unit_local_base = local_base;
        let unit_local_count = unit.parsed.locals;

        // Node/scope id bases for this unit: the running totals of the merged
        // carrier. Every parser-produced id in this unit starts at zero, so
        // rebasing by these offsets keeps the merged index collision-free
        // while preserving each unit's internal ordering.
        let node_offset = merged_parsed_index
            .as_ref()
            .map(|merged| merged.next_node_id)
            .unwrap_or(0);
        let scope_offset = merged_parsed_index
            .as_ref()
            .map(|merged| merged.next_scope_id)
            .unwrap_or(0);

        if let Some(unit_index) = &unit.parsed.parsed_semantic_index {
            let rebased = rebase_parsed_semantic_index(
                unit_index,
                node_offset,
                scope_offset,
                unit_local_base,
                &function_map,
            )?;
            match &mut merged_parsed_index {
                Some(merged) => merge_parsed_semantic_index(merged, rebased),
                None => merged_parsed_index = Some(rebased),
            }
        }
        if let Some(visibility) = &unit.parsed.catalog_visibility {
            match &mut merged_catalog_visibility {
                Some(merged) => merge_catalog_visibility(merged, visibility, &source_name)?,
                None => {
                    // Tag the first unit's module namespace aliases with their
                    // owning source so the merged carrier is uniformly
                    // source-keyed from the start, and reject any genuine
                    // same-source conflict (same alias, different module).
                    let mut owned = visibility.clone();
                    for alias in &mut owned.module_namespace_aliases {
                        if alias.source.is_empty() {
                            alias.source = source_name.clone();
                        }
                    }
                    for alias in &owned.module_namespace_aliases {
                        if let Some(existing) =
                            owned.module_namespace_aliases.iter().find(|existing| {
                                existing.alias == alias.alias
                                    && existing.module_path != alias.module_path
                            })
                        {
                            return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                                span: None,
                                code: None,
                                line: 1,
                                message: format!(
                                    "module namespace alias conflict ({source_name}): alias '{}' maps to both '{}' and '{}'",
                                    alias.alias, existing.module_path, alias.module_path
                                ),
                            })));
                        }
                    }
                    merged_catalog_visibility = Some(owned);
                }
            }
        }

        merged_lexer_tokens.extend(unit.parsed.lexer_tokens.iter().cloned());

        let mut remapped_stmts = unit.parsed.stmts;
        for stmt in &mut remapped_stmts {
            remap_stmt_indices(
                stmt,
                unit_local_base,
                node_offset,
                &function_map,
                &flat_index_by_symbol,
            )?;
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
                remap_stmt_indices(
                    stmt,
                    unit_local_base,
                    node_offset,
                    &function_map,
                    &flat_index_by_symbol,
                )?;
            }
            remap_expr_indices(
                &mut function_impl.body_expr,
                unit_local_base,
                node_offset,
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
        // Fingerprint-bound host candidate catalog carried by the merged IR.
        // `None` when no supplied unit carried catalog metadata; otherwise the
        // validated, remapped, uniformly fingerprint-bound carrier.
        host_api_metadata: merged_host_api_metadata,
        semantic_index: None,
        parsed_semantic_index: merged_parsed_index,
        catalog_visibility: merged_catalog_visibility,
        // Lexer token streams concatenate in unit order; token spans carry
        // their owning source ids and need no rebasing.
        lexer_tokens: merged_lexer_tokens,
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

fn metadata_error(source_name: &str, message: String) -> SourcePathError {
    SourcePathError::Source(SourceError::Parse(ParseError {
        span: None,
        code: None,
        line: 1,
        message: format!("host metadata ({source_name}): {message}"),
    }))
}

/// Merge one unit's fingerprint-bound host candidate metadata onto the
/// compilation-wide carrier.
///
/// Presence is uniform across **every supplied unit**, including zero-function
/// units: an empty unit carries catalog content solely through its metadata
/// carrier and still asserts or refutes metadata presence. An empty unit that
/// carries `Some` metadata contributes its fingerprint with zero recorded
/// candidates; a zero-function unit with `None` refutes presence. Mixing
/// `Some`/`None` is rejected so a compilation is never split between a
/// catalog-backed module and a catalog-less one — in either order. An empty
/// `Vec<ParsedUnit>` yields `None`.
///
/// For `Some` metadata, every unit must be bound to the same catalog
/// [`HostApiFingerprint`](crate::host_api::HostApiFingerprint). Each recorded
/// unit function index is validated and remapped through the unit's
/// `function_map` onto its merged flat index:
/// * the index must name a matching unit [`FunctionDecl`];
/// * that function must be implementation-less (a host import);
/// * a `function_map` entry must exist;
/// * a candidate set must be present, whose schemas all match the declared
///   name and arity.
///
/// Each ordered candidate list is the **complete** catalog discovery-order
/// candidate set for the owning `(fingerprint, host name, arity)` — every
/// candidate the catalog discovered for that identity, including all type and
/// parameter-passing overloads, never a per-call subset or an arbitrary
/// slice. The list is recorded verbatim at the merged index. When the same
/// `(name, arity)` host import is deduplicated across units, the candidate
/// lists must be exactly equal — any difference is a conflict, never a union
/// or overwrite. The same host name at a different arity is a distinct flat
/// function with its own complete candidate set.
fn merge_host_api_metadata_for_unit(
    unit: &ParsedUnit,
    source_name: &str,
    function_map: &HashMap<u16, u16>,
    merged: &mut Option<HostApiIrMetadata>,
    rejected_missing_metadata: &mut bool,
) -> Result<(), SourcePathError> {
    let Some(metadata) = &unit.parsed.host_api_metadata else {
        if merged.is_some() {
            return Err(metadata_error(
                &source_name,
                "this module carries no host catalog metadata while another imported module does"
                    .to_string(),
            ));
        }
        *rejected_missing_metadata = true;
        return Ok(());
    };
    if *rejected_missing_metadata {
        return Err(metadata_error(
            &source_name,
            "this module carries host catalog metadata while another imported module does not"
                .to_string(),
        ));
    }
    match merged {
        None => *merged = Some(HostApiIrMetadata::new(metadata.fingerprint())),
        Some(existing) => {
            if existing.fingerprint() != metadata.fingerprint() {
                return Err(metadata_error(
                    &source_name,
                    format!(
                        "host catalog fingerprint mismatch ({} vs {})",
                        existing.fingerprint(),
                        metadata.fingerprint()
                    ),
                ));
            }
        }
    }
    let target = merged.as_mut().expect("metadata carrier is present above");

    // Replay the unit's candidate lists in sorted unit-index order, remapping
    // each onto its merged flat index.
    for unit_index in metadata.function_indices() {
        let merged_index = function_map.get(&unit_index).copied().ok_or_else(|| {
            metadata_error(
                &source_name,
                format!(
                    "host metadata references function index {unit_index} with no merged entry"
                ),
            )
        })?;
        let declaration = unit
            .parsed
            .functions
            .iter()
            .find(|function| function.index == unit_index)
            .ok_or_else(|| {
                metadata_error(
                    &source_name,
                    format!("host metadata references missing function index {unit_index}"),
                )
            })?;
        if unit.parsed.function_impls.contains_key(&unit_index) {
            return Err(metadata_error(
                &source_name,
                format!(
                    "host metadata recorded for function index {unit_index} which has an implementation; metadata is only valid for host imports"
                ),
            ));
        }
        let candidates = metadata.candidates(unit_index).ok_or_else(|| {
            metadata_error(
                &source_name,
                format!(
                    "host metadata records no candidate schemas for function index {unit_index}"
                ),
            )
        })?;
        for candidate in candidates {
            if candidate.name != declaration.name {
                return Err(metadata_error(
                    &source_name,
                    format!(
                        "host candidate '{}' name does not match declaration '{}' for function index {unit_index}",
                        candidate.name, declaration.name
                    ),
                ));
            }
            if candidate.params.len() != usize::from(declaration.arity) {
                return Err(metadata_error(
                    &source_name,
                    format!(
                        "host candidate '{}' arity {} does not match declaration arity {} for function index {unit_index}",
                        candidate.name,
                        candidate.params.len(),
                        declaration.arity
                    ),
                ));
            }
        }
        // Record at the merged index, or require an exact deduplicated match
        // when the same host name already contributed candidates.
        if target.candidates(merged_index).is_none() {
            let clones = candidates.to_vec();
            target
                .record_candidates(merged_index, clones)
                .map_err(|error| SourcePathError::Source(SourceError::Parse(error)))?;
        } else if target.candidates(merged_index) != Some(candidates) {
            return Err(metadata_error(
                &source_name,
                format!(
                    "host candidate conflict for merged function index {merged_index} (host '{}')",
                    declaration.name
                ),
            ));
        }
    }
    Ok(())
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
    host_index_by_arity: &mut HashMap<(String, u8), u16>,
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
            // Host import: `(name, arity)`-keyed deduplication. The same name
            // at the same arity collapses to one flat candidate-set identity
            // (full discovery-order candidate list retained) rather than a
            // runtime binding; the same name at a different arity is a distinct
            // overload with its own flat identity and candidate set.
            let host_identity = (func.name.clone(), func.arity);
            if let Some(&existing) = host_index_by_arity.get(&host_identity) {
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
            host_index_by_arity.insert(host_identity, flat);
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

/// Apply the `(name, arity)`-bound host-import merge rules for a host import
/// that is declared by more than one unit at the same name **and** arity (the
/// flat dedup key). Different arities of the same host name are distinct flat
/// functions and never reach this helper, so the caller guarantees
/// `existing.arity == func.arity`; the arity branch is therefore not needed
/// here. `Unknown` return types are refined, and schemas/type parameters
/// merge; conflicting non-`Unknown` returns or arg schemas are errors.
fn merge_host_import_metadata(
    existing: &mut FunctionDecl,
    func: &FunctionDecl,
) -> Result<(), SourcePathError> {
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
    node_offset: u32,
    function_map: &HashMap<u16, u16>,
    flat_index_by_symbol: &HashMap<SymbolId, u16>,
) -> Result<(), SourcePathError> {
    match stmt {
        Stmt::Noop { .. } => {}
        Stmt::Let { index, expr, .. } => {
            *index = remap_local_index(*index, local_base)?;
            remap_expr_indices(
                expr,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Stmt::Assign { index, expr, .. } => {
            *index = remap_local_index(*index, local_base)?;
            remap_expr_indices(
                expr,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Stmt::ClosureLet { closure, .. } => {
            for (source_index, captured_slot) in &mut closure.capture_copies {
                *source_index = remap_local_index(*source_index, local_base)?;
                *captured_slot = remap_local_index(*captured_slot, local_base)?;
            }
            remap_expr_indices(
                &mut closure.body,
                local_base,
                node_offset,
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
            remap_expr_indices(
                expr,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            remap_expr_indices(
                condition,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            for stmt in then_branch {
                remap_stmt_indices(
                    stmt,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
            }
            for stmt in else_branch {
                remap_stmt_indices(
                    stmt,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            remap_stmt_indices(
                init,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_expr_indices(
                condition,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_stmt_indices(
                post,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            for stmt in body {
                remap_stmt_indices(
                    stmt,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            remap_expr_indices(
                condition,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            for stmt in body {
                remap_stmt_indices(
                    stmt,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
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
    node_offset: u32,
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
        Expr::Call(index, _, args, _, semantic_id) => {
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
            rebase_semantic_id(semantic_id, node_offset);
            for arg in args {
                remap_expr_indices(
                    arg,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
            }
        }
        Expr::ModuleCall(symbol, type_args, args, semantic_id) => {
            for arg in args.iter_mut() {
                remap_expr_indices(
                    arg,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
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
            *expr = Expr::Call(
                flat,
                std::mem::take(type_args),
                std::mem::take(args),
                None,
                rebase_optional_semantic_id(*semantic_id, node_offset),
            );
        }
        Expr::OptionalGet {
            container,
            key,
            container_slot,
            key_slot,
            semantic_id,
        } => {
            *container_slot = remap_local_index(*container_slot, local_base)?;
            *key_slot = remap_local_index(*key_slot, local_base)?;
            rebase_semantic_id(semantic_id, node_offset);
            remap_expr_indices(
                container,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_expr_indices(
                key,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Expr::OptionUnwrapOr {
            value,
            value_slot,
            fallback,
            semantic_id,
        } => {
            *value_slot = remap_local_index(*value_slot, local_base)?;
            rebase_semantic_id(semantic_id, node_offset);
            remap_expr_indices(
                value,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_expr_indices(
                fallback,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Expr::LocalCall(index, _, args, semantic_id) => {
            *index = remap_local_index(*index, local_base)?;
            rebase_semantic_id(semantic_id, node_offset);
            for arg in args {
                remap_expr_indices(
                    arg,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
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
                node_offset,
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
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            for arg in args {
                remap_expr_indices(
                    arg,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
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
            remap_expr_indices(
                lhs,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_expr_indices(
                rhs,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            remap_expr_indices(
                inner,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
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
            remap_expr_indices(
                condition,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_expr_indices(
                then_expr,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            remap_expr_indices(
                else_expr,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
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
            remap_expr_indices(
                value,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
            for (pattern, arm_expr) in arms {
                if let crate::compiler::ir::MatchPattern::SomeBinding(binding_slot) = pattern {
                    *binding_slot = remap_local_index(*binding_slot, local_base)?;
                }
                remap_expr_indices(
                    arm_expr,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
            }
            remap_expr_indices(
                default,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                remap_stmt_indices(
                    stmt,
                    local_base,
                    node_offset,
                    function_map,
                    flat_index_by_symbol,
                )?;
            }
            remap_expr_indices(
                expr,
                local_base,
                node_offset,
                function_map,
                flat_index_by_symbol,
            )?;
        }
    }
    Ok(())
}

/// Rebase a parser-assigned call-site [`SemanticNodeId`] by the unit's node
/// offset so merged IR from multiple units stays collision-free.
fn rebase_semantic_id(semantic_id: &mut Option<SemanticNodeId>, node_offset: u32) {
    if let Some(id) = semantic_id.as_mut() {
        id.0 =
            id.0.checked_add(node_offset)
                .expect("semantic node id overflow");
    }
}

fn rebase_optional_semantic_id(
    semantic_id: Option<SemanticNodeId>,
    node_offset: u32,
) -> Option<SemanticNodeId> {
    semantic_id.map(|mut id| {
        id.0 =
            id.0.checked_add(node_offset)
                .expect("semantic node id overflow");
        id
    })
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

/// Rebase one unit's parser provenance onto the merged id space.
///
/// Every parser-produced [`SemanticNodeId`] and [`ScopeId`] starts at zero
/// per unit; adding the running merged totals yields a collision-free merged
/// index that preserves each unit's internal ordering. Local slots are
/// remapped by the unit's `local_base` and function indices through the
/// unit's `function_map` exactly like the IR statements they describe, so
/// the merged index stays consistent with the merged `Expr`/`Stmt` trees.
/// Spans are copied verbatim — their `source_id` already names the owning
/// compilation-wide source and must never be rewritten.
fn rebase_parsed_semantic_index(
    unit: &ParsedSemanticIndex,
    node_offset: u32,
    scope_offset: u32,
    local_base: usize,
    function_map: &HashMap<u16, u16>,
) -> Result<ParsedSemanticIndex, SourcePathError> {
    let remap_node = |id: SemanticNodeId| -> SemanticNodeId {
        SemanticNodeId(
            id.0.checked_add(node_offset)
                .expect("semantic node id overflow"),
        )
    };
    let remap_scope = |id: ScopeId| id.checked_add(scope_offset).expect("scope id overflow");
    let remap_slot = |slot: LocalSlot| remap_local_index(slot, local_base);
    // Remap a recorded unit-local function index through the unit's flat
    // `function_map`. Indices the map does not cover are either builtins
    // (which keep their reserved index space) or implicit-extern indices from
    // loader-resolved module calls. The latter are never rewritten by the
    // loader — the call-site *target* is upgraded to `Module(symbol)` for the
    // actual call, while an orphaned `func_ref`/`func_decl` for the resolved
    // decl keeps its unit-local index. Those are preserved verbatim: the
    // merged flat index is unknowable for a symbol-less decl, and the merged
    // IR carries the correct flat target on the lowered `Expr::Call` node.
    let remap_function = |index: u16| -> u16 {
        if let Some(remapped) = function_map.get(&index).copied() {
            return remapped;
        }
        index
    };

    let mut call_sites = Vec::with_capacity(unit.call_sites.len());
    for site in &unit.call_sites {
        let target = match site.target {
            ParsedCallTarget::Function(index) => ParsedCallTarget::Function(remap_function(index)),
            ParsedCallTarget::Local(slot) => ParsedCallTarget::Local(remap_slot(slot)?),
            // Module targets carry a compilation-wide [`SymbolId`]; the
            // merged IR keeps the symbol identity, so no remap applies.
            ParsedCallTarget::Module(symbol) => ParsedCallTarget::Module(symbol),
            ParsedCallTarget::Unresolved => ParsedCallTarget::Unresolved,
        };
        call_sites.push(ParsedCallSite {
            id: remap_node(site.id),
            callee_span: site.callee_span,
            expr_span: site.expr_span,
            target,
            name: site.name.clone(),
            scope_id: remap_scope(site.scope_id),
            is_namespace_call: site.is_namespace_call,
        });
    }

    let mut local_decls = Vec::with_capacity(unit.local_decls.len());
    for decl in &unit.local_decls {
        local_decls.push(LocalDeclSite {
            id: remap_node(decl.id),
            ident_span: decl.ident_span,
            stmt_span: decl.stmt_span,
            slot: remap_slot(decl.slot)?,
            name: decl.name.clone(),
            scope_id: remap_scope(decl.scope_id),
            decl_order: decl.decl_order,
        });
    }

    let mut local_refs = Vec::with_capacity(unit.local_refs.len());
    for reference in &unit.local_refs {
        local_refs.push(LocalRefSite {
            id: remap_node(reference.id),
            ident_span: reference.ident_span,
            slot: remap_slot(reference.slot)?,
            name: reference.name.clone(),
            scope_id: remap_scope(reference.scope_id),
        });
    }

    let mut func_decls = Vec::with_capacity(unit.func_decls.len());
    for decl in &unit.func_decls {
        func_decls.push(FunctionDeclSite {
            id: remap_node(decl.id),
            ident_span: decl.ident_span,
            function_index: remap_function(decl.function_index),
            name: decl.name.clone(),
            scope_id: remap_scope(decl.scope_id),
            decl_order: decl.decl_order,
        });
    }

    // Struct declarations carry no flat function index; only the node id and
    // scope id are rebased, and the spans are copied verbatim (their source id
    // already names the owning compilation-wide source).
    let mut struct_decls = Vec::with_capacity(unit.struct_decls.len());
    for decl in &unit.struct_decls {
        struct_decls.push(StructDeclSite {
            id: remap_node(decl.id),
            ident_span: decl.ident_span,
            decl_span: decl.decl_span,
            name: decl.name.clone(),
            scope_id: remap_scope(decl.scope_id),
        });
    }

    let mut func_refs = Vec::with_capacity(unit.func_refs.len());
    for reference in &unit.func_refs {
        let target = match reference.target {
            FunctionRefTarget::Function(index) => {
                FunctionRefTarget::Function(remap_function(index))
            }
            // Module targets carry a compilation-wide [`SymbolId`]; the
            // merged IR keeps the symbol identity, so no remap applies.
            FunctionRefTarget::Module(symbol) => FunctionRefTarget::Module(symbol),
        };
        func_refs.push(FunctionRefSite {
            id: remap_node(reference.id),
            ident_span: reference.ident_span,
            target,
            name: reference.name.clone(),
            scope_id: remap_scope(reference.scope_id),
        });
    }

    let mut scopes = Vec::with_capacity(unit.scopes.len());
    for scope in &unit.scopes {
        let mut declarations = Vec::with_capacity(scope.declarations.len());
        for slot in &scope.declarations {
            declarations.push(remap_slot(*slot)?);
        }
        let mut functions = Vec::with_capacity(scope.functions.len());
        for index in &scope.functions {
            functions.push(remap_function(*index));
        }
        scopes.push(ParsedLexicalScope {
            id: remap_scope(scope.id),
            parent: scope.parent.map(remap_scope),
            range: scope.range,
            declarations,
            functions,
        });
    }

    // Statement spans carry their owning source id and are copied verbatim:
    // the line key and exact span are both parser-origin and independent of
    // the merged id space.
    let stmt_spans = unit.stmt_spans.clone();

    Ok(ParsedSemanticIndex {
        call_sites,
        local_decls,
        local_refs,
        func_decls,
        struct_decls,
        func_refs,
        scopes,
        stmt_spans,
        next_node_id: checked_node_total(unit.next_node_id, node_offset)?,
        next_scope_id: checked_scope_total(unit.next_scope_id, scope_offset)?,
    })
}

/// Checked addition for the merged node-id running total. Linking failure is
/// reported as a typed [`SourcePathError`] instead of wrapping.
fn checked_node_total(unit_total: u32, node_offset: u32) -> Result<u32, SourcePathError> {
    unit_total.checked_add(node_offset).ok_or_else(|| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "merged semantic node id space exhausted (u32 overflow)".to_string(),
        }))
    })
}

/// Checked addition for the merged scope-id running total. Linking failure is
/// reported as a typed [`SourcePathError`] instead of wrapping.
fn checked_scope_total(unit_total: u32, scope_offset: u32) -> Result<u32, SourcePathError> {
    unit_total.checked_add(scope_offset).ok_or_else(|| {
        SourcePathError::Source(SourceError::Parse(ParseError {
            span: None,
            code: None,
            line: 1,
            message: "merged scope id space exhausted (u32 overflow)".to_string(),
        }))
    })
}

/// Append one rebased unit index onto the merged carrier. The rebased unit's
/// ids occupy the contiguous range starting at the previous merged totals, so
/// appending preserves collision-freedom and the running `next_*` counters.
fn merge_parsed_semantic_index(merged: &mut ParsedSemanticIndex, unit: ParsedSemanticIndex) {
    debug_assert!(merged.next_node_id <= unit.next_node_id);
    debug_assert!(merged.next_scope_id <= unit.next_scope_id);
    merged.call_sites.extend(unit.call_sites);
    merged.local_decls.extend(unit.local_decls);
    merged.local_refs.extend(unit.local_refs);
    merged.func_decls.extend(unit.func_decls);
    merged.struct_decls.extend(unit.struct_decls);
    merged.func_refs.extend(unit.func_refs);
    merged.scopes.extend(unit.scopes);
    merged.stmt_spans.extend(unit.stmt_spans);
    merged.next_node_id = unit.next_node_id;
    merged.next_scope_id = unit.next_scope_id;
}

/// Merge one unit's parser visibility onto the compilation-wide carrier.
///
/// Host namespace and direct host call aliases map to global canonical host
/// names, so an alias present in two units must map to the identical target
/// (deduplicated) or the merge fails with a typed [`SourcePathError`].
/// Module namespace aliases are different: they are unit-local bindings whose
/// canonical values are module-relative import paths (`c` vs `self::c` name
/// the same module from different importers), so the same alias legitimately
/// names different modules in different sources. They merge keyed by owning
/// source: entries from the same source deduplicate on identical
/// (alias, path) and error on a genuine same-source conflict, while entries
/// from different sources are all retained so per-module query context never
/// collapses. Wildcard import sets are deduplicated unions. Structured `use`
/// declarations are appended with exact (path, clause) duplicates dropped;
/// spans are never compared, so identical directives from different sources
/// collapse to one entry.
fn merge_catalog_visibility(
    merged: &mut CatalogVisibility,
    unit: &CatalogVisibility,
    source_name: &str,
) -> Result<(), SourcePathError> {
    merge_alias_vec(
        &mut merged.host_namespace_aliases,
        &unit.host_namespace_aliases,
        source_name,
        "host namespace",
    )?;
    merge_alias_vec(
        &mut merged.direct_host_call_aliases,
        &unit.direct_host_call_aliases,
        source_name,
        "direct host call",
    )?;
    // Module namespace aliases are unit-local: dedupe within the owning
    // source, retain across sources, and reject a genuine same-source
    // conflict (which the parser's own alias map already prevents, but the
    // merge defends against mixed hand-built carriers).
    for alias in &unit.module_namespace_aliases {
        let same_source = merged
            .module_namespace_aliases
            .iter()
            .filter(|existing| existing.source == source_name && existing.alias == alias.alias)
            .collect::<Vec<_>>();
        if let Some(existing) = same_source.first() {
            if existing.module_path != alias.module_path {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: format!(
                        "module namespace alias conflict ({source_name}): alias '{}' maps to both '{}' and '{}'",
                        alias.alias, existing.module_path, alias.module_path
                    ),
                })));
            }
            continue;
        }
        merged.module_namespace_aliases.push(ModuleNamespaceAlias {
            alias: alias.alias.clone(),
            module_path: alias.module_path.clone(),
            source: source_name.to_string(),
        });
    }
    for prefix in &unit.direct_host_wildcard_imports {
        if !merged.direct_host_wildcard_imports.contains(prefix) {
            merged.direct_host_wildcard_imports.push(prefix.clone());
        }
    }
    for decl in &unit.use_declarations {
        if !merged
            .use_declarations
            .iter()
            .any(|existing| use_decl_semantic_eq(existing, decl))
        {
            merged.use_declarations.push(decl.clone());
        }
    }
    Ok(())
}

/// Deterministically merge one alias vector: identical entries deduplicate,
/// conflicting aliases (same name, different canonical target) error.
fn merge_alias_vec(
    merged: &mut Vec<(String, String)>,
    unit: &[(String, String)],
    source_name: &str,
    kind: &str,
) -> Result<(), SourcePathError> {
    for (alias, canonical) in unit {
        if let Some((_, existing)) = merged.iter().find(|(name, _)| name == alias) {
            if existing != canonical {
                return Err(SourcePathError::Source(SourceError::Parse(ParseError {
                    span: None,
                    code: None,
                    line: 1,
                    message: format!(
                        "catalog alias conflict ({source_name}): {kind} alias '{alias}' maps to both '{existing}' and '{canonical}'"
                    ),
                })));
            }
            continue;
        }
        merged.push((alias.clone(), canonical.clone()));
    }
    Ok(())
}

/// Semantic equality of two `use` directives: identical path and clause.
/// Spans and lines are per-source and never compared.
fn use_decl_semantic_eq(
    lhs: &crate::compiler::modules::UseDecl,
    rhs: &crate::compiler::modules::UseDecl,
) -> bool {
    use crate::compiler::source_loader::ImportClause;
    let path_eq = lhs.path.len() == rhs.path.len()
        && lhs
            .path
            .iter()
            .zip(rhs.path.iter())
            .all(|(a, b)| use_path_segment_eq(a, b));
    if !path_eq {
        return false;
    }
    match (&lhs.clause, &rhs.clause) {
        (ImportClause::AllPublic, ImportClause::AllPublic) => true,
        (ImportClause::Namespace(a), ImportClause::Namespace(b)) => a == b,
        (ImportClause::Prefix(a), ImportClause::Prefix(b)) => a == b,
        (ImportClause::Named(a), ImportClause::Named(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| x.imported == y.imported && x.local == y.local)
        }
        _ => false,
    }
}

fn use_path_segment_eq(
    lhs: &crate::compiler::modules::UsePathSegment,
    rhs: &crate::compiler::modules::UsePathSegment,
) -> bool {
    use crate::compiler::modules::UsePathSegment;
    match (lhs, rhs) {
        (UsePathSegment::Self_, UsePathSegment::Self_) => true,
        (UsePathSegment::Super, UsePathSegment::Super) => true,
        (UsePathSegment::Ident(a), UsePathSegment::Ident(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod linker_metadata_remap_tests {
    use super::super::ir::HostApiIrMetadata;
    use super::super::modules::{ModuleId, SymbolId};
    use super::*;
    use crate::host_api::{
        HostApiFingerprint, HostFunctionSchema, HostParamSchema, HostTypeSchema,
    };

    fn fingerprint(n: u64) -> HostApiFingerprint {
        serde_json::from_value(serde_json::Value::Number(n.into())).unwrap()
    }

    fn host_candidate(name: &str, params: Vec<HostParamSchema>) -> HostFunctionSchema {
        HostFunctionSchema::with_return(name, params, HostTypeSchema::Unknown)
    }

    fn symbol(module: u32, index: u32) -> SymbolId {
        SymbolId {
            module: ModuleId(module),
            index,
        }
    }

    fn decl(index: u16, name: &str, arity: u8, module: u32) -> FunctionDecl {
        FunctionDecl {
            name: name.to_string(),
            arity,
            index,
            args: Vec::new(),
            arg_schemas: Vec::new(),
            return_schema: None,
            type_params: Vec::new(),
            exported: false,
            return_type: crate::ValueType::Int,
            symbol: Some(symbol(module, index as u32)),
        }
    }

    fn simple_impl() -> FunctionImpl {
        FunctionImpl {
            param_slots: Vec::new(),
            capture_copies: Vec::new(),
            body_stmts: Vec::new(),
            body_expr: Expr::Int(1),
            body_expr_line: 1,
        }
    }

    fn metadata(
        fingerprint_n: u64,
        index: u16,
        candidates: Vec<HostFunctionSchema>,
    ) -> HostApiIrMetadata {
        let mut md = HostApiIrMetadata::new(fingerprint(fingerprint_n));
        md.record_candidates(index, candidates).unwrap();
        md
    }

    fn unit(
        source_name: &str,
        module: u32,
        functions: Vec<FunctionDecl>,
        function_impls: HashMap<u16, FunctionImpl>,
        host_api_metadata: Option<HostApiIrMetadata>,
    ) -> ParsedUnit {
        ParsedUnit {
            parsed: FrontendIr {
                stmts: Vec::new(),
                locals: 0,
                local_bindings: Vec::new(),
                struct_schemas: HashMap::new(),
                unknown_type_spans: Vec::new(),
                functions,
                function_impls,
                stmt_sources: Vec::new(),
                function_sources: HashMap::new(),
                use_declarations: Vec::new(),
                implicit_extern_names: Vec::new(),
                host_api_metadata: host_api_metadata,
                semantic_index: None,
                parsed_semantic_index: None,
                catalog_visibility: None,
                lexer_tokens: Vec::new(),
            },
            source_name: source_name.to_string(),
            scope_identity: None,
            module: ModuleId(module),
            source_id: 0,
        }
    }

    #[test]
    fn single_unit_source_index_remaps_to_merged_candidate() {
        // Single unit declares a host import at unit index 7; after merge the
        // candidate must land on the flat index 0.
        let u = unit(
            "catalog.rss",
            1,
            vec![decl(7, "read", 0, 1)],
            HashMap::new(),
            Some(metadata(1, 7, vec![host_candidate("read", vec![])])),
        );
        let merged = merge_units(vec![u]).expect("single-unit merge must succeed");
        assert_eq!(merged.functions.len(), 1);
        assert_eq!(merged.functions[0].index, 0);
        let md = merged
            .host_api_metadata
            .as_ref()
            .expect("metadata must be carried");
        assert_eq!(md.fingerprint(), fingerprint(1));
        assert_eq!(md.function_indices().collect::<Vec<_>>(), vec![0]);
        let candidates = md
            .candidates(0)
            .expect("candidate must be recorded at merged index 0");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "read");
    }

    #[test]
    fn same_host_and_fingerprint_units_dedup_to_single_merged_candidate() {
        // Two units declare the same host import with the same fingerprint and
        // identical candidate list; the merged catalog records it exactly once
        // at the shared merged index 0.
        let candidates = vec![host_candidate(
            "read",
            vec![HostParamSchema::value("bytes", HostTypeSchema::Bytes)],
        )];
        let a = unit(
            "a.rss",
            1,
            vec![decl(0, "read", 1, 1)],
            HashMap::new(),
            Some(metadata(1, 0, candidates.clone())),
        );
        let b = unit(
            "b.rss",
            2,
            vec![decl(0, "read", 1, 2)],
            HashMap::new(),
            Some(metadata(1, 0, candidates)),
        );
        let merged = merge_units(vec![a, b]).expect("dedup merge must succeed");
        assert_eq!(merged.functions.len(), 1);
        assert_eq!(merged.functions[0].index, 0);
        let md = merged
            .host_api_metadata
            .as_ref()
            .expect("metadata must be carried");
        assert_eq!(md.function_indices().count(), 1);
        assert_eq!(md.candidates(0).unwrap().len(), 1);
    }

    #[test]
    fn fingerprint_mismatch_across_units_is_rejected() {
        let a = unit(
            "a.rss",
            1,
            vec![decl(0, "read", 0, 1)],
            HashMap::new(),
            Some(metadata(1, 0, vec![host_candidate("read", vec![])])),
        );
        let b = unit(
            "b.rss",
            2,
            vec![decl(0, "read", 0, 2)],
            HashMap::new(),
            Some(metadata(2, 0, vec![host_candidate("read", vec![])])),
        );
        let err = merge_units(vec![a, b]).expect_err("fingerprint mismatch must fail");
        assert!(
            err.to_string().contains("fingerprint mismatch"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn candidate_conflict_with_same_fingerprint_is_rejected() {
        let a = unit(
            "a.rss",
            1,
            vec![decl(0, "f", 1, 1)],
            HashMap::new(),
            Some(metadata(
                1,
                0,
                vec![host_candidate(
                    "f",
                    vec![HostParamSchema::value("x", HostTypeSchema::Int)],
                )],
            )),
        );
        let b = unit(
            "b.rss",
            2,
            vec![decl(0, "f", 1, 2)],
            HashMap::new(),
            Some(metadata(
                1,
                0,
                vec![host_candidate(
                    "f",
                    vec![HostParamSchema::value("x", HostTypeSchema::String)],
                )],
            )),
        );
        let err = merge_units(vec![a, b]).expect_err("candidate conflict must fail");
        assert!(err.to_string().contains("conflict"), "unexpected: {err}");
    }

    #[test]
    fn mixed_metadata_presence_is_rejected_in_both_orders() {
        let some_unit = || {
            unit(
                "a.rss",
                1,
                vec![decl(0, "read", 0, 1)],
                HashMap::new(),
                Some(metadata(1, 0, vec![host_candidate("read", vec![])])),
            )
        };
        let none_unit = || {
            unit(
                "b.rss",
                2,
                vec![decl(0, "plain", 0, 2)],
                HashMap::new(),
                None,
            )
        };
        let err =
            merge_units(vec![some_unit(), none_unit()]).expect_err("Some-then-None must fail");
        assert!(
            err.to_string().contains("host catalog metadata"),
            "unexpected order Some/None error: {err}"
        );
        let err2 =
            merge_units(vec![none_unit(), some_unit()]).expect_err("None-then-Some must fail");
        assert!(
            err2.to_string().contains("host catalog metadata"),
            "unexpected order None/Some error: {err2}"
        );
    }

    #[test]
    fn metadata_index_missing_from_functions_and_map_is_rejected() {
        // Unit declares index 0 but metadata records index 5.
        let u = unit(
            "a.rss",
            1,
            vec![decl(0, "read", 0, 1)],
            HashMap::new(),
            Some(metadata(1, 5, vec![host_candidate("read", vec![])])),
        );
        let err = merge_units(vec![u]).expect_err("missing metadata index must fail");
        assert!(
            err.to_string().contains("5") && err.to_string().contains("index"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn metadata_on_function_with_implementation_is_rejected() {
        let function_impls = HashMap::from([(0u16, simple_impl())]);
        let u = unit(
            "a.rss",
            1,
            vec![decl(0, "slow", 0, 1)],
            function_impls,
            Some(metadata(1, 0, vec![host_candidate("slow", vec![])])),
        );
        let err = merge_units(vec![u]).expect_err("metadata on implemented function must fail");
        assert!(
            err.to_string().contains("implementation"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn metadata_candidate_name_mismatch_is_rejected() {
        let u = unit(
            "a.rss",
            1,
            vec![decl(0, "read", 0, 1)],
            HashMap::new(),
            Some(metadata(1, 0, vec![host_candidate("write", vec![])])),
        );
        let err = merge_units(vec![u]).expect_err("candidate name mismatch must fail");
        assert!(
            err.to_string().contains("name does not match"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn metadata_candidate_arity_mismatch_is_rejected() {
        let u = unit(
            "a.rss",
            1,
            vec![decl(0, "read", 1, 1)],
            HashMap::new(),
            Some(metadata(1, 0, vec![host_candidate("read", vec![])])),
        );
        let err = merge_units(vec![u]).expect_err("candidate arity mismatch must fail");
        assert!(err.to_string().contains("arity"), "unexpected: {err}");
    }

    #[test]
    fn all_units_without_metadata_yield_none() {
        let u = unit(
            "a.rss",
            1,
            vec![decl(0, "plain", 0, 1)],
            HashMap::new(),
            None,
        );
        let merged = merge_units(vec![u]).expect("supplied unit without metadata must merge");
        assert!(merged.host_api_metadata.is_none());
    }

    #[test]
    fn empty_units_yield_none() {
        let a = unit("a.rss", 1, Vec::new(), HashMap::new(), None);
        let b = unit("b.rss", 2, Vec::new(), HashMap::new(), None);
        let merged = merge_units(vec![a, b]).expect("empty units must merge");
        assert!(merged.functions.is_empty());
        assert!(merged.host_api_metadata.is_none());
    }

    #[test]
    fn single_empty_unit_with_some_metadata_preserves_fingerprint() {
        // A zero-function unit that carries `Some` metadata must still assert
        // its fingerprint and yield an empty-but-fingerprint-bound carrier,
        // never silently drop the catalog identity.
        let empty_md = HostApiIrMetadata::new(fingerprint(0xCAFE)); // zero candidates
        let u = unit("empty.rss", 1, Vec::new(), HashMap::new(), Some(empty_md));
        let merged = merge_units(vec![u]).expect("empty Some unit must merge");
        assert!(merged.functions.is_empty());
        let md = merged
            .host_api_metadata
            .as_ref()
            .expect("empty Some unit must preserve metadata");
        assert_eq!(md.fingerprint(), fingerprint(0xCAFE));
        assert_eq!(md.function_indices().count(), 0);
    }

    #[test]
    fn empty_unit_with_none_metadata_remains_none() {
        let u = unit("empty.rss", 1, Vec::new(), HashMap::new(), None);
        let merged = merge_units(vec![u]).expect("empty None unit must merge");
        assert!(merged.functions.is_empty());
        assert!(merged.host_api_metadata.is_none());
    }

    #[test]
    fn empty_vec_of_units_yields_none() {
        let merged = merge_units(Vec::new()).expect("empty vec must merge to empty IR");
        assert!(merged.functions.is_empty());
        assert!(merged.host_api_metadata.is_none());
    }

    #[test]
    fn empty_units_mixed_metadata_presence_is_rejected_in_both_orders() {
        let some_empty = || {
            unit(
                "a.rss",
                1,
                Vec::new(),
                HashMap::new(),
                Some(HostApiIrMetadata::new(fingerprint(1))),
            )
        };
        let none_empty = || unit("b.rss", 2, Vec::new(), HashMap::new(), None);
        let err =
            merge_units(vec![some_empty(), none_empty()]).expect_err("Some-then-None must fail");
        assert!(
            err.to_string().contains("host catalog metadata"),
            "unexpected order Some/None empty error: {err}"
        );
        let err2 =
            merge_units(vec![none_empty(), some_empty()]).expect_err("None-then-Some must fail");
        assert!(
            err2.to_string().contains("host catalog metadata"),
            "unexpected order None/Some empty error: {err2}"
        );
    }

    #[test]
    fn same_host_different_arity_keeps_two_functions_and_exact_candidates() {
        // The same exposed host name at different arities is a distinct flat
        // function with its own merged index and its own complete candidate
        // set; it must never error as a dedup conflict.
        let arity0_candidates = vec![host_candidate("read", vec![])];
        let arity1_candidates = vec![host_candidate(
            "read",
            vec![HostParamSchema::value("bytes", HostTypeSchema::Bytes)],
        )];
        let a = unit(
            "a.rss",
            1,
            vec![decl(0, "read", 0, 1)],
            HashMap::new(),
            Some(metadata(1, 0, arity0_candidates.clone())),
        );
        let b = unit(
            "b.rss",
            2,
            vec![decl(0, "read", 1, 2)],
            HashMap::new(),
            Some(metadata(1, 0, arity1_candidates.clone())),
        );
        let merged = merge_units(vec![a, b]).expect("different-arity overloads must merge");
        assert_eq!(
            merged.functions.len(),
            2,
            "two overloads become two flat functions"
        );
        // Candidate sets are matched exactly and independently per flat index.
        let md = merged
            .host_api_metadata
            .as_ref()
            .expect("metadata must be carried");
        assert_eq!(md.function_indices().count(), 2);
        let flat_arity_by_name: Vec<(String, u8, &[crate::host_api::HostFunctionSchema])> = merged
            .functions
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    f.arity,
                    md.candidates(f.index).expect("index has candidates"),
                )
            })
            .collect();
        assert!(flat_arity_by_name.iter().all(|(n, _, _)| n == "read"));
        assert_ne!(
            flat_arity_by_name[0].1, flat_arity_by_name[1].1,
            "two overloads must differ in arity"
        );
        // Each flat index carries exactly its own complete candidate list.
        let by_arity: std::collections::HashMap<u8, &[crate::host_api::HostFunctionSchema]> =
            flat_arity_by_name
                .iter()
                .map(|(_, a, c)| (*a, *c))
                .collect();
        assert_eq!(by_arity[&0], &arity0_candidates[..]);
        assert_eq!(by_arity[&1], &arity1_candidates[..]);
    }

    #[test]
    fn index_remap_preserves_call_resolution() {
        use super::super::{ResolvedHostCall, ResolvedHostParam};
        use crate::compiler::TypeSchema;
        let res = ResolvedHostCall {
            name: "read".to_string(),
            params: vec![ResolvedHostParam {
                name: "x".to_string(),
                schema: TypeSchema::Int,
            }],
            return_type: TypeSchema::Int,
            passing: vec![crate::host_api::HostParamPassing::Borrow],
            fingerprint: fingerprint(4),
        };
        let mut annotated =
            Expr::Call(7, Vec::new(), Vec::new(), Some(Box::new(res.clone())), None);
        let mut function_map = HashMap::new();
        function_map.insert(7u16, 11u16);
        remap_expr_indices(&mut annotated, 0, 0, &function_map, &HashMap::new()).unwrap();
        let Expr::Call(flat, _, _, resolution, _) = annotated else {
            panic!("expected a Call");
        };
        assert_eq!(flat, 11);
        // The remap rewrote the flat index but must carry the resolution.
        assert_eq!(resolution.as_deref().unwrap().name, "read");
        assert_eq!(resolution, Some(Box::new(res)));
    }
}

#[cfg(test)]
mod linker_provenance_merge_tests {
    use super::super::ir::{ParsedCallTarget, ParsedLexicalScope, ParsedSemanticIndex};
    use super::super::modules::{ModuleId, SymbolId};
    use super::*;

    fn symbol(module: u32, index: u32) -> SymbolId {
        SymbolId {
            module: ModuleId(module),
            index,
        }
    }

    fn decl(index: u16, name: &str, module: u32) -> FunctionDecl {
        FunctionDecl {
            name: name.to_string(),
            arity: 0,
            index,
            args: Vec::new(),
            arg_schemas: Vec::new(),
            return_schema: None,
            type_params: Vec::new(),
            exported: false,
            return_type: crate::ValueType::Int,
            symbol: Some(symbol(module, index as u32)),
        }
    }

    fn simple_impl() -> FunctionImpl {
        FunctionImpl {
            param_slots: Vec::new(),
            capture_copies: Vec::new(),
            body_stmts: Vec::new(),
            body_expr: Expr::Int(1),
            body_expr_line: 1,
        }
    }

    fn unit_with_semantic(
        source_name: &str,
        module: u32,
        source_id: u32,
        locals: usize,
        functions: Vec<FunctionDecl>,
        function_impls: HashMap<u16, FunctionImpl>,
        parsed: ParsedSemanticIndex,
        visibility: CatalogVisibility,
    ) -> ParsedUnit {
        ParsedUnit {
            parsed: FrontendIr {
                stmts: Vec::new(),
                locals,
                local_bindings: Vec::new(),
                struct_schemas: HashMap::new(),
                unknown_type_spans: Vec::new(),
                functions,
                function_impls,
                stmt_sources: Vec::new(),
                function_sources: HashMap::new(),
                use_declarations: Vec::new(),
                implicit_extern_names: Vec::new(),
                host_api_metadata: None,
                semantic_index: None,
                parsed_semantic_index: Some(parsed),
                catalog_visibility: Some(visibility),
                lexer_tokens: Vec::new(),
            },
            source_name: source_name.to_string(),
            scope_identity: None,
            module: ModuleId(module),
            source_id,
        }
    }

    fn span(source_id: u32, lo: usize, hi: usize) -> crate::compiler::source_map::Span {
        crate::compiler::source_map::Span::new(source_id, lo, hi)
    }

    /// A parsed index whose call sites, decls, refs, and scopes all start at
    /// id 0 — the shape every real parser-produced unit has. The call-site
    /// target and function refs reference unit function index 0 (the single
    /// declared function), which the unit's `function_map` covers. Spans are
    /// written against `source_id`, mirroring a unit parsed with that id.
    fn two_node_index(
        source_id: u32,
        next_node_id: u32,
        next_scope_id: u32,
    ) -> ParsedSemanticIndex {
        ParsedSemanticIndex {
            call_sites: vec![ParsedCallSite {
                id: SemanticNodeId(0),
                callee_span: span(source_id, 0, 3),
                expr_span: span(source_id, 0, 6),
                target: ParsedCallTarget::Function(0),
                name: "f".to_string(),
                scope_id: 0,
                is_namespace_call: false,
            }],
            local_decls: vec![LocalDeclSite {
                id: SemanticNodeId(1),
                ident_span: span(source_id, 10, 11),
                stmt_span: span(source_id, 8, 20),
                slot: LocalSlot::try_from(0).unwrap(),
                name: "x".to_string(),
                scope_id: 0,
                decl_order: 0,
            }],
            local_refs: vec![LocalRefSite {
                id: SemanticNodeId(2),
                ident_span: span(source_id, 15, 16),
                slot: LocalSlot::try_from(0).unwrap(),
                name: "x".to_string(),
                scope_id: 0,
            }],
            func_decls: vec![FunctionDeclSite {
                id: SemanticNodeId(3),
                ident_span: span(source_id, 0, 1),
                function_index: 0,
                name: "f".to_string(),
                scope_id: 0,
                decl_order: 0,
            }],
            func_refs: vec![FunctionRefSite {
                id: SemanticNodeId(4),
                ident_span: span(source_id, 0, 1),
                target: FunctionRefTarget::Function(0),
                name: "f".to_string(),
                scope_id: 0,
            }],
            scopes: vec![ParsedLexicalScope {
                id: 0,
                parent: None,
                range: span(source_id, 0, 30),
                declarations: vec![LocalSlot::try_from(0).unwrap()],
                functions: vec![0],
            }],
            stmt_spans: Vec::new(),
            struct_decls: Vec::new(),
            next_node_id,
            next_scope_id,
        }
    }

    #[test]
    fn two_units_rebase_node_and_scope_ids_collision_free() {
        // Both units start their SemanticNodeId/ScopeId sequences at 0; the
        // merged index must rebase the second unit so no id collides.
        let f0 = decl(0, "f", 1);
        let g0 = decl(0, "g", 2);
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            1,
            vec![f0],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            CatalogVisibility::default(),
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            1,
            vec![g0],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            CatalogVisibility::default(),
        );

        let merged = merge_units(vec![a, b]).expect("two-unit merge must succeed");
        let index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        assert_eq!(index.next_node_id, 10, "two 5-id units");
        assert_eq!(index.next_scope_id, 2, "two single-scope units");
        assert_eq!(index.call_sites.len(), 2);
        assert_eq!(index.local_decls.len(), 2);
        assert_eq!(index.local_refs.len(), 2);
        assert_eq!(index.func_decls.len(), 2);
        assert_eq!(index.func_refs.len(), 2);
        assert_eq!(index.scopes.len(), 2);

        // First unit keeps its ids; the second unit is rebased by the first
        // unit's totals (5 nodes, 1 scope).
        assert_eq!(index.call_sites[0].id, SemanticNodeId(0));
        assert_eq!(index.call_sites[1].id, SemanticNodeId(5));
        assert_eq!(index.local_decls[1].id, SemanticNodeId(6));
        assert_eq!(index.local_refs[1].id, SemanticNodeId(7));
        assert_eq!(index.func_decls[1].id, SemanticNodeId(8));
        assert_eq!(index.func_refs[1].id, SemanticNodeId(9));
        assert_eq!(index.scopes[0].id, 0);
        assert_eq!(index.scopes[1].id, 1);
        assert_eq!(index.scopes[1].parent, None);
    }

    #[test]
    fn two_units_remap_local_slots_by_unit_base() {
        // Unit b's local slot 0 is rebased onto merged slot 1 (after unit a's
        // single local). Call targets and scope declaration lists follow.
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            1,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            CatalogVisibility::default(),
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            1,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            CatalogVisibility::default(),
        );

        let merged = merge_units(vec![a, b]).expect("two-unit merge must succeed");
        let index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        // Unit a's decl/ref slot 0 stays 0; unit b's becomes 1.
        assert_eq!(index.local_decls[0].slot, LocalSlot::try_from(0).unwrap());
        assert_eq!(index.local_decls[1].slot, LocalSlot::try_from(1).unwrap());
        assert_eq!(index.local_refs[0].slot, LocalSlot::try_from(0).unwrap());
        assert_eq!(index.local_refs[1].slot, LocalSlot::try_from(1).unwrap());
        assert_eq!(
            index.scopes[0].declarations[0],
            LocalSlot::try_from(0).unwrap()
        );
        assert_eq!(
            index.scopes[1].declarations[0],
            LocalSlot::try_from(1).unwrap()
        );
        // The second unit's call target Function(1) maps to its merged flat
        // index 1 (unit b's only function becomes flat index 1).
        match index.call_sites[1].target {
            ParsedCallTarget::Function(flat) => assert_eq!(flat, 1),
            ref other => panic!("expected Function target, got {other:?}"),
        }
    }

    #[test]
    fn two_units_remap_function_indices_through_function_map() {
        // Unit a declares f at unit index 3, unit b declares g at unit index
        // 5. The merged flat table assigns 0 and 1; decl sites, ref sites,
        // call targets, and scope function lists all follow the map.
        let f3 = decl(3, "f", 1);
        let g5 = decl(5, "g", 2);
        let index_a = ParsedSemanticIndex {
            call_sites: vec![ParsedCallSite {
                id: SemanticNodeId(0),
                callee_span: span(1, 0, 3),
                expr_span: span(1, 0, 6),
                target: ParsedCallTarget::Function(3),
                name: "f".to_string(),
                scope_id: 0,
                is_namespace_call: false,
            }],
            local_decls: Vec::new(),
            local_refs: Vec::new(),
            func_decls: vec![FunctionDeclSite {
                id: SemanticNodeId(1),
                ident_span: span(1, 0, 1),
                function_index: 3,
                name: "f".to_string(),
                scope_id: 0,
                decl_order: 0,
            }],
            func_refs: vec![FunctionRefSite {
                id: SemanticNodeId(2),
                ident_span: span(1, 0, 1),
                target: FunctionRefTarget::Function(3),
                name: "f".to_string(),
                scope_id: 0,
            }],
            scopes: vec![ParsedLexicalScope {
                id: 0,
                parent: None,
                range: span(1, 0, 10),
                declarations: Vec::new(),
                functions: vec![3],
            }],
            stmt_spans: Vec::new(),
            struct_decls: Vec::new(),
            next_node_id: 3,
            next_scope_id: 1,
        };
        let index_b = ParsedSemanticIndex {
            call_sites: Vec::new(),
            local_decls: Vec::new(),
            local_refs: Vec::new(),
            func_decls: vec![FunctionDeclSite {
                id: SemanticNodeId(0),
                ident_span: span(2, 0, 1),
                function_index: 5,
                name: "g".to_string(),
                scope_id: 0,
                decl_order: 0,
            }],
            func_refs: Vec::new(),
            scopes: vec![ParsedLexicalScope {
                id: 0,
                parent: None,
                range: span(2, 0, 10),
                declarations: Vec::new(),
                functions: vec![5],
            }],
            stmt_spans: Vec::new(),
            struct_decls: Vec::new(),
            next_node_id: 1,
            next_scope_id: 1,
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![f3],
            HashMap::from([(3u16, simple_impl())]),
            index_a,
            CatalogVisibility::default(),
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![g5],
            HashMap::from([(5u16, simple_impl())]),
            index_b,
            CatalogVisibility::default(),
        );

        let merged = merge_units(vec![a, b]).expect("two-unit merge must succeed");
        let index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        assert_eq!(merged.functions.len(), 2);
        assert_eq!(index.func_decls[0].function_index, 0, "a's f -> flat 0");
        assert_eq!(index.func_decls[1].function_index, 1, "b's g -> flat 1");
        assert_eq!(
            index.func_refs[0].target,
            FunctionRefTarget::Function(0),
            "a's func ref -> flat 0"
        );
        match index.call_sites[0].target {
            ParsedCallTarget::Function(flat) => assert_eq!(flat, 0),
            ref other => panic!("expected Function target, got {other:?}"),
        }
        assert_eq!(index.scopes[0].functions, vec![0]);
        assert_eq!(index.scopes[1].functions, vec![1]);
    }

    #[test]
    fn two_units_preserve_span_source_ids() {
        // Every span keeps the source_id it was parsed with; the merge never
        // rewrites span provenance.
        let a = unit_with_semantic(
            "a.rss",
            1,
            7,
            1,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(7, 5, 1),
            CatalogVisibility::default(),
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            9,
            1,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(9, 5, 1),
            CatalogVisibility::default(),
        );

        let merged = merge_units(vec![a, b]).expect("two-unit merge must succeed");
        let index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        assert_eq!(index.call_sites[0].callee_span.source_id, 7);
        assert_eq!(index.call_sites[1].callee_span.source_id, 9);
        assert_eq!(index.local_decls[0].ident_span.source_id, 7);
        assert_eq!(index.local_decls[1].ident_span.source_id, 9);
        assert_eq!(index.scopes[0].range.source_id, 7);
        assert_eq!(index.scopes[1].range.source_id, 9);
        assert_eq!(index.func_decls[1].ident_span.source_id, 9);
    }

    #[test]
    fn merged_expression_semantic_ids_match_rebased_index() {
        // A call in each unit's function body carries the parser's id; the
        // merge rebases both the Expr node and the parsed index identically.
        let f_impl = FunctionImpl {
            param_slots: Vec::new(),
            capture_copies: Vec::new(),
            body_stmts: Vec::new(),
            body_expr: Expr::Call(0, Vec::new(), Vec::new(), None, Some(SemanticNodeId(0))),
            body_expr_line: 1,
        };
        let g_impl = FunctionImpl {
            param_slots: Vec::new(),
            capture_copies: Vec::new(),
            body_stmts: Vec::new(),
            body_expr: Expr::Call(0, Vec::new(), Vec::new(), None, Some(SemanticNodeId(0))),
            body_expr_line: 1,
        };
        let index_a = ParsedSemanticIndex {
            call_sites: vec![ParsedCallSite {
                id: SemanticNodeId(0),
                callee_span: span(1, 0, 3),
                expr_span: span(1, 0, 6),
                target: ParsedCallTarget::Function(0),
                name: "f".to_string(),
                scope_id: 0,
                is_namespace_call: false,
            }],
            local_decls: Vec::new(),
            local_refs: Vec::new(),
            func_decls: Vec::new(),
            struct_decls: Vec::new(),
            func_refs: Vec::new(),
            scopes: Vec::new(),
            stmt_spans: Vec::new(),
            next_node_id: 1,
            next_scope_id: 0,
        };
        let index_b = ParsedSemanticIndex {
            call_sites: vec![ParsedCallSite {
                id: SemanticNodeId(0),
                callee_span: span(2, 0, 3),
                expr_span: span(2, 0, 6),
                target: ParsedCallTarget::Function(0),
                name: "g".to_string(),
                scope_id: 0,
                is_namespace_call: false,
            }],
            local_decls: Vec::new(),
            local_refs: Vec::new(),
            func_decls: Vec::new(),
            struct_decls: Vec::new(),
            func_refs: Vec::new(),
            scopes: Vec::new(),
            stmt_spans: Vec::new(),
            next_node_id: 1,
            next_scope_id: 0,
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, f_impl)]),
            index_a,
            CatalogVisibility::default(),
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, g_impl)]),
            index_b,
            CatalogVisibility::default(),
        );

        let merged = merge_units(vec![a, b]).expect("two-unit merge must succeed");
        let index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        assert_eq!(index.call_sites[0].id, SemanticNodeId(0));
        assert_eq!(index.call_sites[1].id, SemanticNodeId(1));
        // The Expr node in unit b's merged function body carries the rebased
        // id, matching the rebased index entry.
        let g_flat = merged
            .functions
            .iter()
            .find(|function| function.name == "g")
            .expect("g flat entry")
            .index;
        let merged_impl = &merged.function_impls[&g_flat];
        match &merged_impl.body_expr {
            Expr::Call(_, _, _, _, semantic_id) => {
                assert_eq!(*semantic_id, Some(SemanticNodeId(1)));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn module_call_target_symbols_survive_merge() {
        // ParsedCallTarget::Module carries a compilation-wide SymbolId that
        // needs no rebase; the merged index preserves it verbatim.
        let target = symbol(3, 7);
        let index_a = ParsedSemanticIndex {
            call_sites: vec![ParsedCallSite {
                id: SemanticNodeId(0),
                callee_span: span(1, 0, 10),
                expr_span: span(1, 0, 14),
                target: ParsedCallTarget::Module(target),
                name: "au::helper".to_string(),
                scope_id: 0,
                is_namespace_call: true,
            }],
            local_decls: Vec::new(),
            local_refs: Vec::new(),
            func_decls: Vec::new(),
            struct_decls: Vec::new(),
            func_refs: Vec::new(),
            scopes: Vec::new(),
            stmt_spans: Vec::new(),
            next_node_id: 1,
            next_scope_id: 0,
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            index_a,
            CatalogVisibility::default(),
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            CatalogVisibility::default(),
        );

        let merged = merge_units(vec![a, b]).expect("two-unit merge must succeed");
        let index = merged
            .parsed_semantic_index
            .as_ref()
            .expect("merged index present");
        assert_eq!(index.call_sites[0].target, ParsedCallTarget::Module(target));
        // The second unit's site rebased normally.
        assert_eq!(index.call_sites[1].id, SemanticNodeId(1));
    }

    #[test]
    fn catalog_alias_vectors_dedupe_identically() {
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: vec![("io".to_string(), "std::io".to_string())],
            direct_host_call_aliases: vec![("read".to_string(), "io::read".to_string())],
            direct_host_wildcard_imports: vec!["std::io".to_string()],
            module_namespace_aliases: vec![ModuleNamespaceAlias {
                alias: "au".to_string(),
                module_path: "a/util".to_string(),
                source: String::new(),
            }],
            use_declarations: Vec::new(),
        };
        // Unit b repeats the identical aliases and wildcard import; the merge
        // must collapse them, not duplicate or error.
        let visibility_b = visibility_a.clone();
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_b,
        );

        let merged = merge_units(vec![a, b]).expect("dedup merge must succeed");
        let visibility = merged
            .catalog_visibility
            .as_ref()
            .expect("merged visibility present");
        assert_eq!(
            visibility.host_namespace_aliases,
            vec![("io".to_string(), "std::io".to_string())]
        );
        assert_eq!(visibility.direct_host_call_aliases.len(), 1);
        assert_eq!(visibility.direct_host_wildcard_imports, vec!["std::io"]);
        // Module namespace aliases are unit-local: the identical alias from
        // two different sources is retained for each owner, not collapsed.
        assert_eq!(
            visibility.module_namespace_aliases.len(),
            2,
            "module aliases stay per owning source"
        );
        assert_eq!(
            visibility.module_namespace_aliases[0].source, "a.rss",
            "first entry owned by a.rss"
        );
        assert_eq!(
            visibility.module_namespace_aliases[1].source, "b.rss",
            "second entry owned by b.rss"
        );
    }

    #[test]
    fn catalog_alias_conflicts_error() {
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: vec![("io".to_string(), "std::io".to_string())],
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: Vec::new(),
        };
        let visibility_b = CatalogVisibility {
            host_namespace_aliases: vec![("io".to_string(), "other::io".to_string())],
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: Vec::new(),
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_b,
        );

        let err = merge_units(vec![a, b]).expect_err("conflicting aliases must fail");
        assert!(
            err.to_string().contains("alias conflict"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("host namespace alias 'io'"),
            "unexpected: {err}"
        );
    }

    /// A genuine same-source module namespace alias conflict (same alias,
    /// different module path within one unit) is a typed error — the merge
    /// must never silently pick the first spelling.
    #[test]
    fn same_source_module_alias_conflict_errors() {
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: vec![
                ModuleNamespaceAlias {
                    alias: "x".to_string(),
                    module_path: "a".to_string(),
                    source: String::new(),
                },
                ModuleNamespaceAlias {
                    alias: "x".to_string(),
                    module_path: "b".to_string(),
                    source: String::new(),
                },
            ],
            use_declarations: Vec::new(),
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );

        let err = merge_units(vec![a]).expect_err("conflicting aliases must fail");
        assert!(
            err.to_string().contains("module namespace alias conflict"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("alias 'x' maps to both"),
            "unexpected: {err}"
        );
        assert!(
            err.to_string().contains("'b' and 'a'") || err.to_string().contains("'a' and 'b'"),
            "unexpected: {err}"
        );
    }

    /// Independent units that use the *same alias name for different modules*
    /// merge cleanly with per-source ownership retained: neither unit's
    /// alias collapses into the other's.
    #[test]
    fn independent_unit_module_aliases_do_not_collapse() {
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: vec![ModuleNamespaceAlias {
                alias: "x".to_string(),
                module_path: "a".to_string(),
                source: String::new(),
            }],
            use_declarations: Vec::new(),
        };
        let visibility_b = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: vec![ModuleNamespaceAlias {
                alias: "x".to_string(),
                module_path: "b".to_string(),
                source: String::new(),
            }],
            use_declarations: Vec::new(),
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_b,
        );

        let merged = merge_units(vec![a, b]).expect("independent aliases must merge");
        let visibility = merged
            .catalog_visibility
            .as_ref()
            .expect("merged visibility present");
        assert_eq!(visibility.module_namespace_aliases.len(), 2);
        let by_source = |source: &str| {
            visibility
                .module_namespace_aliases
                .iter()
                .find(|alias| alias.source == source)
                .expect("alias for source")
        };
        let a_alias = by_source("a.rss");
        let b_alias = by_source("b.rss");
        assert_eq!(a_alias.alias, "x");
        assert_eq!(a_alias.module_path, "a", "a's `x` names module a");
        assert_eq!(b_alias.alias, "x");
        assert_eq!(b_alias.module_path, "b", "b's `x` names module b");
        assert_ne!(
            a_alias.module_path, b_alias.module_path,
            "same alias in different units keeps distinct module targets"
        );
    }

    #[test]
    fn mixed_direct_alias_conflict_across_vectors() {
        // Same alias name in a different vector is not a conflict: vectors
        // are merged independently.
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: vec![("io".to_string(), "std::io".to_string())],
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: Vec::new(),
        };
        let visibility_b = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: vec![("io".to_string(), "io::open".to_string())],
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: Vec::new(),
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_b,
        );

        let merged = merge_units(vec![a, b]).expect("independent vectors must merge");
        let visibility = merged
            .catalog_visibility
            .as_ref()
            .expect("merged visibility present");
        assert_eq!(visibility.host_namespace_aliases.len(), 1);
        assert_eq!(visibility.direct_host_call_aliases.len(), 1);
    }

    #[test]
    fn use_declarations_dedupe_by_path_and_clause() {
        use crate::compiler::modules::{UseDecl, UsePathSegment};
        use crate::compiler::source_loader::{ImportClause, NamedImport};
        let make_decl = |source_id: u32, line: usize| UseDecl {
            path: vec![
                UsePathSegment::Ident("a".to_string()),
                UsePathSegment::Ident("util".to_string()),
            ],
            clause: ImportClause::Named(vec![NamedImport {
                imported: "helper".to_string(),
                local: "h".to_string(),
            }]),
            span: span(source_id, 0, 20),
            line,
        };
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: vec![make_decl(1, 2)],
        };
        let visibility_b = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            // Same path+clause, different span/line: must collapse.
            use_declarations: vec![make_decl(2, 9)],
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_b,
        );

        let merged = merge_units(vec![a, b]).expect("dedup merge must succeed");
        let visibility = merged
            .catalog_visibility
            .as_ref()
            .expect("merged visibility present");
        assert_eq!(
            visibility.use_declarations.len(),
            1,
            "identical directives collapse to one entry"
        );
    }

    #[test]
    fn distinct_use_declarations_are_both_kept() {
        use crate::compiler::modules::{UseDecl, UsePathSegment};
        use crate::compiler::source_loader::ImportClause;
        let a_decl = UseDecl {
            path: vec![UsePathSegment::Ident("a".to_string())],
            clause: ImportClause::Namespace("au".to_string()),
            span: span(1, 0, 20),
            line: 2,
        };
        let b_decl = UseDecl {
            path: vec![UsePathSegment::Ident("b".to_string())],
            clause: ImportClause::Namespace("bu".to_string()),
            span: span(2, 0, 20),
            line: 3,
        };
        let visibility_a = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: vec![a_decl],
        };
        let visibility_b = CatalogVisibility {
            host_namespace_aliases: Vec::new(),
            direct_host_call_aliases: Vec::new(),
            direct_host_wildcard_imports: Vec::new(),
            module_namespace_aliases: Vec::new(),
            use_declarations: vec![b_decl],
        };
        let a = unit_with_semantic(
            "a.rss",
            1,
            1,
            0,
            vec![decl(0, "f", 1)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_a,
        );
        let b = unit_with_semantic(
            "b.rss",
            2,
            2,
            0,
            vec![decl(0, "g", 2)],
            HashMap::from([(0u16, simple_impl())]),
            two_node_index(1, 5, 1),
            visibility_b,
        );

        let merged = merge_units(vec![a, b]).expect("distinct directives must merge");
        let visibility = merged
            .catalog_visibility
            .as_ref()
            .expect("merged visibility present");
        assert_eq!(visibility.use_declarations.len(), 2);
    }

    #[test]
    fn units_without_provenance_leave_merged_carrier_none() {
        // REPL/test fixtures carry no provenance; the merged IR must stay
        // `None` for both carriers.
        let a = ParsedUnit {
            parsed: FrontendIr {
                stmts: Vec::new(),
                locals: 0,
                local_bindings: Vec::new(),
                struct_schemas: HashMap::new(),
                unknown_type_spans: Vec::new(),
                functions: vec![decl(0, "f", 1)],
                function_impls: HashMap::from([(0u16, simple_impl())]),
                stmt_sources: Vec::new(),
                function_sources: HashMap::new(),
                use_declarations: Vec::new(),
                implicit_extern_names: Vec::new(),
                host_api_metadata: None,
                semantic_index: None,
                parsed_semantic_index: None,
                catalog_visibility: None,
                lexer_tokens: Vec::new(),
            },
            source_name: "a.rss".to_string(),
            scope_identity: None,
            module: ModuleId(1),
            source_id: 1,
        };
        let merged = merge_units(vec![a]).expect("provenance-less unit must merge");
        assert!(merged.parsed_semantic_index.is_none());
        assert!(merged.catalog_visibility.is_none());
    }
}
