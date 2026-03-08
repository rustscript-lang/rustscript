use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::ParseError;
use super::super::ir::{Expr, FrontendIr, LocalIrBuilder, Stmt};
use super::{is_ident_continue, is_ident_start};
use crate::builtins::{BuiltinFunction, is_builtin_namespace, resolve_builtin_namespace_call};

static GENSYM_COUNTER: AtomicUsize = AtomicUsize::new(0);
const ROOT_HOST_NAMESPACE_SPEC: &str = "vm";

#[derive(Clone, Debug, Default)]
pub(crate) struct SchemeImportContext {
    pub(crate) declared_functions: Vec<(String, Option<u8>)>,
    pub(crate) direct_aliases: HashMap<String, String>,
    pub(crate) namespace_imports: HashMap<String, HashSet<String>>,
    pub(crate) namespace_prefixes: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
struct NormalizedSchemeImportContext {
    declared_functions: Vec<(String, Option<u8>)>,
    direct_aliases_exact: HashMap<String, String>,
    direct_aliases_normalized: HashMap<String, String>,
    namespace_imports: HashMap<String, HashSet<String>>,
    namespace_prefixes: HashMap<String, Vec<String>>,
}

enum SchemeResolvedCallHead {
    Direct(String),
    Namespace(Vec<String>),
}

impl NormalizedSchemeImportContext {
    fn from_raw(raw: &SchemeImportContext) -> Self {
        let mut direct_aliases_exact = HashMap::new();
        let mut direct_aliases_normalized = HashMap::new();
        for (alias, target) in &raw.direct_aliases {
            direct_aliases_exact.insert(alias.clone(), target.clone());
            if let Some(normalized_alias) = canonicalize_identifier(alias) {
                direct_aliases_normalized.insert(normalized_alias, target.clone());
            }
        }

        let mut namespace_imports = HashMap::<String, HashSet<String>>::new();
        for (namespace, members) in &raw.namespace_imports {
            let Some(normalized_namespace) = canonicalize_identifier(namespace) else {
                continue;
            };
            let entry = namespace_imports.entry(normalized_namespace).or_default();
            for member in members {
                if let Some(normalized_member) = canonicalize_identifier(member) {
                    entry.insert(normalized_member);
                }
            }
        }

        let mut namespace_prefixes = HashMap::<String, Vec<String>>::new();
        for (namespace, prefix) in &raw.namespace_prefixes {
            let Some(normalized_namespace) = canonicalize_identifier(namespace) else {
                continue;
            };
            let Some(prefix_segments) = canonicalize_call_path(prefix) else {
                continue;
            };
            namespace_prefixes.insert(normalized_namespace, prefix_segments);
        }

        Self {
            declared_functions: raw.declared_functions.clone(),
            direct_aliases_exact,
            direct_aliases_normalized,
            namespace_imports,
            namespace_prefixes,
        }
    }

    fn resolve_direct_alias<'a>(&'a self, head: &'a str) -> Option<&'a str> {
        self.direct_aliases_exact
            .get(head)
            .map(String::as_str)
            .or_else(|| {
                let normalized = canonicalize_identifier(head)?;
                self.direct_aliases_normalized
                    .get(&normalized)
                    .map(String::as_str)
            })
    }

    fn resolve_namespace_import(&self, segments: &[String]) -> Option<Vec<String>> {
        if segments.len() == 2
            && self
                .namespace_imports
                .get(&segments[0])
                .is_some_and(|members| members.contains(&segments[1]))
        {
            return Some(vec![segments[1].clone()]);
        }

        let prefix = self.namespace_prefixes.get(&segments[0])?;
        let mut rewritten = prefix.clone();
        rewritten.extend(segments.iter().skip(1).cloned());
        Some(rewritten)
    }
}

fn gensym(prefix: &str) -> String {
    let id = GENSYM_COUNTER
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("scheme gensym counter exhausted");
    format!("__{prefix}_{id}")
}

pub(super) fn lower_to_ir(source: &str) -> Result<FrontendIr, ParseError> {
    lower_to_ir_with_import_context(source, None)
}

pub(super) fn lower_to_ir_with_import_context(
    source: &str,
    import_context: Option<&SchemeImportContext>,
) -> Result<FrontendIr, ParseError> {
    if let Some(ir) = try_lower_direct_subset_to_ir(source, import_context)? {
        return Ok(ir);
    }
    Err(ParseError::at_line(
        1,
        "scheme direct lowering does not yet support this construct",
    ))
}

fn try_lower_direct_subset_to_ir(
    source: &str,
    import_context: Option<&SchemeImportContext>,
) -> Result<Option<FrontendIr>, ParseError> {
    let mut parser = SchemeParser::new(source)?;
    let forms = parser.parse_program()?;
    let normalized_import_context = import_context.map(NormalizedSchemeImportContext::from_raw);

    let mut builder = LocalIrBuilder::new();
    if let Some(imports) = normalized_import_context.as_ref() {
        for (name, arity) in &imports.declared_functions {
            builder.declare_function(name, *arity)?;
        }
    }
    let mut stmts = Vec::<Stmt>::new();
    for form in &forms {
        let Some(mut lowered) =
            lower_scheme_direct_stmt(form, &mut builder, normalized_import_context.as_ref())?
        else {
            return Ok(None);
        };
        stmts.append(&mut lowered);
    }
    Ok(Some(builder.finish(stmts)))
}

fn lower_scheme_direct_stmt(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Vec<Stmt>>, ParseError> {
    if let Some(items) = form.as_list()
        && let Some(head) = items.first().and_then(|item| item.as_symbol())
    {
        let args = &items[1..];
        let line = u32::try_from(form.line).unwrap_or(u32::MAX);
        match head {
            "import" | "require" => {
                if args.len() == 1
                    && let Some(clause) = args[0].as_list()
                    && clause.len() >= 3
                    && clause[0]
                        .as_symbol()
                        .is_some_and(|keyword| keyword == "only-in")
                    && clause[1]
                        .as_symbol()
                        .or_else(|| match &clause[1].node {
                            SchemeNode::String(value) => Some(value.as_str()),
                            _ => None,
                        })
                        .is_some_and(|spec| spec == ROOT_HOST_NAMESPACE_SPEC)
                {
                    for member in &clause[2..] {
                        if let Some(member_raw) = member.as_symbol() {
                            let name =
                                normalize_identifier(member_raw, member.line, "require member")?;
                            builder.declare_function(&name, None)?;
                        }
                    }
                }
                // Source loader handles module import rewriting before direct lowering.
                return Ok(Some(Vec::new()));
            }
            "define" => {
                if args.len() < 2 {
                    return Ok(None);
                }
                if let Some(name_raw) = args[0].as_symbol() {
                    let name = normalize_identifier(name_raw, args[0].line, "define target")?;
                    let Some(expr) =
                        lower_scheme_direct_expr_top(&args[1], builder, import_context)?
                    else {
                        return Ok(None);
                    };
                    return Ok(Some(vec![builder.lower_local(&name, expr, line)?]));
                }

                let Some(signature) = args[0].as_list() else {
                    return Ok(None);
                };
                let Some(name_raw) = signature.first().and_then(|item| item.as_symbol()) else {
                    return Ok(None);
                };
                let name = normalize_identifier(name_raw, signature[0].line, "function name")?;
                let mut params = Vec::new();
                for param in &signature[1..] {
                    let Some(param_raw) = param.as_symbol() else {
                        return Ok(None);
                    };
                    params.push(normalize_identifier(
                        param_raw,
                        param.line,
                        "function parameter",
                    )?);
                }
                let Some(closure) = lower_scheme_direct_lambda_expr_from_params(
                    &params,
                    &args[1..],
                    form.line,
                    builder,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                return Ok(Some(vec![builder.lower_local(&name, closure, line)?]));
            }
            "set!" => {
                if args.len() != 2 {
                    return Ok(None);
                }
                let Some(name_raw) = args[0].as_symbol() else {
                    return Ok(None);
                };
                let name = normalize_identifier(name_raw, args[0].line, "set! target")?;
                let Some(expr) = lower_scheme_direct_expr_top(&args[1], builder, import_context)?
                else {
                    return Ok(None);
                };
                return Ok(Some(vec![builder.lower_assign(&name, expr, line)?]));
            }
            "declare" => {
                if args.len() != 1 {
                    return Ok(None);
                }
                let Some(signature) = args[0].as_list() else {
                    return Ok(None);
                };
                let Some(name_raw) = signature.first().and_then(|item| item.as_symbol()) else {
                    return Ok(None);
                };
                let Ok(name) = normalize_identifier(name_raw, args[0].line, "declare function")
                else {
                    return Ok(None);
                };
                let mut params = Vec::new();
                for param in &signature[1..] {
                    let Some(param_raw) = param.as_symbol() else {
                        return Ok(None);
                    };
                    let Ok(param_name) =
                        normalize_identifier(param_raw, param.line, "declare parameter")
                    else {
                        return Ok(None);
                    };
                    params.push(param_name);
                }
                let arity = u8::try_from(params.len()).map_err(|_| ParseError {
                    span: None,
                    code: None,
                    line: form.line,
                    message: format!("too many parameters in declaration '{name}'"),
                })?;
                builder.declare_function(&name, Some(arity))?;
                let index = builder.function_index(&name).ok_or_else(|| ParseError {
                    span: None,
                    code: None,
                    line: form.line,
                    message: format!("internal function index missing for '{name}'"),
                })?;
                return Ok(Some(vec![Stmt::FuncDecl {
                    name,
                    index,
                    arity,
                    args: params,
                    exported: false,
                    line,
                }]));
            }
            "if" => {
                if !(2..=3).contains(&args.len()) {
                    return Ok(None);
                }
                let Some(condition) =
                    lower_scheme_direct_expr_top(&args[0], builder, import_context)?
                else {
                    return Ok(None);
                };
                let Some(then_branch) =
                    lower_scheme_direct_branch(&args[1], builder, import_context)?
                else {
                    return Ok(None);
                };
                let else_branch = if args.len() == 3 {
                    let Some(branch) =
                        lower_scheme_direct_branch(&args[2], builder, import_context)?
                    else {
                        return Ok(None);
                    };
                    branch
                } else {
                    Vec::new()
                };
                return Ok(Some(vec![Stmt::IfElse {
                    condition,
                    then_branch,
                    else_branch,
                    line,
                }]));
            }
            "while" => {
                if args.is_empty() {
                    return Ok(None);
                }
                let Some(condition) =
                    lower_scheme_direct_expr_top(&args[0], builder, import_context)?
                else {
                    return Ok(None);
                };
                let mut body = Vec::new();
                for body_form in &args[1..] {
                    let Some(mut lowered) =
                        lower_scheme_direct_stmt(body_form, builder, import_context)?
                    else {
                        return Ok(None);
                    };
                    body.append(&mut lowered);
                }
                return Ok(Some(vec![Stmt::While {
                    condition,
                    body,
                    line,
                }]));
            }
            "for" => {
                if args.len() < 2 {
                    return Ok(None);
                }
                let Some(header) = args[0].as_list() else {
                    return Ok(None);
                };
                if header.len() < 3 || header.len() > 4 {
                    return Ok(None);
                }
                let Some(name_raw) = header[0].as_symbol() else {
                    return Ok(None);
                };
                let name = normalize_identifier(name_raw, header[0].line, "for loop variable")?;
                let Some(start) =
                    lower_scheme_direct_expr_top(&header[1], builder, import_context)?
                else {
                    return Ok(None);
                };
                let Some(end) = lower_scheme_direct_expr_top(&header[2], builder, import_context)?
                else {
                    return Ok(None);
                };
                let Some(step) = (if header.len() == 4 {
                    lower_scheme_direct_expr_top(&header[3], builder, import_context)?
                } else {
                    Some(Expr::Int(1))
                }) else {
                    return Ok(None);
                };

                let init = builder.lower_local(&name, start, line)?;
                let Some(Expr::Var(loop_slot)) = builder.resolve_local_expr(&name) else {
                    return Ok(None);
                };
                let post = builder.lower_assign(
                    &name,
                    Expr::Add(Box::new(Expr::Var(loop_slot)), Box::new(step)),
                    line,
                )?;

                let mut body = Vec::new();
                for body_form in &args[1..] {
                    let Some(mut lowered) =
                        lower_scheme_direct_stmt(body_form, builder, import_context)?
                    else {
                        return Ok(None);
                    };
                    body.append(&mut lowered);
                }
                return Ok(Some(vec![Stmt::For {
                    init: Box::new(init),
                    condition: Expr::Lt(Box::new(Expr::Var(loop_slot)), Box::new(end)),
                    post: Box::new(post),
                    body,
                    line,
                }]));
            }
            "begin" => {
                let mut out = Vec::new();
                for expr in args {
                    let Some(mut lowered) =
                        lower_scheme_direct_stmt(expr, builder, import_context)?
                    else {
                        return Ok(None);
                    };
                    out.append(&mut lowered);
                }
                return Ok(Some(out));
            }
            _ => {}
        }
    }

    let line = u32::try_from(form.line).unwrap_or(u32::MAX);
    let Some(expr) = lower_scheme_direct_expr_top(form, builder, import_context)? else {
        return Ok(None);
    };
    Ok(Some(vec![Stmt::Expr { expr, line }]))
}

fn lower_scheme_direct_branch(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Vec<Stmt>>, ParseError> {
    lower_scheme_direct_stmt(form, builder, import_context)
}

fn lower_scheme_direct_expr_top(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Expr>, ParseError> {
    let params = HashMap::new();
    let mut captures = HashMap::new();
    lower_scheme_direct_expr(form, builder, &params, &mut captures, false, import_context)
}

fn lower_scheme_direct_expr(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Expr>, ParseError> {
    match &form.node {
        SchemeNode::Int(value) => Ok(Some(Expr::Int(*value))),
        SchemeNode::Float(value) => Ok(Some(Expr::Float(*value))),
        SchemeNode::Bool(value) => Ok(Some(Expr::Bool(*value))),
        SchemeNode::Char(ch) => Ok(Some(Expr::Int(*ch as i64))),
        SchemeNode::String(value) => Ok(Some(Expr::String(value.clone()))),
        SchemeNode::Symbol(symbol) => lower_scheme_direct_symbol_expr(
            symbol,
            form.line,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
        ),
        SchemeNode::List(items) => lower_scheme_direct_list_expr(
            items,
            form.line,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
        ),
    }
}

fn lower_scheme_direct_symbol_expr(
    symbol: &str,
    line: usize,
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
) -> Result<Option<Expr>, ParseError> {
    if symbol == "null" || symbol == "nil" {
        return Ok(Some(Expr::Null));
    }
    if symbol == "true" {
        return Ok(Some(Expr::Bool(true)));
    }
    if symbol == "false" {
        return Ok(Some(Expr::Bool(false)));
    }
    if symbol.contains("?.") {
        return lower_scheme_direct_optional_chain_expr(
            symbol,
            line,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
        );
    }

    let name = normalize_identifier(symbol, line, "symbol")?;
    if let Some(slot) = param_slots.get(&name).copied() {
        return Ok(Some(Expr::Var(slot)));
    }
    if let Some(Expr::Var(source_slot)) = builder.resolve_local_expr(&name) {
        if !capture_enabled {
            return Ok(Some(Expr::Var(source_slot)));
        }
        if let Some(captured_slot) = capture_slots.get(&source_slot).copied() {
            return Ok(Some(Expr::Var(captured_slot)));
        }
        let captured_slot = builder.alloc_local_named(&gensym("scheme_capture_slot"))?;
        capture_slots.insert(source_slot, captured_slot);
        return Ok(Some(Expr::Var(captured_slot)));
    }
    Err(ParseError {
        span: None,
        code: None,
        line,
        message: format!("unknown local '{name}'"),
    })
}

fn lower_scheme_direct_optional_chain_expr(
    symbol: &str,
    line: usize,
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
) -> Result<Option<Expr>, ParseError> {
    let parts = symbol.split("?.").collect::<Vec<_>>();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("invalid optional chain symbol '{symbol}'"),
        });
    }
    let root = normalize_identifier(parts[0], line, "optional chain root")?;
    let mut expr = if let Some(slot) = param_slots.get(&root).copied() {
        Expr::Var(slot)
    } else if let Some(Expr::Var(source_slot)) = builder.resolve_local_expr(&root) {
        if !capture_enabled {
            Expr::Var(source_slot)
        } else if let Some(captured_slot) = capture_slots.get(&source_slot).copied() {
            Expr::Var(captured_slot)
        } else {
            let captured_slot = builder.alloc_local_named(&gensym("scheme_capture_slot"))?;
            capture_slots.insert(source_slot, captured_slot);
            Expr::Var(captured_slot)
        }
    } else {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("unknown local '{root}'"),
        });
    };

    for member in &parts[1..] {
        if !is_valid_member_ident(member) {
            return Err(ParseError {
                span: None,
                code: None,
                line,
                message: format!("invalid optional chain member '{member}' in '{symbol}'"),
            });
        }
        expr = build_scheme_optional_member_expr(expr, (*member).to_string(), line, builder)?;
    }
    Ok(Some(expr))
}

fn build_scheme_optional_member_expr(
    target: Expr,
    member: String,
    line: usize,
    builder: &mut LocalIrBuilder,
) -> Result<Expr, ParseError> {
    let line_u32 = u32::try_from(line).unwrap_or(u32::MAX);
    let target_slot = builder.alloc_local_named(&gensym("scheme_opt_target"))?;
    let result_slot = builder.alloc_local_named(&gensym("scheme_opt_result"))?;
    let keys_slot = builder.alloc_local_named(&gensym("scheme_opt_keys"))?;
    let idx_slot = builder.alloc_local_named(&gensym("scheme_opt_idx"))?;
    let found_slot = builder.alloc_local_named(&gensym("scheme_opt_found"))?;

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

    Ok(Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: target_slot,
                expr: target,
                line: line_u32,
            },
            Stmt::Let {
                index: result_slot,
                expr: Expr::Null,
                line: line_u32,
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
                        line: line_u32,
                    },
                    Stmt::Let {
                        index: idx_slot,
                        expr: Expr::Int(0),
                        line: line_u32,
                    },
                    Stmt::Let {
                        index: found_slot,
                        expr: Expr::Bool(false),
                        line: line_u32,
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
                                    line: line_u32,
                                },
                                Stmt::Assign {
                                    index: idx_slot,
                                    expr: keys_len_expr(),
                                    line: line_u32,
                                },
                            ],
                            else_branch: vec![Stmt::Assign {
                                index: idx_slot,
                                expr: Expr::Add(
                                    Box::new(Expr::Var(idx_slot)),
                                    Box::new(Expr::Int(1)),
                                ),
                                line: line_u32,
                            }],
                            line: line_u32,
                        }],
                        line: line_u32,
                    },
                    Stmt::IfElse {
                        condition: Expr::Var(found_slot),
                        then_branch: vec![Stmt::Assign {
                            index: result_slot,
                            expr: Expr::Call(
                                BuiltinFunction::Get.call_index(),
                                vec![Expr::Var(target_slot), Expr::String(member)],
                            ),
                            line: line_u32,
                        }],
                        else_branch: Vec::new(),
                        line: line_u32,
                    },
                ],
                else_branch: Vec::new(),
                line: line_u32,
            },
        ],
        expr: Box::new(Expr::Var(result_slot)),
    })
}

fn lower_scheme_direct_list_expr(
    items: &[SchemeForm],
    line: usize,
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Expr>, ParseError> {
    let Some(head) = items.first().and_then(|item| item.as_symbol()) else {
        return Ok(None);
    };
    let args = &items[1..];

    match head {
        "+" => lower_scheme_direct_fold(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            SchemeFoldOps {
                build: Expr::Add,
                eval_int: fold_int_add,
            },
        ),
        "*" => lower_scheme_direct_fold(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            SchemeFoldOps {
                build: Expr::Mul,
                eval_int: fold_int_mul,
            },
        ),
        "-" => {
            if args.is_empty() {
                return Ok(None);
            }
            if args.len() == 1 {
                let Some(inner) = lower_scheme_direct_expr(
                    &args[0],
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                return Ok(Some(Expr::Neg(Box::new(inner))));
            }
            lower_scheme_direct_fold(
                args,
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
                SchemeFoldOps {
                    build: Expr::Sub,
                    eval_int: fold_int_sub,
                },
            )
        }
        "/" => lower_scheme_direct_fold(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            SchemeFoldOps {
                build: Expr::Div,
                eval_int: fold_int_div,
            },
        ),
        "modulo" | "remainder" => lower_scheme_direct_binary(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            Expr::Mod,
        ),
        "=" => lower_scheme_direct_compare_fold(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            Expr::Eq,
        ),
        "/=" => {
            let Some(eq) = lower_scheme_direct_compare_fold(
                args,
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
                Expr::Eq,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(Expr::Not(Box::new(eq))))
        }
        "<" => lower_scheme_direct_compare_fold(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            Expr::Lt,
        ),
        ">" => lower_scheme_direct_compare_fold(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            Expr::Gt,
        ),
        "<=" => lower_scheme_non_strict_compare(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            Expr::Lt,
        ),
        ">=" => lower_scheme_non_strict_compare(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
            Expr::Gt,
        ),
        "and" => {
            if args.is_empty() {
                return Ok(Some(Expr::Bool(true)));
            }
            let mut it = args.iter();
            let Some(first_form) = it.next() else {
                return Ok(Some(Expr::Bool(true)));
            };
            let Some(mut expr) = lower_scheme_direct_expr(
                first_form,
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            for arg in it {
                let Some(rhs) = lower_scheme_direct_expr(
                    arg,
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                expr = Expr::And(Box::new(expr), Box::new(rhs));
            }
            Ok(Some(expr))
        }
        "or" => {
            if args.is_empty() {
                return Ok(Some(Expr::Bool(false)));
            }
            let mut it = args.iter();
            let Some(first_form) = it.next() else {
                return Ok(Some(Expr::Bool(false)));
            };
            let Some(mut expr) = lower_scheme_direct_expr(
                first_form,
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            for arg in it {
                let Some(rhs) = lower_scheme_direct_expr(
                    arg,
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                expr = Expr::Or(Box::new(expr), Box::new(rhs));
            }
            Ok(Some(expr))
        }
        "not" => {
            if args.len() != 1 {
                return Ok(None);
            }
            let Some(inner) = lower_scheme_direct_expr(
                &args[0],
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(Expr::Not(Box::new(inner))))
        }
        "if" => {
            if !(2..=3).contains(&args.len()) {
                return Ok(None);
            }
            let Some(condition) = lower_scheme_direct_expr(
                &args[0],
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            let Some(then_expr) = lower_scheme_direct_expr(
                &args[1],
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            let else_expr = if args.len() == 3 {
                let Some(expr) = lower_scheme_direct_expr(
                    &args[2],
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                expr
            } else {
                Expr::Bool(false)
            };
            Ok(Some(Expr::IfElse {
                condition: Box::new(condition),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
            }))
        }
        "list" | "vector" => {
            let mut out = Expr::Call(BuiltinFunction::ArrayNew.call_index(), Vec::new());
            for arg in args {
                let Some(value) = lower_scheme_direct_expr(
                    arg,
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                out = Expr::Call(BuiltinFunction::ArrayPush.call_index(), vec![out, value]);
            }
            Ok(Some(out))
        }
        "print" => {
            let mut lowered_args = Vec::with_capacity(args.len());
            for arg in args {
                let Some(lowered) = lower_scheme_direct_expr(
                    arg,
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                lowered_args.push(lowered);
            }
            builder.declare_function("print", None)?;
            let Some(expr) = builder.resolve_call_expr("print", lowered_args) else {
                return Ok(None);
            };
            Ok(Some(expr))
        }
        "hash" => lower_scheme_direct_hash_expr(
            args,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
        ),
        "hash-ref" => {
            if args.len() != 2 {
                return Ok(None);
            }
            let Some(container) = lower_scheme_direct_expr(
                &args[0],
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            let Some(key) = lower_scheme_direct_expr(
                &args[1],
                builder,
                param_slots,
                capture_slots,
                capture_enabled,
                import_context,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(Expr::Call(
                BuiltinFunction::Get.call_index(),
                vec![container, key],
            )))
        }
        "lambda" => {
            if args.len() < 2 {
                return Ok(None);
            }
            let Some(params_list) = args[0].as_list() else {
                return Ok(None);
            };
            let mut params = Vec::new();
            for param in params_list {
                let Some(param_raw) = param.as_symbol() else {
                    return Ok(None);
                };
                params.push(normalize_identifier(
                    param_raw,
                    param.line,
                    "lambda parameter",
                )?);
            }
            lower_scheme_direct_lambda_expr_from_params(
                &params,
                &args[1..],
                line,
                builder,
                import_context,
            )
        }
        _ => {
            if is_forbidden_scheme_builtin_name(head) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line,
                    message: format!(
                        "direct builtin call '{head}' is not exposed in Scheme frontend; {}",
                        scheme_builtin_syntax_hint(head)
                    ),
                });
            }

            let mut lowered_args = Vec::with_capacity(args.len());
            for arg in args {
                let Some(lowered) = lower_scheme_direct_expr(
                    arg,
                    builder,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    import_context,
                )?
                else {
                    return Ok(None);
                };
                lowered_args.push(lowered);
            }

            match resolve_scheme_call_head(head, line, import_context)? {
                SchemeResolvedCallHead::Direct(name) => {
                    if let Some(expr) = builder.resolve_call_expr(&name, lowered_args) {
                        return Ok(Some(expr));
                    }
                    Err(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: format!("unknown function '{name}'"),
                    })
                }
                SchemeResolvedCallHead::Namespace(segments) => {
                    lower_scheme_direct_namespace_call(&segments, lowered_args, builder, line)
                        .map(Some)
                }
            }
        }
    }
}

fn resolve_scheme_call_head(
    head: &str,
    line: usize,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<SchemeResolvedCallHead, ParseError> {
    if let Some(target) = import_context.and_then(|imports| imports.resolve_direct_alias(head)) {
        return parse_resolved_scheme_call_target(target, line);
    }

    if let Some(segments) = split_namespace_segments(head, line)? {
        if let Some(rewritten) =
            import_context.and_then(|imports| imports.resolve_namespace_import(&segments))
        {
            return Ok(match rewritten.as_slice() {
                [name] => SchemeResolvedCallHead::Direct(name.clone()),
                _ => SchemeResolvedCallHead::Namespace(rewritten),
            });
        }
        return Ok(SchemeResolvedCallHead::Namespace(segments));
    }

    Ok(SchemeResolvedCallHead::Direct(normalize_identifier(
        head,
        line,
        "function call",
    )?))
}

fn parse_resolved_scheme_call_target(
    target: &str,
    line: usize,
) -> Result<SchemeResolvedCallHead, ParseError> {
    if let Some(segments) = split_namespace_segments(target, line)? {
        return Ok(SchemeResolvedCallHead::Namespace(segments));
    }
    Ok(SchemeResolvedCallHead::Direct(normalize_identifier(
        target,
        line,
        "imported function",
    )?))
}

fn lower_scheme_direct_namespace_call(
    segments: &[String],
    args: Vec<Expr>,
    builder: &mut LocalIrBuilder,
    line: usize,
) -> Result<Expr, ParseError> {
    if segments.len() < 2 {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: "invalid namespaced call".to_string(),
        });
    }
    let root = segments[0].as_str();
    let member = segments[1].as_str();

    if root == ROOT_HOST_NAMESPACE_SPEC && segments.len() >= 3 {
        let namespace = member;
        let host_member = segments[2].as_str();
        if is_builtin_namespace(namespace)
            && let Some(expr) = lower_scheme_direct_regex_or_builtin_call(
                namespace,
                host_member,
                args.clone(),
                line,
            )?
        {
            return Ok(expr);
        }

        let call_name = segments[1..].join("::");
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line,
            message: "too many call arguments".to_string(),
        })?;
        builder.declare_function(&call_name, Some(arity))?;
        return builder
            .resolve_call_expr(&call_name, args)
            .ok_or_else(|| ParseError {
                span: None,
                code: None,
                line,
                message: format!("unknown function '{call_name}'"),
            });
    }

    if !is_builtin_namespace(root) && segments.len() >= 2 {
        let call_name = segments.join("::");
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line,
            message: "too many call arguments".to_string(),
        })?;
        builder.declare_function(&call_name, Some(arity))?;
        return builder
            .resolve_call_expr(&call_name, args)
            .ok_or_else(|| ParseError {
                span: None,
                code: None,
                line,
                message: format!("unknown function '{call_name}'"),
            });
    }

    if segments.len() == 2 {
        if let Some(expr) =
            lower_scheme_direct_regex_or_builtin_call(root, member, args.clone(), line)?
        {
            return Ok(expr);
        }
        if let Some(expr) = builder.resolve_call_expr(member, args) {
            return Ok(expr);
        }
    }

    Err(ParseError {
        span: None,
        code: None,
        line,
        message: format!("unknown namespace call '{}'", segments.join("::")),
    })
}

fn lower_scheme_direct_regex_or_builtin_call(
    namespace: &str,
    member: &str,
    mut args: Vec<Expr>,
    line: usize,
) -> Result<Option<Expr>, ParseError> {
    if namespace == "re" {
        let (builtin, base_arity) = match member {
            "match" | "is_match" => (BuiltinFunction::ReIsMatch, 2usize),
            "find" => (BuiltinFunction::ReFind, 2usize),
            "replace" => (BuiltinFunction::ReReplace, 3usize),
            "split" => (BuiltinFunction::ReSplit, 2usize),
            "captures" => (BuiltinFunction::ReCaptures, 2usize),
            _ => return Ok(None),
        };
        if args.len() == base_arity {
            return Ok(Some(Expr::Call(builtin.call_index(), args)));
        }
        if args.len() == base_arity + 1 {
            let flags = args.pop().ok_or_else(|| ParseError {
                span: None,
                code: None,
                line,
                message: "missing regex flags argument".to_string(),
            })?;
            let pattern = args.first().cloned().ok_or_else(|| ParseError {
                span: None,
                code: None,
                line,
                message: "missing regex pattern argument".to_string(),
            })?;
            args[0] = apply_regex_flags_to_pattern_expr_direct(pattern, flags);
            return Ok(Some(Expr::Call(builtin.call_index(), args)));
        }
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!(
                "function 're::{member}' expects {base_arity} or {} arguments",
                base_arity + 1
            ),
        });
    }

    if let Some(builtin) = resolve_builtin_namespace_call(namespace, member) {
        let expected = usize::from(builtin.arity());
        if args.len() != expected {
            return Err(ParseError {
                span: None,
                code: None,
                line,
                message: format!("function '{namespace}::{member}' expects {expected} arguments"),
            });
        }
        return Ok(Some(Expr::Call(builtin.call_index(), args)));
    }
    Ok(None)
}

fn apply_regex_flags_to_pattern_expr_direct(pattern: Expr, flags: Expr) -> Expr {
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

fn lower_scheme_direct_hash_expr(
    args: &[SchemeForm],
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Expr>, ParseError> {
    let mut expr = Expr::Call(BuiltinFunction::MapNew.call_index(), Vec::new());
    for entry in args {
        let Some(pair) = entry.as_list() else {
            return Ok(None);
        };
        if pair.len() != 2 {
            return Ok(None);
        }
        let Some(key) = lower_scheme_direct_hash_key_expr(
            &pair[0],
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
        )?
        else {
            return Ok(None);
        };
        let Some(value) = lower_scheme_direct_expr(
            &pair[1],
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
        )?
        else {
            return Ok(None);
        };
        expr = Expr::Call(BuiltinFunction::Set.call_index(), vec![expr, key, value]);
    }
    Ok(Some(expr))
}

fn lower_scheme_direct_hash_key_expr(
    form: &SchemeForm,
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Expr>, ParseError> {
    if let Some(symbol) = form.as_symbol() {
        return Ok(Some(Expr::String(symbol.to_string())));
    }
    lower_scheme_direct_expr(
        form,
        builder,
        param_slots,
        capture_slots,
        capture_enabled,
        import_context,
    )
}

fn lower_scheme_direct_lambda_expr_from_params(
    params: &[String],
    body_forms: &[SchemeForm],
    _line: usize,
    builder: &mut LocalIrBuilder,
    import_context: Option<&NormalizedSchemeImportContext>,
) -> Result<Option<Expr>, ParseError> {
    if body_forms.len() != 1 {
        return Ok(None);
    }

    let mut param_lookup = HashMap::new();
    let mut param_slots = Vec::new();
    for _param in params {
        let slot = builder.alloc_local_named(&gensym("scheme_lambda_param"))?;
        param_slots.push(slot);
    }
    for (param, slot) in params.iter().zip(param_slots.iter().copied()) {
        param_lookup.insert(param.clone(), slot);
    }

    let mut captures = HashMap::new();
    let Some(body_expr) = lower_scheme_direct_expr(
        &body_forms[0],
        builder,
        &param_lookup,
        &mut captures,
        true,
        import_context,
    )?
    else {
        return Ok(None);
    };

    let mut capture_copies = captures.into_iter().collect::<Vec<_>>();
    capture_copies.sort_by_key(|(source_slot, _)| *source_slot);

    Ok(Some(Expr::Closure(super::super::ir::ClosureExpr {
        param_slots,
        capture_copies,
        body: Box::new(body_expr),
    })))
}

fn lower_scheme_direct_binary<F>(
    args: &[SchemeForm],
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
    build: F,
) -> Result<Option<Expr>, ParseError>
where
    F: Fn(Box<Expr>, Box<Expr>) -> Expr,
{
    if args.len() != 2 {
        return Ok(None);
    }
    let Some(lhs) = lower_scheme_direct_expr(
        &args[0],
        builder,
        param_slots,
        capture_slots,
        capture_enabled,
        import_context,
    )?
    else {
        return Ok(None);
    };
    let Some(rhs) = lower_scheme_direct_expr(
        &args[1],
        builder,
        param_slots,
        capture_slots,
        capture_enabled,
        import_context,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(build(Box::new(lhs), Box::new(rhs))))
}

struct SchemeFoldOps<F> {
    build: F,
    eval_int: fn(i64, i64) -> Option<i64>,
}

fn lower_scheme_direct_fold<F>(
    args: &[SchemeForm],
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
    ops: SchemeFoldOps<F>,
) -> Result<Option<Expr>, ParseError>
where
    F: Fn(Box<Expr>, Box<Expr>) -> Expr + Copy,
{
    if args.len() < 2 {
        return Ok(None);
    }

    let mut lowered_exprs = Vec::with_capacity(args.len());
    let mut int_values = Vec::with_capacity(args.len());
    let mut all_int_values = true;

    for arg in args {
        let Some(expr) = lower_scheme_direct_expr(
            arg,
            builder,
            param_slots,
            capture_slots,
            capture_enabled,
            import_context,
        )?
        else {
            return Ok(None);
        };
        if let Expr::Int(value) = &expr {
            int_values.push(*value);
        } else {
            all_int_values = false;
        }
        lowered_exprs.push(expr);
    }

    if all_int_values {
        let mut iter = int_values.into_iter();
        let Some(mut acc) = iter.next() else {
            return Ok(None);
        };
        let mut foldable = true;
        for rhs in iter {
            let Some(next) = (ops.eval_int)(acc, rhs) else {
                foldable = false;
                break;
            };
            acc = next;
        }
        if foldable {
            return Ok(Some(Expr::Int(acc)));
        }
    }

    let mut iter = lowered_exprs.into_iter();
    let Some(mut expr) = iter.next() else {
        return Ok(None);
    };
    for rhs in iter {
        expr = (ops.build)(Box::new(expr), Box::new(rhs));
    }
    Ok(Some(expr))
}

fn lower_scheme_direct_compare_fold<F>(
    args: &[SchemeForm],
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
    build: F,
) -> Result<Option<Expr>, ParseError>
where
    F: Fn(Box<Expr>, Box<Expr>) -> Expr + Copy,
{
    if args.len() != 2 {
        return Ok(None);
    }
    lower_scheme_direct_binary(
        args,
        builder,
        param_slots,
        capture_slots,
        capture_enabled,
        import_context,
        build,
    )
}

fn lower_scheme_non_strict_compare(
    args: &[SchemeForm],
    builder: &mut LocalIrBuilder,
    param_slots: &HashMap<String, u16>,
    capture_slots: &mut HashMap<u16, u16>,
    capture_enabled: bool,
    import_context: Option<&NormalizedSchemeImportContext>,
    build_strict: fn(Box<Expr>, Box<Expr>) -> Expr,
) -> Result<Option<Expr>, ParseError> {
    if args.len() != 2 {
        return Ok(None);
    }
    let Some(lhs) = lower_scheme_direct_expr(
        &args[0],
        builder,
        param_slots,
        capture_slots,
        capture_enabled,
        import_context,
    )?
    else {
        return Ok(None);
    };
    let Some(rhs) = lower_scheme_direct_expr(
        &args[1],
        builder,
        param_slots,
        capture_slots,
        capture_enabled,
        import_context,
    )?
    else {
        return Ok(None);
    };

    let lhs_slot = builder.alloc_local_named(&gensym("scheme_cmp_lhs"))?;
    let rhs_slot = builder.alloc_local_named(&gensym("scheme_cmp_rhs"))?;
    let lhs_var = Expr::Var(lhs_slot);
    let rhs_var = Expr::Var(rhs_slot);
    Ok(Some(Expr::Block {
        stmts: vec![
            Stmt::Let {
                index: lhs_slot,
                expr: lhs,
                line: 1,
            },
            Stmt::Let {
                index: rhs_slot,
                expr: rhs,
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

#[derive(Clone, Debug)]
struct SchemeForm {
    line: usize,
    node: SchemeNode,
}

impl SchemeForm {
    fn as_symbol(&self) -> Option<&str> {
        match &self.node {
            SchemeNode::Symbol(value) => Some(value),
            _ => None,
        }
    }

    fn as_list(&self) -> Option<&[SchemeForm]> {
        match &self.node {
            SchemeNode::List(values) => Some(values),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum SchemeNode {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    String(String),
    Symbol(String),
    List(Vec<SchemeForm>),
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    LParen,
    RParen,
    Quote,
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    String(String),
    Symbol(String),
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    line: usize,
}

struct SchemeLexer<'a> {
    chars: std::str::Chars<'a>,
    current: Option<char>,
    line: usize,
}

impl<'a> SchemeLexer<'a> {
    fn new(source: &'a str) -> Self {
        let mut chars = source.chars();
        let current = chars.next();
        Self {
            chars,
            current,
            line: 1,
        }
    }

    fn next_token(&mut self) -> Result<Token, ParseError> {
        self.skip_whitespace_and_comments();
        let line = self.line;

        let token = match self.current {
            None => TokenKind::Eof,
            Some('(') => {
                self.advance();
                TokenKind::LParen
            }
            Some(')') => {
                self.advance();
                TokenKind::RParen
            }
            Some('\'') => {
                self.advance();
                TokenKind::Quote
            }
            Some('"') => TokenKind::String(self.consume_string()?),
            Some(_) => {
                let atom = self.consume_atom();
                self.classify_atom(atom, line)?
            }
        };

        Ok(Token { kind: token, line })
    }

    fn advance(&mut self) {
        if self.current == Some('\n') {
            self.line += 1;
        }
        self.current = self.chars.next();
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            while matches!(self.current, Some(ch) if ch.is_whitespace()) {
                self.advance();
            }

            // Line comment: ; ... newline
            if self.current == Some(';') {
                while let Some(ch) = self.current {
                    self.advance();
                    if ch == '\n' {
                        break;
                    }
                }
                continue;
            }

            // Block comment: #| ... |#
            if self.current == Some('#') {
                let saved = self.chars.clone();
                let saved_line = self.line;
                self.advance();
                if self.current == Some('|') {
                    self.advance();
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.current {
                            None => break,
                            Some('#') => {
                                self.advance();
                                if self.current == Some('|') {
                                    self.advance();
                                    depth += 1;
                                }
                            }
                            Some('|') => {
                                self.advance();
                                if self.current == Some('#') {
                                    self.advance();
                                    depth -= 1;
                                }
                            }
                            _ => self.advance(),
                        }
                    }
                    continue;
                } else {
                    // Not a block comment, restore state
                    self.chars = saved;
                    self.line = saved_line;
                    self.current = Some('#');
                }
            }

            break;
        }
    }

    fn consume_string(&mut self) -> Result<String, ParseError> {
        let line = self.line;
        self.advance();

        let mut out = String::new();
        loop {
            let Some(ch) = self.current else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line,
                    message: "unterminated string literal".to_string(),
                });
            };

            match ch {
                '"' => {
                    self.advance();
                    break;
                }
                '\\' => {
                    self.advance();
                    let Some(escaped) = self.current else {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line,
                            message: "unterminated string escape".to_string(),
                        });
                    };
                    let mapped = match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '"' => '"',
                        '0' => '\0',
                        other => {
                            return Err(ParseError {
                                span: None,
                                code: None,
                                line,
                                message: format!("invalid escape '\\{other}'"),
                            });
                        }
                    };
                    out.push(mapped);
                    self.advance();
                }
                other => {
                    out.push(other);
                    self.advance();
                }
            }
        }

        Ok(out)
    }

    fn consume_atom(&mut self) -> String {
        let mut out = String::new();
        while let Some(ch) = self.current {
            if is_scheme_delimiter(ch) {
                break;
            }
            out.push(ch);
            self.advance();
        }
        out
    }

    fn classify_atom(&self, atom: String, line: usize) -> Result<TokenKind, ParseError> {
        if atom.is_empty() {
            return Err(ParseError {
                span: None,
                code: None,
                line,
                message: "expected token".to_string(),
            });
        }

        if atom == "#t" || atom == "#true" {
            return Ok(TokenKind::Bool(true));
        }
        if atom == "#f" || atom == "#false" {
            return Ok(TokenKind::Bool(false));
        }

        // Character literals: #\a, #\space, #\newline, #\tab
        if let Some(rest) = atom.strip_prefix("#\\") {
            let ch = match rest {
                "space" => ' ',
                "newline" => '\n',
                "tab" => '\t',
                "return" => '\r',
                "nul" | "null" => '\0',
                s if s.chars().count() == 1 => s.chars().next().unwrap(),
                _ => {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line,
                        message: format!("unknown character literal '#\\{rest}'"),
                    });
                }
            };
            return Ok(TokenKind::Char(ch));
        }

        if let Some(kind) = parse_number_atom(&atom) {
            return Ok(kind);
        }

        Ok(TokenKind::Symbol(atom))
    }
}

fn is_scheme_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '(' | ')' | ';' | '\'' | '"')
}

fn parse_number_atom(atom: &str) -> Option<TokenKind> {
    if atom.is_empty() {
        return None;
    }

    let body = atom.strip_prefix('-').unwrap_or(atom);
    if body.is_empty() || !body.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }

    // Check for float
    if body.contains('.') {
        return atom.parse::<f64>().ok().map(TokenKind::Float);
    }

    if body.chars().all(|ch| ch.is_ascii_digit()) {
        return atom.parse::<i64>().ok().map(TokenKind::Int);
    }

    None
}

struct SchemeParser {
    tokens: Vec<Token>,
    pos: usize,
}

impl SchemeParser {
    fn new(source: &str) -> Result<Self, ParseError> {
        let mut lexer = SchemeLexer::new(source);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token()?;
            let is_eof = matches!(token.kind, TokenKind::Eof);
            tokens.push(token);
            if is_eof {
                break;
            }
        }

        Ok(Self { tokens, pos: 0 })
    }

    fn parse_program(&mut self) -> Result<Vec<SchemeForm>, ParseError> {
        let mut forms = Vec::new();
        while !self.check_eof() {
            forms.push(self.parse_form()?);
        }
        Ok(forms)
    }

    fn parse_form(&mut self) -> Result<SchemeForm, ParseError> {
        // Handle #; datum comment: skip one entire form
        while self.check_datum_comment() {
            self.advance(); // skip the #; symbol token
            if self.check_eof() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current().line,
                    message: "expected form after #;".to_string(),
                });
            }
            self.parse_form()?; // parse and discard
        }

        let token = self.advance().clone();
        match token.kind {
            TokenKind::LParen => self.parse_list(token.line),
            TokenKind::RParen => Err(ParseError {
                span: None,
                code: None,
                line: token.line,
                message: "unexpected ')'".to_string(),
            }),
            TokenKind::Quote => {
                let inner = self.parse_form()?;
                Ok(SchemeForm {
                    line: token.line,
                    node: SchemeNode::List(vec![
                        SchemeForm {
                            line: token.line,
                            node: SchemeNode::Symbol("quote".to_string()),
                        },
                        inner,
                    ]),
                })
            }
            TokenKind::Int(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Int(value),
            }),
            TokenKind::Float(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Float(value),
            }),
            TokenKind::Bool(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Bool(value),
            }),
            TokenKind::Char(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Char(value),
            }),
            TokenKind::String(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::String(value),
            }),
            TokenKind::Symbol(value) => Ok(SchemeForm {
                line: token.line,
                node: SchemeNode::Symbol(value),
            }),
            TokenKind::Eof => Err(ParseError {
                span: None,
                code: None,
                line: token.line,
                message: "unexpected end of input".to_string(),
            }),
        }
    }

    fn check_datum_comment(&self) -> bool {
        matches!(&self.current().kind, TokenKind::Symbol(s) if s == "#;")
    }

    fn parse_list(&mut self, line: usize) -> Result<SchemeForm, ParseError> {
        let mut items = Vec::new();
        while !self.check_rparen() {
            if self.check_eof() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line,
                    message: "unterminated list".to_string(),
                });
            }
            items.push(self.parse_form()?);
        }

        let _ = self.advance();
        Ok(SchemeForm {
            line,
            node: SchemeNode::List(items),
        })
    }

    fn check_rparen(&self) -> bool {
        matches!(self.current().kind, TokenKind::RParen)
    }

    fn check_eof(&self) -> bool {
        matches!(self.current().kind, TokenKind::Eof)
    }

    fn current(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or_else(|| {
            self.tokens
                .last()
                .expect("scheme parser token stream is never empty")
        })
    }

    fn advance(&mut self) -> &Token {
        let idx = self.pos;
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        self.tokens.get(idx).unwrap_or_else(|| {
            self.tokens
                .last()
                .expect("scheme parser token stream is never empty")
        })
    }
}

fn is_valid_member_ident(member: &str) -> bool {
    let mut chars = member.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue)
}

fn canonicalize_call_path(path: &str) -> Option<Vec<String>> {
    let canonical = path.replace("::", ".").replace(':', ".");
    if !canonical.contains('.') {
        return canonicalize_identifier(path).map(|value| vec![value]);
    }

    let mut segments = Vec::new();
    for segment in canonical.split('.') {
        if segment.is_empty() {
            return None;
        }
        segments.push(canonicalize_identifier(segment)?);
    }
    if segments.len() < 2 {
        return None;
    }
    Some(segments)
}

fn split_namespace_segments(head: &str, line: usize) -> Result<Option<Vec<String>>, ParseError> {
    if !head.replace("::", ".").replace(':', ".").contains('.') {
        return Ok(None);
    }
    canonicalize_call_path(head)
        .map(Some)
        .ok_or_else(|| ParseError {
            span: None,
            code: None,
            line,
            message: format!("invalid namespace call target '{head}'"),
        })
}

fn is_forbidden_scheme_builtin_name(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "slice"
            | "concat"
            | "array_new"
            | "array_push"
            | "map_new"
            | "get"
            | "set"
            | "count"
            | "__to_string"
            | "type_of"
            | "io_open"
            | "io_popen"
            | "io_read_all"
            | "io_read_line"
            | "io_write"
            | "io_flush"
            | "io_close"
            | "io_exists"
            | "re_is_match"
            | "re_find"
            | "re_replace"
            | "re_split"
            | "re_captures"
    )
}

fn scheme_builtin_syntax_hint(name: &str) -> &'static str {
    match name {
        "len" | "count" => "use (length value)",
        "type_of" => "use (type value) or (type-of value)",
        "get" => "use (vector-ref v i) or (hash-ref m k)",
        "set" => "use (vector-set! v i x) or (hash-set! m k x)",
        "concat" => "use (+ a b) for strings or (append xs ys) for lists",
        "slice" => "use (slice-range ...), (slice-to ...), or (slice-from ...)",
        "io_open" | "io_popen" | "io_read_all" | "io_read_line" | "io_write" | "io_flush"
        | "io_close" | "io_exists" => "use io namespace syntax (for example io::open)",
        "re_is_match" | "re_find" | "re_replace" | "re_split" | "re_captures" => {
            "use re namespace syntax (for example re::match with optional flags arg)"
        }
        _ => "use Scheme frontend forms instead of VM builtin helpers",
    }
}

fn fold_int_add(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_add(rhs))
}

#[inline]
fn fold_int_sub(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_sub(rhs))
}

#[inline]
fn fold_int_mul(lhs: i64, rhs: i64) -> Option<i64> {
    Some(lhs.wrapping_mul(rhs))
}

#[inline]
fn fold_int_div(lhs: i64, rhs: i64) -> Option<i64> {
    if rhs == 0 || (lhs == i64::MIN && rhs == -1) {
        return None;
    }
    Some(lhs / rhs)
}

fn normalize_identifier(name: &str, line: usize, context: &str) -> Result<String, ParseError> {
    if name.is_empty() {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("{context} cannot be empty"),
        });
    }

    let Some(out) = canonicalize_identifier(name) else {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("unsupported identifier '{name}' in {context}"),
        });
    };

    if is_reserved_identifier(&out) {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: format!("identifier '{name}' is reserved"),
        });
    }

    Ok(out)
}

fn canonicalize_identifier(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }

    let mut out = String::new();
    for ch in name.chars() {
        let mapped = if ch == '-' { '_' } else { ch };
        out.push(mapped);
    }

    let mut chars = out.chars();
    let first = chars.next()?;
    if !is_ident_start(first) || !chars.all(is_ident_continue) {
        return None;
    }
    Some(out)
}

fn is_reserved_identifier(name: &str) -> bool {
    matches!(
        name,
        "fn" | "let" | "for" | "if" | "else" | "while" | "break" | "continue" | "true" | "false"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_example_complex_parses() {
        let source = include_str!("../../../examples/example_complex.scm");
        let mut parser = SchemeParser::new(source).expect("scheme parser should initialize");
        let forms = parser
            .parse_program()
            .expect("scheme source should parse into forms");
        assert!(!forms.is_empty(), "scheme source should not be empty");
    }
}
