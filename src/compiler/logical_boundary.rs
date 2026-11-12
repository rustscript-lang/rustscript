//! Compiler-private lowering of schema-directed JSON calls.
//!
//! Public `json::decode` / `json::encode` arities stay unchanged. A typed
//! decode call is rewritten to the fixed two-argument internal builtin and a
//! lossless schema constant before ownership planning.

use crate::builtins::BuiltinFunction;
use crate::compiler::ir::{Expr, FrontendIr, FunctionImpl, LocalSlot, Stmt, TypeSchema};
use crate::compiler::CompileError;

pub(super) fn lower_typed_json(
    ir: &mut FrontendIr,
    local_schemas: &[Option<TypeSchema>],
) -> Result<(), CompileError> {
    attach_inferred_encode_schemas(ir, local_schemas)?;
    for stmt in &mut ir.stmts {
        lower_stmt(stmt)?;
    }
    for function in ir.function_impls.values_mut() {
        lower_function(function)?;
    }
    Ok(())
}

fn lower_function(function: &mut FunctionImpl) -> Result<(), CompileError> {
    for stmt in &mut function.body_stmts {
        lower_stmt(stmt)?;
    }
    lower_expr(&mut function.body_expr)
}

fn lower_stmt(stmt: &mut Stmt) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            lower_expr(expr)
        }
        Stmt::ClosureLet { closure, .. } => lower_expr(&mut closure.body),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            lower_expr(condition)?;
            for stmt in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                lower_stmt(stmt)?;
            }
            Ok(())
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            lower_stmt(init)?;
            lower_expr(condition)?;
            lower_stmt(post)?;
            for stmt in body {
                lower_stmt(stmt)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            lower_expr(condition)?;
            for stmt in body {
                lower_stmt(stmt)?;
            }
            Ok(())
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => Ok(()),
    }
}

fn lower_expr(expr: &mut Expr) -> Result<(), CompileError> {
    match expr {
        Expr::ResultConstruct { payload, .. } => lower_expr(payload),
        Expr::ResultQuery { value, .. }
        | Expr::ResultTry { value, .. }
        | Expr::ResultConvert { value, .. } => lower_expr(value),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            lower_expr(value)?;
            lower_expr(ok_expr)?;
            lower_expr(err_expr)
        }
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            lower_expr(receiver)?;
            lower_expr(argument)
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                lower_expr(payload)?;
            }
            Ok(())
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            lower_expr(value)?;
            lower_expr(some_expr)?;
            lower_expr(none_expr)
        }
        Expr::OptionalGet { container, key, .. } => {
            lower_expr(container)?;
            lower_expr(key)
        }
        Expr::OptionUnwrapOr { value, fallback, .. } => {
            lower_expr(value)?;
            lower_expr(fallback)
        }
        Expr::Call(index, type_args, args, _, _semantic_id) => {
            for arg in args.iter_mut() {
                lower_expr(arg)?;
            }
            if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::JsonDecode)
                && type_args.len() == 1
                && args.len() == 1
            {
                let schema = type_args[0].clone();
                rewrite_typed_json(expr, schema, BuiltinFunction::JsonDecodeTyped)?;
            } else if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::JsonEncode)
                && type_args.len() == 1
                && args.len() == 1
            {
                let schema = type_args[0].clone();
                rewrite_typed_json(expr, schema, BuiltinFunction::JsonEncodeTyped)?;
            }
            Ok(())
        }
        Expr::ModuleCall(_, _, args, _) | Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                lower_expr(arg)?;
            }
            Ok(())
        }
        Expr::Closure(closure) => lower_expr(&mut closure.body),
        Expr::ClosureCall(closure, args) => {
            lower_expr(&mut closure.body)?;
            for arg in args {
                lower_expr(arg)?;
            }
            Ok(())
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
            lower_expr(lhs)?;
            lower_expr(rhs)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => lower_expr(inner),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            lower_expr(condition)?;
            lower_expr(then_expr)?;
            lower_expr(else_expr)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            lower_expr(value)?;
            for (_, arm) in arms {
                lower_expr(arm)?;
            }
            lower_expr(default)
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                lower_stmt(stmt)?;
            }
            lower_expr(expr)
        }
        Expr::Null
        | Expr::Unit
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::String(_)
        | Expr::Bytes(_)
        | Expr::FunctionRef(..)
        | Expr::ModuleFunctionRef(..)
        | Expr::UnresolvedFunctionRef { .. }
        | Expr::Var(_)
        | Expr::MoveVar(_)
        | Expr::MoveField { .. }
        | Expr::MoveIndex { .. } => Ok(()),
    }
}

fn rewrite_typed_json(
    expr: &mut Expr,
    schema: crate::compiler::ir::TypeSchema,
    builtin: BuiltinFunction,
) -> Result<(), CompileError> {
    let descriptor = crate::vmbc::encode_type_schema_bytes(&schema).map_err(|err| {
        CompileError::HostCallResolve {
            line: None,
            source_name: None,
            detail: format!("typed JSON descriptor: {err}"),
            span: None,
        }
    })?;
    let Expr::Call(_, _, args, _, semantic_id) = expr else {
        unreachable!("typed json rewrite requires a call");
    };
    let value = args.remove(0);
    let id = semantic_id.take();
    *expr = Expr::Call(
        builtin.call_index(),
        vec![schema],
        vec![value, Expr::Bytes(descriptor)],
        None,
        id,
    );
    Ok(())
}

fn attach_inferred_encode_schemas(
    ir: &mut FrontendIr,
    local_schemas: &[Option<TypeSchema>],
) -> Result<(), CompileError> {
    let typed = super::FrontendTypedIr {
        source_id: None,
        ir,
        local_schemas,
        host_api_catalog: None,
        source_map: None,
    };
    let mut facts = Vec::new();
    for stmt in &ir.stmts {
        collect_encode_stmt(stmt, &typed, &mut facts);
    }
    for function in ir.function_impls.values() {
        for stmt in &function.body_stmts {
            collect_encode_stmt(stmt, &typed, &mut facts);
        }
        collect_encode_expr(&function.body_expr, &typed, &mut facts);
    }
    let mut facts = facts.into_iter();
    for stmt in &mut ir.stmts {
        apply_encode_stmt(stmt, &mut facts)?;
    }
    for function in ir.function_impls.values_mut() {
        for stmt in &mut function.body_stmts {
            apply_encode_stmt(stmt, &mut facts)?;
        }
        apply_encode_expr(&mut function.body_expr, &mut facts)?;
    }
    Ok(())
}

fn collect_encode_stmt(
    stmt: &Stmt,
    typed: &super::FrontendTypedIr<'_>,
    facts: &mut Vec<Option<TypeSchema>>,
) {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            collect_encode_expr(expr, typed, facts);
        }
        Stmt::ClosureLet { closure, .. } => collect_encode_expr(&closure.body, typed, facts),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_encode_expr(condition, typed, facts);
            for nested in then_branch.iter().chain(else_branch) {
                collect_encode_stmt(nested, typed, facts);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_encode_expr(condition, typed, facts);
            for nested in body {
                collect_encode_stmt(nested, typed, facts);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_encode_stmt(init, typed, facts);
            collect_encode_expr(condition, typed, facts);
            collect_encode_stmt(post, typed, facts);
            for nested in body {
                collect_encode_stmt(nested, typed, facts);
            }
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => {}
    }
}

fn collect_encode_expr(
    expr: &Expr,
    typed: &super::FrontendTypedIr<'_>,
    facts: &mut Vec<Option<TypeSchema>>,
) {
    match expr {
        Expr::Call(index, type_args, args, _, _) => {
            for arg in args {
                collect_encode_expr(arg, typed, facts);
            }
            if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::JsonEncode)
                && type_args.is_empty()
                && args.len() == 1
            {
                facts.push(typed.schema_of_expr(&args[0]));
            }
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                collect_encode_stmt(stmt, typed, facts);
            }
            collect_encode_expr(expr, typed, facts);
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
            collect_encode_expr(lhs, typed, facts);
            collect_encode_expr(rhs, typed, facts);
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_encode_expr(condition, typed, facts);
            collect_encode_expr(then_expr, typed, facts);
            collect_encode_expr(else_expr, typed, facts);
        }
        other => visit_encode_children(other, typed, facts),
    }
}

fn visit_encode_children(
    expr: &Expr,
    typed: &super::FrontendTypedIr<'_>,
    facts: &mut Vec<Option<TypeSchema>>,
) {
    match expr {
        Expr::ResultConstruct { payload, .. }
        | Expr::ResultQuery { value: payload, .. }
        | Expr::ResultTry { value: payload, .. }
        | Expr::ResultConvert { value: payload, .. }
        | Expr::Neg(payload)
        | Expr::Not(payload)
        | Expr::ToOwned(payload)
        | Expr::Borrow(payload)
        | Expr::BorrowMut(payload) => collect_encode_expr(payload, typed, facts),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            collect_encode_expr(value, typed, facts);
            collect_encode_expr(ok_expr, typed, facts);
            collect_encode_expr(err_expr, typed, facts);
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            collect_encode_expr(value, typed, facts);
            collect_encode_expr(some_expr, typed, facts);
            collect_encode_expr(none_expr, typed, facts);
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                collect_encode_expr(payload, typed, facts);
            }
        }
        Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                collect_encode_expr(arg, typed, facts);
            }
        }
        Expr::Closure(closure) => collect_encode_expr(&closure.body, typed, facts),
        Expr::Match {
            value, arms, default, ..
        } => {
            collect_encode_expr(value, typed, facts);
            for (_, arm) in arms {
                collect_encode_expr(arm, typed, facts);
            }
            collect_encode_expr(default, typed, facts);
        }
        _ => {}
    }
}

fn apply_encode_stmt(
    stmt: &mut Stmt,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            apply_encode_expr(expr, facts)
        }
        Stmt::ClosureLet { closure, .. } => apply_encode_expr(&mut closure.body, facts),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            apply_encode_expr(condition, facts)?;
            for nested in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                apply_encode_stmt(nested, facts)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            apply_encode_expr(condition, facts)?;
            for nested in body {
                apply_encode_stmt(nested, facts)?;
            }
            Ok(())
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            apply_encode_stmt(init, facts)?;
            apply_encode_expr(condition, facts)?;
            apply_encode_stmt(post, facts)?;
            for nested in body {
                apply_encode_stmt(nested, facts)?;
            }
            Ok(())
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => Ok(()),
    }
}

fn typed_json_schema_is_concrete(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Int
        | TypeSchema::Float
        | TypeSchema::Number
        | TypeSchema::Bool
        | TypeSchema::String
        | TypeSchema::Unit => true,
        TypeSchema::Array(inner)
        | TypeSchema::Map(inner)
        | TypeSchema::Option(inner) => typed_json_schema_is_concrete(inner),
        TypeSchema::ArrayTuple(items) => items.iter().all(typed_json_schema_is_concrete),
        TypeSchema::Object(fields) => fields.values().all(typed_json_schema_is_concrete),
        TypeSchema::Named(_, args) => args.iter().all(typed_json_schema_is_concrete),
        TypeSchema::Result { ok, err } => {
            typed_json_schema_is_concrete(ok) && typed_json_schema_is_concrete(err)
        }
        TypeSchema::Unknown
        | TypeSchema::Null
        | TypeSchema::Optional(_)
        | TypeSchema::GenericParam(_)
        | TypeSchema::Bytes
        | TypeSchema::Callable { .. }
        | TypeSchema::Resource(_)
        | TypeSchema::ArrayTupleRest { .. } => false,
    }
}

fn apply_encode_expr(
    expr: &mut Expr,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(index, type_args, args, _, _) => {
            for arg in args.iter_mut() {
                apply_encode_expr(arg, facts)?;
            }
            if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::JsonEncode)
                && type_args.is_empty()
                && args.len() == 1
            {
                if let Some(Some(schema)) = facts.next()
                    && typed_json_schema_is_concrete(&schema)
                {
                    type_args.push(schema);
                }
            }
            Ok(())
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                apply_encode_stmt(stmt, facts)?;
            }
            apply_encode_expr(expr, facts)
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
            apply_encode_expr(lhs, facts)?;
            apply_encode_expr(rhs, facts)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            apply_encode_expr(condition, facts)?;
            apply_encode_expr(then_expr, facts)?;
            apply_encode_expr(else_expr, facts)
        }
        other => apply_encode_children(other, facts),
    }
}

fn apply_encode_children(
    expr: &mut Expr,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
) -> Result<(), CompileError> {
    match expr {
        Expr::ResultConstruct { payload, .. }
        | Expr::ResultQuery { value: payload, .. }
        | Expr::ResultTry { value: payload, .. }
        | Expr::ResultConvert { value: payload, .. }
        | Expr::Neg(payload)
        | Expr::Not(payload)
        | Expr::ToOwned(payload)
        | Expr::Borrow(payload)
        | Expr::BorrowMut(payload) => apply_encode_expr(payload, facts),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            apply_encode_expr(value, facts)?;
            apply_encode_expr(ok_expr, facts)?;
            apply_encode_expr(err_expr, facts)
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            apply_encode_expr(value, facts)?;
            apply_encode_expr(some_expr, facts)?;
            apply_encode_expr(none_expr, facts)
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                apply_encode_expr(payload, facts)?;
            }
            Ok(())
        }
        Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                apply_encode_expr(arg, facts)?;
            }
            Ok(())
        }
        Expr::Closure(closure) => apply_encode_expr(&mut closure.body, facts),
        Expr::Match {
            value, arms, default, ..
        } => {
            apply_encode_expr(value, facts)?;
            for (_, arm) in arms {
                apply_encode_expr(arm, facts)?;
            }
            apply_encode_expr(default, facts)
        }
        _ => Ok(()),
    }
}

pub(super) fn reject_dynamic_host_output(ir: &FrontendIr) -> Result<(), CompileError> {
    for stmt in &ir.stmts {
        reject_dynamic_host_stmt(stmt)?;
    }
    for function in ir.function_impls.values() {
        for stmt in &function.body_stmts {
            reject_dynamic_host_stmt(stmt)?;
        }
        reject_dynamic_host_expr(&function.body_expr)?;
    }
    Ok(())
}

fn reject_dynamic_host_stmt(stmt: &Stmt) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            reject_dynamic_host_expr(expr)
        }
        Stmt::ClosureLet { closure, .. } => reject_dynamic_host_expr(&closure.body),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            reject_dynamic_host_expr(condition)?;
            for nested in then_branch.iter().chain(else_branch) {
                reject_dynamic_host_stmt(nested)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            reject_dynamic_host_expr(condition)?;
            for nested in body {
                reject_dynamic_host_stmt(nested)?;
            }
            Ok(())
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            reject_dynamic_host_stmt(init)?;
            reject_dynamic_host_expr(condition)?;
            reject_dynamic_host_stmt(post)?;
            for nested in body {
                reject_dynamic_host_stmt(nested)?;
            }
            Ok(())
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => Ok(()),
    }
}

fn reject_dynamic_host_expr(expr: &Expr) -> Result<(), CompileError> {
    if let Some(resolved) = expr.host_call_resolution() {
        if host_output_forbidden(&resolved.host_return_type) {
            return Err(CompileError::StrictTypingRequired {
                line: None,
                source_name: None,
                detail: format!(
                    "host function '{}' returns a dynamic or legacy-null schema, which RustScript does not admit",
                    resolved.name
                ),
                span: None,
            });
        }
    }
    match expr {
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                reject_dynamic_host_expr(arg)?;
            }
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                reject_dynamic_host_stmt(stmt)?;
            }
            reject_dynamic_host_expr(expr)?;
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
            reject_dynamic_host_expr(lhs)?;
            reject_dynamic_host_expr(rhs)?;
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            reject_dynamic_host_expr(condition)?;
            reject_dynamic_host_expr(then_expr)?;
            reject_dynamic_host_expr(else_expr)?;
        }
        Expr::Closure(closure) => reject_dynamic_host_expr(&closure.body)?,
        _ => {}
    }
    Ok(())
}

fn host_output_forbidden(schema: &crate::host_api::HostTypeSchema) -> bool {
    use crate::host_api::HostTypeSchema;
    match schema {
        HostTypeSchema::Unknown | HostTypeSchema::Null | HostTypeSchema::Optional(_) => true,
        HostTypeSchema::Array(inner)
        | HostTypeSchema::Map(inner)
        | HostTypeSchema::Option(inner) => host_output_forbidden(inner),
        HostTypeSchema::Result { ok, err } => {
            host_output_forbidden(ok) || host_output_forbidden(err)
        }
        HostTypeSchema::Callable { result, .. } => host_output_forbidden(result),
        HostTypeSchema::Named { fields, .. } => {
            fields.iter().any(|field| host_output_forbidden(&field.ty))
        }
        HostTypeSchema::Unit
        | HostTypeSchema::Int
        | HostTypeSchema::Float
        | HostTypeSchema::Number
        | HostTypeSchema::Bool
        | HostTypeSchema::String
        | HostTypeSchema::Bytes
        | HostTypeSchema::Resource(_) => false,
    }
}

fn logical_label_schema(schema: &TypeSchema) -> bool {
    matches!(schema, TypeSchema::Option(_) | TypeSchema::Unit)
}

pub(super) fn lower_logical_print(
    ir: &mut FrontendIr,
    local_schemas: &[Option<TypeSchema>],
) -> Result<(), CompileError> {
    let mut schemas = declared_schemas(ir);
    for (slot, schema) in local_schemas.iter().enumerate() {
        if let Some(schema) = schema {
            let slot = slot as LocalSlot;
            if let Some(existing) = schemas.get(&slot) {
                if logical_label_schema(existing) && !logical_label_schema(schema) {
                    continue;
                }
            }
            schemas.insert(slot, schema.clone());
        }
    }
    let print_index = ir
        .functions
        .iter()
        .find(|function| function.name == "print" && function.arity == 1)
        .map(|function| function.index);
    let to_string = BuiltinFunction::ToString.call_index();
    lower_print_stmts(
        &mut ir.stmts,
        print_index,
        to_string,
        &schemas,
        &mut ir.locals,
    )?;
    for function in ir.function_impls.values_mut() {
        lower_print_function(function, print_index, to_string, &schemas, &mut ir.locals)?;
    }
    Ok(())
}

fn lower_print_function(
    function: &mut FunctionImpl,
    print_index: Option<u16>,
    to_string: u16,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    lower_print_stmts(
        &mut function.body_stmts,
        print_index,
        to_string,
        schemas,
        locals,
    )?;
    lower_print_expr(
        &mut function.body_expr,
        print_index,
        to_string,
        schemas,
        locals,
    )
}

fn declared_schemas(ir: &FrontendIr) -> std::collections::HashMap<LocalSlot, TypeSchema> {
    let mut schemas = std::collections::HashMap::new();
    for stmt in &ir.stmts {
        collect_declared(stmt, &mut schemas);
    }
    for function in ir.function_impls.values() {
        for stmt in &function.body_stmts {
            collect_declared(stmt, &mut schemas);
        }
    }
    schemas
}

fn collect_declared(stmt: &Stmt, schemas: &mut std::collections::HashMap<LocalSlot, TypeSchema>) {
    match stmt {
        Stmt::Let {
            index,
            declared_schema: Some(schema),
            expr,
            ..
        } => {
            schemas.insert(*index, schema.clone());
            collect_declared_expr(expr, schemas);
        }
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            collect_declared_expr(expr, schemas);
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_declared_expr(condition, schemas);
            for nested in then_branch.iter().chain(else_branch) {
                collect_declared(nested, schemas);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_declared_expr(condition, schemas);
            for nested in body {
                collect_declared(nested, schemas);
            }
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            collect_declared(init, schemas);
            collect_declared_expr(condition, schemas);
            collect_declared(post, schemas);
            for nested in body {
                collect_declared(nested, schemas);
            }
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. }
        | Stmt::ClosureLet { .. } => {}
    }
}

fn collect_declared_expr(expr: &Expr, schemas: &mut std::collections::HashMap<LocalSlot, TypeSchema>) {
    if let Expr::Block { stmts, expr } = expr {
        for stmt in stmts {
            collect_declared(stmt, schemas);
        }
        collect_declared_expr(expr, schemas);
    }
}

fn lower_print_stmts(
    stmts: &mut [Stmt],
    print_index: Option<u16>,
    to_string: u16,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    for stmt in stmts {
        lower_print_stmt(stmt, print_index, to_string, schemas, locals)?;
    }
    Ok(())
}

fn lower_print_stmt(
    stmt: &mut Stmt,
    print_index: Option<u16>,
    to_string: u16,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            lower_print_expr(expr, print_index, to_string, schemas, locals)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            lower_print_expr(condition, print_index, to_string, schemas, locals)?;
            lower_print_stmts(then_branch, print_index, to_string, schemas, locals)?;
            lower_print_stmts(else_branch, print_index, to_string, schemas, locals)
        }
        Stmt::While {
            condition, body, ..
        } => {
            lower_print_expr(condition, print_index, to_string, schemas, locals)?;
            lower_print_stmts(body, print_index, to_string, schemas, locals)
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            lower_print_stmt(init, print_index, to_string, schemas, locals)?;
            lower_print_expr(condition, print_index, to_string, schemas, locals)?;
            lower_print_stmt(post, print_index, to_string, schemas, locals)?;
            lower_print_stmts(body, print_index, to_string, schemas, locals)
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. }
        | Stmt::ClosureLet { .. } => Ok(()),
    }
}

fn lower_print_expr(
    expr: &mut Expr,
    print_index: Option<u16>,
    to_string: u16,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    if let Expr::Block { stmts, expr } = expr {
        lower_print_stmts(stmts, print_index, to_string, schemas, locals)?;
        return lower_print_expr(expr, print_index, to_string, schemas, locals);
    }
    rewrite_format_templates(expr, schemas)?;
    if rewrite_logical_type(expr, schemas, locals)? {
        return Ok(());
    }
    let Expr::Call(index, _, args, _, _) = &*expr else {
        return Ok(());
    };
    if print_index != Some(*index) || args.len() != 1 {
        return Ok(());
    }
    let Some((value_slot, newline, schema)) = logical_print_arg(&args[0], to_string, schemas) else {
        return Ok(());
    };
    let descriptor = crate::vmbc::encode_type_schema_bytes(&schema).map_err(|err| {
        CompileError::HostCallResolve {
            line: None,
            source_name: None,
            detail: format!("logical render descriptor: {err}"),
            span: None,
        }
    })?;
    let hidden = (*locals).try_into().map_err(|_| CompileError::HostCallResolve {
        line: None,
        source_name: None,
        detail: "logical print local overflow".to_string(),
        span: None,
    })?;
    *locals += 1;
    let mut rendered = Expr::Call(
        BuiltinFunction::LogicalRender.call_index(),
        Vec::new(),
        vec![Expr::Var(hidden), Expr::Bytes(descriptor)],
        None,
        None,
    );
    if newline {
        rendered = Expr::Add(Box::new(rendered), Box::new(Expr::String("\n".to_string())));
    }
    let print = Expr::Call(print_index.expect("print rewrite requires print"), Vec::new(), vec![rendered], None, None);
    *expr = Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: hidden,
                declared_schema: Some(schema),
                expr: Expr::Var(value_slot),
                line: 0,
            },
            Stmt::Expr { expr: print, line: 0 },
        ],
        expr: Box::new(Expr::Var(hidden)),
    };
    Ok(())
}

fn rewrite_format_templates(
    expr: &mut Expr,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(index, _, args, _, _) => {
            for arg in args.iter_mut() {
                rewrite_format_templates(arg, schemas)?;
            }
            if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::FormatTemplate)
                && args.len() == 2
            {
                rewrite_format_payload(&mut args[1], schemas)?;
            }
        }
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs) => {
            rewrite_format_templates(lhs, schemas)?;
            rewrite_format_templates(rhs, schemas)?;
        }
        _ => {}
    }
    Ok(())
}

fn rewrite_format_payload(
    expr: &mut Expr,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
) -> Result<(), CompileError> {
    let Expr::Call(index, _, args, _, _) = expr else {
        return Ok(());
    };
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::ArrayPush) || args.len() != 2
    {
        return Ok(());
    }
    rewrite_format_payload(&mut args[0], schemas)?;
    let Some(schema) = logical_schema_of_expr(&args[1], schemas) else {
        return Ok(());
    };
    let descriptor = crate::vmbc::encode_type_schema_bytes(&schema).map_err(|err| {
        CompileError::HostCallResolve {
            line: None,
            source_name: None,
            detail: format!("logical format descriptor: {err}"),
            span: None,
        }
    })?;
    let value = args[1].clone();
    args[1] = Expr::Call(
        BuiltinFunction::LogicalRender.call_index(),
        Vec::new(),
        vec![value, Expr::Bytes(descriptor)],
        None,
        None,
    );
    Ok(())
}

fn logical_schema_of_expr(
    expr: &Expr,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
) -> Option<TypeSchema> {
    let Expr::Var(slot) = expr else {
        return None;
    };
    let schema = schemas.get(slot)?;
    logical_label_schema(schema).then(|| schema.clone())
}

fn logical_print_arg(
    arg: &Expr,
    to_string: u16,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
) -> Option<(LocalSlot, bool, TypeSchema)> {
    let (inner, newline) = match arg {
        Expr::Add(left, right) if matches!(right.as_ref(), Expr::String(text) if text == "\n") => {
            (left.as_ref(), true)
        }
        other => (other, false),
    };
    let Expr::Call(index, _, args, _, _) = inner else {
        return None;
    };
    if *index != to_string || args.len() != 1 {
        return None;
    }
    let Expr::Var(slot) = &args[0] else {
        return None;
    };
    let schema = schemas.get(slot)?.clone();
    if matches!(schema, TypeSchema::Option(_) | TypeSchema::Unit) {
        Some((*slot, newline, schema))
    } else {
        None
    }
}

fn rewrite_logical_type(
    expr: &mut Expr,
    schemas: &std::collections::HashMap<LocalSlot, TypeSchema>,
    locals: &mut usize,
) -> Result<bool, CompileError> {
    let Expr::Call(index, _, args, _, _) = &*expr else {
        return Ok(false);
    };
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::TypeOf) || args.len() != 1
    {
        return Ok(false);
    }
    let Expr::Var(slot) = &args[0] else {
        return Ok(false);
    };
    let Some(schema) = schemas.get(slot) else {
        return Ok(false);
    };
    let label = match schema {
        TypeSchema::Option(_) => "Option",
        TypeSchema::Unit => "()",
        _ => return Ok(false),
    };
    let hidden = (*locals).try_into().map_err(|_| CompileError::HostCallResolve {
        line: None,
        source_name: None,
        detail: "logical type local overflow".to_string(),
        span: None,
    })?;
    *locals += 1;
    *expr = Expr::Block {
        stmts: vec![Stmt::Let {
            index: hidden,
            declared_schema: Some(schema.clone()),
            expr: Expr::Var(*slot),
            line: 0,
        }],
        expr: Box::new(Expr::String(label.to_string())),
    };
    Ok(true)
}
