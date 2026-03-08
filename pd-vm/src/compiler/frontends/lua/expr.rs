use super::super::super::ParseError;
use super::super::super::ir::{ClosureExpr, Expr, LocalIrBuilder, LocalSlot, Stmt};
use super::super::{is_ident_continue, is_ident_start};
use super::support::{build_lua_return_expr, lower_lua_return_body_exprs, lua_return_arity};
use super::{LuaLoweredExpr, fresh_lua_direct_temp};
use crate::builtins::{BuiltinFunction, is_builtin_namespace, resolve_builtin_namespace_call};
use std::collections::HashMap;

pub(super) struct LuaDirectLowering<'a> {
    pub(super) builder: &'a mut LocalIrBuilder,
    pub(super) namespace_aliases: &'a HashMap<String, String>,
    pub(super) param_slots: &'a HashMap<String, LocalSlot>,
    pub(super) capture_slots: &'a mut HashMap<LocalSlot, LocalSlot>,
    pub(super) capture_enabled: bool,
    pub(super) callable_return_arities: &'a HashMap<LocalSlot, usize>,
}

impl<'a> LuaDirectLowering<'a> {
    pub(super) fn new(
        builder: &'a mut LocalIrBuilder,
        namespace_aliases: &'a HashMap<String, String>,
        param_slots: &'a HashMap<String, LocalSlot>,
        capture_slots: &'a mut HashMap<LocalSlot, LocalSlot>,
        capture_enabled: bool,
        callable_return_arities: &'a HashMap<LocalSlot, usize>,
    ) -> Self {
        Self {
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
        }
    }
}

#[derive(Clone)]
pub(super) enum LuaDirectExpr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Var(String),
    Call(Box<LuaDirectExpr>, Vec<LuaDirectExpr>),
    Member(Box<LuaDirectExpr>, String),
    OptionalMember(Box<LuaDirectExpr>, String),
    Index(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    TableArray(Vec<LuaDirectExpr>),
    TableMap(Vec<(String, LuaDirectExpr)>),
    Closure {
        params: Vec<String>,
        body: Vec<LuaDirectExpr>,
    },
    Add(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Sub(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Mul(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Div(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Mod(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Eq(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Ne(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Lt(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Gt(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Le(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Ge(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    And(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Or(Box<LuaDirectExpr>, Box<LuaDirectExpr>),
    Neg(Box<LuaDirectExpr>),
    Not(Box<LuaDirectExpr>),
}

#[derive(Clone)]
enum LuaDirectToken {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Null,
    Ident(String),
    Function,
    Return,
    End,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    QuestionDot,
    ColonColon,
    Assign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Less,
    Greater,
    LessEq,
    GreaterEq,
    And,
    Or,
    Not,
}

pub(super) fn parse_lua_direct_expr(
    input: &str,
    lowering: &mut LuaDirectLowering<'_>,
    preserve_multi_return_root: bool,
) -> Result<Option<LuaLoweredExpr>, ParseError> {
    let Some(tokens) = tokenize_lua_direct_expr(input) else {
        return Ok(None);
    };
    let mut parser = LuaDirectExprParser { tokens, pos: 0 };
    let Some(expr) = parser.parse_or() else {
        return Ok(None);
    };
    if parser.pos != parser.tokens.len() {
        return Ok(None);
    }
    let lowered = lower_lua_direct_expr(expr.clone(), lowering, preserve_multi_return_root);
    if lowered.is_none()
        && let Some(name) =
            unresolved_lua_direct_call_name(&expr, lowering.builder, lowering.param_slots)
    {
        return Err(ParseError::at_line(1, format!("unknown function '{name}'")));
    }
    Ok(lowered)
}

fn unresolved_lua_direct_call_name(
    expr: &LuaDirectExpr,
    builder: &LocalIrBuilder,
    param_slots: &HashMap<String, LocalSlot>,
) -> Option<String> {
    let LuaDirectExpr::Call(callee, _) = expr else {
        return None;
    };
    let LuaDirectExpr::Var(name) = callee.as_ref() else {
        return None;
    };
    if name == "print"
        || param_slots.contains_key(name)
        || builder.resolve_local_expr(name).is_some()
        || builder.has_declared_function(name)
    {
        return None;
    }
    Some(name.clone())
}

pub(super) fn parse_lua_direct_expr_top(
    input: &str,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    callable_return_arities: &HashMap<LocalSlot, usize>,
) -> Result<Option<Expr>, ParseError> {
    let params = HashMap::new();
    let mut captures = HashMap::new();
    let mut lowering = LuaDirectLowering::new(
        builder,
        namespace_aliases,
        &params,
        &mut captures,
        false,
        callable_return_arities,
    );
    Ok(parse_lua_direct_expr(input, &mut lowering, false)?.map(|expr| expr.expr))
}

pub(super) fn build_lua_unpack_get_expr(target: Expr, index: i64) -> Expr {
    Expr::Call(
        BuiltinFunction::Get.call_index(),
        vec![target, Expr::Int(index)],
    )
}

fn finalize_lua_root_expr(
    expr: Expr,
    unpack_arity: usize,
    preserve_multi_return_root: bool,
) -> LuaLoweredExpr {
    let lowered = LuaLoweredExpr {
        expr,
        unpack_arity: unpack_arity.max(1),
        callable_return_arity: None,
    };
    if preserve_multi_return_root {
        lowered
    } else {
        lowered.scalarized()
    }
}

fn lookup_lua_callable_return_arity(
    name: &str,
    builder: &LocalIrBuilder,
    callable_return_arities: &HashMap<LocalSlot, usize>,
) -> Option<usize> {
    let Expr::Var(slot) = builder.resolve_local_expr(name)? else {
        return None;
    };
    callable_return_arities.get(&slot).copied()
}

fn lower_lua_call_args(
    args: Vec<LuaDirectExpr>,
    lowering: &mut LuaDirectLowering<'_>,
) -> Option<Vec<Expr>> {
    let mut lowered_args = Vec::with_capacity(args.len());
    for arg in args {
        lowered_args.push(lower_lua_direct_expr(arg, lowering, false)?);
    }
    Some(lowered_args.into_iter().map(|value| value.expr).collect())
}

fn lower_lua_callable_call(
    callee: LuaDirectExpr,
    args: Vec<Expr>,
    lowering: &mut LuaDirectLowering<'_>,
) -> Option<LuaLoweredExpr> {
    if let LuaDirectExpr::Var(name) = &callee {
        if let Some(expr) = lowering.builder.resolve_call_expr(name, args.clone()) {
            let unpack_arity = lookup_lua_callable_return_arity(
                name,
                lowering.builder,
                lowering.callable_return_arities,
            )
            .unwrap_or(1);
            return Some(LuaLoweredExpr {
                expr,
                unpack_arity,
                callable_return_arity: None,
            });
        }
        if name == "print" && args.len() == 1 {
            lowering.builder.declare_function("print", Some(1)).ok()?;
            return lowering
                .builder
                .resolve_call_expr("print", args)
                .map(LuaLoweredExpr::scalar);
        }
    }

    if let Some(path) = flatten_lua_member_path(&callee)
        && let Some(expr) = lower_lua_namespace_call(
            &path,
            args.clone(),
            lowering.builder,
            lowering.namespace_aliases,
        )
    {
        return Some(LuaLoweredExpr::scalar(expr));
    }

    let callee = lower_lua_direct_expr(callee, lowering, false)?;
    let unpack_arity = callee.callable_return_arity.unwrap_or(1);
    match callee.expr {
        Expr::Var(slot) => Some(LuaLoweredExpr {
            expr: Expr::LocalCall(slot, args),
            unpack_arity,
            callable_return_arity: None,
        }),
        Expr::Closure(closure) => Some(LuaLoweredExpr {
            expr: Expr::ClosureCall(closure, args),
            unpack_arity,
            callable_return_arity: None,
        }),
        Expr::FunctionRef(index) => Some(LuaLoweredExpr {
            expr: Expr::Call(index, args),
            unpack_arity,
            callable_return_arity: None,
        }),
        _ => None,
    }
}

fn lower_lua_protected_call(
    callee: LuaDirectExpr,
    args: Vec<Expr>,
    lowering: &mut LuaDirectLowering<'_>,
    preserve_multi_return_root: bool,
) -> Option<LuaLoweredExpr> {
    let call_result = lower_lua_callable_call(callee, args, lowering)?;
    let packed = build_lua_return_expr(
        Some(vec![
            LuaLoweredExpr::scalar(Expr::Bool(true)),
            LuaLoweredExpr {
                expr: call_result.expr,
                unpack_arity: call_result.unpack_arity,
                callable_return_arity: None,
            },
        ]),
        call_result.unpack_arity.saturating_add(1),
        lowering.builder,
        1,
    );
    Some(finalize_lua_root_expr(
        packed.expr,
        packed.unpack_arity,
        preserve_multi_return_root,
    ))
}

pub(super) fn lower_lua_direct_expr(
    expr: LuaDirectExpr,
    lowering: &mut LuaDirectLowering<'_>,
    preserve_multi_return_root: bool,
) -> Option<LuaLoweredExpr> {
    match expr {
        LuaDirectExpr::Null => Some(LuaLoweredExpr::scalar(Expr::Null)),
        LuaDirectExpr::Bool(value) => Some(LuaLoweredExpr::scalar(Expr::Bool(value))),
        LuaDirectExpr::Int(value) => Some(LuaLoweredExpr::scalar(Expr::Int(value))),
        LuaDirectExpr::Float(value) => Some(LuaLoweredExpr::scalar(Expr::Float(value))),
        LuaDirectExpr::String(value) => Some(LuaLoweredExpr::scalar(Expr::String(value))),
        LuaDirectExpr::Var(name) => {
            if let Some(slot) = lowering.param_slots.get(&name).copied() {
                return Some(LuaLoweredExpr::scalar(Expr::Var(slot)));
            }
            if let Some(Expr::Var(source_slot)) = lowering.builder.resolve_local_expr(&name) {
                let callable_return_arity =
                    lowering.callable_return_arities.get(&source_slot).copied();
                if !lowering.capture_enabled {
                    return Some(LuaLoweredExpr {
                        expr: Expr::Var(source_slot),
                        unpack_arity: 1,
                        callable_return_arity,
                    });
                }
                if let Some(captured_slot) = lowering.capture_slots.get(&source_slot).copied() {
                    return Some(LuaLoweredExpr {
                        expr: Expr::Var(captured_slot),
                        unpack_arity: 1,
                        callable_return_arity,
                    });
                }
                let capture_name = fresh_lua_direct_temp("capture_slot");
                let captured_slot = lowering.builder.alloc_local_named(&capture_name).ok()?;
                lowering.capture_slots.insert(source_slot, captured_slot);
                return Some(LuaLoweredExpr {
                    expr: Expr::Var(captured_slot),
                    unpack_arity: 1,
                    callable_return_arity,
                });
            }
            None
        }
        LuaDirectExpr::Call(callee, args) => {
            if let LuaDirectExpr::Var(name) = callee.as_ref() {
                if *name == "pcall" {
                    let mut args = args.into_iter();
                    let callable = args.next()?;
                    let lowered_args = lower_lua_call_args(args.collect(), lowering)?;
                    return lower_lua_protected_call(
                        callable,
                        lowered_args,
                        lowering,
                        preserve_multi_return_root,
                    );
                }
                if *name == "xpcall" {
                    let mut args = args.into_iter();
                    let callable = args.next()?;
                    let _handler = args.next()?;
                    let lowered_args = lower_lua_call_args(args.collect(), lowering)?;
                    return lower_lua_protected_call(
                        callable,
                        lowered_args,
                        lowering,
                        preserve_multi_return_root,
                    );
                }
            }

            let lowered_args = lower_lua_call_args(args, lowering)?;
            let call_result = lower_lua_callable_call(*callee, lowered_args, lowering)?;
            Some(finalize_lua_root_expr(
                call_result.expr,
                call_result.unpack_arity,
                preserve_multi_return_root,
            ))
        }
        LuaDirectExpr::Member(target, member) => {
            let target = lower_lua_direct_expr(*target, lowering, false)?;
            Some(LuaLoweredExpr::scalar(Expr::Call(
                BuiltinFunction::Get.call_index(),
                vec![target.expr, Expr::String(member)],
            )))
        }
        LuaDirectExpr::OptionalMember(target, member) => {
            let target = lower_lua_direct_expr(*target, lowering, false)?;
            build_lua_optional_member_expr(target.expr, member, lowering.builder)
                .map(LuaLoweredExpr::scalar)
        }
        LuaDirectExpr::Index(target, key) => {
            let target = lower_lua_direct_expr(*target, lowering, false)?;
            let key = lower_lua_direct_expr(*key, lowering, false)?;
            Some(LuaLoweredExpr::scalar(Expr::Call(
                BuiltinFunction::Get.call_index(),
                vec![target.expr, key.expr],
            )))
        }
        LuaDirectExpr::TableArray(values) => {
            let mut out = Expr::Call(BuiltinFunction::ArrayNew.call_index(), Vec::new());
            for value in values {
                let value = lower_lua_direct_expr(value, lowering, false)?;
                out = Expr::Call(
                    BuiltinFunction::ArrayPush.call_index(),
                    vec![out, value.expr],
                );
            }
            Some(LuaLoweredExpr::scalar(out))
        }
        LuaDirectExpr::TableMap(entries) => {
            let mut out = Expr::Call(BuiltinFunction::MapNew.call_index(), Vec::new());
            for (key, value) in entries {
                let value = lower_lua_direct_expr(value, lowering, false)?;
                out = Expr::Call(
                    BuiltinFunction::Set.call_index(),
                    vec![out, Expr::String(key), value.expr],
                );
            }
            Some(LuaLoweredExpr::scalar(out))
        }
        LuaDirectExpr::Closure { params, body } => {
            let mut closure_params = HashMap::new();
            let mut param_slots_vec = Vec::new();
            for name in params {
                let slot_name = fresh_lua_direct_temp(&format!("param_{name}"));
                let slot = lowering.builder.alloc_local_named(&slot_name).ok()?;
                closure_params.insert(name, slot);
                param_slots_vec.push(slot);
            }
            let mut captures = HashMap::new();
            let lowered_body = lower_lua_return_body_exprs(
                body,
                lowering.builder,
                lowering.namespace_aliases,
                &closure_params,
                &mut captures,
                lowering.callable_return_arities,
            )?;
            let mut capture_copies = captures.into_iter().collect::<Vec<_>>();
            capture_copies.sort_by_key(|(source_slot, _)| *source_slot);
            let target_arity = lua_return_arity(Some(lowered_body.as_slice()));
            Some(LuaLoweredExpr::callable(
                Expr::Closure(ClosureExpr {
                    param_slots: param_slots_vec,
                    capture_copies,
                    body: Box::new(
                        build_lua_return_expr(
                            Some(lowered_body),
                            target_arity,
                            lowering.builder,
                            1,
                        )
                        .expr,
                    ),
                }),
                target_arity,
            ))
        }
        LuaDirectExpr::Add(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Add),
        LuaDirectExpr::Sub(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Sub),
        LuaDirectExpr::Mul(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Mul),
        LuaDirectExpr::Div(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Div),
        LuaDirectExpr::Mod(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Mod),
        LuaDirectExpr::Eq(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Eq),
        LuaDirectExpr::Ne(lhs, rhs) => {
            let eq = lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Eq)?;
            Some(LuaLoweredExpr::scalar(Expr::Not(Box::new(eq.expr))))
        }
        LuaDirectExpr::Lt(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Lt),
        LuaDirectExpr::Gt(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Gt),
        LuaDirectExpr::Le(lhs, rhs) => lower_lua_non_strict_compare(*lhs, *rhs, lowering, Expr::Lt),
        LuaDirectExpr::Ge(lhs, rhs) => lower_lua_non_strict_compare(*lhs, *rhs, lowering, Expr::Gt),
        LuaDirectExpr::And(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::And),
        LuaDirectExpr::Or(lhs, rhs) => lower_lua_binary_expr(*lhs, *rhs, lowering, Expr::Or),
        LuaDirectExpr::Neg(inner) => lower_lua_unary_expr(*inner, lowering, Expr::Neg),
        LuaDirectExpr::Not(inner) => lower_lua_unary_expr(*inner, lowering, Expr::Not),
    }
}

fn lower_lua_non_strict_compare(
    lhs: LuaDirectExpr,
    rhs: LuaDirectExpr,
    lowering: &mut LuaDirectLowering<'_>,
    build_strict: fn(Box<Expr>, Box<Expr>) -> Expr,
) -> Option<LuaLoweredExpr> {
    let lhs = lower_lua_direct_expr(lhs, lowering, false)?;
    let rhs = lower_lua_direct_expr(rhs, lowering, false)?;
    let lhs_slot = lowering
        .builder
        .alloc_local_named(&fresh_lua_direct_temp("cmp_lhs"))
        .ok()?;
    let rhs_slot = lowering
        .builder
        .alloc_local_named(&fresh_lua_direct_temp("cmp_rhs"))
        .ok()?;
    let lhs_var = Expr::Var(lhs_slot);
    let rhs_var = Expr::Var(rhs_slot);
    Some(LuaLoweredExpr::scalar(Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: lhs_slot,
                expr: lhs.expr,
                line: 1,
            },
            Stmt::Let {
                index: rhs_slot,
                expr: rhs.expr,
                line: 1,
            },
        ],
        expr: Box::new(Expr::Or(
            Box::new(build_strict(
                Box::new(lhs_var.clone()),
                Box::new(rhs_var.clone()),
            )),
            Box::new(Expr::Eq(Box::new(lhs_var), Box::new(rhs_var))),
        )),
    }))
}

fn lower_lua_binary_expr(
    lhs: LuaDirectExpr,
    rhs: LuaDirectExpr,
    lowering: &mut LuaDirectLowering<'_>,
    build: fn(Box<Expr>, Box<Expr>) -> Expr,
) -> Option<LuaLoweredExpr> {
    let lhs = lower_lua_direct_expr(lhs, lowering, false)?;
    let rhs = lower_lua_direct_expr(rhs, lowering, false)?;
    Some(LuaLoweredExpr::scalar(build(
        Box::new(lhs.expr),
        Box::new(rhs.expr),
    )))
}

fn lower_lua_unary_expr(
    inner: LuaDirectExpr,
    lowering: &mut LuaDirectLowering<'_>,
    build: fn(Box<Expr>) -> Expr,
) -> Option<LuaLoweredExpr> {
    let inner = lower_lua_direct_expr(inner, lowering, false)?;
    Some(LuaLoweredExpr::scalar(build(Box::new(inner.expr))))
}

fn flatten_lua_member_path(expr: &LuaDirectExpr) -> Option<Vec<String>> {
    match expr {
        LuaDirectExpr::Var(name) => Some(vec![name.clone()]),
        LuaDirectExpr::Member(target, member) => {
            let mut out = flatten_lua_member_path(target)?;
            out.push(member.clone());
            Some(out)
        }
        _ => None,
    }
}

fn lower_lua_namespace_call(
    path: &[String],
    args: Vec<Expr>,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
) -> Option<Expr> {
    if path.is_empty() {
        return None;
    }
    let imported_root = namespace_aliases.get(&path[0]).cloned();
    let root = imported_root.clone().unwrap_or_else(|| path[0].clone());

    if let Some(imported_root) = imported_root
        && path.len() >= 2 && !is_builtin_namespace(&imported_root) {
            let mut segments = vec![imported_root];
            segments.extend(path.iter().skip(1).cloned());
            let call_name = segments.join("::");
            let arity = u8::try_from(args.len()).ok()?;
            builder.declare_function(&call_name, Some(arity)).ok()?;
            return builder.resolve_call_expr(&call_name, args);
        }

    if path.len() == 2 && is_builtin_namespace(&root) {
        return lower_lua_regex_or_builtin_namespace_call(&root, &path[1], args);
    }

    if path.len() == 2 {
        if let Some(expr) = builder.resolve_call_expr(&path[1], args.clone()) {
            return Some(expr);
        }
        let arity = u8::try_from(args.len()).ok()?;
        builder.declare_function(&path[1], Some(arity)).ok()?;
        return builder.resolve_call_expr(&path[1], args);
    }

    None
}

fn lower_lua_regex_or_builtin_namespace_call(
    namespace: &str,
    member: &str,
    mut args: Vec<Expr>,
) -> Option<Expr> {
    if namespace == "re" {
        let (builtin, base_arity) = match member {
            "match" | "is_match" => (BuiltinFunction::ReIsMatch, 2usize),
            "find" => (BuiltinFunction::ReFind, 2usize),
            "replace" => (BuiltinFunction::ReReplace, 3usize),
            "split" => (BuiltinFunction::ReSplit, 2usize),
            "captures" => (BuiltinFunction::ReCaptures, 2usize),
            _ => return None,
        };
        if args.len() == base_arity {
            return Some(Expr::Call(builtin.call_index(), args));
        }
        if args.len() == base_arity + 1 {
            let flags = args.pop()?;
            let pattern = args.first().cloned()?;
            args[0] = apply_lua_regex_flags_to_pattern_expr(pattern, flags);
            return Some(Expr::Call(builtin.call_index(), args));
        }
        return None;
    }

    let builtin = resolve_builtin_namespace_call(namespace, member)?;
    if usize::from(builtin.arity()) != args.len() {
        return None;
    }
    Some(Expr::Call(builtin.call_index(), args))
}

fn apply_lua_regex_flags_to_pattern_expr(pattern: Expr, flags: Expr) -> Expr {
    let prefix = Expr::Call(
        BuiltinFunction::Concat.call_index(),
        vec![Expr::String("(?".to_string()), flags],
    );
    let prefix = Expr::Call(
        BuiltinFunction::Concat.call_index(),
        vec![prefix, Expr::String(")".to_string())],
    );
    Expr::Call(BuiltinFunction::Concat.call_index(), vec![prefix, pattern])
}

fn build_lua_optional_member_expr(
    target: Expr,
    member: String,
    builder: &mut LocalIrBuilder,
) -> Option<Expr> {
    let line = 1;
    let target_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("opt_target"))
        .ok()?;
    let result_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("opt_result"))
        .ok()?;
    let keys_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("opt_keys"))
        .ok()?;
    let idx_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("opt_idx"))
        .ok()?;
    let found_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("opt_found"))
        .ok()?;

    let keys_len_expr = || {
        Expr::Call(
            BuiltinFunction::Len.call_index(),
            vec![Expr::Var(keys_slot)],
        )
    };
    let current_key_expr = || {
        Expr::Call(
            BuiltinFunction::Get.call_index(),
            vec![Expr::Var(keys_slot), Expr::Var(idx_slot)],
        )
    };

    Some(Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: target_slot,
                expr: target,
                line,
            },
            Stmt::Let {
                index: result_slot,
                expr: Expr::Null,
                line,
            },
            Stmt::IfElse {
                condition: Expr::Not(Box::new(Expr::Eq(
                    Box::new(Expr::Var(target_slot)),
                    Box::new(Expr::Null),
                ))),
                then_branch: vec![
                    Stmt::Let {
                        index: keys_slot,
                        expr: Expr::Call(
                            BuiltinFunction::Keys.call_index(),
                            vec![Expr::Var(target_slot)],
                        ),
                        line,
                    },
                    Stmt::Let {
                        index: idx_slot,
                        expr: Expr::Int(0),
                        line,
                    },
                    Stmt::Let {
                        index: found_slot,
                        expr: Expr::Bool(false),
                        line,
                    },
                    Stmt::While {
                        condition: Expr::Lt(
                            Box::new(Expr::Var(idx_slot)),
                            Box::new(keys_len_expr()),
                        ),
                        body: vec![Stmt::IfElse {
                            condition: Expr::Eq(
                                Box::new(current_key_expr()),
                                Box::new(Expr::String(member.clone())),
                            ),
                            then_branch: vec![
                                Stmt::Assign {
                                    index: found_slot,
                                    expr: Expr::Bool(true),
                                    line,
                                },
                                Stmt::Assign {
                                    index: idx_slot,
                                    expr: keys_len_expr(),
                                    line,
                                },
                            ],
                            else_branch: vec![Stmt::Assign {
                                index: idx_slot,
                                expr: Expr::Add(
                                    Box::new(Expr::Var(idx_slot)),
                                    Box::new(Expr::Int(1)),
                                ),
                                line,
                            }],
                            line,
                        }],
                        line,
                    },
                    Stmt::IfElse {
                        condition: Expr::Var(found_slot),
                        then_branch: vec![Stmt::Assign {
                            index: result_slot,
                            expr: Expr::Call(
                                BuiltinFunction::Get.call_index(),
                                vec![Expr::Var(target_slot), Expr::String(member)],
                            ),
                            line,
                        }],
                        else_branch: Vec::new(),
                        line,
                    },
                ],
                else_branch: Vec::new(),
                line,
            },
        ],
        expr: Box::new(Expr::Var(result_slot)),
    })
}

struct LuaDirectExprParser {
    tokens: Vec<LuaDirectToken>,
    pos: usize,
}

impl LuaDirectExprParser {
    fn parse_or(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_and()?;
        while self.match_token(|token| matches!(token, LuaDirectToken::Or)) {
            expr = LuaDirectExpr::Or(Box::new(expr), Box::new(self.parse_and()?));
        }
        Some(expr)
    }

    fn parse_and(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_equality()?;
        while self.match_token(|token| matches!(token, LuaDirectToken::And)) {
            expr = LuaDirectExpr::And(Box::new(expr), Box::new(self.parse_equality()?));
        }
        Some(expr)
    }

    fn parse_equality(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_relational()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::EqEq)) {
                expr = LuaDirectExpr::Eq(Box::new(expr), Box::new(self.parse_relational()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::NotEq)) {
                expr = LuaDirectExpr::Ne(Box::new(expr), Box::new(self.parse_relational()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_relational(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_add()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::Less)) {
                expr = LuaDirectExpr::Lt(Box::new(expr), Box::new(self.parse_add()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Greater)) {
                expr = LuaDirectExpr::Gt(Box::new(expr), Box::new(self.parse_add()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::LessEq)) {
                expr = LuaDirectExpr::Le(Box::new(expr), Box::new(self.parse_add()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::GreaterEq)) {
                expr = LuaDirectExpr::Ge(Box::new(expr), Box::new(self.parse_add()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_add(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_mul()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::Plus)) {
                expr = LuaDirectExpr::Add(Box::new(expr), Box::new(self.parse_mul()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Minus)) {
                expr = LuaDirectExpr::Sub(Box::new(expr), Box::new(self.parse_mul()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_mul(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_unary()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::Star)) {
                expr = LuaDirectExpr::Mul(Box::new(expr), Box::new(self.parse_unary()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Slash)) {
                expr = LuaDirectExpr::Div(Box::new(expr), Box::new(self.parse_unary()?));
            } else if self.match_token(|token| matches!(token, LuaDirectToken::Percent)) {
                expr = LuaDirectExpr::Mod(Box::new(expr), Box::new(self.parse_unary()?));
            } else {
                break;
            }
        }
        Some(expr)
    }

    fn parse_unary(&mut self) -> Option<LuaDirectExpr> {
        if self.match_token(|token| matches!(token, LuaDirectToken::Not)) {
            return Some(LuaDirectExpr::Not(Box::new(self.parse_unary()?)));
        }
        if self.match_token(|token| matches!(token, LuaDirectToken::Minus)) {
            return Some(LuaDirectExpr::Neg(Box::new(self.parse_unary()?)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Option<LuaDirectExpr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.match_token(|token| matches!(token, LuaDirectToken::LParen)) {
                let args = self.parse_call_args()?;
                expr = LuaDirectExpr::Call(Box::new(expr), args);
                continue;
            }
            if self.match_token(|token| {
                matches!(token, LuaDirectToken::Dot | LuaDirectToken::ColonColon)
            }) {
                let member = self.match_ident()?;
                expr = LuaDirectExpr::Member(Box::new(expr), member);
                continue;
            }
            if self.match_token(|token| matches!(token, LuaDirectToken::QuestionDot)) {
                let member = self.match_ident()?;
                expr = LuaDirectExpr::OptionalMember(Box::new(expr), member);
                continue;
            }
            if self.match_token(|token| matches!(token, LuaDirectToken::LBracket)) {
                let key = self.parse_or()?;
                if !self.match_token(|token| matches!(token, LuaDirectToken::RBracket)) {
                    return None;
                }
                expr = LuaDirectExpr::Index(Box::new(expr), Box::new(key));
                continue;
            }
            break;
        }
        Some(expr)
    }

    fn parse_primary(&mut self) -> Option<LuaDirectExpr> {
        if let Some(token) = self.peek().cloned() {
            match token {
                LuaDirectToken::Int(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Int(value))
                }
                LuaDirectToken::Float(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Float(value))
                }
                LuaDirectToken::String(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::String(value))
                }
                LuaDirectToken::Bool(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Bool(value))
                }
                LuaDirectToken::Null => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Null)
                }
                LuaDirectToken::Ident(value) => {
                    self.pos += 1;
                    Some(LuaDirectExpr::Var(value))
                }
                LuaDirectToken::LParen => {
                    self.pos += 1;
                    let expr = self.parse_or()?;
                    if !self.match_token(|token| matches!(token, LuaDirectToken::RParen)) {
                        return None;
                    }
                    Some(expr)
                }
                LuaDirectToken::LBrace => self.parse_table_literal(),
                LuaDirectToken::Function => self.parse_inline_function_literal(),
                _ => None,
            }
        } else {
            None
        }
    }

    fn parse_call_args(&mut self) -> Option<Vec<LuaDirectExpr>> {
        let mut args = Vec::new();
        if self.match_token(|token| matches!(token, LuaDirectToken::RParen)) {
            return Some(args);
        }
        loop {
            args.push(self.parse_or()?);
            if self.match_token(|token| matches!(token, LuaDirectToken::Comma)) {
                continue;
            }
            if self.match_token(|token| matches!(token, LuaDirectToken::RParen)) {
                break;
            }
            return None;
        }
        Some(args)
    }

    fn parse_table_literal(&mut self) -> Option<LuaDirectExpr> {
        // Consume '{'
        self.pos += 1;
        if self.match_token(|token| matches!(token, LuaDirectToken::RBrace)) {
            return Some(LuaDirectExpr::TableMap(Vec::new()));
        }

        let mut array_values = Vec::new();
        let mut map_values = Vec::new();

        loop {
            if let Some((key, value)) = self.parse_table_key_value_entry() {
                map_values.push((key, value));
            } else {
                array_values.push(self.parse_or()?);
            }

            if self.match_token(|token| matches!(token, LuaDirectToken::Comma)) {
                if self.match_token(|token| matches!(token, LuaDirectToken::RBrace)) {
                    break;
                }
                continue;
            }
            if self.match_token(|token| matches!(token, LuaDirectToken::RBrace)) {
                break;
            }
            return None;
        }

        if !map_values.is_empty() && !array_values.is_empty() {
            return None;
        }
        if !map_values.is_empty() {
            return Some(LuaDirectExpr::TableMap(map_values));
        }
        Some(LuaDirectExpr::TableArray(array_values))
    }

    fn parse_table_key_value_entry(&mut self) -> Option<(String, LuaDirectExpr)> {
        let save = self.pos;
        let key = self.match_ident()?;
        if !self.match_token(|token| matches!(token, LuaDirectToken::Assign)) {
            self.pos = save;
            return None;
        }
        let value = self.parse_or()?;
        Some((key, value))
    }

    fn parse_inline_function_literal(&mut self) -> Option<LuaDirectExpr> {
        // Consume 'function'
        self.pos += 1;
        if !self.match_token(|token| matches!(token, LuaDirectToken::LParen)) {
            return None;
        }
        let mut params = Vec::new();
        if !self.match_token(|token| matches!(token, LuaDirectToken::RParen)) {
            loop {
                params.push(self.match_ident()?);
                if self.match_token(|token| matches!(token, LuaDirectToken::Comma)) {
                    continue;
                }
                if self.match_token(|token| matches!(token, LuaDirectToken::RParen)) {
                    break;
                }
                return None;
            }
        }
        let body = if self.match_token(|token| matches!(token, LuaDirectToken::End)) {
            vec![LuaDirectExpr::Null]
        } else if self.match_token(|token| matches!(token, LuaDirectToken::Return)) {
            if self.match_token(|token| matches!(token, LuaDirectToken::End)) {
                vec![LuaDirectExpr::Null]
            } else {
                let mut body = vec![self.parse_or()?];
                while self.match_token(|token| matches!(token, LuaDirectToken::Comma)) {
                    body.push(self.parse_or()?);
                }
                if !self.match_token(|token| matches!(token, LuaDirectToken::End)) {
                    return None;
                }
                body
            }
        } else {
            return None;
        };
        Some(LuaDirectExpr::Closure { params, body })
    }

    fn peek(&self) -> Option<&LuaDirectToken> {
        self.tokens.get(self.pos)
    }

    fn match_ident(&mut self) -> Option<String> {
        let LuaDirectToken::Ident(value) = self.peek()?.clone() else {
            return None;
        };
        self.pos += 1;
        Some(value)
    }

    fn match_token<F>(&mut self, predicate: F) -> bool
    where
        F: Fn(&LuaDirectToken) -> bool,
    {
        if self.peek().is_some_and(predicate) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

fn tokenize_lua_direct_expr(input: &str) -> Option<Vec<LuaDirectToken>> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let mut is_float = false;
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                is_float = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text = std::str::from_utf8(&bytes[start..i]).ok()?;
            if is_float {
                out.push(LuaDirectToken::Float(text.parse::<f64>().ok()?));
            } else {
                out.push(LuaDirectToken::Int(text.parse::<i64>().ok()?));
            }
            continue;
        }
        if b == b'"' || b == b'\'' {
            let quote = b;
            i += 1;
            let mut text = String::new();
            let mut escaped = false;
            while i < bytes.len() {
                let ch = bytes[i];
                i += 1;
                if escaped {
                    let mapped = match ch {
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'\\' => '\\',
                        b'"' => '"',
                        b'\'' => '\'',
                        b'x' => {
                            if i + 1 > bytes.len() {
                                return None;
                            }
                            let hi = bytes.get(i).copied()?;
                            let lo = bytes.get(i + 1).copied()?;
                            i += 2;
                            let hex = [hi, lo];
                            let value = std::str::from_utf8(&hex).ok()?;
                            let value = u8::from_str_radix(value, 16).ok()?;
                            value as char
                        }
                        other => other as char,
                    };
                    text.push(mapped);
                    escaped = false;
                    continue;
                }
                if ch == b'\\' {
                    escaped = true;
                    continue;
                }
                if ch == quote {
                    break;
                }
                text.push(ch as char);
            }
            if escaped {
                return None;
            }
            out.push(LuaDirectToken::String(text));
            continue;
        }
        if is_ident_start(b as char) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident = std::str::from_utf8(&bytes[start..i]).ok()?;
            match ident {
                "true" => out.push(LuaDirectToken::Bool(true)),
                "false" => out.push(LuaDirectToken::Bool(false)),
                "nil" => out.push(LuaDirectToken::Null),
                "and" => out.push(LuaDirectToken::And),
                "or" => out.push(LuaDirectToken::Or),
                "not" => out.push(LuaDirectToken::Not),
                "function" => out.push(LuaDirectToken::Function),
                "return" => out.push(LuaDirectToken::Return),
                "end" => out.push(LuaDirectToken::End),
                _ => out.push(LuaDirectToken::Ident(ident.to_string())),
            }
            continue;
        }
        match b {
            b'(' => {
                out.push(LuaDirectToken::LParen);
                i += 1;
            }
            b')' => {
                out.push(LuaDirectToken::RParen);
                i += 1;
            }
            b'[' => {
                out.push(LuaDirectToken::LBracket);
                i += 1;
            }
            b']' => {
                out.push(LuaDirectToken::RBracket);
                i += 1;
            }
            b'{' => {
                out.push(LuaDirectToken::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(LuaDirectToken::RBrace);
                i += 1;
            }
            b',' => {
                out.push(LuaDirectToken::Comma);
                i += 1;
            }
            b'=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(LuaDirectToken::EqEq);
                    i += 2;
                } else {
                    out.push(LuaDirectToken::Assign);
                    i += 1;
                }
            }
            b'?' if i + 1 < bytes.len() && bytes[i + 1] == b'.' => {
                out.push(LuaDirectToken::QuestionDot);
                i += 2;
            }
            b'.' => {
                out.push(LuaDirectToken::Dot);
                i += 1;
            }
            b':' if i + 1 < bytes.len() && bytes[i + 1] == b':' => {
                out.push(LuaDirectToken::ColonColon);
                i += 2;
            }
            b'+' => {
                out.push(LuaDirectToken::Plus);
                i += 1;
            }
            b'-' => {
                out.push(LuaDirectToken::Minus);
                i += 1;
            }
            b'*' => {
                out.push(LuaDirectToken::Star);
                i += 1;
            }
            b'/' => {
                out.push(LuaDirectToken::Slash);
                i += 1;
            }
            b'%' => {
                out.push(LuaDirectToken::Percent);
                i += 1;
            }
            b'~' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::NotEq);
                i += 2;
            }
            b'<' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::LessEq);
                i += 2;
            }
            b'>' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(LuaDirectToken::GreaterEq);
                i += 2;
            }
            b'<' => {
                out.push(LuaDirectToken::Less);
                i += 1;
            }
            b'>' => {
                out.push(LuaDirectToken::Greater);
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}
