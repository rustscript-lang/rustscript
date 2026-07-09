mod collect;
mod context;
mod helpers;
mod state;
mod validate;

use std::collections::HashMap;

use crate::bytecode::ValueType;

use self::collect::{
    CollectFunctionTypeOutputs, CollectFunctionTypesEnv, collect_function_types,
    collect_stmt_types, record_callable_slot, record_local_schema, record_local_schema_label,
    record_local_type, record_optional_slot,
};
use self::context::TypeContext;
pub(crate) use self::context::bound_type_from_schema;
use self::helpers::{
    FunctionLegalizeEnv, build_function_decl_map, build_function_names,
    build_host_import_return_types, legalize_function_impl, legalize_stmts, validate_function_impl,
    validate_stmts,
};
pub(crate) use self::state::{
    BoundType, HostCallableSignature, LocalTypeState, TypeInferenceResult,
};
use super::CompileError;
use super::TypingMode;
use super::ir::{
    Expr, FrontendIr, FunctionDecl, FunctionImpl, LocalSlot, Stmt, StructDecl, TypeSchema,
};

#[derive(Clone, Debug)]
pub(super) struct EntryLocalType {
    pub(super) slot: LocalSlot,
    pub(super) schema: Option<TypeSchema>,
    pub(super) optional: bool,
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
    mut ir: FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> FrontendIr {
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
    );
    legalize_stmts(&mut ir.stmts, &mut top_state, &mut context);
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
    for (index, function_impl) in ir.function_impls.iter_mut() {
        legalize_function_impl(*index, function_impl, &legalize_env);
    }

    ir
}

pub(super) fn infer_types(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> TypeInferenceResult {
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
    collect_stmt_types(
        &ir.stmts,
        &mut top_state,
        &mut local_types,
        &mut local_schemas,
        &mut local_schema_labels,
        &mut callable_slots,
        &mut optional_slots,
        &mut context,
    );
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
        collect_function_types(decl.index, function_impl, decl, &mut outputs, &env);
    }

    TypeInferenceResult {
        local_types,
        local_schemas,
        local_schema_labels,
        callable_slots,
        optional_slots,
    }
}

pub(super) fn validate_if_else_type_consistency(
    ir: &FrontendIr,
    typing_mode: TypingMode,
    entry_local_types: &[EntryLocalType],
) -> Result<(), CompileError> {
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
    );
    for (index, stmt) in ir.stmts.iter().enumerate() {
        validate_stmts(
            std::slice::from_ref(stmt),
            &mut top_state,
            None,
            ir.stmt_sources
                .get(index)
                .and_then(|source| source.as_deref()),
            &mut context,
            false,
        )?;
    }

    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        validate_function_impl(
            decl.index,
            function_impl,
            ir.function_sources.get(&decl.index).map(String::as_str),
            &mut context,
        )?;
    }

    Ok(())
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
