mod collect;
mod context;
mod helpers;
mod state;
mod validate;

use std::collections::HashMap;

use crate::bytecode::ValueType;

use self::collect::{collect_function_types, collect_stmt_types};
use self::context::TypeContext;
use self::helpers::{
    build_function_names, build_host_import_return_types, legalize_function_impl, legalize_stmts,
    validate_function_impl, validate_stmts,
};
pub(crate) use self::state::{
    BoundType, HostCallableSignature, LocalTypeState, TypeInferenceResult,
};
use super::CompileError;
use super::ir::{Expr, FrontendIr, FunctionDecl, FunctionImpl, Stmt};
pub(super) fn legalize_builtins_and_bind_types(mut ir: FrontendIr) -> FrontendIr {
    let function_names = build_function_names(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
    );
    legalize_stmts(&mut ir.stmts, &mut top_state, &mut context);
    let observed_function_param_types = context.observed_function_param_types.clone();

    let function_impls = ir.function_impls.clone();
    for (index, function_impl) in ir.function_impls.iter_mut() {
        legalize_function_impl(
            *index,
            function_impl,
            &function_impls,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            &observed_function_param_types,
        );
    }

    ir
}

pub(super) fn infer_types(ir: &FrontendIr) -> TypeInferenceResult {
    let function_names = build_function_names(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut local_types = vec![ValueType::Unknown; ir.locals];
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
    );
    collect_stmt_types(&ir.stmts, &mut top_state, &mut local_types, &mut context);
    let observed_function_param_types = context.observed_function_param_types.clone();

    for decl in &ir.functions {
        let Some(function_impl) = ir.function_impls.get(&decl.index) else {
            continue;
        };
        collect_function_types(
            decl.index,
            function_impl,
            &mut local_types,
            &ir.function_impls,
            &function_names,
            &host_import_return_types,
            &host_import_signatures,
            &observed_function_param_types,
        );
    }

    TypeInferenceResult { local_types }
}

pub(super) fn validate_if_else_type_consistency(ir: &FrontendIr) -> Result<(), CompileError> {
    let function_names = build_function_names(&ir.functions);
    let host_import_return_types =
        build_host_import_return_types(&ir.functions, &ir.function_impls);
    let host_import_signatures = build_host_import_signatures(&ir.functions, &ir.function_impls);
    let mut top_state = LocalTypeState::default();
    let mut context = TypeContext::new(
        &ir.function_impls,
        &function_names,
        &host_import_return_types,
        &host_import_signatures,
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
    let empty_imports: HashMap<u16, BoundType> = HashMap::new();
    let empty_signatures: HashMap<u16, HostCallableSignature> = HashMap::new();
    infer_expr_type_with_function_impls_and_imports(
        expr,
        state,
        &empty_impls,
        &empty_imports,
        &empty_signatures,
    )
}

pub(crate) fn infer_expr_type_with_function_impls_and_imports(
    expr: &Expr,
    state: &LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) -> BoundType {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
    );
    context.infer_expr_type(expr, state)
}

pub(crate) fn apply_stmts_with_function_impls_and_imports(
    stmts: &[Stmt],
    state: &mut LocalTypeState,
    function_impls: &HashMap<u16, FunctionImpl>,
    host_import_return_types: &HashMap<u16, BoundType>,
    host_import_signatures: &HashMap<u16, HostCallableSignature>,
) {
    let empty_function_names: HashMap<u16, String> = HashMap::new();
    let mut context = TypeContext::new(
        function_impls,
        &empty_function_names,
        host_import_return_types,
        host_import_signatures,
    );
    context.apply_stmts(stmts, state);
}

pub(crate) fn build_host_import_signatures(
    functions: &[FunctionDecl],
    function_impls: &HashMap<u16, FunctionImpl>,
) -> HashMap<u16, HostCallableSignature> {
    helpers::build_host_import_signatures(functions, function_impls)
}
