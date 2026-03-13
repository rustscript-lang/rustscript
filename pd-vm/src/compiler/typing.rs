mod collect;
mod context;
mod helpers;
mod state;
mod validate;

use std::collections::HashMap;

use crate::bytecode::ValueType;

use self::collect::{collect_function_types, collect_stmt_types};
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
use super::ir::{Expr, FrontendIr, FunctionDecl, FunctionImpl, Stmt, StructDecl, TypeSchema};
pub(super) fn legalize_builtins_and_bind_types(mut ir: FrontendIr) -> FrontendIr {
    let function_names = build_function_names(&ir.functions);
    let function_decls = build_function_decl_map(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_decls,
        &ir.struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        false,
    );
    legalize_stmts(&mut ir.stmts, &mut top_state, &mut context);
    let observed_function_param_types = context.observed_function_param_types.clone();
    let observed_function_param_schemas = context.observed_function_param_schemas.clone();
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
        observed_function_capture_states: &observed_function_capture_states,
    };
    for (index, function_impl) in ir.function_impls.iter_mut() {
        legalize_function_impl(*index, function_impl, &legalize_env);
    }

    ir
}

pub(super) fn infer_types(ir: &FrontendIr) -> TypeInferenceResult {
    let function_names = build_function_names(&ir.functions);
    let function_decls = build_function_decl_map(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut local_types = vec![ValueType::Unknown; ir.locals];
    let mut local_schema_labels = vec![None; ir.locals];
    let mut callable_slots = vec![false; ir.locals];
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_decls,
        &ir.struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        false,
    );
    collect_stmt_types(
        &ir.stmts,
        &mut top_state,
        &mut local_types,
        &mut local_schema_labels,
        &mut callable_slots,
        &mut context,
    );
    let observed_function_param_types = context.observed_function_param_types.clone();
    let observed_function_param_schemas = context.observed_function_param_schemas.clone();
    let observed_function_capture_states = context.observed_function_capture_states.clone();

    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        collect_function_types(
            decl.index,
            function_impl,
            decl,
            &mut local_types,
            &mut local_schema_labels,
            &mut callable_slots,
            &ir.function_impls,
            &function_decls,
            &function_names,
            &ir.struct_schemas,
            &host_import_return_types,
            &host_import_signatures,
            &observed_function_param_types,
            &observed_function_param_schemas,
            &observed_function_capture_states,
        );
    }

    TypeInferenceResult {
        local_types,
        local_schema_labels,
        callable_slots,
    }
}

pub(super) fn validate_if_else_type_consistency(
    ir: &FrontendIr,
    require_declared_schema_for_optional_access: bool,
) -> Result<(), CompileError> {
    let function_names = build_function_names(&ir.functions);
    let function_decls = build_function_decl_map(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_decls,
        &ir.struct_schemas,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
        require_declared_schema_for_optional_access,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
