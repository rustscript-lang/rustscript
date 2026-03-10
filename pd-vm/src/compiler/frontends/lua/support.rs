use super::super::super::ParseError;
use super::super::super::ir::{Expr, LocalIrBuilder, LocalSlot, Stmt};
use super::super::{is_ident_continue, is_ident_start};
use super::expr::{
    LuaDirectExpr, LuaDirectLowering, build_lua_unpack_get_expr, lower_lua_direct_expr,
    parse_lua_direct_expr,
};
use super::{LuaLoweredExpr, fresh_lua_direct_temp};
use crate::builtins::BuiltinFunction;
use std::collections::HashMap;

pub(super) fn parse_lua_function_signature(signature: &str) -> Option<(String, Vec<String>)> {
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

pub(super) fn parse_lua_pub_fn_declaration(line: &str) -> Option<(String, Vec<String>)> {
    let rest = line.strip_prefix("pub fn ")?;
    let sig = rest.trim().trim_end_matches(';').trim();
    parse_lua_function_signature(sig)
}

pub(super) fn parse_lua_numeric_for_header(
    header: &str,
) -> Option<(String, String, String, String)> {
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

pub(super) fn parse_lua_require_call(input: &str) -> Option<(String, String)> {
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

pub(super) fn is_valid_lua_ident(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    chars.all(is_ident_continue)
}

pub(super) fn parse_lua_local_assignment(line: &str) -> Option<(&str, &str)> {
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

pub(super) fn parse_lua_assignment_targets(input: &str) -> Option<Vec<String>> {
    let names = split_top_level_csv(input)
        .into_iter()
        .map(|value| value.trim().to_string())
        .collect::<Vec<_>>();
    if names.is_empty() || !names.iter().all(|name| is_valid_lua_ident(name)) {
        return None;
    }
    Some(names)
}

pub(super) fn sync_callable_return_arity(
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

pub(super) fn lower_lua_multi_local_binding(
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
        declared_struct: None,
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

pub(super) fn lower_lua_return_body_exprs(
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
        let mut lowering = LuaDirectLowering::new(
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            true,
            callable_return_arities,
        );
        lowered.push(lower_lua_direct_expr(
            expr,
            &mut lowering,
            index == last_index,
        )?);
    }
    Some(lowered)
}

pub(super) fn lua_return_arity(exprs: Option<&[LuaLoweredExpr]>) -> usize {
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

pub(super) fn build_lua_return_expr(
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
            expr: build_lua_packed_array_expr(
                prefix_values.into_iter().take(target_arity).collect(),
            ),
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
        values.push(build_lua_unpack_get_expr(
            Expr::Var(packed_slot),
            index as i64,
        ));
    }
    while values.len() < target_arity {
        values.push(Expr::Null);
    }

    LuaLoweredExpr {
        expr: Expr::Block {
            stmts: vec![Stmt::Let {
                index: packed_slot,
                declared_struct: None,
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

pub(super) fn parse_lua_direct_return_exprs(
    input: &str,
    builder: &mut LocalIrBuilder,
    namespace_aliases: &HashMap<String, String>,
    param_slots: &HashMap<String, LocalSlot>,
    capture_slots: &mut HashMap<LocalSlot, LocalSlot>,
    callable_return_arities: &HashMap<LocalSlot, usize>,
) -> Result<Option<Vec<LuaLoweredExpr>>, ParseError> {
    let parts = split_top_level_csv(input);
    let last_index = parts
        .len()
        .checked_sub(1)
        .ok_or_else(|| ParseError::at_line(1, "lua return expression list cannot be empty"))?;
    let mut out = Vec::with_capacity(parts.len());
    for (index, part) in parts.into_iter().enumerate() {
        let mut lowering = LuaDirectLowering::new(
            builder,
            namespace_aliases,
            param_slots,
            capture_slots,
            true,
            callable_return_arities,
        );
        let Some(expr) = parse_lua_direct_expr(part.trim(), &mut lowering, index == last_index)?
        else {
            return Ok(None);
        };
        out.push(expr);
    }
    Ok(Some(out))
}

pub(super) fn split_top_level_csv(input: &str) -> Vec<String> {
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

pub(super) fn remove_lua_comments(source: &str) -> Result<String, ParseError> {
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
