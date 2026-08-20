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
    FunctionLegalizeEnv, HostCallResolutionPass, HostCallResolutionPhase, build_function_decl_map,
    build_function_names, build_host_import_return_types, legalize_function_impl, legalize_stmts,
    validate_function_impl, validate_stmts,
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
) -> Result<FrontendIr, CompileError> {
    let Some(metadata) = ir.host_api_metadata.clone() else {
        let mut pass = HostCallResolutionPass::new(None, HostCallResolutionPhase::Disabled);
        run_legalize_round(&mut ir, typing_mode, entry_local_types, &mut pass);
        return Ok(ir);
    };

    loop {
        let mut refine =
            HostCallResolutionPass::new(Some(&metadata), HostCallResolutionPhase::Refine);
        run_legalize_round(&mut ir, typing_mode, entry_local_types, &mut refine);
        if refine.changed() > 0 {
            continue;
        }
        if refine.unresolved() == 0 {
            return Ok(ir);
        }

        let mut final_pass =
            HostCallResolutionPass::new(Some(&metadata), HostCallResolutionPhase::Final);
        run_legalize_round(&mut ir, typing_mode, entry_local_types, &mut final_pass);
        if final_pass.changed() > 0 {
            continue;
        }
        if final_pass.unresolved() == 0 {
            return Ok(ir);
        }
        return Err(final_pass
            .take_error()
            .expect("a final unresolved catalog call must record a compile error"));
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
            CompileSourceFileOptions::default().with_host_api_catalog(catalog),
        )
        .expect_err("hint collection must return the resolver error");

        for error in [compile_error, hint_error] {
            let source_error = match error {
                crate::compiler::SourcePathError::Source(error) => error,
                crate::compiler::SourcePathError::SourceWithMap { error, .. } => error,
                other => panic!("unexpected path error: {other}"),
            };
            assert!(matches!(
                source_error,
                crate::compiler::SourceError::Compile(CompileError::HostCallResolve {
                    line: Some(3),
                    ..
                })
            ));
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
        } = error
        else {
            panic!("expected HostCallResolve");
        };
        assert_eq!(line, Some(3));
        assert_eq!(source_name.as_deref(), Some("unit.rss"));
        assert!(detail.contains("value"), "{detail}");
        assert!(detail.contains("take_owned"), "{detail}");
    }
}
