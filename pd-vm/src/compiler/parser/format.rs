use super::lexer::{Lexer, Token, TokenKind};
use super::{ParseError, ParserDialect};

const SOFT_MAX_LINE_WIDTH: usize = 100;

pub(super) fn format_source(
    source: &str,
    dialect: &'static dyn ParserDialect,
) -> Result<String, ParseError> {
    let tokens = lex_tokens(source, dialect)?;
    let mut formatter = SourceFormatter::new(source, tokens);
    formatter.format()
}

fn lex_tokens(source: &str, dialect: &'static dyn ParserDialect) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Lexer::new(source, 0, dialect);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token()?;
        let is_eof = matches!(token.kind, TokenKind::Eof);
        tokens.push(token);
        if is_eof {
            break;
        }
    }
    Ok(tokens)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BraceKind {
    Block,
    MatchBody,
    StructBody,
    Collection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextKind {
    Brace(BraceKind),
    Paren { for_head: bool },
    Bracket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Context {
    kind: ContextKind,
    indented: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DelimiterDepths {
    paren: usize,
    bracket: usize,
    brace: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BraceExpectation {
    kind: BraceKind,
    depths: DelimiterDepths,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrevKind {
    Word,
    Pub,
    Use,
    Import,
    From,
    As,
    Fn,
    Struct,
    Let,
    For,
    If,
    Else,
    Match,
    While,
    Break,
    Continue,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    PathSeparator,
    OptionalAccess,
    Semicolon,
    Equal,
    FatArrow,
    ReturnArrow,
    Plus,
    PlusPlus,
    PlusEqual,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Ampersand,
    Pipe,
    PipePipe,
    AndAnd,
    OrOr,
    EqEq,
    BangEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Ellipsis,
}

struct GapResult {
    saw_line_break: bool,
}

struct SourceFormatter<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    index: usize,
    cursor: usize,
    out: String,
    indent: usize,
    line_len: usize,
    line_start: bool,
    pending_space: bool,
    pending_newlines: usize,
    pending_code_break: bool,
    prev_kind: Option<PrevKind>,
    contexts: Vec<Context>,
    brace_expectations: Vec<BraceExpectation>,
    generic_angle_depth: usize,
    at_stmt_start: bool,
    in_fn_signature: bool,
    in_closure_params: bool,
}

impl<'a> SourceFormatter<'a> {
    fn new(source: &'a str, tokens: Vec<Token>) -> Self {
        Self {
            source,
            tokens,
            index: 0,
            cursor: 0,
            out: String::new(),
            indent: 0,
            line_len: 0,
            line_start: true,
            pending_space: false,
            pending_newlines: 0,
            pending_code_break: false,
            prev_kind: None,
            contexts: Vec::new(),
            brace_expectations: Vec::new(),
            generic_angle_depth: 0,
            at_stmt_start: true,
            in_fn_signature: false,
            in_closure_params: false,
        }
    }

    fn format(&mut self) -> Result<String, ParseError> {
        while self.index < self.tokens.len() {
            let span = self.tokens[self.index].span;
            let gap = &self.source[self.cursor..span.lo];
            let gap_result = self.emit_gap(gap);

            if matches!(self.tokens[self.index].kind, TokenKind::Eof) {
                break;
            }

            if self.pending_code_break && !gap_result.saw_line_break {
                self.request_newline(1);
            }
            if gap_result.saw_line_break {
                self.pending_code_break = false;
            }

            self.emit_token()?;
        }

        if !self.out.ends_with('\n') {
            self.trim_trailing_spaces();
            self.out.push('\n');
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn emit_token(&mut self) -> Result<(), ParseError> {
        let kind = self
            .tokens
            .get(self.index)
            .expect("formatter token index should be valid")
            .kind
            .clone();
        if self.should_emit_keyword_as_word(&kind) {
            let text = self.token_text(self.index).to_string();
            self.emit_word(&text);
            self.cursor = self.tokens[self.index].span.hi;
            self.index += 1;
            return Ok(());
        }
        match kind {
            TokenKind::Pub => {
                self.emit_keyword("pub", PrevKind::Pub, true);
            }
            TokenKind::Use => {
                self.emit_keyword("use", PrevKind::Use, true);
            }
            TokenKind::Import => {
                self.emit_keyword("import", PrevKind::Import, true);
            }
            TokenKind::From => {
                self.emit_keyword("from", PrevKind::From, true);
            }
            TokenKind::As => {
                self.emit_keyword("as", PrevKind::As, true);
            }
            TokenKind::Fn => {
                self.emit_keyword("fn", PrevKind::Fn, true);
                self.in_fn_signature = true;
                self.push_brace_expectation(BraceKind::Block);
            }
            TokenKind::Struct => {
                self.emit_keyword("struct", PrevKind::Struct, true);
                self.push_brace_expectation(BraceKind::StructBody);
            }
            TokenKind::Let => {
                self.emit_keyword("let", PrevKind::Let, true);
            }
            TokenKind::For => {
                self.emit_keyword("for", PrevKind::For, true);
                self.push_brace_expectation(BraceKind::Block);
            }
            TokenKind::If => {
                self.emit_keyword("if", PrevKind::If, true);
                if self.at_stmt_start && !self.check_if_expression_start(self.index) {
                    self.push_brace_expectation(BraceKind::Block);
                }
            }
            TokenKind::Else => {
                self.emit_keyword("else", PrevKind::Else, true);
                if !matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::If)) {
                    self.push_brace_expectation(BraceKind::Block);
                }
            }
            TokenKind::Match => {
                self.emit_keyword("match", PrevKind::Match, true);
                self.push_brace_expectation(BraceKind::MatchBody);
            }
            TokenKind::While => {
                self.emit_keyword("while", PrevKind::While, true);
                self.push_brace_expectation(BraceKind::Block);
            }
            TokenKind::Break => {
                self.emit_keyword("break", PrevKind::Break, false);
            }
            TokenKind::Continue => {
                self.emit_keyword("continue", PrevKind::Continue, false);
            }
            TokenKind::Ident(_)
            | TokenKind::Int(_)
            | TokenKind::IntMinMagnitude(_)
            | TokenKind::Float(_)
            | TokenKind::String(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Null => {
                let text = self.token_text(self.index).to_string();
                self.emit_word(&text);
            }
            TokenKind::LParen => {
                self.emit_open_paren();
            }
            TokenKind::RParen => {
                self.emit_close_delimiter(ContextKind::Paren { for_head: false }, ")");
            }
            TokenKind::LBracket => {
                self.emit_open_bracket();
            }
            TokenKind::RBracket => {
                self.emit_close_delimiter(ContextKind::Bracket, "]");
            }
            TokenKind::LBrace => {
                self.emit_open_brace();
            }
            TokenKind::RBrace => {
                self.emit_close_brace();
            }
            TokenKind::Comma => {
                self.clear_pending_space();
                self.write_raw(",");
                let in_generic_angle_group = self.generic_angle_depth > 0;
                self.prev_kind = Some(PrevKind::Comma);
                let should_break = !in_generic_angle_group
                    && matches!(
                        self.contexts.last().map(|context| context.kind),
                        Some(ContextKind::Brace(
                            BraceKind::MatchBody | BraceKind::StructBody
                        ))
                    );
                if should_break {
                    self.pending_code_break = true;
                    self.at_stmt_start = true;
                } else if in_generic_angle_group {
                    self.request_space();
                } else if self.current_context_breaks_after_commas() {
                    self.request_newline(1);
                } else if !self.next_is_closer(self.index + 1) {
                    self.request_space();
                }
            }
            TokenKind::Colon => {
                if matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::Colon)) {
                    self.clear_pending_space();
                    self.write_raw("::");
                    self.prev_kind = Some(PrevKind::PathSeparator);
                    self.advance_to(self.index + 1);
                } else {
                    self.clear_pending_space();
                    self.write_raw(":");
                    self.prev_kind = Some(PrevKind::Colon);
                    if !self.in_slice_context() && !self.next_is_closer(self.index + 1) {
                        self.request_space();
                    }
                }
            }
            TokenKind::Question => {
                if matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::Dot)) {
                    self.clear_pending_space();
                    self.write_raw("?.");
                    self.prev_kind = Some(PrevKind::OptionalAccess);
                    self.advance_to(self.index + 1);
                } else {
                    self.clear_pending_space();
                    self.write_raw("?");
                }
            }
            TokenKind::Dot => {
                self.clear_pending_space();
                self.write_raw(".");
                self.prev_kind = Some(PrevKind::Dot);
            }
            TokenKind::Ellipsis => {
                self.clear_pending_space();
                self.write_raw("...");
                self.prev_kind = Some(PrevKind::Ellipsis);
            }
            TokenKind::Semicolon => {
                self.clear_pending_space();
                self.write_raw(";");
                self.prev_kind = Some(PrevKind::Semicolon);
                if self.in_for_head_context() {
                    if !matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::RParen)) {
                        self.request_space();
                    }
                } else {
                    self.pending_code_break = true;
                    self.at_stmt_start = true;
                    self.in_fn_signature = false;
                    self.pop_latest_block_expectation();
                }
            }
            TokenKind::Equal => {
                self.emit_binary_operator("=", PrevKind::Equal);
                self.in_fn_signature = false;
                self.pop_latest_block_expectation();
            }
            TokenKind::FatArrow => {
                self.emit_binary_operator("=>", PrevKind::FatArrow);
                if self.fat_arrow_is_if_branch(self.index) {
                    self.push_brace_expectation(BraceKind::Block);
                }
            }
            TokenKind::PlusPlus => self.emit_increment_operator(),
            TokenKind::Plus => self.emit_binary_operator("+", PrevKind::Plus),
            TokenKind::PlusEqual => self.emit_binary_operator("+=", PrevKind::PlusEqual),
            TokenKind::Star => {
                if self.prev_kind == Some(PrevKind::PathSeparator)
                    || self.prev_kind == Some(PrevKind::LBrace)
                    || self.prev_kind == Some(PrevKind::Comma)
                {
                    self.clear_pending_space();
                    self.write_raw("*");
                    self.prev_kind = Some(PrevKind::Star);
                } else {
                    self.emit_binary_operator("*", PrevKind::Star);
                }
            }
            TokenKind::Slash => self.emit_binary_operator("/", PrevKind::Slash),
            TokenKind::Percent => self.emit_binary_operator("%", PrevKind::Percent),
            TokenKind::Minus => {
                if self.in_fn_signature
                    && matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::Greater))
                {
                    self.request_space();
                    self.write_raw("->");
                    self.prev_kind = Some(PrevKind::ReturnArrow);
                    self.request_space();
                    self.advance_to(self.index + 1);
                } else if self.is_unary_position() {
                    self.emit_prefix_operator("-", PrevKind::Minus, false);
                } else {
                    self.emit_binary_operator("-", PrevKind::Minus);
                }
            }
            TokenKind::Bang => {
                self.emit_prefix_operator("!", PrevKind::Bang, false);
            }
            TokenKind::BangEqual => self.emit_binary_operator("!=", PrevKind::BangEq),
            TokenKind::Ampersand => {
                self.emit_prefix_operator("&", PrevKind::Ampersand, false);
            }
            TokenKind::AmpersandAmpersand => self.emit_binary_operator("&&", PrevKind::AndAnd),
            TokenKind::PipePipe => {
                if self.is_unary_position() {
                    self.emit_prefix_operator("||", PrevKind::PipePipe, true);
                } else {
                    self.emit_binary_operator("||", PrevKind::OrOr);
                }
            }
            TokenKind::Pipe => {
                self.emit_pipe();
            }
            TokenKind::EqualEqual => self.emit_binary_operator("==", PrevKind::EqEq),
            TokenKind::Less => {
                if self.starts_generic_angle_group(self.index) {
                    self.emit_generic_open();
                } else {
                    self.emit_binary_operator("<", PrevKind::Less);
                }
            }
            TokenKind::LessEqual => self.emit_binary_operator("<=", PrevKind::LessEq),
            TokenKind::Greater => {
                if self.generic_angle_depth > 0 {
                    self.emit_generic_close();
                } else {
                    self.emit_binary_operator(">", PrevKind::Greater);
                }
            }
            TokenKind::GreaterEqual => self.emit_binary_operator(">=", PrevKind::GreaterEq),
            TokenKind::Eof => {}
        }

        self.cursor = self.tokens[self.index].span.hi;
        self.index += 1;
        Ok(())
    }

    fn emit_gap(&mut self, gap: &str) -> GapResult {
        let mut cursor = 0usize;
        let bytes = gap.as_bytes();
        let mut saw_line_break = false;

        while cursor < bytes.len() {
            match bytes[cursor] {
                b' ' | b'\t' | b'\r' => {
                    cursor += 1;
                }
                b'\n' => {
                    let mut count = 1usize;
                    cursor += 1;
                    while cursor < bytes.len() && bytes[cursor] == b'\n' {
                        count += 1;
                        cursor += 1;
                    }
                    if self.should_collapse_gap_line_break() {
                        self.request_space();
                    } else {
                        self.note_context_newline();
                        self.request_newline(count.min(2));
                        self.pending_code_break = false;
                        saw_line_break = true;
                    }
                }
                b'/' if cursor + 1 < bytes.len() && bytes[cursor + 1] == b'/' => {
                    let start = cursor;
                    cursor += 2;
                    while cursor < bytes.len() && bytes[cursor] != b'\n' {
                        cursor += 1;
                    }
                    let comment = &gap[start..cursor];
                    if self.line_start || saw_line_break {
                        self.write_raw(comment);
                    } else {
                        self.request_space();
                        self.write_raw(comment);
                    }
                    if cursor < bytes.len() && bytes[cursor] == b'\n' {
                        cursor += 1;
                        self.note_context_newline();
                        self.request_newline(1);
                        self.pending_code_break = false;
                        saw_line_break = true;
                    }
                }
                b'/' if cursor + 1 < bytes.len() && bytes[cursor + 1] == b'*' => {
                    let start = cursor;
                    cursor += 2;
                    while cursor + 1 < bytes.len()
                        && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/')
                    {
                        cursor += 1;
                    }
                    cursor = (cursor + 2).min(bytes.len());
                    let comment = &gap[start..cursor];
                    let multiline = comment.contains('\n');
                    if multiline {
                        self.note_context_newline();
                        if !self.line_start {
                            self.request_newline(1);
                        }
                        self.write_raw(comment);
                        self.pending_code_break = false;
                        saw_line_break = true;
                    } else if self.line_start || saw_line_break {
                        self.write_raw(comment);
                    } else {
                        self.request_space();
                        self.write_raw(comment);
                        self.request_space();
                    }
                }
                _ => {
                    cursor += 1;
                }
            }
        }

        GapResult { saw_line_break }
    }

    fn emit_keyword(&mut self, text: &str, kind: PrevKind, trailing_space: bool) {
        if self.should_space_before_word() {
            self.request_space();
        }
        self.write_raw(text);
        self.prev_kind = Some(kind);
        self.at_stmt_start = false;
        if trailing_space {
            self.request_space();
        }
    }

    fn emit_word(&mut self, text: &str) {
        if self.should_space_before_word() {
            self.request_space();
        }
        self.write_raw(text);
        self.prev_kind = Some(PrevKind::Word);
        self.at_stmt_start = false;
    }

    fn emit_binary_operator(&mut self, text: &str, kind: PrevKind) {
        self.request_space();
        self.write_raw(text);
        self.prev_kind = Some(kind);
        self.request_space();
        self.at_stmt_start = false;
    }

    fn emit_increment_operator(&mut self) {
        let postfix = matches!(
            self.prev_kind,
            Some(PrevKind::Word | PrevKind::RParen | PrevKind::RBracket | PrevKind::RBrace)
        );
        if postfix {
            self.clear_pending_space();
        }
        self.write_raw("++");
        self.prev_kind = Some(PrevKind::PlusPlus);
        self.at_stmt_start = false;
    }

    fn emit_prefix_operator(&mut self, text: &str, kind: PrevKind, trailing_space: bool) {
        self.write_raw(text);
        self.prev_kind = Some(kind);
        if trailing_space {
            self.request_space();
        }
        self.at_stmt_start = false;
    }

    fn emit_generic_open(&mut self) {
        self.clear_pending_space();
        self.write_raw("<");
        self.prev_kind = Some(PrevKind::Less);
        self.generic_angle_depth += 1;
        self.at_stmt_start = false;
    }

    fn emit_generic_close(&mut self) {
        self.clear_pending_space();
        self.write_raw(">");
        self.prev_kind = Some(PrevKind::Greater);
        self.generic_angle_depth = self.generic_angle_depth.saturating_sub(1);
        self.at_stmt_start = false;
    }

    fn emit_open_paren(&mut self) {
        let needs_space = matches!(
            self.prev_kind,
            Some(
                PrevKind::If
                    | PrevKind::For
                    | PrevKind::While
                    | PrevKind::Match
                    | PrevKind::Import
                    | PrevKind::Equal
            )
        );
        if needs_space {
            self.request_space();
        } else {
            self.clear_pending_space();
        }
        self.write_raw("(");
        let for_head = self.prev_kind == Some(PrevKind::For);
        let should_fold = self
            .should_force_multiline_group(self.index, ContextKind::Paren { for_head })
            && !matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::RParen));
        self.contexts.push(Context {
            kind: ContextKind::Paren { for_head },
            indented: should_fold,
        });
        if should_fold {
            self.indent += 1;
            self.request_newline(1);
        }
        self.prev_kind = Some(PrevKind::LParen);
        self.at_stmt_start = false;
    }

    fn emit_open_bracket(&mut self) {
        if self.should_tighten_before_open_bracket() {
            self.clear_pending_space();
        }
        self.write_raw("[");
        let should_fold = self.should_force_multiline_group(self.index, ContextKind::Bracket)
            && !matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::RBracket));
        self.contexts.push(Context {
            kind: ContextKind::Bracket,
            indented: should_fold,
        });
        if should_fold {
            self.indent += 1;
            self.request_newline(1);
        }
        self.prev_kind = Some(PrevKind::LBracket);
        self.at_stmt_start = false;
    }

    fn emit_open_brace(&mut self) {
        let kind = self.resolve_brace_kind();
        let needs_space = !matches!(
            self.prev_kind,
            Some(PrevKind::PathSeparator | PrevKind::LParen | PrevKind::LBracket)
        ) && !self.line_start;
        if needs_space {
            self.request_space();
        } else {
            self.clear_pending_space();
        }
        self.write_raw("{");

        let empty = matches!(self.peek_kind_at(self.index + 1), Some(TokenKind::RBrace));
        let block_like = matches!(
            kind,
            BraceKind::Block | BraceKind::MatchBody | BraceKind::StructBody
        );
        let should_fold = kind == BraceKind::Collection
            && !empty
            && self.should_force_multiline_group(self.index, ContextKind::Brace(kind));
        let mut context = Context {
            kind: ContextKind::Brace(kind),
            indented: false,
        };
        if block_like && !empty {
            context.indented = true;
            self.indent += 1;
            self.pending_code_break = true;
            self.at_stmt_start = true;
        } else if should_fold {
            context.indented = true;
            self.indent += 1;
            self.request_newline(1);
        }
        self.contexts.push(context);
        self.prev_kind = Some(PrevKind::LBrace);
        self.at_stmt_start = block_like;
        if self.in_fn_signature {
            self.in_fn_signature = false;
        }
    }

    fn emit_close_brace(&mut self) {
        let next_kind = self.peek_kind_at(self.index + 1).cloned();
        let context = self.pop_context_of_kind(ContextKind::Brace(BraceKind::Collection));
        let context = context.expect("brace close should have a matching context");
        self.prepare_close(&context);
        self.write_raw("}");
        self.prev_kind = Some(PrevKind::RBrace);
        self.pending_code_break = false;

        if matches!(
            context.kind,
            ContextKind::Brace(BraceKind::Block | BraceKind::StructBody)
        ) && self.should_break_after_block(next_kind.as_ref())
        {
            self.pending_code_break = true;
            self.at_stmt_start = true;
        } else {
            self.at_stmt_start = false;
        }
    }

    fn emit_close_delimiter(&mut self, fallback_kind: ContextKind, text: &str) {
        let context = self.pop_context_of_kind(fallback_kind);
        let context = context.expect("close delimiter should have a matching context");
        self.prepare_close(&context);
        self.write_raw(text);
        self.prev_kind = Some(match text {
            ")" => PrevKind::RParen,
            "]" => PrevKind::RBracket,
            _ => PrevKind::Word,
        });
        self.pending_code_break = false;
        self.at_stmt_start = false;
    }

    fn prepare_close(&mut self, context: &Context) {
        if context.indented {
            self.indent = self.indent.saturating_sub(1);
            if !self.line_start
                && !matches!(
                    self.prev_kind,
                    Some(PrevKind::LBrace | PrevKind::LBracket | PrevKind::LParen)
                )
            {
                self.request_newline(1);
            }
        }
        self.clear_pending_space();
    }

    fn emit_pipe(&mut self) {
        self.write_raw("|");
        self.prev_kind = Some(PrevKind::Pipe);
        if self.in_closure_params {
            self.in_closure_params = false;
            self.request_space();
        } else {
            self.in_closure_params = true;
        }
        self.at_stmt_start = false;
    }

    fn token_text(&self, index: usize) -> &str {
        let span = self.tokens[index].span;
        &self.source[span.lo..span.hi]
    }

    fn advance_to(&mut self, index: usize) {
        self.index = index;
    }

    fn push_brace_expectation(&mut self, kind: BraceKind) {
        self.brace_expectations.push(BraceExpectation {
            kind,
            depths: self.current_depths(),
        });
    }

    fn resolve_brace_kind(&mut self) -> BraceKind {
        let current_depths = self.current_depths();
        if let Some(position) = self
            .brace_expectations
            .iter()
            .rposition(|expectation| expectation.depths == current_depths)
        {
            let expectation = self.brace_expectations.remove(position);
            return expectation.kind;
        }
        BraceKind::Collection
    }

    fn pop_latest_block_expectation(&mut self) {
        if let Some(position) = self
            .brace_expectations
            .iter()
            .rposition(|expectation| expectation.kind == BraceKind::Block)
        {
            self.brace_expectations.remove(position);
        }
    }

    fn current_depths(&self) -> DelimiterDepths {
        let mut depths = DelimiterDepths::default();
        for context in &self.contexts {
            match context.kind {
                ContextKind::Brace(_) => depths.brace += 1,
                ContextKind::Paren { .. } => depths.paren += 1,
                ContextKind::Bracket => depths.bracket += 1,
            }
        }
        depths
    }

    fn should_break_after_block(&self, next_kind: Option<&TokenKind>) -> bool {
        !matches!(
            next_kind,
            None | Some(TokenKind::Else)
                | Some(TokenKind::Semicolon)
                | Some(TokenKind::Comma)
                | Some(TokenKind::Dot)
                | Some(TokenKind::Question)
                | Some(TokenKind::LParen)
                | Some(TokenKind::LBracket)
                | Some(TokenKind::Plus)
                | Some(TokenKind::Minus)
                | Some(TokenKind::Star)
                | Some(TokenKind::Slash)
                | Some(TokenKind::Percent)
                | Some(TokenKind::EqualEqual)
                | Some(TokenKind::BangEqual)
                | Some(TokenKind::Less)
                | Some(TokenKind::LessEqual)
                | Some(TokenKind::Greater)
                | Some(TokenKind::GreaterEqual)
                | Some(TokenKind::AmpersandAmpersand)
                | Some(TokenKind::PipePipe)
                | Some(TokenKind::PlusEqual)
        )
    }

    fn pop_context_of_kind(&mut self, fallback_kind: ContextKind) -> Option<Context> {
        let context = self.contexts.pop()?;
        match (context.kind, fallback_kind) {
            (ContextKind::Brace(_), ContextKind::Brace(_))
            | (ContextKind::Bracket, ContextKind::Bracket)
            | (ContextKind::Paren { .. }, ContextKind::Paren { .. }) => Some(context),
            _ => Some(context),
        }
    }

    fn should_space_before_word(&self) -> bool {
        matches!(
            self.prev_kind,
            Some(PrevKind::Word | PrevKind::RParen | PrevKind::RBracket | PrevKind::RBrace)
        )
    }

    fn in_for_head_context(&self) -> bool {
        matches!(
            self.contexts.last().map(|context| context.kind),
            Some(ContextKind::Paren { for_head: true })
        )
    }

    fn in_slice_context(&self) -> bool {
        matches!(
            self.contexts.last().map(|context| context.kind),
            Some(ContextKind::Bracket)
        )
    }

    fn next_is_closer(&self, index: usize) -> bool {
        matches!(
            self.peek_kind_at(index),
            Some(TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace)
        )
    }

    fn should_emit_keyword_as_word(&self, kind: &TokenKind) -> bool {
        matches!(
            self.prev_kind,
            Some(PrevKind::PathSeparator | PrevKind::Dot | PrevKind::OptionalAccess)
        ) && matches!(
            kind,
            TokenKind::Pub
                | TokenKind::Use
                | TokenKind::Import
                | TokenKind::From
                | TokenKind::As
                | TokenKind::Fn
                | TokenKind::Struct
                | TokenKind::Let
                | TokenKind::For
                | TokenKind::If
                | TokenKind::Else
                | TokenKind::Match
                | TokenKind::While
                | TokenKind::Break
                | TokenKind::Continue
        )
    }

    fn should_tighten_before_open_bracket(&self) -> bool {
        matches!(
            self.prev_kind,
            Some(
                PrevKind::Word
                    | PrevKind::RParen
                    | PrevKind::RBracket
                    | PrevKind::RBrace
                    | PrevKind::Dot
                    | PrevKind::OptionalAccess
            )
        )
    }

    fn current_context_breaks_after_commas(&self) -> bool {
        matches!(
            self.contexts.last(),
            Some(Context {
                kind: ContextKind::Paren { .. },
                indented: true,
            }) | Some(Context {
                kind: ContextKind::Bracket,
                indented: true,
            }) | Some(Context {
                kind: ContextKind::Brace(BraceKind::Collection),
                indented: true,
            })
        )
    }

    fn previous_token_kind(&self) -> Option<&TokenKind> {
        self.index
            .checked_sub(1)
            .and_then(|index| self.peek_kind_at(index))
    }

    fn peek_kind_at(&self, index: usize) -> Option<&TokenKind> {
        self.tokens.get(index).map(|token| &token.kind)
    }

    fn starts_generic_angle_group(&self, less_index: usize) -> bool {
        if !matches!(self.peek_kind_at(less_index), Some(TokenKind::Less)) {
            return false;
        }

        let Some(close_index) = self.find_generic_angle_group_close(less_index) else {
            return false;
        };

        let next_kind = self.peek_kind_at(close_index + 1);
        if self.prev_kind == Some(PrevKind::PathSeparator) {
            return matches!(next_kind, Some(TokenKind::LParen));
        }

        matches!(
            next_kind,
            Some(
                TokenKind::LParen
                    | TokenKind::LBrace
                    | TokenKind::LBracket
                    | TokenKind::Equal
                    | TokenKind::Comma
                    | TokenKind::RParen
                    | TokenKind::RBracket
                    | TokenKind::RBrace
                    | TokenKind::Greater
                    | TokenKind::Semicolon
            )
        )
    }

    fn find_generic_angle_group_close(&self, less_index: usize) -> Option<usize> {
        let mut angle_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut saw_content = false;

        for index in less_index + 1..self.tokens.len() {
            match self.peek_kind_at(index)? {
                TokenKind::Ident(_) | TokenKind::Null => {
                    saw_content = true;
                }
                TokenKind::Comma | TokenKind::Ellipsis => {}
                TokenKind::LBracket => {
                    bracket_depth += 1;
                }
                TokenKind::RBracket => {
                    if bracket_depth == 0 {
                        return None;
                    }
                    bracket_depth -= 1;
                }
                TokenKind::Less => {
                    angle_depth += 1;
                }
                TokenKind::Greater => {
                    if bracket_depth > 0 {
                        return None;
                    }
                    if angle_depth == 0 {
                        return saw_content.then_some(index);
                    }
                    angle_depth -= 1;
                }
                _ => return None,
            }
        }

        None
    }

    fn should_collapse_gap_line_break(&self) -> bool {
        if self.pending_code_break {
            return false;
        }
        if self.generic_angle_depth > 0 {
            return true;
        }

        let previous = self.previous_token_kind();
        let next = self.peek_kind_at(self.index);

        matches!(previous, Some(kind) if Self::is_continuation_operator(kind))
            || matches!(next, Some(kind) if Self::is_continuation_operator(kind))
            || self.previous_token_is_return_keyword() && self.return_has_inline_value(next)
    }

    fn previous_token_is_return_keyword(&self) -> bool {
        matches!(
            self.previous_token_kind(),
            Some(TokenKind::Ident(name)) if name == "return"
        )
    }

    fn return_has_inline_value(&self, next: Option<&TokenKind>) -> bool {
        !matches!(
            next,
            None | Some(TokenKind::Semicolon | TokenKind::RBrace | TokenKind::Eof)
        )
    }

    fn is_continuation_operator(kind: &TokenKind) -> bool {
        matches!(
            kind,
            TokenKind::Plus
                | TokenKind::PlusEqual
                | TokenKind::Minus
                | TokenKind::Star
                | TokenKind::Slash
                | TokenKind::Percent
                | TokenKind::EqualEqual
                | TokenKind::BangEqual
                | TokenKind::Less
                | TokenKind::LessEqual
                | TokenKind::Greater
                | TokenKind::GreaterEqual
                | TokenKind::AmpersandAmpersand
                | TokenKind::PipePipe
        )
    }

    fn is_unary_position(&self) -> bool {
        !matches!(
            self.prev_kind,
            Some(PrevKind::Word | PrevKind::RParen | PrevKind::RBracket | PrevKind::RBrace)
        )
    }

    fn fat_arrow_is_if_branch(&self, index: usize) -> bool {
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut brace_depth = 0usize;
        let mut cursor = index;

        while cursor > 0 {
            cursor -= 1;
            match self.tokens[cursor].kind {
                TokenKind::RParen => paren_depth += 1,
                TokenKind::LParen => {
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                        continue;
                    }
                    paren_depth = paren_depth.saturating_sub(1);
                }
                TokenKind::RBracket => bracket_depth += 1,
                TokenKind::LBracket => {
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                        continue;
                    }
                    bracket_depth = bracket_depth.saturating_sub(1);
                }
                TokenKind::RBrace => brace_depth += 1,
                TokenKind::LBrace => {
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                        return false;
                    }
                    brace_depth = brace_depth.saturating_sub(1);
                }
                TokenKind::If | TokenKind::Else
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    return true;
                }
                TokenKind::Comma | TokenKind::Semicolon
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    return false;
                }
                _ => {}
            }
        }

        false
    }

    fn check_if_expression_start(&self, index: usize) -> bool {
        if !matches!(self.peek_kind_at(index), Some(TokenKind::If)) {
            return false;
        }

        let mut cursor = index + 1;
        let mut paren_depth = 0usize;
        let mut bracket_depth = 0usize;
        let mut brace_depth = 0usize;

        while let Some(token) = self.tokens.get(cursor) {
            match token.kind {
                TokenKind::LParen => paren_depth += 1,
                TokenKind::RParen => {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                TokenKind::LBracket => bracket_depth += 1,
                TokenKind::RBracket => {
                    bracket_depth = bracket_depth.saturating_sub(1);
                }
                TokenKind::LBrace => {
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
                        return false;
                    }
                    brace_depth += 1;
                }
                TokenKind::RBrace => {
                    if brace_depth == 0 {
                        return false;
                    }
                    brace_depth -= 1;
                }
                TokenKind::FatArrow
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    return true;
                }
                TokenKind::Semicolon | TokenKind::Eof
                    if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
                {
                    return false;
                }
                _ => {}
            }
            cursor += 1;
        }

        false
    }

    fn note_context_newline(&mut self) {
        if let Some(context) = self.contexts.last_mut()
            && !context.indented
            && !matches!(
                context.kind,
                ContextKind::Brace(BraceKind::Block | BraceKind::MatchBody | BraceKind::StructBody)
            )
        {
            context.indented = true;
            self.indent += 1;
        }
    }

    fn should_force_multiline_group(&self, open_index: usize, root_kind: ContextKind) -> bool {
        let Some((close_index, comma_count)) =
            self.find_group_close_and_top_level_commas(open_index, root_kind)
        else {
            return false;
        };
        if comma_count == 0 {
            return false;
        }

        let open_span = self.tokens[open_index].span;
        let close_span = self.tokens[close_index].span;
        let group = &self.source[open_span.lo..close_span.hi];
        let estimated_width = Self::normalized_inline_width(group) + comma_count;
        self.current_line_len_with_pending_prefix() + estimated_width > SOFT_MAX_LINE_WIDTH
    }

    fn find_group_close_and_top_level_commas(
        &self,
        open_index: usize,
        root_kind: ContextKind,
    ) -> Option<(usize, usize)> {
        let mut depths = Self::root_depths_for_kind(root_kind);
        let root_depths = depths;
        let mut comma_count = 0usize;

        for index in open_index + 1..self.tokens.len() {
            if depths == root_depths && matches!(self.peek_kind_at(index), Some(TokenKind::Comma)) {
                comma_count += 1;
            }

            match self.peek_kind_at(index)? {
                TokenKind::LParen => depths.paren += 1,
                TokenKind::RParen => {
                    if matches!(root_kind, ContextKind::Paren { .. }) && depths == root_depths {
                        return Some((index, comma_count));
                    }
                    depths.paren = depths.paren.saturating_sub(1);
                }
                TokenKind::LBracket => depths.bracket += 1,
                TokenKind::RBracket => {
                    if matches!(root_kind, ContextKind::Bracket) && depths == root_depths {
                        return Some((index, comma_count));
                    }
                    depths.bracket = depths.bracket.saturating_sub(1);
                }
                TokenKind::LBrace => depths.brace += 1,
                TokenKind::RBrace => {
                    if matches!(root_kind, ContextKind::Brace(_)) && depths == root_depths {
                        return Some((index, comma_count));
                    }
                    depths.brace = depths.brace.saturating_sub(1);
                }
                TokenKind::Eof => return None,
                _ => {}
            }
        }

        None
    }

    fn root_depths_for_kind(kind: ContextKind) -> DelimiterDepths {
        match kind {
            ContextKind::Paren { .. } => DelimiterDepths {
                paren: 1,
                ..DelimiterDepths::default()
            },
            ContextKind::Bracket => DelimiterDepths {
                bracket: 1,
                ..DelimiterDepths::default()
            },
            ContextKind::Brace(_) => DelimiterDepths {
                brace: 1,
                ..DelimiterDepths::default()
            },
        }
    }

    fn normalized_inline_width(text: &str) -> usize {
        let mut width = 0usize;
        let mut pending_space = false;

        for ch in text.chars() {
            if ch.is_whitespace() {
                pending_space = width > 0;
                continue;
            }

            if pending_space {
                width += 1;
                pending_space = false;
            }
            width += 1;
        }

        width
    }

    fn current_line_len_with_pending_prefix(&self) -> usize {
        let mut len = if self.pending_newlines > 0 {
            0
        } else {
            self.line_len
        };
        if self.pending_newlines > 0 || self.line_start {
            len += self.indent * 4;
        }
        if self.pending_space && len > 0 {
            len += 1;
        }
        len
    }

    fn request_space(&mut self) {
        if !self.line_start {
            self.pending_space = true;
        }
    }

    fn clear_pending_space(&mut self) {
        self.pending_space = false;
    }

    fn request_newline(&mut self, count: usize) {
        self.pending_newlines = self.pending_newlines.max(count.min(2));
        self.pending_space = false;
    }

    fn write_raw(&mut self, text: &str) {
        self.flush_pending_prefix();
        if self.line_start {
            let indent = "    ".repeat(self.indent);
            self.line_len = indent.len();
            self.out.push_str(&indent);
            self.line_start = false;
        }
        self.out.push_str(text);
        self.pending_space = false;
        if let Some(last_newline) = text.rfind('\n') {
            self.line_start = text.ends_with('\n');
            self.line_len = if self.line_start {
                0
            } else {
                text[last_newline + 1..].chars().count()
            };
        } else {
            self.line_len += text.chars().count();
            self.line_start = false;
        }
    }

    fn flush_pending_prefix(&mut self) {
        if self.pending_newlines > 0 {
            self.trim_trailing_spaces();
            for _ in 0..self.pending_newlines {
                self.out.push('\n');
            }
            self.pending_newlines = 0;
            self.line_start = true;
            self.line_len = 0;
        } else if self.pending_space && !self.line_start && !self.out.ends_with(' ') {
            self.out.push(' ');
            self.pending_space = false;
            self.line_len += 1;
        }
    }

    fn trim_trailing_spaces(&mut self) {
        while self.out.ends_with(' ') || self.out.ends_with('\t') {
            self.out.pop();
            self.line_len = self.line_len.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_source;
    use crate::compiler::SourceFlavor;
    use crate::compiler::frontends;

    #[test]
    fn collapses_binary_operator_breaks_in_tail_expressions() {
        let input = "fn mix(seed) {\n    v\n        +\n        seed\n}\n";
        let formatted = format_source(
            input,
            frontends::parser_dialect_for_flavor(SourceFlavor::RustScript)
                .expect("rustscript formatter dialect should exist"),
        )
        .expect("formatting should succeed");

        assert_eq!(formatted, "fn mix(seed) {\n    v + seed\n}\n");
    }

    #[test]
    fn formats_javascript_numeric_update_operators() {
        let input = "let total=0;\ntotal+=1;\nlet before=total++;\nlet after=++total;\n";
        let formatted = format_source(
            input,
            frontends::parser_dialect_for_flavor(SourceFlavor::JavaScript)
                .expect("javascript formatter dialect should exist"),
        )
        .expect("formatting should succeed");

        assert_eq!(
            formatted,
            "let total = 0;\ntotal += 1;\nlet before = total++;\nlet after = ++total;\n"
        );
    }
}
