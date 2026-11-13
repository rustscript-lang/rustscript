//! Compiler-private lowering of schema-directed JSON calls.
//!
//! Public `json::decode` / `json::encode` arities stay unchanged. A typed
//! decode call is rewritten to the fixed two-argument internal builtin and a
//! lossless schema constant before ownership planning.

use crate::builtins::BuiltinFunction;
use crate::compiler::CompileError;
use crate::compiler::ir::{
    Expr, FrontendIr, FunctionImpl, LocalSlot, SemanticNodeId, Stmt, TypeSchema,
};
use std::collections::HashMap;

/// Which source producers receive the RustScript logical/JSON policy.
///
/// `All` is a single-producer IR compiled by an already-selected RustScript
/// frontend. `Sources` maps RustScript producer names to their linked source
/// ids, so a merged compilation can keep other frontend capabilities.
#[derive(Clone, Debug)]
pub(super) enum SourceScope {
    All,
    Sources(HashMap<String, u32>),
    None,
}

impl SourceScope {
    pub(super) fn all() -> Self {
        Self::All
    }

    pub(super) fn none() -> Self {
        Self::None
    }

    pub(super) fn sources(names: HashMap<String, u32>) -> Self {
        if names.is_empty() {
            Self::None
        } else {
            Self::Sources(names)
        }
    }
}

/// Per-compilation site attribution for one lowering pass.
struct LoweringScope<'a> {
    scope: &'a SourceScope,
    call_sources: HashMap<SemanticNodeId, u32>,
    call_spans: HashMap<SemanticNodeId, crate::compiler::source_map::Span>,
}

impl<'a> LoweringScope<'a> {
    fn new(ir: &FrontendIr, scope: &'a SourceScope) -> Self {
        let mut call_sources = HashMap::new();
        let mut call_spans = HashMap::new();
        if let Some(index) = ir.parsed_semantic_index.as_ref() {
            for site in &index.call_sites {
                call_sources.insert(site.id, site.callee_span.source_id);
                call_spans.insert(site.id, site.callee_span);
            }
        }
        Self {
            scope,
            call_sources,
            call_spans,
        }
    }

    fn allows(&self, id: Option<SemanticNodeId>, default_source: Option<&str>) -> bool {
        let SourceScope::Sources(names) = self.scope else {
            return matches!(self.scope, SourceScope::All);
        };
        if let Some(id) = id
            && self
                .call_sources
                .get(&id)
                .is_some_and(|source_id| names.values().any(|candidate| candidate == source_id))
        {
            return true;
        }
        default_source.is_some_and(|name| names.contains_key(name))
    }
}

fn call_site_id(expr: &Expr) -> Option<SemanticNodeId> {
    match expr {
        Expr::Call(_, _, _, _, id) => *id,
        Expr::ModuleCall(_, _, _, id) => *id,
        Expr::LocalCall(_, _, _, id) => *id,
        _ => None,
    }
}

/// Value expressions of a presentation site, in the order their schemas are
/// recorded. `None` means this expression is not a site. Trusted print is the
/// language binding: no user impl and no catalog resolution. A same-name
/// concrete host or user function is not a site.
pub(super) fn presentation_inputs<'a>(
    function_impls: &HashMap<u16, FunctionImpl>,
    function_names: &HashMap<u16, String>,
    expr: &'a Expr,
) -> Option<Vec<&'a Expr>> {
    let Expr::Call(index, _, args, resolution, _) = expr else {
        return None;
    };
    if resolution.is_none()
        && !function_impls.contains_key(index)
        && function_names.get(index).map(String::as_str) == Some("print")
        && args.len() == 1
    {
        return Some(vec![print_input_expr(&args[0])]);
    }
    if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::TypeOf) && args.len() == 1
    {
        return Some(vec![&args[0]]);
    }
    if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::FormatTemplate)
        && args.len() == 2
    {
        return Some(format_payload_exprs(&args[1]));
    }
    None
}

pub(super) fn is_language_print(
    function_impls: &HashMap<u16, FunctionImpl>,
    function_names: &HashMap<u16, String>,
    index: u16,
) -> bool {
    !function_impls.contains_key(&index)
        && function_names.get(&index).map(String::as_str) == Some("print")
}

fn print_input_expr(arg: &Expr) -> &Expr {
    let (inner, _) = split_print_newline(arg);
    to_string_input(inner).unwrap_or(inner)
}

fn split_print_newline(arg: &Expr) -> (&Expr, bool) {
    match arg {
        Expr::Add(left, right) if matches!(right.as_ref(), Expr::String(text) if text == "\n") => {
            (left.as_ref(), true)
        }
        other => (other, false),
    }
}

fn to_string_input(expr: &Expr) -> Option<&Expr> {
    let Expr::Call(index, _, args, _, _) = expr else {
        return None;
    };
    if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::ToString)
        && args.len() == 1
    {
        Some(&args[0])
    } else {
        None
    }
}

fn format_payload_exprs(expr: &Expr) -> Vec<&Expr> {
    let mut values = Vec::new();
    collect_format_payload(expr, &mut values);
    values
}

fn collect_format_payload<'a>(expr: &'a Expr, values: &mut Vec<&'a Expr>) {
    let Expr::Call(index, _, args, _, _) = expr else {
        return;
    };
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::ArrayPush)
        || args.len() != 2
    {
        return;
    }
    collect_format_payload(&args[0], values);
    values.push(&args[1]);
}

pub(super) fn contains_logical_display(schema: &TypeSchema) -> bool {
    match schema {
        TypeSchema::Option(_) | TypeSchema::Unit | TypeSchema::Result { .. } => true,
        TypeSchema::Array(inner) | TypeSchema::Map(inner) | TypeSchema::Optional(inner) => {
            contains_logical_display(inner)
        }
        TypeSchema::ArrayTuple(items) => items.iter().any(contains_logical_display),
        TypeSchema::ArrayTupleRest { prefix, rest } => {
            prefix.iter().any(contains_logical_display) || contains_logical_display(rest)
        }
        TypeSchema::Object(fields) => fields.values().any(contains_logical_display),
        TypeSchema::Named(_, args) => args.iter().any(contains_logical_display),
        TypeSchema::Callable { params, result } => {
            params.iter().any(contains_logical_display) || contains_logical_display(result)
        }
        _ => false,
    }
}

pub(super) fn lower_typed_json(
    ir: &mut FrontendIr,
    local_schemas: &[Option<TypeSchema>],
    scope: &SourceScope,
) -> Result<(), CompileError> {
    let lowering = LoweringScope::new(ir, scope);
    attach_inferred_encode_schemas(ir, local_schemas, &lowering)?;
    for (index, stmt) in ir.stmts.iter_mut().enumerate() {
        let default_source = ir.stmt_sources.get(index).and_then(Option::as_deref);
        lower_stmt(stmt, &lowering, default_source)?;
    }
    let indexes = ir.function_impls.keys().copied().collect::<Vec<_>>();
    for index in indexes {
        let default_source = ir.function_sources.get(&index).map(String::as_str).or(None);
        let Some(function) = ir.function_impls.get_mut(&index) else {
            continue;
        };
        lower_function(function, &lowering, default_source)?;
    }
    Ok(())
}

fn lower_function(
    function: &mut FunctionImpl,
    lowering: &LoweringScope<'_>,
    default_source: Option<&str>,
) -> Result<(), CompileError> {
    for stmt in &mut function.body_stmts {
        lower_stmt(stmt, lowering, default_source)?;
    }
    lower_expr(&mut function.body_expr, lowering, default_source)
}

fn lower_stmt(
    stmt: &mut Stmt,
    lowering: &LoweringScope<'_>,
    default_source: Option<&str>,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            lower_expr(expr, lowering, default_source)
        }
        Stmt::ClosureLet { closure, .. } => lower_expr(&mut closure.body, lowering, default_source),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            lower_expr(condition, lowering, default_source)?;
            for stmt in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                lower_stmt(stmt, lowering, default_source)?;
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
            lower_stmt(init, lowering, default_source)?;
            lower_expr(condition, lowering, default_source)?;
            lower_stmt(post, lowering, default_source)?;
            for stmt in body {
                lower_stmt(stmt, lowering, default_source)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            lower_expr(condition, lowering, default_source)?;
            for stmt in body {
                lower_stmt(stmt, lowering, default_source)?;
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

fn lower_expr(
    expr: &mut Expr,
    lowering: &LoweringScope<'_>,
    default_source: Option<&str>,
) -> Result<(), CompileError> {
    match expr {
        Expr::ResultConstruct { payload, .. } => lower_expr(payload, lowering, default_source),
        Expr::ResultQuery { value, .. }
        | Expr::ResultTry { value, .. }
        | Expr::ResultConvert { value, .. } => lower_expr(value, lowering, default_source),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            lower_expr(value, lowering, default_source)?;
            lower_expr(ok_expr, lowering, default_source)?;
            lower_expr(err_expr, lowering, default_source)
        }
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            lower_expr(receiver, lowering, default_source)?;
            lower_expr(argument, lowering, default_source)
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                lower_expr(payload, lowering, default_source)?;
            }
            Ok(())
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            lower_expr(value, lowering, default_source)?;
            lower_expr(some_expr, lowering, default_source)?;
            lower_expr(none_expr, lowering, default_source)
        }
        Expr::OptionalGet { container, key, .. } => {
            lower_expr(container, lowering, default_source)?;
            lower_expr(key, lowering, default_source)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            lower_expr(value, lowering, default_source)?;
            lower_expr(fallback, lowering, default_source)
        }
        Expr::Call(index, type_args, args, _, semantic_id) => {
            for arg in args.iter_mut() {
                lower_expr(arg, lowering, default_source)?;
            }
            let builtin = BuiltinFunction::from_call_index(*index);
            let allowed = lowering.allows(*semantic_id, default_source);
            if builtin == Some(BuiltinFunction::JsonDecode)
                && type_args.len() == 1
                && args.len() == 1
                && allowed
            {
                let schema = type_args[0].clone();
                rewrite_typed_json(expr, schema, BuiltinFunction::JsonDecodeTyped)?;
            } else if builtin == Some(BuiltinFunction::JsonEncode)
                && type_args.len() == 1
                && args.len() == 1
                && allowed
            {
                let schema = type_args[0].clone();
                rewrite_typed_json(expr, schema, BuiltinFunction::JsonEncodeTyped)?;
            } else if allowed
                && matches!(
                    builtin,
                    Some(BuiltinFunction::JsonDecode | BuiltinFunction::JsonEncode)
                )
            {
                let builtin = builtin.expect("json builtin");
                return Err(CompileError::StrictTypingRequired {
                    line: None,
                    source_name: None,
                    detail: format!(
                        "RustScript source JSON calls require a concrete typed JSON target; untyped {} is not part of the RustScript boundary",
                        builtin.name()
                    ),
                    span: semantic_id.and_then(|id| lowering.call_spans.get(&id).copied()),
                });
            }
            Ok(())
        }
        Expr::ModuleCall(_, _, args, _) | Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                lower_expr(arg, lowering, default_source)?;
            }
            Ok(())
        }
        Expr::Closure(closure) => lower_expr(&mut closure.body, lowering, default_source),
        Expr::ClosureCall(closure, args) => {
            lower_expr(&mut closure.body, lowering, default_source)?;
            for arg in args {
                lower_expr(arg, lowering, default_source)?;
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
            lower_expr(lhs, lowering, default_source)?;
            lower_expr(rhs, lowering, default_source)
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => lower_expr(inner, lowering, default_source),
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            lower_expr(condition, lowering, default_source)?;
            lower_expr(then_expr, lowering, default_source)?;
            lower_expr(else_expr, lowering, default_source)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            lower_expr(value, lowering, default_source)?;
            for (_, arm) in arms {
                lower_expr(arm, lowering, default_source)?;
            }
            lower_expr(default, lowering, default_source)
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                lower_stmt(stmt, lowering, default_source)?;
            }
            lower_expr(expr, lowering, default_source)
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
    let descriptor = crate::compiler::type_schema_codec::encode_type_schema_bytes(&schema)
        .map_err(|err| CompileError::HostCallResolve {
            line: None,
            source_name: None,
            detail: format!("typed JSON descriptor: {err}"),
            span: None,
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
    lowering: &LoweringScope<'_>,
) -> Result<(), CompileError> {
    let mut facts = Vec::new();
    {
        let typed = super::FrontendTypedIr {
            source_id: None,
            ir,
            local_schemas,
            host_api_catalog: None,
            source_map: None,
        };
        for stmt in &ir.stmts {
            collect_encode_stmt(stmt, &typed, &mut facts);
        }
        for function in ir.function_impls.values() {
            for stmt in &function.body_stmts {
                collect_encode_stmt(stmt, &typed, &mut facts);
            }
            collect_encode_expr(&function.body_expr, &typed, &mut facts);
        }
    }
    let mut facts = facts.into_iter();
    for (index, stmt) in ir.stmts.iter_mut().enumerate() {
        let default_source = ir.stmt_sources.get(index).and_then(Option::as_deref);
        apply_encode_stmt(stmt, lowering, default_source, &mut facts)?;
    }
    for (index, function) in ir.function_impls.iter_mut() {
        let default_source = ir.function_sources.get(index).map(String::as_str).or(None);
        for stmt in &mut function.body_stmts {
            apply_encode_stmt(stmt, lowering, default_source, &mut facts)?;
        }
        apply_encode_expr(
            &mut function.body_expr,
            lowering,
            default_source,
            &mut facts,
        )?;
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
        Expr::OptionConstruct {
            payload: Some(payload),
            ..
        } => {
            collect_encode_expr(payload, typed, facts);
        }
        Expr::ModuleCall(_, _, args, _) | Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                collect_encode_expr(arg, typed, facts);
            }
        }
        Expr::ClosureCall(closure, args) => {
            collect_encode_expr(&closure.body, typed, facts);
            for arg in args {
                collect_encode_expr(arg, typed, facts);
            }
        }
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            collect_encode_expr(receiver, typed, facts);
            collect_encode_expr(argument, typed, facts);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            collect_encode_expr(value, typed, facts);
            collect_encode_expr(fallback, typed, facts);
        }
        Expr::OptionalGet { container, key, .. } => {
            collect_encode_expr(container, typed, facts);
            collect_encode_expr(key, typed, facts);
        }
        Expr::Closure(closure) => collect_encode_expr(&closure.body, typed, facts),
        Expr::Match {
            value,
            arms,
            default,
            ..
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
    lowering: &LoweringScope<'_>,
    default_source: Option<&str>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            apply_encode_expr(expr, lowering, default_source, facts)
        }
        Stmt::ClosureLet { closure, .. } => {
            apply_encode_expr(&mut closure.body, lowering, default_source, facts)
        }
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            apply_encode_expr(condition, lowering, default_source, facts)?;
            for nested in then_branch.iter_mut().chain(else_branch.iter_mut()) {
                apply_encode_stmt(nested, lowering, default_source, facts)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            apply_encode_expr(condition, lowering, default_source, facts)?;
            for nested in body {
                apply_encode_stmt(nested, lowering, default_source, facts)?;
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
            apply_encode_stmt(init, lowering, default_source, facts)?;
            apply_encode_expr(condition, lowering, default_source, facts)?;
            apply_encode_stmt(post, lowering, default_source, facts)?;
            for nested in body {
                apply_encode_stmt(nested, lowering, default_source, facts)?;
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
        TypeSchema::Array(inner) | TypeSchema::Map(inner) | TypeSchema::Option(inner) => {
            typed_json_schema_is_concrete(inner)
        }
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
    lowering: &LoweringScope<'_>,
    default_source: Option<&str>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
) -> Result<(), CompileError> {
    match expr {
        Expr::Call(index, type_args, args, _, semantic_id) => {
            for arg in args.iter_mut() {
                apply_encode_expr(arg, lowering, default_source, facts)?;
            }
            if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::JsonEncode)
                && type_args.is_empty()
                && args.len() == 1
            {
                let fact = facts.next();
                if lowering.allows(*semantic_id, default_source)
                    && let Some(Some(schema)) = fact
                    && typed_json_schema_is_concrete(&schema)
                {
                    type_args.push(schema);
                }
            }
            Ok(())
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                apply_encode_stmt(stmt, lowering, default_source, facts)?;
            }
            apply_encode_expr(expr, lowering, default_source, facts)
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
            apply_encode_expr(lhs, lowering, default_source, facts)?;
            apply_encode_expr(rhs, lowering, default_source, facts)
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            apply_encode_expr(condition, lowering, default_source, facts)?;
            apply_encode_expr(then_expr, lowering, default_source, facts)?;
            apply_encode_expr(else_expr, lowering, default_source, facts)
        }
        other => apply_encode_children(other, lowering, default_source, facts),
    }
}

fn apply_encode_children(
    expr: &mut Expr,
    lowering: &LoweringScope<'_>,
    default_source: Option<&str>,
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
        | Expr::BorrowMut(payload) => apply_encode_expr(payload, lowering, default_source, facts),
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            apply_encode_expr(value, lowering, default_source, facts)?;
            apply_encode_expr(ok_expr, lowering, default_source, facts)?;
            apply_encode_expr(err_expr, lowering, default_source, facts)
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            apply_encode_expr(value, lowering, default_source, facts)?;
            apply_encode_expr(some_expr, lowering, default_source, facts)?;
            apply_encode_expr(none_expr, lowering, default_source, facts)
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                apply_encode_expr(payload, lowering, default_source, facts)?;
            }
            Ok(())
        }
        Expr::ModuleCall(_, _, args, _) | Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                apply_encode_expr(arg, lowering, default_source, facts)?;
            }
            Ok(())
        }
        Expr::ClosureCall(closure, args) => {
            apply_encode_expr(&mut closure.body, lowering, default_source, facts)?;
            for arg in args {
                apply_encode_expr(arg, lowering, default_source, facts)?;
            }
            Ok(())
        }
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            apply_encode_expr(receiver, lowering, default_source, facts)?;
            apply_encode_expr(argument, lowering, default_source, facts)
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            apply_encode_expr(value, lowering, default_source, facts)?;
            apply_encode_expr(fallback, lowering, default_source, facts)
        }
        Expr::OptionalGet { container, key, .. } => {
            apply_encode_expr(container, lowering, default_source, facts)?;
            apply_encode_expr(key, lowering, default_source, facts)
        }
        Expr::Closure(closure) => {
            apply_encode_expr(&mut closure.body, lowering, default_source, facts)
        }
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            apply_encode_expr(value, lowering, default_source, facts)?;
            for (_, arm) in arms {
                apply_encode_expr(arm, lowering, default_source, facts)?;
            }
            apply_encode_expr(default, lowering, default_source, facts)
        }
        _ => Ok(()),
    }
}

struct RejectContext<'a, 'b> {
    lowering: &'b LoweringScope<'b>,
    default_source: Option<&'a str>,
}

impl RejectContext<'_, '_> {
    fn allows(&self, expr: &Expr) -> bool {
        self.lowering
            .allows(call_site_id(expr), self.default_source)
    }
}

pub(super) fn reject_dynamic_host_output(
    ir: &FrontendIr,
    scope: &SourceScope,
) -> Result<(), CompileError> {
    let lowering = LoweringScope::new(ir, scope);
    for (index, stmt) in ir.stmts.iter().enumerate() {
        let default_source = ir.stmt_sources.get(index).and_then(Option::as_deref);
        let ctx = RejectContext {
            lowering: &lowering,
            default_source,
        };
        reject_dynamic_host_stmt(&ctx, stmt)?;
    }
    for (index, function) in ir.function_impls.iter() {
        let default_source = ir.function_sources.get(index).map(String::as_str).or(None);
        let ctx = RejectContext {
            lowering: &lowering,
            default_source,
        };
        for stmt in &function.body_stmts {
            reject_dynamic_host_stmt(&ctx, stmt)?;
        }
        reject_dynamic_host_expr(&ctx, &function.body_expr)?;
    }
    Ok(())
}

fn reject_dynamic_host_stmt(ctx: &RejectContext<'_, '_>, stmt: &Stmt) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { expr, .. } | Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
            reject_dynamic_host_expr(ctx, expr)
        }
        Stmt::ClosureLet { closure, .. } => reject_dynamic_host_expr(ctx, &closure.body),
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            reject_dynamic_host_expr(ctx, condition)?;
            for nested in then_branch.iter().chain(else_branch) {
                reject_dynamic_host_stmt(ctx, nested)?;
            }
            Ok(())
        }
        Stmt::While {
            condition, body, ..
        } => {
            reject_dynamic_host_expr(ctx, condition)?;
            for nested in body {
                reject_dynamic_host_stmt(ctx, nested)?;
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
            reject_dynamic_host_stmt(ctx, init)?;
            reject_dynamic_host_expr(ctx, condition)?;
            reject_dynamic_host_stmt(ctx, post)?;
            for nested in body {
                reject_dynamic_host_stmt(ctx, nested)?;
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

fn reject_dynamic_host_expr(ctx: &RejectContext<'_, '_>, expr: &Expr) -> Result<(), CompileError> {
    if ctx.allows(expr)
        && let Some(resolved) = expr.host_call_resolution()
        && host_output_forbidden(&resolved.host_return_type)
    {
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
    // Exhaustive child traversal: every structured position that can hold a
    // value expression must be visited, including match/option/result
    // carriers, projection and method nodes, and a closure-call body. Guest
    // consumption (host arguments) and the timer callback result exception
    // stay outside this host-to-guest output check.
    match expr {
        Expr::Call(_, _, args, _, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                reject_dynamic_host_expr(ctx, arg)?;
            }
        }
        Expr::ClosureCall(closure, args) => {
            reject_dynamic_host_expr(ctx, &closure.body)?;
            for arg in args {
                reject_dynamic_host_expr(ctx, arg)?;
            }
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                reject_dynamic_host_stmt(ctx, stmt)?;
            }
            reject_dynamic_host_expr(ctx, expr)?;
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
            reject_dynamic_host_expr(ctx, lhs)?;
            reject_dynamic_host_expr(ctx, rhs)?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => reject_dynamic_host_expr(ctx, inner)?,
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            reject_dynamic_host_expr(ctx, condition)?;
            reject_dynamic_host_expr(ctx, then_expr)?;
            reject_dynamic_host_expr(ctx, else_expr)?;
        }
        Expr::Closure(closure) => reject_dynamic_host_expr(ctx, &closure.body)?,
        Expr::Match {
            value,
            arms,
            default,
            ..
        } => {
            reject_dynamic_host_expr(ctx, value)?;
            for (_, arm) in arms {
                reject_dynamic_host_expr(ctx, arm)?;
            }
            reject_dynamic_host_expr(ctx, default)?;
        }
        Expr::ResultConstruct { payload, .. } => reject_dynamic_host_expr(ctx, payload)?,
        Expr::ResultMatch {
            value,
            ok_expr,
            err_expr,
            ..
        } => {
            reject_dynamic_host_expr(ctx, value)?;
            reject_dynamic_host_expr(ctx, ok_expr)?;
            reject_dynamic_host_expr(ctx, err_expr)?;
        }
        Expr::ResultQuery { value, .. }
        | Expr::ResultTry { value, .. }
        | Expr::ResultConvert { value, .. } => reject_dynamic_host_expr(ctx, value)?,
        Expr::ResultMethod {
            receiver, argument, ..
        }
        | Expr::OptionMethod {
            receiver, argument, ..
        } => {
            reject_dynamic_host_expr(ctx, receiver)?;
            reject_dynamic_host_expr(ctx, argument)?;
        }
        Expr::OptionConstruct { payload, .. } => {
            if let Some(payload) = payload {
                reject_dynamic_host_expr(ctx, payload)?;
            }
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            reject_dynamic_host_expr(ctx, value)?;
            reject_dynamic_host_expr(ctx, some_expr)?;
            reject_dynamic_host_expr(ctx, none_expr)?;
        }
        Expr::OptionalGet { container, key, .. } => {
            reject_dynamic_host_expr(ctx, container)?;
            reject_dynamic_host_expr(ctx, key)?;
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            reject_dynamic_host_expr(ctx, value)?;
            reject_dynamic_host_expr(ctx, fallback)?;
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
        | Expr::MoveIndex { .. } => {}
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

/// Rewrite context shared by the presentation walk.
struct RewriteContext<'a, 'b> {
    names: &'a HashMap<u16, String>,
    impls: &'a HashMap<u16, ()>,
    lowering: &'b LoweringScope<'b>,
    default_source: Option<&'a str>,
}

impl RewriteContext<'_, '_> {
    fn allows(&self, id: Option<SemanticNodeId>) -> bool {
        self.lowering.allows(id, self.default_source)
    }
}

pub(super) fn lower_logical_print(
    ir: &mut FrontendIr,
    typing_mode: super::TypingMode,
    entry_local_types: &[super::typing::EntryLocalType],
    scope: &SourceScope,
) -> Result<(), CompileError> {
    let facts = super::typing::presentation_site_schemas(ir, typing_mode, entry_local_types);
    let mut facts = facts.into_iter();
    let names: HashMap<u16, String> = ir
        .functions
        .iter()
        .map(|function| (function.index, function.name.clone()))
        .collect();
    let impls: HashMap<u16, ()> = ir
        .function_impls
        .keys()
        .copied()
        .map(|index| (index, ()))
        .collect();
    let lowering = LoweringScope::new(ir, scope);
    let mut aliases = HashMap::new();
    for (index, stmt) in ir.stmts.iter_mut().enumerate() {
        let default_source = ir.stmt_sources.get(index).and_then(Option::as_deref);
        let ctx = RewriteContext {
            names: &names,
            impls: &impls,
            lowering: &lowering,
            default_source,
        };
        rewrite_stmt(stmt, &ctx, &mut facts, &mut aliases, &mut ir.locals)?;
    }
    let mut indexes = ir.function_impls.keys().copied().collect::<Vec<_>>();
    indexes.sort_unstable();
    for index in indexes {
        let default_source = ir.function_sources.get(&index).map(String::as_str).or(None);
        let Some(function) = ir.function_impls.get_mut(&index) else {
            continue;
        };
        let ctx = RewriteContext {
            names: &names,
            impls: &impls,
            lowering: &lowering,
            default_source,
        };
        let mut function_aliases = HashMap::new();
        rewrite_stmts(
            &mut function.body_stmts,
            &ctx,
            &mut facts,
            &mut function_aliases,
            &mut ir.locals,
        )?;
        rewrite_expr(
            &mut function.body_expr,
            &ctx,
            &mut facts,
            &mut function_aliases,
            &mut ir.locals,
        )?;
    }
    if facts.next().is_some() {
        return Err(presentation_error("unused presentation fact"));
    }
    Ok(())
}

fn rewrite_stmts(
    stmts: &mut [Stmt],
    ctx: &RewriteContext<'_, '_>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &mut HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    for stmt in stmts {
        rewrite_stmt(stmt, ctx, facts, aliases, locals)?;
    }
    Ok(())
}

fn rewrite_stmt(
    stmt: &mut Stmt,
    ctx: &RewriteContext<'_, '_>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &mut HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Let { index, expr, .. } | Stmt::Assign { index, expr, .. } => {
            rewrite_expr(expr, ctx, facts, aliases, locals)?;
            note_print_alias(*index, expr, ctx.names, ctx.impls, aliases);
        }
        Stmt::Expr { expr, .. } => rewrite_expr(expr, ctx, facts, aliases, locals)?,
        Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_expr(condition, ctx, facts, aliases, locals)?;
            let mut then_aliases = aliases.clone();
            let mut else_aliases = aliases.clone();
            rewrite_stmts(then_branch, ctx, facts, &mut then_aliases, locals)?;
            rewrite_stmts(else_branch, ctx, facts, &mut else_aliases, locals)?;
        }
        Stmt::While {
            condition, body, ..
        } => {
            rewrite_expr(condition, ctx, facts, aliases, locals)?;
            rewrite_stmts(body, ctx, facts, &mut aliases.clone(), locals)?;
        }
        Stmt::For {
            init,
            condition,
            post,
            body,
            ..
        } => {
            rewrite_stmt(init, ctx, facts, aliases, locals)?;
            rewrite_expr(condition, ctx, facts, aliases, locals)?;
            rewrite_stmt(post, ctx, facts, aliases, locals)?;
            rewrite_stmts(body, ctx, facts, aliases, locals)?;
        }
        Stmt::ClosureLet { closure, .. } => {
            rewrite_closure(closure, ctx, facts, aliases, locals)?;
        }
        Stmt::Noop { .. }
        | Stmt::FuncDecl { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop { .. } => {}
    }
    Ok(())
}

fn rewrite_expr(
    expr: &mut Expr,
    ctx: &RewriteContext<'_, '_>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &mut HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    if let Expr::Closure(closure) = expr {
        return rewrite_closure(closure, ctx, facts, aliases, locals);
    }
    if let Expr::Block { stmts, expr } = expr {
        let mut nested = aliases.clone();
        rewrite_stmts(stmts, ctx, facts, &mut nested, locals)?;
        return rewrite_expr(expr, ctx, facts, &mut nested, locals);
    }
    if let Expr::LocalCall(slot, _, args, semantic_id) = expr
        && args.len() == 1
        && aliases.contains_key(slot)
    {
        let allowed = ctx.allows(*semantic_id);
        let schema = next_fact(facts)?;
        rewrite_expr(&mut args[0], ctx, facts, aliases, locals)?;
        if allowed && let Some(schema) = schema {
            return rewrite_print_call(expr, schema, false, locals);
        }
        return Ok(());
    }
    if presentation_inputs_mut(ctx.names, ctx.impls, expr).is_some() {
        return rewrite_presentation_site(expr, ctx, facts, aliases, locals);
    }
    match expr {
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            rewrite_expr(condition, ctx, facts, aliases, locals)?;
            rewrite_expr(then_expr, ctx, facts, aliases, locals)?;
            rewrite_expr(else_expr, ctx, facts, aliases, locals)?;
        }
        Expr::Call(_, _, args, _, _)
        | Expr::LocalCall(_, _, args, _)
        | Expr::ModuleCall(_, _, args, _)
        | Expr::ClosureCall(_, args) => {
            for arg in args {
                rewrite_expr(arg, ctx, facts, aliases, locals)?;
            }
        }
        Expr::Add(lhs, rhs)
        | Expr::Sub(lhs, rhs)
        | Expr::Mul(lhs, rhs)
        | Expr::Div(lhs, rhs)
        | Expr::Mod(lhs, rhs)
        | Expr::Eq(lhs, rhs)
        | Expr::Lt(lhs, rhs)
        | Expr::Gt(lhs, rhs)
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs) => {
            rewrite_expr(lhs, ctx, facts, aliases, locals)?;
            rewrite_expr(rhs, ctx, facts, aliases, locals)?;
        }
        Expr::Neg(inner)
        | Expr::Not(inner)
        | Expr::ToOwned(inner)
        | Expr::Borrow(inner)
        | Expr::BorrowMut(inner) => {
            rewrite_expr(inner, ctx, facts, aliases, locals)?;
        }
        Expr::OptionConstruct {
            payload: Some(payload),
            ..
        } => {
            rewrite_expr(payload, ctx, facts, aliases, locals)?;
        }
        Expr::ResultConstruct { payload, .. }
        | Expr::ResultQuery { value: payload, .. }
        | Expr::ResultTry { value: payload, .. } => {
            rewrite_expr(payload, ctx, facts, aliases, locals)?;
        }
        Expr::OptionMatch {
            value,
            some_expr,
            none_expr,
            ..
        } => {
            rewrite_expr(value, ctx, facts, aliases, locals)?;
            rewrite_expr(some_expr, ctx, facts, aliases, locals)?;
            rewrite_expr(none_expr, ctx, facts, aliases, locals)?;
        }
        _ => {}
    }
    Ok(())
}

fn rewrite_closure(
    closure: &mut crate::compiler::ir::ClosureExpr,
    ctx: &RewriteContext<'_, '_>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    let mut inner = HashMap::new();
    for (source, capture) in &closure.capture_copies {
        if aliases.contains_key(source) {
            inner.insert(*capture, ());
        }
    }
    rewrite_expr(&mut closure.body, ctx, facts, &mut inner, locals)
}

fn rewrite_presentation_site(
    expr: &mut Expr,
    ctx: &RewriteContext<'_, '_>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &mut HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    let kind = presentation_kind(ctx.names, ctx.impls, expr);
    let allowed = ctx.allows(call_site_id(expr));
    let count = match &kind {
        PresentationKind::Print | PresentationKind::Type => 1,
        PresentationKind::Format(count) => *count,
        PresentationKind::None => 0,
    };
    let mut schemas = Vec::with_capacity(count);
    for _ in 0..count {
        schemas.push(next_fact(facts)?);
    }
    match kind {
        PresentationKind::Print => {
            let newline = print_newline(expr);
            if let Expr::Call(_, _, args, _, _) = expr
                && let Some(arg) = args.get_mut(0)
            {
                if allowed {
                    rewrite_print_input(arg, ctx, facts, aliases, locals)?;
                } else {
                    rewrite_expr(arg, ctx, facts, aliases, locals)?;
                }
            }
            if allowed && let Some(schema) = schemas.pop().flatten() {
                rewrite_print_call(expr, schema, newline, locals)?;
            }
        }
        PresentationKind::Type => {
            if let Expr::Call(_, _, args, _, _) = expr
                && let Some(arg) = args.get_mut(0)
            {
                rewrite_expr(arg, ctx, facts, aliases, locals)?;
            }
            if allowed
                && let Some(schema) = schemas.pop().flatten()
                && let Some(label) = type_label(&schema)
            {
                rewrite_type_call(expr, schema, label, locals)?;
            }
        }
        PresentationKind::Format(_) => {
            schemas.reverse();
            if let Expr::Call(_, _, args, _, _) = expr
                && args.len() == 2
            {
                rewrite_format_values(
                    &mut args[1],
                    &mut schemas,
                    ctx,
                    allowed,
                    facts,
                    aliases,
                    locals,
                )?;
            }
        }
        PresentationKind::None => {}
    }
    Ok(())
}

enum PresentationKind {
    None,
    Print,
    Type,
    Format(usize),
}

fn presentation_kind(
    names: &HashMap<u16, String>,
    impls: &HashMap<u16, ()>,
    expr: &Expr,
) -> PresentationKind {
    let Expr::Call(index, _, args, resolution, _) = expr else {
        return PresentationKind::None;
    };
    if resolution.is_none()
        && !impls.contains_key(index)
        && names.get(index).map(String::as_str) == Some("print")
        && args.len() == 1
    {
        return PresentationKind::Print;
    }
    if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::TypeOf) && args.len() == 1
    {
        return PresentationKind::Type;
    }
    if BuiltinFunction::from_call_index(*index) == Some(BuiltinFunction::FormatTemplate)
        && args.len() == 2
    {
        return PresentationKind::Format(format_payload_exprs(&args[1]).len());
    }
    PresentationKind::None
}

fn presentation_inputs_mut(
    names: &HashMap<u16, String>,
    impls: &HashMap<u16, ()>,
    expr: &Expr,
) -> Option<()> {
    match presentation_kind(names, impls, expr) {
        PresentationKind::None => None,
        _ => Some(()),
    }
}

fn print_newline(expr: &Expr) -> bool {
    let Expr::Call(_, _, args, _, _) = expr else {
        return false;
    };
    let Some(arg) = args.first() else {
        return false;
    };
    let (inner, newline) = split_print_newline(arg);
    newline && to_string_input(inner).is_some()
}

enum PrintInputLoc {
    ToStringNewline,
    PlainToString,
    NewlineLeft,
    Whole,
}

fn print_input_loc(arg: &Expr) -> PrintInputLoc {
    let (inner, newline) = split_print_newline(arg);
    if to_string_input(inner).is_some() {
        if newline {
            PrintInputLoc::ToStringNewline
        } else {
            PrintInputLoc::PlainToString
        }
    } else if newline {
        PrintInputLoc::NewlineLeft
    } else {
        PrintInputLoc::Whole
    }
}

fn rewrite_print_input(
    arg: &mut Expr,
    ctx: &RewriteContext<'_, '_>,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &mut HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    match print_input_loc(arg) {
        PrintInputLoc::ToStringNewline => {
            if let Expr::Add(left, _) = arg
                && let Expr::Call(_, _, args, _, _) = left.as_mut()
                && let Some(input) = args.get_mut(0)
            {
                return rewrite_expr(input, ctx, facts, aliases, locals);
            }
            Ok(())
        }
        PrintInputLoc::PlainToString => {
            if let Expr::Call(_, _, args, _, _) = arg
                && let Some(input) = args.get_mut(0)
            {
                return rewrite_expr(input, ctx, facts, aliases, locals);
            }
            Ok(())
        }
        PrintInputLoc::NewlineLeft => {
            if let Expr::Add(left, _) = arg {
                return rewrite_expr(left, ctx, facts, aliases, locals);
            }
            Ok(())
        }
        PrintInputLoc::Whole => rewrite_expr(arg, ctx, facts, aliases, locals),
    }
}

fn take_print_input(arg: &mut Expr) -> Expr {
    match print_input_loc(arg) {
        PrintInputLoc::ToStringNewline => {
            if let Expr::Add(left, _) = arg
                && let Expr::Call(_, _, args, _, _) = left.as_mut()
                && !args.is_empty()
            {
                return std::mem::replace(&mut args[0], Expr::Null);
            }
            Expr::Null
        }
        PrintInputLoc::PlainToString => {
            if let Expr::Call(_, _, args, _, _) = arg
                && !args.is_empty()
            {
                return std::mem::replace(&mut args[0], Expr::Null);
            }
            Expr::Null
        }
        PrintInputLoc::NewlineLeft => {
            if let Expr::Add(left, _) = arg {
                return std::mem::replace(left.as_mut(), Expr::Null);
            }
            Expr::Null
        }
        PrintInputLoc::Whole => std::mem::replace(arg, Expr::Null),
    }
}

fn rewrite_print_call(
    expr: &mut Expr,
    schema: TypeSchema,
    newline: bool,
    locals: &mut usize,
) -> Result<(), CompileError> {
    let Expr::Call(index, _, args, _, _) = expr else {
        return Ok(());
    };
    let print_index = *index;
    let input = args.get_mut(0).map(take_print_input).unwrap_or(Expr::Null);
    let descriptor = encode_descriptor(&schema)?;
    let hidden = alloc_local(locals)?;
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
    let print = Expr::Call(print_index, Vec::new(), vec![rendered], None, None);
    *expr = Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: hidden,
                declared_schema: Some(schema),
                expr: input,
                line: 0,
            },
            Stmt::Expr {
                expr: print,
                line: 0,
            },
        ],
        expr: Box::new(Expr::Var(hidden)),
    };
    Ok(())
}

fn rewrite_type_call(
    expr: &mut Expr,
    schema: TypeSchema,
    label: &'static str,
    locals: &mut usize,
) -> Result<(), CompileError> {
    let Expr::Call(_, _, args, _, _) = expr else {
        return Ok(());
    };
    let input = args
        .get_mut(0)
        .map(|arg| std::mem::replace(arg, Expr::Null))
        .unwrap_or(Expr::Null);
    let hidden = alloc_local(locals)?;
    *expr = Expr::Block {
        stmts: vec![Stmt::Let {
            index: hidden,
            declared_schema: Some(schema),
            expr: input,
            line: 0,
        }],
        expr: Box::new(Expr::String(label.to_string())),
    };
    Ok(())
}

fn rewrite_format_values(
    expr: &mut Expr,
    schemas: &mut Vec<Option<TypeSchema>>,
    ctx: &RewriteContext<'_, '_>,
    allowed: bool,
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
    aliases: &mut HashMap<LocalSlot, ()>,
    locals: &mut usize,
) -> Result<(), CompileError> {
    let Expr::Call(index, _, args, _, _) = expr else {
        return Ok(());
    };
    if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::ArrayPush)
        || args.len() != 2
    {
        return Ok(());
    }
    rewrite_format_values(&mut args[0], schemas, ctx, allowed, facts, aliases, locals)?;
    rewrite_expr(&mut args[1], ctx, facts, aliases, locals)?;
    if !allowed {
        let _ = schemas.pop();
        return Ok(());
    }
    let Some(schema) = schemas.pop().flatten() else {
        return Ok(());
    };
    if !contains_logical_display(&schema) {
        return Ok(());
    }
    let descriptor = encode_descriptor(&schema)?;
    let value = std::mem::replace(&mut args[1], Expr::Null);
    args[1] = Expr::Call(
        BuiltinFunction::LogicalRender.call_index(),
        Vec::new(),
        vec![value, Expr::Bytes(descriptor)],
        None,
        None,
    );
    let _ = locals;
    Ok(())
}

fn note_print_alias(
    slot: LocalSlot,
    expr: &Expr,
    names: &HashMap<u16, String>,
    impls: &HashMap<u16, ()>,
    aliases: &mut HashMap<LocalSlot, ()>,
) {
    let trusted = match expr {
        Expr::FunctionRef(index, _) => is_language_print_index(*index, names, impls),
        Expr::Var(source) | Expr::MoveVar(source) => aliases.contains_key(source),
        _ => false,
    };
    if trusted {
        aliases.insert(slot, ());
    } else {
        aliases.remove(&slot);
    }
}

fn is_language_print_index(
    index: u16,
    names: &HashMap<u16, String>,
    impls: &HashMap<u16, ()>,
) -> bool {
    !impls.contains_key(&index) && names.get(&index).map(String::as_str) == Some("print")
}

fn type_label(schema: &TypeSchema) -> Option<&'static str> {
    match schema {
        TypeSchema::Option(_) => Some("Option"),
        TypeSchema::Result { .. } => Some("Result"),
        TypeSchema::Unit => Some("()"),
        _ => None,
    }
}

fn next_fact(
    facts: &mut std::vec::IntoIter<Option<TypeSchema>>,
) -> Result<Option<TypeSchema>, CompileError> {
    facts
        .next()
        .ok_or_else(|| presentation_error("missing presentation fact"))
}

fn encode_descriptor(schema: &TypeSchema) -> Result<Vec<u8>, CompileError> {
    crate::compiler::type_schema_codec::encode_type_schema_bytes(schema)
        .map_err(|err| presentation_error(&format!("logical render descriptor: {err}")))
}

fn alloc_local(locals: &mut usize) -> Result<LocalSlot, CompileError> {
    let hidden = (*locals)
        .try_into()
        .map_err(|_| presentation_error("logical presentation local overflow"))?;
    *locals += 1;
    Ok(hidden)
}

fn presentation_error(detail: &str) -> CompileError {
    CompileError::HostCallResolve {
        line: None,
        source_name: None,
        detail: detail.to_string(),
        span: None,
    }
}
