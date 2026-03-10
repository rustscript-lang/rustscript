use super::*;

impl Parser {
    pub(super) fn expect(&mut self, kind: &TokenKind, message: &str) -> Result<(), ParseError> {
        if self.match_kind(kind) {
            Ok(())
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    pub(super) fn expect_ident(&mut self, message: &str) -> Result<String, ParseError> {
        if let Some(name) = self.match_ident() {
            Ok(name)
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    pub(super) fn expect_string_literal(&mut self, message: &str) -> Result<String, ParseError> {
        if let Some(value) = self.match_string() {
            Ok(value)
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    pub(super) fn expect_namespace_segment(&mut self, message: &str) -> Result<String, ParseError> {
        if let Some(name) = self.match_namespace_segment() {
            Ok(name)
        } else {
            Err(ParseError {
                line: self.current_line(),
                message: message.to_string(),
                span: Some(self.current_span()),
                code: None,
            })
        }
    }

    pub(super) fn match_kind(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn match_return_type_arrow(&mut self) -> bool {
        if self.check(&TokenKind::Minus) && self.check_kind_at(self.pos + 1, &TokenKind::Greater) {
            self.pos += 2;
            true
        } else {
            false
        }
    }

    pub(super) fn check_path_separator(&self) -> bool {
        matches!(
            (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)),
            (
                Some(Token {
                    kind: TokenKind::Colon,
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Colon,
                    ..
                })
            )
        )
    }

    pub(super) fn match_path_separator(&mut self) -> bool {
        if self.check_path_separator() {
            self.pos += 2;
            true
        } else {
            false
        }
    }

    pub(super) fn match_int(&mut self) -> Option<i64> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::Int(value),
                ..
            }) => {
                let value = *value;
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    pub(super) fn match_int_min_magnitude(&mut self) -> Option<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::IntMinMagnitude(value),
                ..
            }) => {
                let value = value.clone();
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    pub(super) fn match_float(&mut self) -> Option<f64> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::Float(value),
                ..
            }) => {
                let value = *value;
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    pub(super) fn match_ident(&mut self) -> Option<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::Ident(name),
                ..
            }) => {
                let name = name.clone();
                self.pos += 1;
                Some(name)
            }
            _ => None,
        }
    }

    pub(super) fn match_namespace_segment(&mut self) -> Option<String> {
        if let Some(name) = self.match_ident() {
            return Some(name);
        }
        if self.match_kind(&TokenKind::Match) {
            return Some("match".to_string());
        }
        None
    }

    pub(super) fn match_string(&mut self) -> Option<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: TokenKind::String(value),
                ..
            }) => {
                let value = value.clone();
                self.pos += 1;
                Some(value)
            }
            _ => None,
        }
    }

    pub(super) fn check_kind_at(&self, index: usize, kind: &TokenKind) -> bool {
        matches!(
            self.tokens.get(index),
            Some(token) if std::mem::discriminant(&token.kind) == std::mem::discriminant(kind)
        )
    }

    pub(super) fn check_ident_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index),
            Some(Token {
                kind: TokenKind::Ident(_),
                ..
            })
        )
    }

    pub(super) fn check_string_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index),
            Some(Token {
                kind: TokenKind::String(_),
                ..
            })
        )
    }

    pub(super) fn check_ident_literal_at(&self, index: usize, literal: &str) -> bool {
        matches!(
            self.tokens.get(index),
            Some(Token {
                kind: TokenKind::Ident(name),
                ..
            }) if name == literal
        )
    }

    pub(super) fn check_ident_literal(&self, literal: &str) -> bool {
        self.check_ident_literal_at(self.pos, literal)
    }

    pub(super) fn reject_out_of_range_int_literal(&self) -> Result<(), ParseError> {
        let Some(Token {
            kind: TokenKind::IntMinMagnitude(text),
            ..
        }) = self.tokens.get(self.pos)
        else {
            return Ok(());
        };
        Err(ParseError {
            span: Some(self.current_span()),
            code: None,
            line: self.current_line(),
            message: format!(
                "integer literal '{text}' is out of range for i64; write '-{text}' for i64::MIN"
            ),
        })
    }

    pub(super) fn match_ident_literal(&mut self, literal: &str) -> bool {
        if self.check_ident_literal(literal) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn check_assignment_start(&self) -> bool {
        matches!(
            (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)),
            (
                Some(Token {
                    kind: TokenKind::Ident(_),
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Equal,
                    ..
                })
            )
        )
    }

    pub(super) fn consume_stmt_terminator(&mut self, message: &str) -> Result<(), ParseError> {
        if self.match_kind(&TokenKind::Semicolon) {
            return Ok(());
        }
        if !self.allow_implicit_semicolons {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: message.to_string(),
            });
        }

        if self.check(&TokenKind::RBrace) || self.check(&TokenKind::Eof) {
            return Ok(());
        }

        let previous_line = self
            .tokens
            .get(self.pos.saturating_sub(1))
            .map(|token| token.line)
            .unwrap_or(1);
        if self.current_line() > previous_line {
            return Ok(());
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: message.to_string(),
        })
    }

    pub(super) fn starts_trailing_expr_block_statement(&self) -> bool {
        if self.check(&TokenKind::Pub)
            || self.check(&TokenKind::Use)
            || (self.dialect.allow_import_stmt() && self.check(&TokenKind::Import))
            || self.check(&TokenKind::Fn)
            || self.check(&TokenKind::Let)
            || self.check(&TokenKind::For)
            || self.check(&TokenKind::While)
            || self.check(&TokenKind::Break)
            || self.check(&TokenKind::Continue)
            || (self.dialect.allow_return_stmt() && self.check_ident_literal("return"))
            || self.check_assignment_start()
            || self.check_index_assignment_start()
        {
            return true;
        }
        self.check(&TokenKind::If) && !self.check_if_expression_start()
    }

    pub(super) fn check_if_expression_start(&self) -> bool {
        if !self.check(&TokenKind::If) {
            return false;
        }
        let mut cursor = self.pos + 1;
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

    pub(super) fn check(&self, kind: &TokenKind) -> bool {
        matches!(self.peek_kind(), Some(k) if std::mem::discriminant(k) == std::mem::discriminant(kind))
    }

    pub(super) fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos).map(|token| &token.kind)
    }

    pub(super) fn current_line(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|token| token.line)
            .unwrap_or(1)
    }

    pub(super) fn current_line_u32(&self) -> u32 {
        u32::try_from(self.current_line()).unwrap_or(u32::MAX)
    }

    pub(super) fn current_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|token| token.span)
            .unwrap_or_else(|| {
                self.tokens
                    .last()
                    .map(|token| Span::new(token.span.source_id, token.span.hi, token.span.hi))
                    .unwrap_or(Span::new(0, 0, 0))
            })
    }

    pub(super) fn last_line(&self) -> u32 {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map(|token| token.line)
            .unwrap_or(1) as u32
    }
}
