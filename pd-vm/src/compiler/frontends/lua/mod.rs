mod expr;
mod support;

use super::super::ParseError;
use super::super::ir::{Expr, FrontendIr, LocalIrBuilder, LocalSlot, Stmt};
use expr::{
    LuaDirectLowering, build_lua_unpack_get_expr, parse_lua_direct_expr, parse_lua_direct_expr_top,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::{
    build_lua_return_expr, is_valid_lua_ident, lower_lua_multi_local_binding, lua_return_arity,
    parse_lua_assignment_targets, parse_lua_direct_return_exprs, parse_lua_function_signature,
    parse_lua_local_assignment, parse_lua_numeric_for_header, parse_lua_pub_fn_declaration,
    parse_lua_require_call, remove_lua_comments, split_top_level_csv, sync_callable_return_arity,
};

static LUA_DIRECT_TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);
const ROOT_HOST_NAMESPACE_SPEC: &str = "vm";

fn fresh_lua_direct_temp(prefix: &str) -> String {
    let id = LUA_DIRECT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__lua_direct_{prefix}_{id}")
}

fn is_virtual_host_namespace_spec(spec: &str) -> bool {
    if spec == ROOT_HOST_NAMESPACE_SPEC || spec.contains('/') || spec.ends_with(".rss") {
        return false;
    }
    is_valid_lua_ident(spec)
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
            if spec == ROOT_HOST_NAMESPACE_SPEC {
                if let Some(member) = remainder.strip_prefix('.') {
                    let member = member.trim();
                    if is_valid_lua_ident(member) {
                        builder.declare_function(member, None).ok();
                        continue;
                    }
                }
                if remainder.is_empty() && is_valid_lua_ident(name) {
                    namespace_aliases
                        .insert(name.to_string(), ROOT_HOST_NAMESPACE_SPEC.to_string());
                    continue;
                }
            }
            if (spec == "io"
                || spec == "re"
                || spec == "json"
                || is_virtual_host_namespace_spec(&spec))
                && remainder.is_empty()
            {
                namespace_aliases.insert(name.to_string(), spec);
                continue;
            }
            // Module require lines are import directives handled by source loader rewrites/preludes.
            continue;
        }

        if let Some((spec, remainder)) = parse_lua_require_call(trimmed) {
            if spec == ROOT_HOST_NAMESPACE_SPEC && remainder.is_empty() {
                namespace_aliases.insert(
                    ROOT_HOST_NAMESPACE_SPEC.to_string(),
                    ROOT_HOST_NAMESPACE_SPEC.to_string(),
                );
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
                    let mut lowering = LuaDirectLowering::new(
                        &mut builder,
                        &namespace_aliases,
                        param_lookup,
                        captures,
                        true,
                        &callable_return_arities,
                    );
                    let Some(condition) =
                        parse_lua_direct_expr(condition_raw, &mut lowering, true)?
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
                let mut lowering = LuaDirectLowering::new(
                    &mut builder,
                    &namespace_aliases,
                    param_lookup,
                    captures,
                    true,
                    &callable_return_arities,
                );
                let Some(condition) = parse_lua_direct_expr(condition_raw, &mut lowering, true)?
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
            let condition = parse_lua_direct_expr_top(
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
            let Some(condition) = parse_lua_direct_expr_top(
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
            let condition = parse_lua_direct_expr_top(
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
            let Some(start) = parse_lua_direct_expr_top(
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
            let Some(step) = parse_lua_direct_expr_top(
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
            let Some(condition) = parse_lua_direct_expr_top(
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
            let mut lowering = LuaDirectLowering::new(
                &mut builder,
                &namespace_aliases,
                &params,
                &mut captures,
                false,
                &callable_return_arities,
            );
            let Some(expr) =
                parse_lua_direct_expr(rhs_parts[0].trim(), &mut lowering, names.len() > 1)?
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
            let empty_params = HashMap::new();
            let mut lowering = LuaDirectLowering::new(
                &mut builder,
                &namespace_aliases,
                &empty_params,
                &mut captures,
                false,
                &callable_return_arities,
            );
            let Some(expr) = parse_lua_direct_expr(rhs.trim(), &mut lowering, false)? else {
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
        then_expr: Box::new(
            build_lua_return_expr(last_then_branch, target_arity, builder, line).expr,
        ),
        else_expr: Box::new(build_lua_return_expr(else_branch, target_arity, builder, line).expr),
    };

    for (condition, then_branch) in iter {
        expr = Expr::IfElse {
            condition: Box::new(condition),
            then_expr: Box::new(
                build_lua_return_expr(then_branch, target_arity, builder, line).expr,
            ),
            else_expr: Box::new(expr),
        };
    }

    LuaLoweredExpr {
        expr,
        unpack_arity: target_arity,
        callable_return_arity: None,
    }
}
