use super::super::ParseError;
use super::super::ir::{Expr, FrontendIr, LocalIrBuilder, LocalSlot, Stmt};
use super::{is_ident_continue, is_ident_start};
use crate::builtins::{BuiltinFunction, is_builtin_namespace, resolve_builtin_namespace_call};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

static LUA_DIRECT_TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn fresh_lua_direct_temp(prefix: &str) -> String {
    let id = LUA_DIRECT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__lua_direct_{prefix}_{id}")
}

#[derive(Clone, Debug)]
struct LuaLoweredExpr {
    expr: Expr,
    unpack_arity: usize,
    callable_return_arity: Option<usize>,
}

impl LuaLoweredExpr {
    fn scalar(expr: Expr) -> Self {
        Self {
            expr,
            unpack_arity: 1,
            callable_return_arity: None,
        }
    }

    fn callable(expr: Expr, return_arity: usize) -> Self {
        Self {
            expr,
            unpack_arity: 1,
            callable_return_arity: Some(return_arity.max(1)),
        }
    }

    fn scalarized(self) -> Self {
        if self.unpack_arity <= 1 {
            return self;
        }
        Self {
            expr: build_lua_unpack_get_expr(self.expr, 0),
            unpack_arity: 1,
            callable_return_arity: None,
        }
    }
}

pub(super) fn lower_to_ir(source: &str) -> Result<FrontendIr, ParseError> {
    if let Some(ir) = try_lower_direct_subset_to_ir(source)? {
        return Ok(ir);
    }
    Err(ParseError::at_line(
        1,
        "lua direct lowering does not yet support this construct",
    ))
}

fn try_lower_direct_subset_to_ir(source: &str) -> Result<Option<FrontendIr>, ParseError> {
    let cleaned_source = remove_lua_comments(source)?;
    let mut builder = LocalIrBuilder::new();
    let mut root_stmts = Vec::<Stmt>::new();
    let mut block_stack = Vec::<LuaDirectBlock>::new();
    let mut namespace_aliases = HashMap::<String, String>::new();
    let mut callable_return_arities = HashMap::<LocalSlot, usize>::new();

    for (index, raw_line) in cleaned_source.lines().enumerate() {
        let line_no = index + 1;
        let line_u32 = u32::try_from(line_no).unwrap_or(u32::MAX);
        let trimmed = raw_line.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some((name, params)) = parse_lua_pub_fn_declaration(trimmed) {
            builder
                .declare_function(&name, Some(u8::try_from(params.len()).unwrap_or(u8::MAX)))
                .ok();
            continue;
        }

        if let Some((name, rhs)) = parse_lua_local_assignment(trimmed)
            && let Some((spec, remainder)) = parse_lua_require_call(rhs)
        {
            if spec == "vm" {
                if let Some(member) = remainder.strip_prefix('.') {
                    let member = member.trim();
                    if is_valid_lua_ident(member) {
                        builder.declare_function(member, None).ok();
                        continue;
                    }
                }
                if remainder.is_empty() && is_valid_lua_ident(name) {
                    namespace_aliases.insert(name.to_string(), "vm".to_string());
                    continue;
                }
            }
            if spec == "io" || spec == "re" || spec == "json" {
                namespace_aliases.insert(name.to_string(), spec);
                continue;
            }
            // Module require lines are import directives handled by source loader rewrites/preludes.
            continue;
        }

        if let Some((spec, remainder)) = parse_lua_require_call(trimmed) {
            if spec == "vm" && remainder.is_empty() {
                namespace_aliases.insert("vm".to_string(), "vm".to_string());
            }
            continue;
        }

        if block_stack.len() >= 2 {
            let split = block_stack.len() - 1;
            let (prefix, tail) = block_stack.split_at_mut(split);
            if let (
                Some(LuaDirectBlock::Function {
                    param_lookup,
                    captures,
                    body_result,
                    ..
                }),
                LuaDirectBlock::FunctionIfChain {
                    branches,
                    active_branch,
                    else_branch,
                    in_else,
                },
            ) = (prefix.last_mut(), &mut tail[0])
            {
                if trimmed == "return" {
                    if *in_else {
                        *else_branch = Some(vec![LuaLoweredExpr::scalar(Expr::Null)]);
                    } else if let Some((_, branch_return)) = branches.get_mut(*active_branch) {
                        *branch_return = Some(vec![LuaLoweredExpr::scalar(Expr::Null)]);
                    } else {
                        return Ok(None);
                    }
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("return ") {
                    let Some(exprs) = parse_lua_direct_return_exprs(
                        rest.trim(),
                        &mut builder,
                        &namespace_aliases,
                        param_lookup,
                        captures,
                        &callable_return_arities,
                    )?
                    else {
                        return Ok(None);
                    };
                    if *in_else {
                        *else_branch = Some(exprs);
                    } else if let Some((_, branch_return)) = branches.get_mut(*active_branch) {
                        *branch_return = Some(exprs);
                    } else {
                        return Ok(None);
                    }
                    continue;
                }

                let elseif_condition = trimmed
                    .strip_prefix("elseif ")
                    .or_else(|| trimmed.strip_prefix("elif "))
                    .and_then(|rest| rest.strip_suffix(" then"));
                if let Some(condition_raw) = elseif_condition {
                    if *in_else {
                        return Ok(None);
                    }
                    let Some(condition) = parse_lua_direct_expr(
                        condition_raw,
                        &mut builder,
                        &namespace_aliases,
                        param_lookup,
                        captures,
                        true,
                        &callable_return_arities,
                        true,
                    )?
                    else {
                        return Ok(None);
                    };
                    branches.push((condition.expr, None));
                    *active_branch = branches.len().saturating_sub(1);
                    continue;
                }

                if trimmed == "else" {
                    if *in_else {
                        return Ok(None);
                    }
                    *in_else = true;
                    continue;
                }

                if trimmed == "end" {
                    *body_result = Some(build_lua_if_chain_expr(
                        branches.clone(),
                        else_branch.clone(),
                        &mut builder,
                        line_u32,
                    ));
                    block_stack.pop();
                    continue;
                }

                return Ok(None);
            }
        }

        if let Some(LuaDirectBlock::Function {
            param_lookup,
            captures,
            body_result,
            ..
        }) = block_stack.last_mut()
        {
            if trimmed == "return" {
                *body_result = Some(LuaLoweredExpr::scalar(Expr::Null));
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("return ") {
                let Some(exprs) = parse_lua_direct_return_exprs(
                    rest.trim(),
                    &mut builder,
                    &namespace_aliases,
                    param_lookup,
                    captures,
                    &callable_return_arities,
                )?
                else {
                    return Ok(None);
                };
                let target_arity = lua_return_arity(Some(exprs.as_slice()));
                *body_result = Some(build_lua_return_expr(
                    Some(exprs),
                    target_arity,
                    &mut builder,
                    line_u32,
                ));
                continue;
            }
            if let Some(condition_raw) = trimmed
                .strip_prefix("if ")
                .and_then(|rest| rest.strip_suffix(" then"))
            {
                if body_result.is_some() {
                    return Ok(None);
                }
                let Some(condition) = parse_lua_direct_expr(
                    condition_raw,
                    &mut builder,
                    &namespace_aliases,
                    param_lookup,
                    captures,
                    true,
                    &callable_return_arities,
                    true,
                )?
                else {
                    return Ok(None);
                };
                block_stack.push(LuaDirectBlock::FunctionIfChain {
                    branches: vec![(condition.expr, None)],
                    active_branch: 0,
                    else_branch: None,
                    in_else: false,
                });
                continue;
            }
            if trimmed == "end" {
                let Some(block) = block_stack.pop() else {
                    return Ok(None);
                };
                let LuaDirectBlock::Function {
                    name,
                    param_slots,
                    captures,
                    body_result,
                    is_local,
                    line,
                    ..
                } = block
                else {
                    return Ok(None);
                };
                let mut capture_copies = captures.into_iter().collect::<Vec<_>>();
                capture_copies.sort_by_key(|(source_slot, _)| *source_slot);
                let body_result = body_result.unwrap_or_else(|| LuaLoweredExpr::scalar(Expr::Null));
                let closure = Expr::Closure(super::super::ir::ClosureExpr {
                    param_slots,
                    capture_copies,
                    body: Box::new(body_result.expr),
                });
                let stmt = if is_local {
                    builder.lower_local(&name, closure, line).ok()
                } else if builder.resolve_local_expr(&name).is_some() {
                    builder.lower_assign(&name, closure, line).ok()
                } else {
                    builder.lower_local(&name, closure, line).ok()
                };
                if let Some(stmt) = stmt {
                    sync_callable_return_arity(
                        &stmt,
                        Some(body_result.unpack_arity),
                        &mut callable_return_arities,
                    );
                    emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
                    continue;
                }
                return Ok(None);
            }
            // Keep function body support minimal: only return is required by fixtures.
            return Ok(None);
        }

        if let Some(rest) = trimmed.strip_prefix("local function ") {
            let Some((name, params)) = parse_lua_function_signature(rest) else {
                return Ok(None);
            };
            let mut param_lookup = HashMap::new();
            let mut param_slots = Vec::new();
            for param in &params {
                let slot_name = fresh_lua_direct_temp(&format!("fn_param_{param}"));
                let slot = match builder.alloc_local_named(&slot_name) {
                    Ok(slot) => slot,
                    Err(_) => return Ok(None),
                };
                param_lookup.insert(param.clone(), slot);
                param_slots.push(slot);
            }
            block_stack.push(LuaDirectBlock::Function {
                name,
                param_lookup,
                param_slots,
                captures: HashMap::new(),
                body_result: None,
                is_local: true,
                line: line_u32,
            });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("function ") {
            let Some((name, params)) = parse_lua_function_signature(rest) else {
                return Ok(None);
            };
            let mut param_lookup = HashMap::new();
            let mut param_slots = Vec::new();
            for param in &params {
                let slot_name = fresh_lua_direct_temp(&format!("fn_param_{param}"));
                let slot = match builder.alloc_local_named(&slot_name) {
                    Ok(slot) => slot,
                    Err(_) => return Ok(None),
                };
                param_lookup.insert(param.clone(), slot);
                param_slots.push(slot);
            }
            block_stack.push(LuaDirectBlock::Function {
                name,
                param_lookup,
                param_slots,
                captures: HashMap::new(),
                body_result: None,
                is_local: false,
                line: line_u32,
            });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("if ")
            && let Some(condition_raw) = rest.strip_suffix(" then")
        {
            let condition =
                parse_lua_direct_expr_top(
                    condition_raw,
                    &mut builder,
                    &namespace_aliases,
                    &callable_return_arities,
                )?;
            let Some(condition) = condition else {
                return Ok(None);
            };
            block_stack.push(LuaDirectBlock::IfChain {
                branches: vec![(condition, Vec::new())],
                in_else: false,
                active_branch: 0,
                else_branch: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        let elseif_condition = trimmed
            .strip_prefix("elseif ")
            .or_else(|| trimmed.strip_prefix("elif "))
            .and_then(|rest| rest.strip_suffix(" then"));
        if let Some(condition_raw) = elseif_condition {
            let Some(LuaDirectBlock::IfChain {
                branches,
                in_else,
                active_branch,
                ..
            }) = block_stack.last_mut()
            else {
                return Ok(None);
            };
            if *in_else {
                return Ok(None);
            }
            let Some(condition) =
                parse_lua_direct_expr_top(
                    condition_raw,
                    &mut builder,
                    &namespace_aliases,
                    &callable_return_arities,
                )?
            else {
                return Ok(None);
            };
            branches.push((condition, Vec::new()));
            *active_branch = branches.len().saturating_sub(1);
            continue;
        }

        if trimmed == "else" {
            let Some(LuaDirectBlock::IfChain { in_else, .. }) = block_stack.last_mut() else {
                return Ok(None);
            };
            *in_else = true;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("while ")
            && let Some(condition_raw) = rest.strip_suffix(" do")
        {
            let condition =
                parse_lua_direct_expr_top(
                    condition_raw,
                    &mut builder,
                    &namespace_aliases,
                    &callable_return_arities,
                )?;
            let Some(condition) = condition else {
                return Ok(None);
            };
            block_stack.push(LuaDirectBlock::While {
                condition,
                body: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("for ")
            && let Some(header) = rest.strip_suffix(" do")
        {
            let Some((name, start_raw, end_raw, step_raw)) = parse_lua_numeric_for_header(header)
            else {
                return Ok(None);
            };
            let Some(start) =
                parse_lua_direct_expr_top(
                    &start_raw,
                    &mut builder,
                    &namespace_aliases,
                    &callable_return_arities,
                )?
            else {
                return Ok(None);
            };
            let Some(end) = parse_lua_direct_expr_top(
                &end_raw,
                &mut builder,
                &namespace_aliases,
                &callable_return_arities,
            )?
            else {
                return Ok(None);
            };
            let Some(step) =
                parse_lua_direct_expr_top(
                    &step_raw,
                    &mut builder,
                    &namespace_aliases,
                    &callable_return_arities,
                )?
            else {
                return Ok(None);
            };
            let init = match builder.lower_local(&name, start, line_u32) {
                Ok(stmt) => stmt,
                Err(_) => return Ok(None),
            };
            let post = match builder.lower_assign(
                &name,
                Expr::Add(
                    Box::new(Expr::Var(match builder.resolve_local_expr(&name) {
                        Some(Expr::Var(slot)) => slot,
                        _ => return Ok(None),
                    })),
                    Box::new(step.clone()),
                ),
                line_u32,
            ) {
                Ok(stmt) => stmt,
                Err(_) => return Ok(None),
            };
            let loop_var = match builder.resolve_local_expr(&name) {
                Some(Expr::Var(slot)) => Expr::Var(slot),
                _ => return Ok(None),
            };
            let condition = Expr::Or(
                Box::new(Expr::And(
                    Box::new(Expr::Gt(Box::new(step.clone()), Box::new(Expr::Int(0)))),
                    Box::new(Expr::Not(Box::new(Expr::Gt(
                        Box::new(loop_var.clone()),
                        Box::new(end.clone()),
                    )))),
                )),
                Box::new(Expr::And(
                    Box::new(Expr::Lt(Box::new(step.clone()), Box::new(Expr::Int(0)))),
                    Box::new(Expr::Not(Box::new(Expr::Lt(
                        Box::new(loop_var),
                        Box::new(end),
                    )))),
                )),
            );
            block_stack.push(LuaDirectBlock::For {
                init: Box::new(init),
                condition,
                post: Box::new(post),
                body: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        if trimmed == "repeat" {
            block_stack.push(LuaDirectBlock::Repeat {
                body: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("until ") {
            let Some(LuaDirectBlock::Repeat { mut body, line }) = block_stack.pop() else {
                return Ok(None);
            };
            let Some(condition) =
                parse_lua_direct_expr_top(
                    rest.trim(),
                    &mut builder,
                    &namespace_aliases,
                    &callable_return_arities,
                )?
            else {
                return Ok(None);
            };
            body.push(Stmt::IfElse {
                condition,
                then_branch: vec![Stmt::Break { line: line_u32 }],
                else_branch: Vec::new(),
                line: line_u32,
            });
            emit_lua_direct_stmt(
                Stmt::While {
                    condition: Expr::Bool(true),
                    body,
                    line,
                },
                &mut root_stmts,
                &mut block_stack,
            );
            continue;
        }

        if trimmed == "do" {
            block_stack.push(LuaDirectBlock::Do {
                body: Vec::new(),
                line: line_u32,
            });
            continue;
        }

        if trimmed == "end" {
            let Some(block) = block_stack.pop() else {
                return Ok(None);
            };
            let stmt = match block {
                LuaDirectBlock::IfChain {
                    branches,
                    else_branch,
                    line,
                    ..
                } => build_lua_if_chain_stmt(branches, else_branch, line),
                LuaDirectBlock::While {
                    condition,
                    body,
                    line,
                } => Stmt::While {
                    condition,
                    body,
                    line,
                },
                LuaDirectBlock::Do { body, line } => Stmt::IfElse {
                    condition: Expr::Bool(true),
                    then_branch: body,
                    else_branch: Vec::new(),
                    line,
                },
                LuaDirectBlock::For {
                    init,
                    condition,
                    post,
                    body,
                    line,
                } => Stmt::For {
                    init,
                    condition,
                    post,
                    body,
                    line,
                },
                LuaDirectBlock::Repeat { .. }
                | LuaDirectBlock::Function { .. }
                | LuaDirectBlock::FunctionIfChain { .. } => return Ok(None),
            };
            emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
            continue;
        }

        if trimmed == "break" {
            emit_lua_direct_stmt(
                Stmt::Break { line: line_u32 },
                &mut root_stmts,
                &mut block_stack,
            );
            continue;
        }

        if trimmed == "continue" {
            emit_lua_direct_stmt(
                Stmt::Continue { line: line_u32 },
                &mut root_stmts,
                &mut block_stack,
            );
            continue;
        }

        if trimmed == "::continue::" {
            continue;
        }
        if trimmed == "goto continue" {
            emit_lua_direct_stmt(
                Stmt::Continue { line: line_u32 },
                &mut root_stmts,
                &mut block_stack,
            );
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("local ") {
            let Some((name_raw, expr_raw)) = rest.split_once('=') else {
                return Ok(None);
            };
            let Some(names) = parse_lua_assignment_targets(name_raw.trim()) else {
                return Ok(None);
            };
            let rhs_parts = split_top_level_csv(expr_raw.trim());
            if rhs_parts.len() != 1 {
                return Ok(None);
            }
            let params = HashMap::new();
            let mut captures = HashMap::new();
            let Some(expr) = parse_lua_direct_expr(
                rhs_parts[0].trim(),
                &mut builder,
                &namespace_aliases,
                &params,
                &mut captures,
                false,
                &callable_return_arities,
                names.len() > 1,
            )?
            else {
                return Ok(None);
            };
            if names.len() == 1 {
                let stmt = builder.lower_local(&names[0], expr.expr, line_u32)?;
                sync_callable_return_arity(
                    &stmt,
                    expr.callable_return_arity,
                    &mut callable_return_arities,
                );
                emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
            } else {
                for stmt in lower_lua_multi_local_binding(
                    names,
                    expr,
                    &mut builder,
                    line_u32,
                    &mut callable_return_arities,
                )? {
                    emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
                }
            }
            continue;
        }

        if let Some((lhs, rhs)) = trimmed.split_once('=')
            && is_valid_lua_ident(lhs.trim())
            && !lhs.contains('!')
            && !lhs.contains('<')
            && !lhs.contains('>')
        {
            let mut captures = HashMap::new();
            let Some(expr) = parse_lua_direct_expr(
                rhs.trim(),
                &mut builder,
                &namespace_aliases,
                &HashMap::new(),
                &mut captures,
                false,
                &callable_return_arities,
                false,
            )?
            else {
                return Ok(None);
            };
            let stmt = builder.lower_assign(lhs.trim(), expr.expr, line_u32)?;
            sync_callable_return_arity(
                &stmt,
                expr.callable_return_arity,
                &mut callable_return_arities,
            );
            emit_lua_direct_stmt(stmt, &mut root_stmts, &mut block_stack);
            continue;
        }

        let expr = parse_lua_direct_expr_top(
            trimmed,
            &mut builder,
            &namespace_aliases,
            &callable_return_arities,
        )?;
        let Some(expr) = expr else {
            return Ok(None);
        };
        emit_lua_direct_stmt(
            Stmt::Expr {
                expr,
                line: line_u32,
            },
            &mut root_stmts,
            &mut block_stack,
        );
    }

    if !block_stack.is_empty() {
        return Ok(None);
    }

    Ok(Some(builder.finish(root_stmts)))
}

enum LuaDirectBlock {
    IfChain {
        branches: Vec<(Expr, Vec<Stmt>)>,
        active_branch: usize,
        else_branch: Vec<Stmt>,
        in_else: bool,
        line: u32,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        line: u32,
    },
    Do {
        body: Vec<Stmt>,
        line: u32,
    },
    For {
        init: Box<Stmt>,
        condition: Expr,
        post: Box<Stmt>,
        body: Vec<Stmt>,
        line: u32,
    },
    Repeat {
        body: Vec<Stmt>,
        line: u32,
    },
    Function {
        name: String,
        param_lookup: HashMap<String, LocalSlot>,
        param_slots: Vec<LocalSlot>,
        captures: HashMap<LocalSlot, LocalSlot>,
        body_result: Option<LuaLoweredExpr>,
        is_local: bool,
        line: u32,
    },
    FunctionIfChain {
        branches: Vec<(Expr, Option<Vec<LuaLoweredExpr>>)>,
        active_branch: usize,
        else_branch: Option<Vec<LuaLoweredExpr>>,
        in_else: bool,
    },
}

fn emit_lua_direct_stmt(stmt: Stmt, root: &mut Vec<Stmt>, blocks: &mut [LuaDirectBlock]) {
    let Some(current) = blocks.last_mut() else {
        root.push(stmt);
        return;
    };
    match current {
        LuaDirectBlock::IfChain {
            branches,
            active_branch,
            else_branch,
            in_else,
            ..
        } => {
            if *in_else {
                else_branch.push(stmt);
            } else if let Some((_, branch_body)) = branches.get_mut(*active_branch) {
                branch_body.push(stmt);
            }
        }
        LuaDirectBlock::While { body, .. }
        | LuaDirectBlock::Do { body, .. }
        | LuaDirectBlock::For { body, .. }
        | LuaDirectBlock::Repeat { body, .. } => body.push(stmt),
        LuaDirectBlock::Function { .. } | LuaDirectBlock::FunctionIfChain { .. } => {}
    }
}

fn build_lua_if_chain_stmt(
    branches: Vec<(Expr, Vec<Stmt>)>,
    else_branch: Vec<Stmt>,
    line: u32,
) -> Stmt {
    let mut iter = branches.into_iter().rev();
    let Some((last_condition, last_then_branch)) = iter.next() else {
        return Stmt::IfElse {
            condition: Expr::Bool(false),
            then_branch: Vec::new(),
            else_branch,
            line,
        };
    };

    let mut stmt = Stmt::IfElse {
        condition: last_condition,
        then_branch: last_then_branch,
        else_branch,
        line,
    };

    for (condition, then_branch) in iter {
        stmt = Stmt::IfElse {
            condition,
            then_branch,
            else_branch: vec![stmt],
            line,
        };
    }
    stmt
}

fn build_lua_if_chain_expr(
    branches: Vec<(Expr, Option<Vec<LuaLoweredExpr>>)>,
    else_branch: Option<Vec<LuaLoweredExpr>>,
    builder: &mut LocalIrBuilder,
    line: u32,
) -> LuaLoweredExpr {
    let target_arity = branches
        .iter()
        .map(|(_, values)| lua_return_arity(values.as_deref()))
        .chain(std::iter::once(lua_return_arity(else_branch.as_deref())))
        .max()
        .unwrap_or(1);
    let mut iter = branches.into_iter().rev();
    let Some((last_condition, last_then_branch)) = iter.next() else {
        return build_lua_return_expr(else_branch, target_arity, builder, line);
    };

    let mut expr = Expr::IfElse {
        condition: Box::new(last_condition),
        then_expr: Box::new(build_lua_return_expr(last_then_branch, target_arity, builder, line).expr),
        else_expr: Box::new(build_lua_return_expr(else_branch, target_arity, builder, line).expr),
    };

    for (condition, then_branch) in iter {
        expr = Expr::IfElse {
            condition: Box::new(condition),
            then_expr: Box::new(build_lua_return_expr(then_branch, target_arity, builder, line).expr),
            else_expr: Box::new(expr),
        };
    }

    LuaLoweredExpr {
        expr,
        unpack_arity: target_arity,
        callable_return_arity: None,
    }
}

fn parse_lua_function_signature(signature: &str) -> Option<(String, Vec<String>)> {
    let sig = signature.trim();
    let open = sig.find('(')?;
    let close = sig.rfind(')')?;
    if close <= open {
        return None;
    }
    let name = sig[..open].trim();
    if !is_valid_lua_ident(name) {
        return None;
    }
    let params = sig[open + 1..close]
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !params.iter().all(|param| is_valid_lua_ident(param)) {
        return None;
    }
    Some((name.to_string(), params))
}

fn parse_lua_pub_fn_declaration(line: &str) -> Option<(String, Vec<String>)> {
    let rest = line.strip_prefix("pub fn ")?;
    let sig = rest.trim().trim_end_matches(';').trim();
    parse_lua_function_signature(sig)
}

fn parse_lua_numeric_for_header(header: &str) -> Option<(String, String, String, String)> {
    let (name, rhs) = header.split_once('=')?;
    let name = name.trim();
    if !is_valid_lua_ident(name) {
        return None;
    }
    let parts = split_top_level_csv(rhs.trim());
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let start = parts[0].trim().to_string();
    let end = parts[1].trim().to_string();
    let step = parts
        .get(2)
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| "1".to_string());
    Some((name.to_string(), start, end, step))
}

#[derive(Clone)]
enum LuaDirectExpr {
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

fn parse_lua_direct_expr(
    input: &str,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    capture_enabled: bool,
    callable_return_arities: &HashMap<LocalSlot, usize>,
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
    let lowered = lower_lua_direct_expr(
        expr.clone(),
        builder,
        namespace_aliases,
        param_slots,
        capture_slots,
        capture_enabled,
        callable_return_arities,
        preserve_multi_return_root,
    );
    if lowered.is_none()
        && let Some(name) = unresolved_lua_direct_call_name(&expr, builder, param_slots)
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

fn parse_lua_direct_expr_top(
    input: &str,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    callable_return_arities: &HashMap<LocalSlot, usize>,
) -> Result<Option<Expr>, ParseError> {
    let params = HashMap::new();
    let mut captures = HashMap::new();
    Ok(parse_lua_direct_expr(
        input,
        builder,
        namespace_aliases,
        &params,
        &mut captures,
        false,
        callable_return_arities,
        false,
    )?
    .map(|expr| expr.expr))
}

fn build_lua_unpack_get_expr(target: Expr, index: i64) -> Expr {
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

fn lower_lua_direct_expr(
    expr: LuaDirectExpr,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    capture_enabled: bool,
    callable_return_arities: &HashMap<LocalSlot, usize>,
    preserve_multi_return_root: bool,
) -> Option<LuaLoweredExpr> {
    match expr {
        LuaDirectExpr::Null => Some(LuaLoweredExpr::scalar(Expr::Null)),
        LuaDirectExpr::Bool(value) => Some(LuaLoweredExpr::scalar(Expr::Bool(value))),
        LuaDirectExpr::Int(value) => Some(LuaLoweredExpr::scalar(Expr::Int(value))),
        LuaDirectExpr::Float(value) => Some(LuaLoweredExpr::scalar(Expr::Float(value))),
        LuaDirectExpr::String(value) => Some(LuaLoweredExpr::scalar(Expr::String(value))),
        LuaDirectExpr::Var(name) => {
            if let Some(slot) = param_slots.get(&name).copied() {
                return Some(LuaLoweredExpr::scalar(Expr::Var(slot)));
            }
            if let Some(Expr::Var(source_slot)) = builder.resolve_local_expr(&name) {
                let callable_return_arity = callable_return_arities.get(&source_slot).copied();
                if !capture_enabled {
                    return Some(LuaLoweredExpr {
                        expr: Expr::Var(source_slot),
                        unpack_arity: 1,
                        callable_return_arity,
                    });
                }
                if let Some(captured_slot) = capture_slots.get(&source_slot).copied() {
                    return Some(LuaLoweredExpr {
                        expr: Expr::Var(captured_slot),
                        unpack_arity: 1,
                        callable_return_arity,
                    });
                }
                let capture_name = fresh_lua_direct_temp("capture_slot");
                let captured_slot = builder.alloc_local_named(&capture_name).ok()?;
                capture_slots.insert(source_slot, captured_slot);
                return Some(LuaLoweredExpr {
                    expr: Expr::Var(captured_slot),
                    unpack_arity: 1,
                    callable_return_arity,
                });
            }
            None
        }
        LuaDirectExpr::Call(callee, args) => {
            let mut lowered_args = Vec::with_capacity(args.len());
            for arg in args {
                lowered_args.push(lower_lua_direct_expr(
                    arg,
                    builder,
                    namespace_aliases,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    callable_return_arities,
                    false,
                )?);
            }
            let lowered_args = lowered_args
                .into_iter()
                .map(|value| value.expr)
                .collect::<Vec<_>>();
            if let LuaDirectExpr::Var(name) = *callee {
                if let Some(expr) = builder.resolve_call_expr(&name, lowered_args.clone()) {
                    let unpack_arity =
                        lookup_lua_callable_return_arity(&name, builder, callable_return_arities)
                            .unwrap_or(1);
                    return Some(finalize_lua_root_expr(
                        expr,
                        unpack_arity,
                        preserve_multi_return_root,
                    ));
                }
                if name == "print" && lowered_args.len() == 1 {
                    builder.declare_function("print", Some(1)).ok()?;
                    return builder
                        .resolve_call_expr("print", lowered_args)
                        .map(LuaLoweredExpr::scalar);
                }
                return None;
            }
            if let Some(path) = flatten_lua_member_path(&callee)
                && let Some(expr) =
                    lower_lua_namespace_call(&path, lowered_args, builder, namespace_aliases)
            {
                return Some(LuaLoweredExpr::scalar(expr));
            }
            None
        }
        LuaDirectExpr::Member(target, member) => {
            let target = lower_lua_direct_expr(
                *target,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                false,
            )?;
            Some(LuaLoweredExpr::scalar(Expr::Call(
                BuiltinFunction::Get.call_index(),
                vec![target.expr, Expr::String(member)],
            )))
        }
        LuaDirectExpr::OptionalMember(target, member) => {
            let target = lower_lua_direct_expr(
                *target,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                false,
            )?;
            build_lua_optional_member_expr(target.expr, member, builder).map(LuaLoweredExpr::scalar)
        }
        LuaDirectExpr::Index(target, key) => {
            let target = lower_lua_direct_expr(
                *target,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                false,
            )?;
            let key = lower_lua_direct_expr(
                *key,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                false,
            )?;
            Some(LuaLoweredExpr::scalar(Expr::Call(
                BuiltinFunction::Get.call_index(),
                vec![target.expr, key.expr],
            )))
        }
        LuaDirectExpr::TableArray(values) => {
            let mut out = Expr::Call(BuiltinFunction::ArrayNew.call_index(), Vec::new());
            for value in values {
                let value = lower_lua_direct_expr(
                    value,
                    builder,
                    namespace_aliases,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    callable_return_arities,
                    false,
                )?;
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
                let value = lower_lua_direct_expr(
                    value,
                    builder,
                    namespace_aliases,
                    param_slots,
                    capture_slots,
                    capture_enabled,
                    callable_return_arities,
                    false,
                )?;
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
                let slot = builder.alloc_local_named(&slot_name).ok()?;
                closure_params.insert(name, slot);
                param_slots_vec.push(slot);
            }
            let mut captures = HashMap::new();
            let lowered_body = lower_lua_return_body_exprs(
                body,
                builder,
                namespace_aliases,
                &closure_params,
                &mut captures,
                callable_return_arities,
            )?;
            let mut capture_copies = captures.into_iter().collect::<Vec<_>>();
            capture_copies.sort_by_key(|(source_slot, _)| *source_slot);
            let target_arity = lua_return_arity(Some(lowered_body.as_slice()));
            Some(LuaLoweredExpr::callable(
                Expr::Closure(super::super::ir::ClosureExpr {
                    param_slots: param_slots_vec,
                    capture_copies,
                    body: Box::new(
                        build_lua_return_expr(Some(lowered_body), target_arity, builder, 1).expr,
                    ),
                }),
                target_arity,
            ))
        }
        LuaDirectExpr::Add(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Add,
        ),
        LuaDirectExpr::Sub(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Sub,
        ),
        LuaDirectExpr::Mul(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Mul,
        ),
        LuaDirectExpr::Div(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Div,
        ),
        LuaDirectExpr::Mod(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Mod,
        ),
        LuaDirectExpr::Eq(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Eq,
        ),
        LuaDirectExpr::Ne(lhs, rhs) => {
            let eq = lower_lua_binary_expr(
                *lhs,
                *rhs,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                Expr::Eq,
            )?;
            Some(LuaLoweredExpr::scalar(Expr::Not(Box::new(eq.expr))))
        }
        LuaDirectExpr::Lt(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Lt,
        ),
        LuaDirectExpr::Gt(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Gt,
        ),
        LuaDirectExpr::Le(lhs, rhs) => {
            let gt = lower_lua_binary_expr(
                *lhs,
                *rhs,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                Expr::Gt,
            )?;
            Some(LuaLoweredExpr::scalar(Expr::Not(Box::new(gt.expr))))
        }
        LuaDirectExpr::Ge(lhs, rhs) => {
            let lt = lower_lua_binary_expr(
                *lhs,
                *rhs,
                builder,
                namespace_aliases,
                param_slots,
                capture_slots,
                capture_enabled,
                callable_return_arities,
                Expr::Lt,
            )?;
            Some(LuaLoweredExpr::scalar(Expr::Not(Box::new(lt.expr))))
        }
        LuaDirectExpr::And(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::And,
        ),
        LuaDirectExpr::Or(lhs, rhs) => lower_lua_binary_expr(
            *lhs,
            *rhs,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Or,
        ),
        LuaDirectExpr::Neg(inner) => lower_lua_unary_expr(
            *inner,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Neg,
        ),
        LuaDirectExpr::Not(inner) => lower_lua_unary_expr(
            *inner,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            capture_enabled,
            callable_return_arities,
            Expr::Not,
        ),
    }
}

fn lower_lua_binary_expr(
    lhs: LuaDirectExpr,
    rhs: LuaDirectExpr,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    capture_enabled: bool,
    callable_return_arities: &HashMap<LocalSlot, usize>,
    build: fn(Box<Expr>, Box<Expr>) -> Expr,
) -> Option<LuaLoweredExpr> {
    let lhs = lower_lua_direct_expr(
        lhs,
        builder,
        namespace_aliases,
        param_slots,
        capture_slots,
        capture_enabled,
        callable_return_arities,
        false,
    )?;
    let rhs = lower_lua_direct_expr(
        rhs,
        builder,
        namespace_aliases,
        param_slots,
        capture_slots,
        capture_enabled,
        callable_return_arities,
        false,
    )?;
    Some(LuaLoweredExpr::scalar(build(
        Box::new(lhs.expr),
        Box::new(rhs.expr),
    )))
}

fn lower_lua_unary_expr(
    inner: LuaDirectExpr,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    capture_enabled: bool,
    callable_return_arities: &HashMap<LocalSlot, usize>,
    build: fn(Box<Expr>) -> Expr,
) -> Option<LuaLoweredExpr> {
    let inner = lower_lua_direct_expr(
        inner,
        builder,
        namespace_aliases,
        param_slots,
        capture_slots,
        capture_enabled,
        callable_return_arities,
        false,
    )?;
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
    let root = namespace_aliases
        .get(&path[0])
        .cloned()
        .unwrap_or_else(|| path[0].clone());

    if root == "vm" && path.len() >= 3 {
        if path.len() == 3
            && is_builtin_namespace(&path[1])
            && let Some(expr) =
                lower_lua_regex_or_builtin_namespace_call(&path[1], &path[2], args.clone())
        {
            return Some(expr);
        }
        let call_name = path[1..].join("::");
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

fn parse_lua_require_call(input: &str) -> Option<(String, String)> {
    let mut rest = input.trim().strip_prefix("require")?.trim_start();
    rest = rest.strip_prefix('(')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    rest = &rest[quote.len_utf8()..];
    let mut end = None;
    for (idx, ch) in rest.char_indices() {
        if ch == quote {
            end = Some(idx);
            break;
        }
    }
    let end = end?;
    let spec = rest[..end].to_string();
    let tail = rest[end + quote.len_utf8()..].trim_start();
    if !tail.starts_with(')') {
        return None;
    }
    let remainder = tail[1..].trim().to_string();
    Some((spec, remainder))
}

fn is_valid_lua_ident(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

fn parse_lua_local_assignment(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("local ")?;
    let (name, rhs) = rest.split_once('=')?;
    let name = name.trim();
    let rhs = rhs.trim();
    if is_valid_lua_ident(name) {
        Some((name, rhs))
    } else {
        None
    }
}

fn parse_lua_assignment_targets(input: &str) -> Option<Vec<String>> {
    let names = split_top_level_csv(input)
        .into_iter()
        .map(|value| value.trim().to_string())
        .collect::<Vec<_>>();
    if names.is_empty() || !names.iter().all(|name| is_valid_lua_ident(name)) {
        return None;
    }
    Some(names)
}

fn sync_callable_return_arity(
    stmt: &Stmt,
    callable_return_arity: Option<usize>,
    callable_return_arities: &mut HashMap<LocalSlot, usize>,
) {
    let slot = match stmt {
        Stmt::Let { index, .. } | Stmt::Assign { index, .. } => *index,
        _ => return,
    };
    if let Some(arity) = callable_return_arity {
        callable_return_arities.insert(slot, arity.max(1));
    } else {
        callable_return_arities.remove(&slot);
    }
}

fn lower_lua_multi_local_binding(
    names: Vec<String>,
    expr: LuaLoweredExpr,
    builder: &mut LocalIrBuilder,
    line: u32,
    callable_return_arities: &mut HashMap<LocalSlot, usize>,
) -> Result<Vec<Stmt>, ParseError> {
    let mut stmts = Vec::new();
    if names.is_empty() {
        return Ok(stmts);
    }

    if expr.unpack_arity <= 1 {
        let mut iter = names.into_iter();
        if let Some(first) = iter.next() {
            let stmt = builder.lower_local(&first, expr.expr, line)?;
            sync_callable_return_arity(&stmt, expr.callable_return_arity, callable_return_arities);
            stmts.push(stmt);
        }
        for name in iter {
            let stmt = builder.lower_local(&name, Expr::Null, line)?;
            sync_callable_return_arity(&stmt, None, callable_return_arities);
            stmts.push(stmt);
        }
        return Ok(stmts);
    }

    let temp_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("multi_ret"))
        .map_err(|err| ParseError::at_line(line as usize, err.to_string()))?;
    stmts.push(Stmt::Let {
        index: temp_slot,
        expr: expr.expr,
        line,
    });

    for (value_index, name) in names.into_iter().enumerate() {
        let value_expr = if value_index < expr.unpack_arity {
            build_lua_unpack_get_expr(Expr::Var(temp_slot), value_index as i64)
        } else {
            Expr::Null
        };
        let stmt = builder.lower_local(&name, value_expr, line)?;
        sync_callable_return_arity(&stmt, None, callable_return_arities);
        stmts.push(stmt);
    }

    Ok(stmts)
}

fn lower_lua_return_body_exprs(
    body: Vec<LuaDirectExpr>,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    callable_return_arities: &HashMap<LocalSlot, usize>,
) -> Option<Vec<LuaLoweredExpr>> {
    let last_index = body.len().checked_sub(1)?;
    let mut lowered = Vec::with_capacity(body.len());
    for (index, expr) in body.into_iter().enumerate() {
        lowered.push(lower_lua_direct_expr(
            expr,
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            true,
            callable_return_arities,
            index == last_index,
        )?);
    }
    Some(lowered)
}

fn lua_return_arity(exprs: Option<&[LuaLoweredExpr]>) -> usize {
    let Some(values) = exprs else {
        return 1;
    };
    let Some((last, head)) = values.split_last() else {
        return 1;
    };
    head.len() + last.unpack_arity.max(1)
}

fn build_lua_packed_array_expr(values: Vec<Expr>) -> Expr {
    values.into_iter().fold(
        Expr::Call(BuiltinFunction::ArrayNew.call_index(), Vec::new()),
        |array, value| Expr::Call(BuiltinFunction::ArrayPush.call_index(), vec![array, value]),
    )
}

fn build_lua_return_expr(
    exprs: Option<Vec<LuaLoweredExpr>>,
    target_arity: usize,
    builder: &mut LocalIrBuilder,
    line: u32,
) -> LuaLoweredExpr {
    let mut exprs = exprs.unwrap_or_default();
    if target_arity <= 1 {
        let expr = exprs
            .drain(..)
            .next()
            .map(|expr| expr.scalarized().expr)
            .unwrap_or(Expr::Null);
        return LuaLoweredExpr::scalar(expr);
    }

    let Some(last) = exprs.pop() else {
        return LuaLoweredExpr {
            expr: build_lua_packed_array_expr(vec![Expr::Null; target_arity]),
            unpack_arity: target_arity,
            callable_return_arity: None,
        };
    };

    let mut prefix_values = exprs
        .into_iter()
        .map(|expr| expr.scalarized().expr)
        .collect::<Vec<_>>();

    if last.unpack_arity <= 1 {
        prefix_values.push(last.scalarized().expr);
        while prefix_values.len() < target_arity {
            prefix_values.push(Expr::Null);
        }
        return LuaLoweredExpr {
            expr: build_lua_packed_array_expr(prefix_values.into_iter().take(target_arity).collect()),
            unpack_arity: target_arity,
            callable_return_arity: None,
        };
    }

    let packed_slot = builder
        .alloc_local_named(&fresh_lua_direct_temp("return_pack"))
        .expect("lua direct lowering temp allocation should not fail");
    let remaining_tail = target_arity.saturating_sub(prefix_values.len());
    let mut values = prefix_values;
    for index in 0..remaining_tail {
        values.push(build_lua_unpack_get_expr(Expr::Var(packed_slot), index as i64));
    }
    while values.len() < target_arity {
        values.push(Expr::Null);
    }

    LuaLoweredExpr {
        expr: Expr::Block {
            stmts: vec![Stmt::Let {
                index: packed_slot,
                expr: last.expr,
                line,
            }],
            expr: Box::new(build_lua_packed_array_expr(
                values.into_iter().take(target_arity).collect(),
            )),
        },
        unpack_arity: target_arity,
        callable_return_arity: None,
    }
}

fn parse_lua_direct_return_exprs(
    input: &str,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    callable_return_arities: &HashMap<LocalSlot, usize>,
) -> Result<Option<Vec<LuaLoweredExpr>>, ParseError> {
    let parts = split_top_level_csv(input);
    let last_index = parts.len().checked_sub(1).ok_or_else(|| {
        ParseError::at_line(1, "lua return expression list cannot be empty")
    })?;
    let mut out = Vec::with_capacity(parts.len());
    for (index, part) in parts.into_iter().enumerate() {
        let Some(expr) = parse_lua_direct_expr(
            part.trim(),
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            true,
            callable_return_arities,
            index == last_index,
        )?
        else {
            return Ok(None);
        };
        out.push(expr);
    }
    Ok(Some(out))
}

fn split_top_level_csv(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut string_delim: Option<char> = None;
    let mut escaped = false;

    for ch in input.chars() {
        if let Some(delim) = string_delim {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delim {
                string_delim = None;
            }
            continue;
        }

        match ch {
            '"' | '\'' => {
                string_delim = Some(ch);
                current.push(ch);
            }
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(ch);
            }
            '{' => {
                brace_depth += 1;
                current.push(ch);
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn remove_lua_comments(source: &str) -> Result<String, ParseError> {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;
    let mut line = 1usize;
    let mut string_delim: Option<u8> = None;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                out.push('\n');
                in_line_comment = false;
                line += 1;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b']' && i + 1 < bytes.len() && bytes[i + 1] == b']' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            if b == b'\n' {
                out.push('\n');
                line += 1;
            }
            i += 1;
            continue;
        }

        if let Some(delim) = string_delim {
            out.push(b as char);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == delim {
                string_delim = None;
            } else if b == b'\n' {
                line += 1;
            }
            i += 1;
            continue;
        }

        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            if i + 3 < bytes.len() && bytes[i + 2] == b'[' && bytes[i + 3] == b'[' {
                in_block_comment = true;
                i += 4;
                continue;
            }
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'"' || b == b'\'' {
            string_delim = Some(b);
            out.push(b as char);
            i += 1;
            continue;
        }

        if b == b'\n' {
            line += 1;
        }
        out.push(b as char);
        i += 1;
    }

    if in_block_comment {
        return Err(ParseError {
            span: None,
            code: None,
            line,
            message: "unterminated lua block comment".to_string(),
        });
    }
    Ok(out)
}
