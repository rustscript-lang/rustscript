//! Span collector for building the semantic index.
//!
//! This module provides the [`SpanCollector`] struct, which walks the IR to
//! populate the [`SemanticIndex`] fields: slot declaration spans, function
//! declaration spans, call expression entries, local reference entries, and
//! lexical scope records.

use std::collections::HashMap;

use super::ir::{
    CallExprEntry, Expr, FrontendIr, LexicalScope, LocalSlot, ScopeId, Stmt, TypeSchema,
};
use super::source_map::Span;

/// Holds the shared mutable state for span collection during
/// [`SemanticIndex::build`](super::ir::SemanticIndex::build).
pub(crate) struct SpanCollector<'a> {
    source_id: u32,
    source_text: &'a str,
    slot_decl_spans: &'a mut HashMap<LocalSlot, Span>,
    func_decl_spans: &'a mut HashMap<u16, Span>,
    call_exprs: &'a mut Vec<CallExprEntry>,
    local_ref_entries: &'a mut Vec<(Span, LocalSlot, String)>,
    scope_records: &'a mut Vec<LexicalScope>,
    scope_stack: Vec<ScopeId>,
    scope_id_counter: ScopeId,
    node_counter: u32,
    ir: &'a FrontendIr,
}

impl<'a> SpanCollector<'a> {
    pub(crate) fn new(
        source_id: u32,
        source_text: &'a str,
        slot_decl_spans: &'a mut HashMap<LocalSlot, Span>,
        func_decl_spans: &'a mut HashMap<u16, Span>,
        call_exprs: &'a mut Vec<CallExprEntry>,
        local_ref_entries: &'a mut Vec<(Span, LocalSlot, String)>,
        scope_records: &'a mut Vec<LexicalScope>,
        ir: &'a FrontendIr,
    ) -> Self {
        Self {
            source_id,
            source_text,
            slot_decl_spans,
            func_decl_spans,
            call_exprs,
            local_ref_entries,
            scope_records,
            scope_stack: vec![0],
            scope_id_counter: 1,
            node_counter: 0,
            ir,
        }
    }

    pub(crate) fn collect_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let { index, expr, line, .. } => {
                if let Some(name) = self
                    .ir
                    .local_bindings
                    .iter()
                    .find(|(_, s)| *s == *index)
                    .map(|(n, _)| n.as_str())
                {
                    let span = find_identifier_span(self.source_id, self.source_text, *line, name)
                        .unwrap_or_else(|| Span::new(self.source_id, 0, 0));
                    self.slot_decl_spans.insert(*index, span);
                    if let Some(scope_idx) = self.scope_stack.last() {
                        if let Some(scope) = self.scope_records.get_mut(*scope_idx as usize) {
                            scope.declarations.push(*index);
                        }
                    }
                }
                self.collect_expr(expr);
            }
            Stmt::Assign { expr, .. } | Stmt::Expr { expr, .. } => {
                self.collect_expr(expr);
            }
            Stmt::FuncDecl { name, index, line, .. } => {
                let span = find_identifier_span(self.source_id, self.source_text, *line, name)
                    .unwrap_or_else(|| Span::new(self.source_id, 0, 0));
                self.func_decl_spans.insert(*index, span);
                if let Some(scope_idx) = self.scope_stack.last() {
                    if let Some(scope) = self.scope_records.get_mut(*scope_idx as usize) {
                        scope.functions.push(*index);
                    }
                }
            }
            Stmt::IfElse {
                condition,
                then_branch,
                else_branch,
                line,
                ..
            } => {
                let if_scope_id = self.scope_id_counter;
                self.scope_id_counter += 1;
                let if_span = if let Some(span) =
                    find_identifier_span(self.source_id, self.source_text, *line, "if")
                {
                    Span::new(self.source_id, span.lo, self.source_text.len().min(span.lo + 20))
                } else {
                    Span::new(self.source_id, 0, 0)
                };
                let parent = *self.scope_stack.last().unwrap_or(&0);
                self.scope_records.push(LexicalScope {
                    parent: Some(parent),
                    range: if_span,
                    declarations: Vec::new(),
                    functions: Vec::new(),
                });
                self.collect_expr(condition);
                self.scope_stack.push(if_scope_id);
                for s in then_branch {
                    self.collect_stmt(s);
                }
                self.scope_stack.pop();
                for s in else_branch {
                    self.collect_stmt(s);
                }
            }
            Stmt::For {
                init,
                condition,
                post,
                body,
                line,
                ..
            } => {
                let for_scope_id = self.scope_id_counter;
                self.scope_id_counter += 1;
                let for_span = if let Some(span) =
                    find_identifier_span(self.source_id, self.source_text, *line, "for")
                {
                    Span::new(self.source_id, span.lo, self.source_text.len().min(span.lo + 20))
                } else {
                    Span::new(self.source_id, 0, 0)
                };
                let parent = *self.scope_stack.last().unwrap_or(&0);
                self.scope_records.push(LexicalScope {
                    parent: Some(parent),
                    range: for_span,
                    declarations: Vec::new(),
                    functions: Vec::new(),
                });
                self.collect_stmt(init);
                self.collect_expr(condition);
                self.collect_stmt(post);
                self.scope_stack.push(for_scope_id);
                for s in body {
                    self.collect_stmt(s);
                }
                self.scope_stack.pop();
            }
            Stmt::While {
                condition, body, line, ..
            } => {
                let while_scope_id = self.scope_id_counter;
                self.scope_id_counter += 1;
                let while_span = if let Some(span) =
                    find_identifier_span(self.source_id, self.source_text, *line, "while")
                {
                    Span::new(self.source_id, span.lo, self.source_text.len().min(span.lo + 20))
                } else {
                    Span::new(self.source_id, 0, 0)
                };
                let parent = *self.scope_stack.last().unwrap_or(&0);
                self.scope_records.push(LexicalScope {
                    parent: Some(parent),
                    range: while_span,
                    declarations: Vec::new(),
                    functions: Vec::new(),
                });
                self.collect_expr(condition);
                self.scope_stack.push(while_scope_id);
                for s in body {
                    self.collect_stmt(s);
                }
                self.scope_stack.pop();
            }
            Stmt::ClosureLet { closure, .. } => {
                self.collect_expr(&closure.body);
            }
            Stmt::Noop { .. } | Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Drop { .. } => {}
        }
    }

    fn collect_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(_, _, args, _) => {
                let call_name = self.find_call_name();
                let call_span = estimate_call_span(self.source_id, self.source_text, &call_name, 0)
                    .unwrap_or_else(|| Span::new(self.source_id, 0, 0));
                let callee_span =
                    find_callee_span(self.source_id, self.source_text, &call_name, call_span.lo);
                let return_type = expr
                    .host_call_resolution()
                    .map(|r| r.return_type.clone())
                    .unwrap_or(TypeSchema::Unknown);
                self.call_exprs.push(CallExprEntry {
                    span: call_span,
                    callee_span,
                    return_type: return_type.clone(),
                    name: call_name,
                });
                self.node_counter += 1;
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            Expr::Var(slot) | Expr::MoveVar(slot) => {
                let name = self
                    .ir
                    .local_bindings
                    .iter()
                    .find(|(_, s)| *s == *slot)
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default();
                if !name.is_empty() {
                    if let Some(decl_span) = self.slot_decl_spans.get(slot) {
                        self.local_ref_entries.push((*decl_span, *slot, name));
                    }
                }
            }
            Expr::Block { stmts, expr: inner } => {
                for s in stmts {
                    self.collect_stmt(s);
                }
                self.collect_expr(inner);
            }
            Expr::IfElse {
                condition,
                then_expr,
                else_expr,
                ..
            } => {
                self.collect_expr(condition);
                self.collect_expr(then_expr);
                self.collect_expr(else_expr);
            }
            Expr::Match {
                value, arms, default, ..
            } => {
                self.collect_expr(value);
                for (_, arm_expr) in arms {
                    self.collect_expr(arm_expr);
                }
                self.collect_expr(default);
            }
            Expr::Closure(closure) => {
                self.collect_expr(&closure.body);
            }
            Expr::ClosureCall(closure, args) => {
                for arg in args {
                    self.collect_expr(arg);
                }
                self.collect_expr(&closure.body);
            }
            Expr::Add(l, r)
            | Expr::Sub(l, r)
            | Expr::Mul(l, r)
            | Expr::Div(l, r)
            | Expr::Mod(l, r)
            | Expr::And(l, r)
            | Expr::Or(l, r)
            | Expr::Eq(l, r)
            | Expr::Lt(l, r)
            | Expr::Gt(l, r) => {
                self.collect_expr(l);
                self.collect_expr(r);
            }
            Expr::Neg(inner)
            | Expr::Not(inner)
            | Expr::ToOwned(inner)
            | Expr::Borrow(inner)
            | Expr::BorrowMut(inner) => {
                self.collect_expr(inner);
            }
            Expr::OptionalGet { container, key, .. } => {
                self.collect_expr(container);
                self.collect_expr(key);
            }
            Expr::OptionUnwrapOr { value, fallback, .. } => {
                self.collect_expr(value);
                self.collect_expr(fallback);
            }
            Expr::ModuleCall(_, _, args) | Expr::LocalCall(_, _, args) => {
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            Expr::Null
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Bytes(_)
            | Expr::FunctionRef(_, _)
            | Expr::ModuleFunctionRef(_, _)
            | Expr::UnresolvedFunctionRef { .. }
            | Expr::MoveField { .. }
            | Expr::MoveIndex { .. } => {}
        }
    }

    /// Find the name of a call expression by its node counter index.
    fn find_call_name(&self) -> String {
        if let Some(ref meta) = self.ir.host_api_metadata {
            // Walk the function indices to find the one matching our node_counter.
            for (idx, _) in meta.function_indices().enumerate() {
                if idx as u32 == self.node_counter {
                    if let Some(candidates) = meta.candidates(idx as u16) {
                        if let Some(first) = candidates.first() {
                            return first.name.clone();
                        }
                    }
                }
            }
        }
        format!("call_{}", self.node_counter)
    }
}

/// Find the span of a variable name in the source text at a given line.
fn find_identifier_span(
    source_id: u32,
    source_text: &str,
    line: u32,
    name: &str,
) -> Option<Span> {
    if line == 0 {
        return None;
    }
    let line_usize = line as usize;
    let mut line_start = 0usize;
    for _ in 1..line_usize {
        line_start = source_text[line_start..].find('\n').map(|pos| line_start + pos + 1)?;
    }
    let line_end = source_text[line_start..]
        .find('\n')
        .map(|pos| line_start + pos)
        .unwrap_or(source_text.len());

    let line_text = &source_text[line_start..line_end];
    let mut search_start = 0usize;
    loop {
        let Some(offset) = line_text[search_start..].find(name) else {
            return None;
        };
        let abs_offset = line_start + search_start + offset;
        let prev_ok = if search_start + offset == 0 {
            true
        } else {
            let prev = line_text.as_bytes()[search_start + offset - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_'
        };
        let next_ok = {
            let next_start = search_start + offset + name.len();
            if next_start >= line_text.len() {
                true
            } else {
                let next = line_text.as_bytes()[next_start];
                !next.is_ascii_alphanumeric() && next != b'_'
            }
        };
        if prev_ok && next_ok {
            return Some(Span::new(source_id, abs_offset, abs_offset + name.len()));
        }
        search_start += offset + 1;
        if search_start >= line_text.len() {
            return None;
        }
    }
}

/// Find the span of a call expression name in the source text.
fn find_callee_span(
    source_id: u32,
    source_text: &str,
    call_name: &str,
    call_start: usize,
) -> Span {
    let text_before = &source_text[..call_start];
    if let Some(pos) = text_before.rfind(call_name) {
        let prev_ok = if pos == 0 {
            true
        } else {
            let prev = source_text.as_bytes()[pos - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_'
        };
        if prev_ok {
            return Span::new(source_id, pos, pos + call_name.len());
        }
    }
    Span::new(source_id, call_start, call_start)
}

/// Estimate the span of a call expression by searching the source text.
fn estimate_call_span(
    source_id: u32,
    source_text: &str,
    name: &str,
    _hint_offset: usize,
) -> Option<Span> {
    let pattern = format!("{}(", name);
    let search_start = 0usize;
    if let Some(pos) = source_text[search_start..].find(&pattern) {
        let abs_pos = search_start + pos;
        let after_open = abs_pos + name.len();
        if let Some(close_paren) = find_matching_close_paren(source_text, after_open) {
            return Some(Span::new(source_id, abs_pos, close_paren));
        }
        return Some(Span::new(
            source_id,
            abs_pos,
            source_text.len().min(abs_pos + name.len() + 10),
        ));
    }
    None
}

/// Find the matching closing paren, accounting for nested parens.
fn find_matching_close_paren(source_text: &str, open_pos: usize) -> Option<usize> {
    let rest = &source_text[open_pos + 1..];
    let mut depth = 1u32;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open_pos + 1 + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}