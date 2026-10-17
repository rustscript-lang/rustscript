mod collect;
mod context;
mod helpers;
mod state;
mod validate;

use std::collections::{HashMap, HashSet};

use crate::bytecode::ValueType;

use self::collect::{
    CollectFunctionTypeOutputs, CollectFunctionTypesEnv, collect_function_types,
    collect_stmt_types, record_callable_slot, record_local_schema, record_local_schema_label,
    record_local_type, record_optional_slot,
};
use self::context::TypeContext;
pub(crate) use self::context::bound_type_from_schema;
use self::helpers::{
    FunctionLegalizeEnv, HostCallResolutionPass, HostCallResolutionPhase, build_function_decl_map,
    build_function_names, build_host_import_return_types, legalize_function_impl, legalize_stmts,
    validate_function_impl, validate_stmts,
};
pub(crate) use self::state::{
    BoundType, HostCallableSignature, LocalTypeState, TypeInferenceResult,
};
use super::CompileError;
use super::FrontendTypedIr;
use super::TypingMode;
use super::ir::{
    Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt, StructDecl, TypeSchema,
};

impl FrontendTypedIr<'_> {
    /// Returns the compiler's existing logical schema inference for `expression`.
    pub fn schema_of_expr(&self, expression: &Expr) -> Option<TypeSchema> {
        let state = local_state_from_schemas(self.local_schemas);
        let function_names = build_function_names(&self.ir.functions);
        let function_decls = build_function_decl_map(&self.ir.functions);
        let host_import_return_types =
            build_host_import_return_types(&self.ir.functions, &self.ir.function_impls);
        let host_import_signatures =
            build_host_import_signatures(&self.ir.functions, &self.ir.function_impls);
        let mut context = TypeContext::new(
            &self.ir.function_impls,
            &function_decls,
            &self.ir.struct_schemas,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            TypingMode::DynamicHints,
            self.ir.parsed_semantic_index.as_ref(),
        );
        let schema = context
            .infer_expr_schema(expression, &state)
            .or_else(|| scalar_schema_from_bound_type(context.infer_expr_type(expression, &state)));
        schema.map(|schema| {
            if context.expr_is_optional(expression, &state)
                && !matches!(schema, TypeSchema::Optional(_))
            {
                TypeSchema::Optional(Box::new(schema))
            } else {
                schema
            }
        })
    }

    /// Returns the complete logical callable schema for a declared function.
    pub fn schema_of_function(&self, index: u16) -> Option<TypeSchema> {
        let declaration = self.ir.functions.iter().find(|decl| decl.index == index)?;
        let state = local_state_from_schemas(self.local_schemas);
        let function_names = build_function_names(&self.ir.functions);
        let function_decls = build_function_decl_map(&self.ir.functions);
        let host_import_return_types =
            build_host_import_return_types(&self.ir.functions, &self.ir.function_impls);
        let host_import_signatures =
            build_host_import_signatures(&self.ir.functions, &self.ir.function_impls);
        let mut context = TypeContext::new(
            &self.ir.function_impls,
            &function_decls,
            &self.ir.struct_schemas,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            TypingMode::DynamicHints,
            self.ir.parsed_semantic_index.as_ref(),
        );
        let type_args = declaration
            .type_params
            .iter()
            .cloned()
            .map(TypeSchema::GenericParam)
            .collect::<Vec<_>>();
        let result = match context.infer_expr_schema(&Expr::FunctionRef(index, type_args), &state) {
            Some(TypeSchema::Callable { result, .. }) => *result,
            _ => TypeSchema::Unknown,
        };
        let params = declaration
            .args
            .iter()
            .enumerate()
            .map(|(index, _)| {
                declaration
                    .arg_schemas
                    .get(index)
                    .cloned()
                    .flatten()
                    // Body-local observations are inference hints, not a
                    // declaration-wide argument contract. Keep unannotated
                    // parameters dynamic, as FunctionRef does.
                    .unwrap_or(TypeSchema::Unknown)
            })
            .collect();
        Some(TypeSchema::Callable {
            params,
            result: Box::new(result),
        })
    }
}

fn scalar_schema_from_bound_type(bound_type: BoundType) -> Option<TypeSchema> {
    match bound_type {
        BoundType::Null => Some(TypeSchema::Null),
        BoundType::Int => Some(TypeSchema::Int),
        BoundType::Float => Some(TypeSchema::Float),
        BoundType::Number => Some(TypeSchema::Number),
        BoundType::Bool => Some(TypeSchema::Bool),
        BoundType::String => Some(TypeSchema::String),
        BoundType::Bytes => Some(TypeSchema::Bytes),
        BoundType::Unknown
        | BoundType::Array
        | BoundType::ArrayOf(_)
        | BoundType::Map
        | BoundType::MapOf(_)
        | BoundType::Callable => None,
    }
}

fn local_state_from_schemas(local_schemas: &[Option<TypeSchema>]) -> LocalTypeState {
    let entry_local_types = local_schemas
        .iter()
        .enumerate()
        .filter_map(|(index, schema)| {
            Some(EntryLocalType {
                slot: u16::try_from(index).ok()?,
                schema: schema.clone(),
                optional: false,
            })
        })
        .collect::<Vec<_>>();
    let mut state = LocalTypeState::default();
    seed_entry_local_state(&mut state, &entry_local_types);
    state
}

pub(crate) use self::context::render_schema_label;

#[derive(Clone, Debug)]
pub(super) struct EntryLocalType {
    pub(super) slot: LocalSlot,
    pub(super) schema: Option<TypeSchema>,
    pub(super) optional: bool,
}

/// Whole logical type for a shared entry view.
///
/// The optional bit and a single `Optional<S>` wrapper are the same fact.
/// Unknown is not wrapped or inserted: it is not a nullable value.
pub(super) fn logical_entry_schema(entry: &EntryLocalType) -> Option<TypeSchema> {
    let schema = entry.schema.clone()?;
    project_logical_schema(schema, entry.optional)
}

pub(super) fn project_logical_schema(schema: TypeSchema, optional: bool) -> Option<TypeSchema> {
    if matches!(schema, TypeSchema::Unknown) {
        return None;
    }
    let (inner, schema_optional) = schema.split_optional();
    if matches!(inner, TypeSchema::Unknown) {
        return None;
    }
    let optional = optional || schema_optional;
    Some(
        if optional && !matches!(inner, TypeSchema::Optional(_) | TypeSchema::Null) {
            TypeSchema::Optional(Box::new(inner))
        } else {
            inner
        },
    )
}

fn seed_entry_local_state(state: &mut LocalTypeState, entry_local_types: &[EntryLocalType]) {
    for entry_local in entry_local_types {
        let (schema, schema_optional) = entry_local
            .schema
            .clone()
            .map(|schema| schema.split_optional())
            .map(|(schema, optional)| (Some(schema), optional))
            .unwrap_or((None, false));
        let optional = entry_local.optional || schema_optional;
        if let Some(schema) = schema {
            state.set_with_optional_schema_origin(
                entry_local.slot,
                bound_type_from_schema(&schema),
                Some(schema),
                true,
                optional,
            );
        } else {
            state.set_with_optional_schema_origin(
                entry_local.slot,
                BoundType::Unknown,
                None,
                false,
                optional,
            );
        }
    }
}

fn record_entry_local_types(
    entry_local_types: &[EntryLocalType],
    state: &LocalTypeState,
    local_types: &mut [ValueType],
    local_schemas: &mut [Option<TypeSchema>],
    local_schema_labels: &mut [Option<String>],
    callable_slots: &mut [bool],
    optional_slots: &mut [bool],
) {
    for entry_local in entry_local_types {
        record_local_type(local_types, entry_local.slot, state.get(entry_local.slot));
        record_local_schema(
            local_schemas,
            entry_local.slot,
            state.schema(entry_local.slot),
        );
        record_local_schema_label(
            local_schema_labels,
            entry_local.slot,
            state.schema(entry_local.slot),
        );
        if state.callable(entry_local.slot).is_some()
            || state.callable_schema(entry_local.slot).is_some()
        {
            record_callable_slot(callable_slots, entry_local.slot);
        }
        if state.is_optional(entry_local.slot) {
            record_optional_slot(optional_slots, entry_local.slot);
        }
    }
}

pub(super) fn legalize_builtins_and_bind_types(
    ir: FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> Result<FrontendIr, CompileError> {
    let (ir, error) = legalize_for_admission(ir, typing_mode, entry_local_types);
    match error {
        Some(error) => Err(error),
        None => Ok(ir),
    }
}

// Retain all successfully resolved facts even when another host call fails.
// This view is admission/diagnostic-only until the error is discharged.
pub(super) fn legalize_for_admission(
    mut ir: FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> (FrontendIr, Option<CompileError>) {
    // Exact callee spans for every parsed call site, keyed by the
    // [`SemanticNodeId`] carried on the typed [`Expr::Call`] nodes. The
    // host-call resolver attaches these to its failure diagnostic so the
    // semantic model surfaces the precise failing call span.
    let call_site_spans = ir
        .parsed_semantic_index
        .as_ref()
        .map(|parsed| {
            parsed
                .call_sites
                .iter()
                .map(|site| (site.id, site.callee_span))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();

    let Some(metadata) = ir.host_api_metadata.clone() else {
        let mut pass = HostCallResolutionPass::new(None, HostCallResolutionPhase::Disabled)
            .with_call_site_spans(&call_site_spans);
        run_legalize_round(&mut ir, typing_mode, entry_local_types, &mut pass);
        return (ir, None);
    };

    loop {
        let mut refine =
            HostCallResolutionPass::new(Some(&metadata), HostCallResolutionPhase::Refine)
                .with_call_site_spans(&call_site_spans);
        run_legalize_round(&mut ir, typing_mode, entry_local_types, &mut refine);
        if refine.changed() > 0 {
            continue;
        }
        if refine.unresolved() == 0 {
            return (ir, None);
        }

        let mut final_pass =
            HostCallResolutionPass::new(Some(&metadata), HostCallResolutionPhase::Final)
                .with_call_site_spans(&call_site_spans);
        run_legalize_round(&mut ir, typing_mode, entry_local_types, &mut final_pass);
        if final_pass.changed() > 0 {
            continue;
        }
        if final_pass.unresolved() == 0 {
            return (ir, None);
        }
        return (
            ir,
            Some(
                final_pass
                    .take_error()
                    .expect("a final unresolved catalog call must record a compile error"),
            ),
        );
    }
}

fn run_legalize_round(
    ir: &mut FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
    host_resolution: &mut HostCallResolutionPass<'_>,
) {
    let function_names = build_function_names(&ir.functions);
    let function_decls = build_function_decl_map(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    seed_entry_local_state(&mut top_state, entry_local_types);
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_decls,
        &ir.struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        typing_mode,
        ir.parsed_semantic_index.as_ref(),
    );
    for (index, stmt) in ir.stmts.iter_mut().enumerate() {
        legalize_stmts(
            std::slice::from_mut(stmt),
            &mut top_state,
            ir.stmt_sources
                .get(index)
                .and_then(|source| source.as_deref()),
            &mut context,
            host_resolution,
        );
    }
    let observed_function_param_types = context.observed_function_param_types.clone();
    let observed_function_param_schemas = context.observed_function_param_schemas.clone();
    let observed_function_param_callables = context.observed_function_param_callables.clone();
    let observed_function_param_capture_states =
        context.observed_function_param_capture_states.clone();
    let observed_function_capture_states = context.observed_function_capture_states.clone();

    let function_impls = ir.function_impls.clone();
    let legalize_env = FunctionLegalizeEnv {
        function_impls: &function_impls,
        function_decls: &function_decls,
        function_names: &function_names,
        struct_schemas: &ir.struct_schemas,
        host_import_return_types: &host_import_return_types,
        host_import_signatures: &host_import_signatures,
        observed_function_param_types: &observed_function_param_types,
        observed_function_param_schemas: &observed_function_param_schemas,
        observed_function_param_callables: &observed_function_param_callables,
        observed_function_param_capture_states: &observed_function_param_capture_states,
        observed_function_capture_states: &observed_function_capture_states,
    };
    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get_mut(&decl.index) else {
            continue;
        };
        legalize_function_impl(
            decl.index,
            function_impl,
            ir.function_sources.get(&decl.index).map(String::as_str),
            &legalize_env,
            host_resolution,
        );
    }
}

pub(super) fn infer_types(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> TypeInferenceResult {
    infer_types_with_root_projection(ir, typing_mode, entry_local_types, &[]).0
}

/// Root bindings describe the final checked root state. The aggregate result
/// continues to summarize physical slots across root and callee frames.
pub(super) fn infer_types_with_root_projection(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
    root_slots: &[LocalSlot],
) -> (TypeInferenceResult, Vec<EntryLocalType>) {
    let (facts, roots, _) =
        infer_types_with_capture_origins(ir, typing_mode, entry_local_types, root_slots);
    (facts, roots)
}

pub(super) fn infer_types_with_capture_origins(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
    root_slots: &[LocalSlot],
) -> (
    TypeInferenceResult,
    Vec<EntryLocalType>,
    super::capture_certification::DeclarationCaptures,
) {
    let function_names = build_function_names(&ir.functions);
    let function_decls = build_function_decl_map(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut local_types = vec![ValueType::Unknown; ir.locals];
    let mut local_schemas = vec![None; ir.locals];
    let mut local_schema_labels = vec![None; ir.locals];
    let mut callable_slots = vec![false; ir.locals];
    let mut optional_slots = vec![false; ir.locals];
    let mut top_state = LocalTypeState::default();
    seed_entry_local_state(&mut top_state, entry_local_types);
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_decls,
        &ir.struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        typing_mode,
        ir.parsed_semantic_index.as_ref(),
    );
    record_entry_local_types(
        entry_local_types,
        &top_state,
        &mut local_types,
        &mut local_schemas,
        &mut local_schema_labels,
        &mut callable_slots,
        &mut optional_slots,
    );
    for (index, stmt) in ir.stmts.iter().enumerate() {
        context.capture_owner = Some(super::capture_certification::CaptureOwner::RootStatement(
            index,
        ));
        collect_stmt_types(
            std::slice::from_ref(stmt),
            &mut top_state,
            &mut local_types,
            &mut local_schemas,
            &mut local_schema_labels,
            &mut callable_slots,
            &mut optional_slots,
            &mut context,
        );
    }
    // The backend collector intentionally summarizes expressions without
    // exporting their writes. Replay only the requested root view with mutable
    // expression state and a separate context, leaving backend observations intact.
    let root_projection = if root_slots.is_empty() {
        Vec::new()
    } else {
        let mut root_state = LocalTypeState::default();
        seed_entry_local_state(&mut root_state, entry_local_types);
        let mut root_context = TypeContext::new(
            &ir.function_impls,
            &function_decls,
            &ir.struct_schemas,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            typing_mode,
            ir.parsed_semantic_index.as_ref(),
        );
        let entry_slots = entry_local_types
            .iter()
            .map(|entry| entry.slot)
            .collect::<Vec<_>>();
        collect::collect_root_stmt_state(
            &ir.stmts,
            &mut root_state,
            &entry_slots,
            &mut root_context,
        );
        root_slots
            .iter()
            .map(|&slot| EntryLocalType {
                slot,
                schema: root_context
                    .infer_expr_schema(&Expr::Var(slot), &root_state)
                    .or_else(|| scalar_schema_from_bound_type(root_state.get(slot))),
                optional: root_state.is_optional(slot),
            })
            .collect()
    };
    let observed_function_param_types = context.observed_function_param_types.clone();
    let observed_function_param_schemas = context.observed_function_param_schemas.clone();
    let observed_function_param_callables = context.observed_function_param_callables.clone();
    let observed_function_param_capture_states =
        context.observed_function_param_capture_states.clone();
    let observed_function_capture_states = context.observed_function_capture_states.clone();
    let env = CollectFunctionTypesEnv {
        function_impls: &ir.function_impls,
        function_decls: &function_decls,
        function_names: &function_names,
        struct_schemas: &ir.struct_schemas,
        host_import_return_types: &host_import_return_types,
        host_import_signatures: &host_import_signatures,
        observed_function_param_types: &observed_function_param_types,
        observed_function_param_schemas: &observed_function_param_schemas,
        observed_function_param_callables: &observed_function_param_callables,
        observed_function_param_capture_states: &observed_function_param_capture_states,
        observed_function_capture_states: &observed_function_capture_states,
    };

    let mut origins = context.capture_origins.clone();
    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        let mut outputs = CollectFunctionTypeOutputs {
            local_types: &mut local_types,
            local_schemas: &mut local_schemas,
            local_schema_labels: &mut local_schema_labels,
            callable_slots: &mut callable_slots,
            optional_slots: &mut optional_slots,
        };
        origins.extend(collect_function_types(
            decl.index,
            function_impl,
            decl,
            &mut outputs,
            &env,
        ));
    }

    let mut facts = TypeInferenceResult {
        local_types,
        local_schemas,
        local_schema_labels,
        callable_slots,
        optional_slots,
        function_schemas: HashMap::new(),
    };
    let schemas = facts.logical_schemas();
    let typed = FrontendTypedIr {
        source_id: None,
        ir,
        local_schemas: &schemas,
        host_api_catalog: None,
        source_map: None,
    };
    facts.function_schemas = ir
        .functions
        .iter()
        .filter_map(|decl| {
            typed
                .schema_of_function(decl.index)
                .map(|schema| (decl.index, schema))
        })
        .collect();
    (facts, root_projection, origins)
}

pub(super) fn validate_if_else_type_consistency(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> Result<(), CompileError> {
    validate_source_type_consistency(ir, typing_mode, entry_local_types, None)
}

pub(super) fn validate_source_type_consistency(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
    owner: Option<&str>,
) -> Result<(), CompileError> {
    check_source_type_consistency(ir, typing_mode, entry_local_types, owner, false, None)
        .map(|_| ())
}

pub(super) fn analyze_source_type_consistency(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
    owner: Option<&str>,
    host_error: Option<&CompileError>,
) -> Result<Vec<CompileError>, CompileError> {
    check_source_type_consistency(ir, typing_mode, entry_local_types, owner, true, host_error)
}

fn check_source_type_consistency(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
    owner: Option<&str>,
    recover_typing_diagnostics: bool,
    host_error: Option<&CompileError>,
) -> Result<Vec<CompileError>, CompileError> {
    let mut diagnostics = Vec::new();
    let dependent_host_returns = host_error
        .map(|error| host_error_dependent_returns(ir, error))
        .unwrap_or_default();
    let function_names = build_function_names(&ir.functions);
    let function_decls = build_function_decl_map(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    seed_entry_local_state(&mut top_state, entry_local_types);
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_decls,
        &ir.struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        typing_mode,
        ir.parsed_semantic_index.as_ref(),
    );
    for (index, stmt) in ir.stmts.iter().enumerate() {
        if owner.is_some_and(|owner| {
            ir.stmt_sources
                .get(index)
                .and_then(|source| source.as_deref())
                != Some(owner)
        }) {
            context.apply_stmts(std::slice::from_ref(stmt), &mut top_state);
            continue;
        }
        if let Err(error) = validate_stmts(
            std::slice::from_ref(stmt),
            &mut top_state,
            None,
            ir.stmt_sources
                .get(index)
                .and_then(|source| source.as_deref()),
            &mut context,
            false,
        ) {
            retain_native_typing_diagnostic(error, recover_typing_diagnostics, &mut diagnostics)?;
            // A failed statement still contributes its real type flow to later
            // validation sites; an earlier diagnostic cannot mask a hard error.
            context.apply_stmts(std::slice::from_ref(stmt), &mut top_state);
        }
    }

    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        if owner.is_some_and(|owner| {
            ir.function_sources.get(&decl.index).map(String::as_str) != Some(owner)
        }) {
            continue;
        }
        if let Err(error) = validate_function_impl(
            decl.index,
            function_impl,
            ir.function_sources.get(&decl.index).map(String::as_str),
            &mut context,
        ) {
            // Only the final inferred-return error of a provenance-proven
            // direct call chain is fallout from this rejected host call.
            // Body validation already ran; independent Strict slot admission
            // and source hooks still run in the shared pipeline afterwards.
            if recover_typing_diagnostics
                && dependent_host_returns.contains(&decl.index)
                && matches!(&error, CompileError::StrictTypingRequired {
                    line: Some(line), span: Some(span), ..
                } if *line == function_impl.body_expr_line
                    && ir.parsed_semantic_index.as_ref().is_some_and(|index| {
                        index.func_decls.iter().any(|site| {
                            site.function_index == decl.index && site.ident_span == *span
                        })
                    }))
            {
                // Its unresolved facts remain unchanged; the exact host error
                // is retained by the pipeline instead of a return cascade.
                continue;
            }
            retain_native_typing_diagnostic(error, recover_typing_diagnostics, &mut diagnostics)?;
        }
    }

    Ok(diagnostics)
}

fn host_error_dependent_returns(ir: &FrontendIr, error: &CompileError) -> HashSet<u16> {
    let CompileError::HostCallResolve {
        span: Some(callee), ..
    } = error
    else {
        return HashSet::new();
    };
    let Some(index) = ir.parsed_semantic_index.as_ref() else {
        return HashSet::new();
    };
    let mut affected = HashSet::new();
    loop {
        let before = affected.len();
        for declaration in &ir.functions {
            if declaration.return_schema.is_some() || affected.contains(&declaration.index) {
                continue;
            }
            let Some(implementation) = ir.function_impls.get(&declaration.index) else {
                continue;
            };
            let Expr::Call(target, _, _, None, Some(id)) = &implementation.body_expr else {
                continue;
            };
            let Some(site) = index.call_sites.iter().find(|site| site.id == *id) else {
                continue;
            };
            let failed_host = site.callee_span == *callee
                && ir
                    .host_api_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.candidates(*target))
                    .is_some();
            if failed_host || affected.contains(target) {
                affected.insert(declaration.index);
            }
        }
        if before == affected.len() {
            break;
        }
    }
    affected
}

fn retain_native_typing_diagnostic(
    error: CompileError,
    recover: bool,
    diagnostics: &mut Vec<CompileError>,
) -> Result<(), CompileError> {
    if recover
        && matches!(
            error,
            CompileError::BinaryOperandTypeMismatch { .. }
                | CompileError::IfElseBranchTypeMismatch { .. }
                | CompileError::CallableArgumentTypeMismatch { .. }
        )
    {
        diagnostics.push(error);
        Ok(())
    } else {
        Err(error)
    }
}

pub(crate) fn infer_expr_type(expr: &Expr, state: &LocalTypeState) -> BoundType {
    let empty_impls: HashMap<u16, FunctionImpl> = HashMap::new();
    let empty_function_decls: HashMap<u16, FunctionDecl> = HashMap::new();
    let empty_struct_schemas: HashMap<String, StructDecl> = HashMap::new();
    let empty_imports: HashMap<u16, BoundType> = HashMap::new();
    let empty_signatures: HashMap<u16, HostCallableSignature> = HashMap::new();
    infer_expr_type_with_function_impls_and_imports(
        expr,
        state,
        &empty_impls,
        &empty_function_decls,
        &empty_struct_schemas,
        &empty_imports,
        &empty_signatures,
    )
}

pub(crate) fn infer_expr_type_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_decls: &HashMap<u16, FunctionDecl>,
    struct_schemas: &HashMap<String, StructDecl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> BoundType {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        function_decls,
        struct_schemas,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
        TypingMode::DynamicHints,
        None,
    );
    context.infer_expr_type(expr, state)
}

pub(crate) fn infer_expr_schema_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_decls: &HashMap<u16, FunctionDecl>,
    struct_schemas: &HashMap<String, StructDecl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> Option<TypeSchema> {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        function_decls,
        struct_schemas,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
        TypingMode::DynamicHints,
        None,
    );
    context.infer_expr_schema(expr, state)
}

pub(crate) fn expr_is_optional_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_decls: &HashMap<u16, FunctionDecl>,
    struct_schemas: &HashMap<String, StructDecl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> bool {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        function_decls,
        struct_schemas,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
        TypingMode::DynamicHints,
        None,
    );
    context.expr_is_optional(expr, state)
}

pub(crate) fn infer_optional_expr_inner_type_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_decls: &HashMap<u16, FunctionDecl>,
    struct_schemas: &HashMap<String, StructDecl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> BoundType {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        function_decls,
        struct_schemas,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
        TypingMode::DynamicHints,
        None,
    );
    context.infer_optional_expr_inner_type(expr, state)
}

pub(crate) fn infer_optional_expr_inner_schema_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_decls: &HashMap<u16, FunctionDecl>,
    struct_schemas: &HashMap<String, StructDecl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> Option<TypeSchema> {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        function_decls,
        struct_schemas,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
        TypingMode::DynamicHints,
        None,
    );
    context.infer_optional_expr_inner_schema(expr, state)
}

pub(crate) fn apply_stmts_with_function_impls_and_imports(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    function_decls: &HashMap<u16, FunctionDecl>,
    struct_schemas: &HashMap<String, StructDecl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        function_decls,
        struct_schemas,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
        TypingMode::DynamicHints,
        None,
    );
    context.apply_stmts(stmts, state);
}

pub(crate) fn refine_state_for_condition(
    state: &LocalTypeState,
    condition: &Expr,
    truthy: bool,
) -> LocalTypeState {
    validate::refine_state_for_condition(state, condition, truthy)
}

pub(crate) fn build_host_import_signatures(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<u16, HostCallableSignature> {
    helpers::build_host_import_signatures(functions, function_impls)
}

struct ResultTyping<'a> {
    function_names: HashMap<u16, String>,
    function_decls: HashMap<u16, FunctionDecl>,
    host_import_return_types: HashMap<u16, BoundType>,
    host_import_signatures: HashMap<u16, HostCallableSignature>,
    function_impls: &'a HashMap<u16, FunctionImpl>,
    struct_schemas: &'a HashMap<String, StructDecl>,
}

impl<'a> ResultTyping<'a> {
    fn new(
        functions: &[FunctionDecl],
        function_impls: &'a HashMap<u16, FunctionImpl>,
        struct_schemas: &'a HashMap<String, StructDecl>,
    ) -> Self {
        Self {
            function_names: build_function_names(functions),
            function_decls: build_function_decl_map(functions),
            host_import_return_types: build_host_import_return_types(functions, function_impls),
            host_import_signatures: build_host_import_signatures(functions, function_impls),
            function_impls,
            struct_schemas,
        }
    }

    fn context(&self) -> TypeContext<'_> {
        TypeContext::new(
            self.function_impls,
            &self.function_decls,
            self.struct_schemas,
            &self.function_names,
            &self.host_import_return_types,
            &self.host_import_signatures,
            TypingMode::Strict,
            None,
        )
    }
}

fn result_local_state(slots: &HashMap<LocalSlot, TypeSchema>) -> LocalTypeState {
    let mut state = LocalTypeState::default();
    for (slot, schema) in slots {
        state.set_with_schema_origin(
            *slot,
            bound_type_from_schema(schema),
            Some(schema.clone()),
            true,
        );
    }
    state
}

fn inferred_value_schema(
    typing: &mut ResultTyping<'_>,
    expr: &Expr,
    slots: &HashMap<LocalSlot, TypeSchema>,
) -> Option<TypeSchema> {
    let state = result_local_state(slots);
    let mut context = typing.context();
    context
        .infer_expr_schema(expr, &state)
        .or_else(|| scalar_schema_from_bound_type(infer_expr_type(expr, &state)))
        .filter(|schema| !matches!(schema, TypeSchema::Unknown))
}

/// Infer and check a Result payload with the typing owner's schema rules.
/// Numeric compatibility applies only at this selected-payload position.
/// `Unknown` and unbound generic parameters are not success.
pub(crate) fn check_result_payload(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    payload: &Expr,
    expected: &TypeSchema,
    slots: &HashMap<LocalSlot, TypeSchema>,
) -> Result<(), String> {
    let mut typing = ResultTyping::new(functions, function_impls, struct_schemas);
    let Some(actual) = inferred_value_schema(&mut typing, payload, slots) else {
        return Err(format!(
            "cannot check Result payload against {}",
            render_schema_label(expected)
        ));
    };
    let mut context = typing.context();
    context.result_payload_strict = true;
    if let Some(detail) =
        helpers::find_declared_schema_mismatch(expected, &actual, &mut context, String::new())
    {
        return Err(detail);
    }
    Ok(())
}

pub(crate) fn contextual_callable_slot_schemas(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    caller_slots: &HashMap<LocalSlot, TypeSchema>,
    param_slots: &[LocalSlot],
    param_schemas: Option<&[TypeSchema]>,
    capture_copies: &[(LocalSlot, LocalSlot)],
) -> HashMap<LocalSlot, TypeSchema> {
    let caller = result_local_state(caller_slots);
    let typing = ResultTyping::new(functions, function_impls, struct_schemas);
    let mut context = typing.context();
    let declared = param_schemas.map(|schemas| {
        schemas
            .iter()
            .map(|schema| (!matches!(schema, TypeSchema::Unknown)).then(|| schema.clone()))
            .collect::<Vec<_>>()
    });
    let Some(nested) = context.build_callable_state(
        param_slots,
        declared.as_deref(),
        capture_copies,
        None,
        &caller,
    ) else {
        return HashMap::new();
    };
    let mut slots = HashMap::new();
    for (_, captured) in capture_copies {
        if let Some(schema) = exported_logical_schema(&nested, *captured) {
            slots.insert(*captured, schema);
        }
    }
    for slot in param_slots {
        if let Some(schema) = exported_logical_schema(&nested, *slot) {
            slots.insert(*slot, schema);
        }
    }
    slots
}

fn exported_logical_schema(state: &LocalTypeState, slot: LocalSlot) -> Option<TypeSchema> {
    let schema = state.schema(slot).cloned()?;
    project_logical_schema(schema, state.is_optional(slot))
}

pub(crate) fn infer_value_schema(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    expr: &Expr,
    slots: &HashMap<LocalSlot, TypeSchema>,
) -> Option<TypeSchema> {
    let mut typing = ResultTyping::new(functions, function_impls, struct_schemas);
    inferred_value_schema(&mut typing, expr, slots)
}

pub(crate) fn argument_schemas_for_call(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    index: u16,
    type_args: &[TypeSchema],
) -> Option<Vec<Option<TypeSchema>>> {
    if crate::builtins::BuiltinFunction::from_call_index(index).is_some() {
        return None;
    }
    let typing = ResultTyping::new(functions, function_impls, struct_schemas);
    let mut context = typing.context();
    context.resolve_function_arg_schemas(index, type_args)
}

pub(crate) fn expected_container_argument_schemas(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    index: u16,
    args: &[Expr],
    expected: Option<&TypeSchema>,
) -> Option<Vec<Option<TypeSchema>>> {
    let builtin = crate::builtins::BuiltinFunction::from_call_index(index)?;
    let expected = expected?;
    match builtin {
        crate::builtins::BuiltinFunction::ArrayPush if args.len() == 2 => {
            expected.array_prefix_and_rest()?;
            let element = expected
                .array_item_schema_at(0)
                .or_else(|| expected.collapsed_array_item_schema())
                .filter(|schema| !matches!(schema, TypeSchema::Unknown));
            Some(vec![Some(expected.clone()), element])
        }
        crate::builtins::BuiltinFunction::Set if args.len() == 3 => {
            let value = expected_named_or_container_field_schema(
                functions,
                function_impls,
                struct_schemas,
                expected,
                &args[1],
            );
            Some(vec![Some(expected.clone()), None, value])
        }
        _ => None,
    }
}

fn expected_named_or_container_field_schema(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    expected: &TypeSchema,
    key: &Expr,
) -> Option<TypeSchema> {
    let Expr::String(name) = key else {
        return None;
    };
    let schema = match expected {
        TypeSchema::Named(struct_name, type_args) => {
            let typing = ResultTyping::new(functions, function_impls, struct_schemas);
            let mut context = typing.context();
            context::source_named_struct_field_schema(struct_name, type_args, name, &mut context)?
        }
        TypeSchema::Object(fields) => fields.get(name).cloned()?,
        TypeSchema::Map(inner) => inner.as_ref().clone(),
        _ => return None,
    };
    (!matches!(schema, TypeSchema::Unknown)).then_some(schema)
}

pub(crate) fn result_branch_schemas_compatible(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
    struct_schemas: &HashMap<String, StructDecl>,
    expected: &TypeSchema,
    actual: &TypeSchema,
) -> bool {
    let typing = ResultTyping::new(functions, function_impls, struct_schemas);
    let context = typing.context();
    context.substitute_schema_generics(expected) == context.substitute_schema_generics(actual)
}

#[cfg(test)]
mod catalog_call_resolution_tests {
    use std::sync::Arc;

    use crate::compiler::frontends::parse_source;
    use crate::compiler::{CompileError, CompileSourceFileOptions, SourceFlavor};
    use crate::host_api::{
        HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
        HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
    };

    use super::*;

    fn catalog(
        resources: Vec<ResourceTypeSchema>,
        functions: Vec<HostFunctionSchema>,
    ) -> Arc<HostApiCatalog> {
        let mut builder = HostApiBuilder::new();
        for resource in resources {
            builder.resource(resource);
        }
        for function in functions {
            builder.function(function);
        }
        Arc::new(builder.build().expect("test catalog must be valid"))
    }

    fn parse(source: &str, catalog: Arc<HostApiCatalog>) -> FrontendIr {
        let options = CompileSourceFileOptions::default().with_host_api_catalog(catalog);
        parse_source(source, SourceFlavor::RustScript, &options).expect("source must parse")
    }

    fn stmt_expr(stmt: &Stmt) -> Option<&Expr> {
        match stmt {
            Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                Some(expr)
            }
            _ => None,
        }
    }

    fn stmt_exprs(ir: &FrontendIr) -> Vec<&Expr> {
        ir.stmts.iter().filter_map(stmt_expr).collect()
    }

    #[test]
    fn root_result_join_preserves_context_guards_and_aggregate() {
        for (source, expected, optional) in [
            (
                "let maybe: float? = 2.5; let y = if true => { maybe } else => { null };",
                Some(TypeSchema::Float),
                true,
            ),
            (
                "let maybe: float? = 2.5; let y = match 1 { 1 => maybe, _ => null };",
                Some(TypeSchema::Float),
                true,
            ),
            // Match keeps its existing explicit-arm disagreement rule.
            (
                "let maybe: float? = 2.5; let y = match 1 { 1 => maybe, 2 => null, _ => maybe };",
                None,
                true,
            ),
            (
                "struct Left { value: int } struct Right { value: int } let left: Left? = { value: 1 }; let right: Right? = { value: 2 }; let y = if true => { left } else => { right };",
                None,
                true,
            ),
            (
                "struct Left { value: int } struct Right { value: int } let left: Left? = { value: 1 }; let right: Right? = { value: 2 }; let y = match 1 { 1 => left, _ => right };",
                None,
                true,
            ),
            (
                "let y = if true => { null } else => { null };",
                Some(TypeSchema::Null),
                false,
            ),
        ] {
            let ir = parse(source, catalog(vec![], vec![]));
            let slot = ir
                .local_bindings
                .iter()
                .find(|(name, _)| name == "y")
                .unwrap()
                .1;
            let before = format!("{ir:?}");
            let aggregate = infer_types(&ir, TypingMode::DynamicHints, &[]);
            let (projected, roots) =
                infer_types_with_root_projection(&ir, TypingMode::DynamicHints, &[], &[slot]);
            assert_eq!(aggregate, projected, "{source}");
            assert_eq!(before, format!("{ir:?}"));
            let declarations = HashMap::new();
            let names = HashMap::new();
            let returns = HashMap::new();
            let signatures = HashMap::new();
            let mut reference = TypeContext::new(
                &ir.function_impls,
                &declarations,
                &ir.struct_schemas,
                &names,
                &returns,
                &signatures,
                TypingMode::DynamicHints,
                None,
            );
            let mut reference_state = LocalTypeState::default();
            reference.apply_stmts(&ir.stmts, &mut reference_state);
            let reference_schema = reference
                .infer_expr_schema(&Expr::Var(slot), &reference_state)
                .or_else(|| scalar_schema_from_bound_type(reference_state.get(slot)));
            assert_eq!(
                roots[0].schema, reference_schema,
                "existing context: {source}"
            );
            assert_eq!(roots[0].optional, reference_state.is_optional(slot));
            assert_eq!(roots[0].schema, expected, "{source}");
            assert_eq!(roots[0].optional, optional, "{source}");
        }
    }

    #[test]
    fn root_result_join_unknown_snapshot_is_not_null_or_a_reused_slot() {
        for reverse in [false, true] {
            let mut ir = parse(
                "let maybe: float? = 2.5; let y = if true => { maybe } else => { null };",
                catalog(vec![], vec![]),
            );
            let Stmt::Let { index, expr, .. } = &mut ir.stmts[1] else {
                panic!("result binding")
            };
            let result_slot = *index;
            let Expr::IfElse {
                then_expr,
                else_expr,
                ..
            } = expr
            else {
                panic!("conditional")
            };
            // An unbound inference-only value carries schema=None, not Null.
            **else_expr = Expr::Var(60000);
            if reverse {
                std::mem::swap(then_expr, else_expr);
            }
            let aggregate = infer_types(&ir, TypingMode::DynamicHints, &[]);
            let (projected, roots) = infer_types_with_root_projection(
                &ir,
                TypingMode::DynamicHints,
                &[],
                &[result_slot],
            );
            assert_eq!(aggregate, projected);
            assert_eq!(roots[0].schema, None);
            assert!(roots[0].optional);
        }
    }

    #[test]
    fn root_expression_effects_block_local_shadow_is_a_distinct_slot() {
        use crate::compiler::ir::AssignmentKind;
        let mut ir = parse("let mut x = 1; let ignored = 0;", catalog(vec![], vec![]));
        ir.locals = 3;
        let shadow = 2;
        let Expr::Int(_) = stmt_expr(&ir.stmts[1]).unwrap() else {
            panic!("literal initializer");
        };
        let Stmt::Let { expr, .. } = &mut ir.stmts[1] else {
            panic!("let initializer");
        };
        *expr = Expr::Block {
            stmts: vec![
                Stmt::Let {
                    index: shadow,
                    declared_schema: None,
                    expr: Expr::Int(2),
                    line: 1,
                },
                Stmt::Assign {
                    index: shadow,
                    kind: AssignmentKind::Set,
                    expr: Expr::Float(2.5),
                    line: 1,
                },
            ],
            expr: Box::new(Expr::Int(0)),
        };
        // A frontend can attach the same spelling to distinct lexical slots.
        ir.local_bindings.push(("x".into(), shadow));
        let aggregate = infer_types(&ir, TypingMode::DynamicHints, &[]);
        let (projected, roots) =
            infer_types_with_root_projection(&ir, TypingMode::DynamicHints, &[], &[0, shadow]);
        assert_eq!(aggregate, projected);
        assert_eq!(roots[0].schema, Some(TypeSchema::Int));
        assert_eq!(
            roots[1].schema, None,
            "a block-only slot must not enter final root state"
        );
    }

    #[test]
    fn root_expression_effects_preserve_aggregate_and_conservative_paths() {
        for (source, expected) in [
            (
                "let mut x = 1; let ignored = if true => { x = 2.5; 0 } else => { x = 2.5; 0 };",
                Some(TypeSchema::Float),
            ),
            (
                "let mut x = 1; let ignored = if (if true => { x = 2.5; true } else => { x = 2.5; true }) => { 0 } else => { 0 };",
                Some(TypeSchema::Float),
            ),
            (
                "let mut x = 1; let ignored = if true => { x = 2.5; 0 } else => { 0 };",
                None,
            ),
            (
                "let mut x = 1; let ignored = if false => { x = 2.5; 0 } else => { 0 };",
                None,
            ),
            (
                "let mut x = 1; let ignored = false && (if true => { x = 2.5; true } else => { x = 2.5; true });",
                None,
            ),
            (
                "let mut x = 1; let ignored = true || (if true => { x = 2.5; false } else => { x = 2.5; false });",
                None,
            ),
            (
                "let mut x = 1; while false { let ignored = if true => { x = 2.5; 0 } else => { x = 2.5; 0 }; }",
                None,
            ),
            (
                "let mut x = 1; let f = |n| if true => { let mut local = x; local = 2.5; n } else => { n };",
                Some(TypeSchema::Int),
            ),
            (
                "let mut x = 1; fn f() -> int { let mut x = 1; x = 2.5; 0 }",
                Some(TypeSchema::Int),
            ),
        ] {
            println!("root expression effects probe source={source}");
            let ir = parse(source, catalog(vec![], vec![]));
            let ir = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[]).unwrap();
            validate_if_else_type_consistency(&ir, TypingMode::DynamicHints, &[]).unwrap();
            let root = ir
                .local_bindings
                .iter()
                .find(|(name, _)| name == "x")
                .unwrap()
                .1;
            let allocated =
                crate::compiler::lifetime::allocate_local_slots_with_root_slots(ir, &[root])
                    .unwrap();
            let root = allocated.mapping[root as usize];
            let before = allocated.ir.clone();
            let aggregate = infer_types(&allocated.ir, TypingMode::DynamicHints, &[]);
            let (projected_aggregate, projection) = infer_types_with_root_projection(
                &allocated.ir,
                TypingMode::DynamicHints,
                &[],
                &[root],
            );
            assert_eq!(aggregate, projected_aggregate, "{source}");
            assert_eq!(
                format!("{before:?}"),
                format!("{:?}", allocated.ir),
                "projection must not rewrite backend IR"
            );
            assert_eq!(projection[0].schema, expected, "{source}");
            assert!(!projection[0].optional, "{source}");
            if source.contains("else => { x = 2.5;") {
                // Both executed alternatives update the root, but aggregate
                // observations still use the original physical-slot merge.
                assert_eq!(
                    aggregate.local_schemas[root as usize],
                    Some(TypeSchema::Float)
                );
            }
        }
    }

    #[test]
    fn root_projection_preserves_aggregate_callee_and_closure_facts() {
        let named = crate::HostStructSchema::new(
            "ProjectionRoot",
            vec![crate::HostStructField::new("value", HostTypeSchema::Int)],
        );
        let mut builder = HostApiBuilder::new();
        builder.named_struct(named.clone());
        builder.function(HostFunctionSchema::with_return(
            "layout::root",
            vec![],
            named.as_type(),
        ));
        builder.function(HostFunctionSchema::with_return(
            "layout::maybe",
            vec![],
            HostTypeSchema::Optional(Box::new(named.as_type())),
        ));
        let catalog = Arc::new(builder.build().unwrap());
        let locals = (0..257)
            .map(|i| format!("let t{i} = {i};"))
            .collect::<Vec<_>>()
            .join(" ");
        for body in [
            format!("fn f() -> int {{ {locals} t256 }} f();"),
            format!("let f = |n| if n > 0 => {{ {locals} t256 + n }} else => {{ n }}; f(1);"),
        ] {
            let ir = parse(
                &format!(
                    "use layout; let root = layout::root(); let maybe = layout::maybe(); {body}"
                ),
                Arc::clone(&catalog),
            );
            let ir = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[]).unwrap();
            let roots = ["root", "maybe"]
                .map(|name| ir.local_bindings.iter().find(|(n, _)| n == name).unwrap().1);
            let allocated =
                crate::compiler::lifetime::allocate_local_slots_with_root_slots(ir, &roots)
                    .unwrap();
            let roots = roots.map(|slot| allocated.mapping[slot as usize]);
            let aggregate = infer_types(&allocated.ir, TypingMode::DynamicHints, &[]);
            let (projected_aggregate, projection) = infer_types_with_root_projection(
                &allocated.ir,
                TypingMode::DynamicHints,
                &[],
                &roots,
            );
            assert_eq!(aggregate.local_types, projected_aggregate.local_types);
            assert_eq!(aggregate.local_schemas, projected_aggregate.local_schemas);
            assert_eq!(
                aggregate.local_schema_labels,
                projected_aggregate.local_schema_labels
            );
            assert_eq!(aggregate.callable_slots, projected_aggregate.callable_slots);
            assert_eq!(aggregate.optional_slots, projected_aggregate.optional_slots);
            assert_eq!(
                aggregate.function_schemas,
                projected_aggregate.function_schemas
            );
            let schema = TypeSchema::Named("ProjectionRoot".into(), vec![]);
            assert_eq!(projection[0].slot, roots[0]);
            assert_eq!(projection[0].schema, Some(schema.clone()));
            assert!(!projection[0].optional);
            assert_eq!(projection[1].slot, roots[1]);
            assert_eq!(
                projection[1].schema,
                Some(TypeSchema::Optional(Box::new(schema)))
            );
            assert!(projection[1].optional);
            let expected_backend = if body.starts_with("fn ") {
                TypeSchema::Int
            } else {
                TypeSchema::Named("ProjectionRoot".into(), vec![])
            };
            assert_eq!(
                aggregate.local_schemas[roots[0] as usize],
                Some(expected_backend)
            );
        }
    }

    #[test]
    fn catalog_calls_at_one_flat_index_resolve_per_site() {
        let catalog = catalog(
            Vec::new(),
            vec![
                HostFunctionSchema::with_return(
                    "acme::id",
                    vec![HostParamSchema::value("value", HostTypeSchema::Int)],
                    HostTypeSchema::Int,
                ),
                HostFunctionSchema::with_return(
                    "acme::id",
                    vec![HostParamSchema::value("value", HostTypeSchema::String)],
                    HostTypeSchema::String,
                ),
            ],
        );
        let fingerprint = catalog.fingerprint();
        let ir = parse("use acme;\nacme::id(1);\nacme::id(\"x\");\n", catalog);
        let ir = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[]).unwrap();
        let expressions = stmt_exprs(&ir);
        let first = expressions[0].host_call_resolution().unwrap();
        let second = expressions[1].host_call_resolution().unwrap();
        assert_eq!(first.return_type, TypeSchema::Int);
        assert_eq!(second.return_type, TypeSchema::String);
        assert_eq!(first.passing, vec![HostParamPassing::Value]);
        assert_eq!(second.passing, vec![HostParamPassing::Value]);
        assert_eq!(first.fingerprint, fingerprint);
        assert_eq!(second.fingerprint, fingerprint);
    }

    #[test]
    fn nested_catalog_call_resolves_child_before_parent() {
        let key = ResourceTypeKey::new("acme.file").unwrap();
        let catalog = catalog(
            vec![ResourceTypeSchema::new(key.clone(), "file")],
            vec![
                HostFunctionSchema::with_return(
                    "acme::open",
                    vec![HostParamSchema::value("path", HostTypeSchema::String)],
                    HostTypeSchema::Resource(key.clone()),
                ),
                HostFunctionSchema::with_return(
                    "acme::consume",
                    vec![HostParamSchema::with_passing(
                        "file",
                        HostTypeSchema::Resource(key.clone()),
                        HostParamPassing::TakeOwned,
                    )],
                    HostTypeSchema::String,
                ),
            ],
        );
        let ir = parse("use acme;\nacme::consume(acme::open(\"x\"));\n", catalog);
        let ir = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[]).unwrap();
        let expressions = stmt_exprs(&ir);
        let Expr::Call(_, _, args, Some(outer), _) = expressions[0] else {
            panic!("outer call must be resolved");
        };
        assert_eq!(outer.return_type, TypeSchema::String);
        assert_eq!(outer.passing, vec![HostParamPassing::TakeOwned]);
        assert_eq!(
            args[0].host_call_resolution().unwrap().return_type,
            TypeSchema::Resource(key)
        );
    }

    #[test]
    fn resource_call_passing_follows_exact_argument_syntax() {
        let key = ResourceTypeKey::new("acme.file").unwrap();
        let resource = HostTypeSchema::Resource(key.clone());
        let catalog = catalog(
            vec![ResourceTypeSchema::new(key, "file")],
            vec![
                HostFunctionSchema::with_return(
                    "acme::open",
                    vec![HostParamSchema::value("path", HostTypeSchema::String)],
                    resource.clone(),
                ),
                HostFunctionSchema::new(
                    "acme::touch",
                    vec![HostParamSchema::with_passing(
                        "file",
                        resource.clone(),
                        HostParamPassing::Borrow,
                    )],
                ),
                HostFunctionSchema::new(
                    "acme::touch",
                    vec![HostParamSchema::with_passing(
                        "file",
                        resource.clone(),
                        HostParamPassing::BorrowMut,
                    )],
                ),
                HostFunctionSchema::new(
                    "acme::consume",
                    vec![HostParamSchema::with_passing(
                        "file",
                        resource,
                        HostParamPassing::TakeOwned,
                    )],
                ),
            ],
        );
        let ir = parse(
            "use acme;\nlet mut file = acme::open(\"x\");\nacme::touch(&file);\nacme::touch(&mut file);\nacme::consume(file);\n",
            catalog,
        );
        let ir = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[]).unwrap();
        let passing = ir
            .stmts
            .iter()
            .filter_map(stmt_expr)
            .filter_map(Expr::host_call_resolution)
            .map(|resolution| resolution.passing.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            passing,
            vec![
                vec![HostParamPassing::Value],
                vec![HostParamPassing::Borrow],
                vec![HostParamPassing::BorrowMut],
                vec![HostParamPassing::TakeOwned],
            ]
        );
    }

    #[test]
    fn loop_probe_does_not_resolve_or_count_cloned_calls() {
        let catalog = catalog(
            Vec::new(),
            vec![HostFunctionSchema::new("acme::ping", Vec::new())],
        );
        let mut ir = parse("use acme;\nwhile false {\n    acme::ping();\n}\n", catalog);
        let metadata = ir.host_api_metadata.clone().unwrap();
        let mut pass =
            HostCallResolutionPass::new(Some(&metadata), HostCallResolutionPhase::Refine);
        run_legalize_round(&mut ir, TypingMode::DynamicHints, &[], &mut pass);
        assert_eq!(pass.changed(), 1, "only the real loop body may annotate");
        assert_eq!(pass.unresolved(), 0);
        let body = ir
            .stmts
            .iter()
            .find_map(|stmt| match stmt {
                Stmt::While { body, .. } => Some(body),
                _ => None,
            })
            .expect("while body");
        assert!(
            body.iter()
                .filter_map(stmt_expr)
                .any(|expr| expr.host_call_resolution().is_some())
        );
    }

    #[test]
    fn function_body_failure_uses_function_source_and_statement_line() {
        let catalog = catalog(
            Vec::new(),
            vec![HostFunctionSchema::new(
                "acme::takes_int",
                vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            )],
        );
        let mut ir = parse(
            "use acme;\nfn bad() {\n    acme::takes_int(\"x\");\n}\nbad();\n",
            catalog,
        );
        let function_indices = ir.function_impls.keys().copied().collect::<Vec<_>>();
        for index in function_indices {
            ir.function_sources.insert(index, "module.rss".to_string());
        }
        let error = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[])
            .expect_err("function-body mismatch must fail");
        let CompileError::HostCallResolve {
            line, source_name, ..
        } = error
        else {
            panic!("expected HostCallResolve, found {error:?}");
        };
        assert_eq!(line, Some(3));
        assert_eq!(source_name.as_deref(), Some("module.rss"));
    }

    #[test]
    fn catalog_only_options_reach_compile_and_hint_resolution_without_panicking() {
        let key = ResourceTypeKey::new("acme.file").unwrap();
        let resource = HostTypeSchema::Resource(key.clone());
        let catalog = catalog(
            vec![ResourceTypeSchema::new(key, "file")],
            vec![
                HostFunctionSchema::with_return(
                    "acme::open",
                    vec![HostParamSchema::value("path", HostTypeSchema::String)],
                    resource.clone(),
                ),
                HostFunctionSchema::new(
                    "acme::consume",
                    vec![HostParamSchema::with_passing(
                        "file",
                        resource,
                        HostParamPassing::TakeOwned,
                    )],
                ),
            ],
        );
        let source = "use acme;\nlet file = acme::open(\"x\");\nacme::consume(file.copy());\n";

        let compile_error = match crate::compiler::compile_source_with_flavor_and_options(
            source,
            SourceFlavor::RustScript,
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
        ) {
            Err(error) => error,
            Ok(_) => panic!("catalog-only compile options must invoke exact resolution"),
        };
        let hint_error = crate::compiler::collect_inferred_local_type_hints_with_options(
            source,
            SourceFlavor::RustScript,
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
        )
        .expect_err("hint collection must return the resolver error");

        for (entrypoint, error) in [("compile", compile_error), ("hint", hint_error)] {
            let source_error = match error {
                crate::compiler::SourcePathError::Source(error) => error,
                crate::compiler::SourcePathError::SourceWithMap { error, .. } => error,
                other => panic!("unexpected path error: {other}"),
            };
            assert!(
                matches!(
                    source_error,
                    crate::compiler::SourceError::Compile(CompileError::HostCallResolve {
                        line: Some(3),
                        ..
                    })
                ),
                "{entrypoint}: {source_error:?}"
            );
        }

        let configured =
            || CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
        let valid = "use acme;\nlet file = acme::open(\"x\");\nacme::consume(file);\n";
        crate::compiler::compile_source_with_flavor_and_options(
            valid,
            SourceFlavor::RustScript,
            configured(),
        )
        .expect("the original owned resource remains admissible");
        let hints = crate::compiler::collect_inferred_local_type_hints_with_options(
            valid,
            SourceFlavor::RustScript,
            configured(),
        )
        .unwrap();
        assert!(hints.iter().any(|hint| {
            hint.name == "file"
                && hint.schema
                    == Some(TypeSchema::Resource(
                        ResourceTypeKey::new("acme.file").unwrap(),
                    ))
        }));

        // Recovery still runs ownership admission. A standalone copy or a
        // use-after-move is independent of an invalid catalog call, including
        // when the independent statement shares the same source line.
        for invalid in [
            source,
            "use acme;\nlet file = acme::open(\"x\");\nfile.copy(); acme::consume(42);\n",
            "use acme;\nlet file = acme::open(\"x\");\nacme::consume(42); file.copy();\n",
            "use acme;\nlet file = acme::open(\"x\");\nacme::consume(file);\nfile;\nacme::consume(42);\n",
        ] {
            let analyzed = crate::compiler::analyze_source_from_string_with_options(
                "main.rss",
                invalid,
                configured(),
            )
            .err()
            .expect("host recovery must retain ownership rejection");
            let mut errors = vec![analyzed];
            if invalid != source {
                errors.push(
                    crate::compiler::compile_source_with_flavor_and_options(
                        invalid,
                        SourceFlavor::RustScript,
                        configured(),
                    )
                    .err()
                    .expect("independent ownership rejection in compile"),
                );
                errors.push(
                    crate::compiler::collect_inferred_local_type_hints_with_options(
                        invalid,
                        SourceFlavor::RustScript,
                        configured(),
                    )
                    .expect_err("independent ownership rejection in hints"),
                );
            }
            for error in errors {
                let crate::compiler::SourcePathError::SourceWithMap {
                    error: crate::compiler::SourceError::Parse(error),
                    ..
                } = error
                else {
                    panic!("independent ownership diagnostic required: {error:?}");
                };
                assert!(
                    error.code.as_deref() == Some("E_OWNERSHIP_COPY_RESOURCE")
                        || error.message.contains("moved"),
                    "{error:?}"
                );
            }
        }
    }

    #[test]
    fn copy_cannot_satisfy_take_owned_and_preserves_site() {
        let key = ResourceTypeKey::new("acme.file").unwrap();
        let resource = HostTypeSchema::Resource(key.clone());
        let catalog = catalog(
            vec![ResourceTypeSchema::new(key, "file")],
            vec![
                HostFunctionSchema::with_return(
                    "acme::open",
                    vec![HostParamSchema::value("path", HostTypeSchema::String)],
                    resource.clone(),
                ),
                HostFunctionSchema::new(
                    "acme::consume",
                    vec![HostParamSchema::with_passing(
                        "file",
                        resource,
                        HostParamPassing::TakeOwned,
                    )],
                ),
            ],
        );
        let mut ir = parse(
            "use acme;\nlet file = acme::open(\"x\");\nacme::consume(file.copy());\n",
            catalog,
        );
        ir.stmt_sources = vec![Some("unit.rss".to_string()); ir.stmts.len()];
        let error = legalize_builtins_and_bind_types(ir, TypingMode::DynamicHints, &[])
            .expect_err("copy is value passing, not take-owned");
        let CompileError::HostCallResolve {
            line,
            source_name,
            detail,
            span,
        } = error
        else {
            panic!("expected HostCallResolve");
        };
        assert_eq!(line, Some(3));
        assert_eq!(source_name.as_deref(), Some("unit.rss"));
        assert!(detail.contains("value"), "{detail}");
        assert!(detail.contains("take_owned"), "{detail}");
        // The failing call `acme::consume(...)` carries parser provenance, so
        // the diagnostic must carry the exact callee span (not a line guess).
        let span = span.expect("failing call must carry its callee span");
        let source = "use acme;\nlet file = acme::open(\"x\");\nacme::consume(file.copy());\n";
        let callee = source.find("acme::consume").expect("callee present");
        assert_eq!(span.source_id, 0, "callee span lives in source 0");
        assert_eq!((span.lo, span.hi), (callee, callee + "acme::consume".len()));
    }
}
