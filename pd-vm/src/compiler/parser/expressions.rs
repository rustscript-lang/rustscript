use super::*;

impl Parser {
    pub(super) fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    pub(super) fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_and()?;
        while self.match_kind(&TokenKind::PipePipe) {
            let rhs = self.parse_and()?;
            expr = Expr::Or(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    pub(super) fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_comparison()?;
        while self.match_kind(&TokenKind::AmpersandAmpersand) {
            let rhs = self.parse_comparison()?;
            expr = Expr::And(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    pub(super) fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_term()?;
        loop {
            if self.match_kind(&TokenKind::EqualEqual) {
                let rhs = self.parse_term()?;
                expr = Expr::Eq(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::BangEqual) {
                let rhs = self.parse_term()?;
                expr = Expr::Not(Box::new(Expr::Eq(Box::new(expr), Box::new(rhs))));
            } else if self.match_kind(&TokenKind::Less) {
                let rhs = self.parse_term()?;
                expr = Expr::Lt(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::LessEqual) {
                let rhs = self.parse_term()?;
                expr = self.build_non_strict_comparison(expr, rhs, Expr::Lt)?;
            } else if self.match_kind(&TokenKind::Greater) {
                let rhs = self.parse_term()?;
                expr = Expr::Gt(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::GreaterEqual) {
                let rhs = self.parse_term()?;
                expr = self.build_non_strict_comparison(expr, rhs, Expr::Gt)?;
            } else {
                break;
            }
        }
        Ok(expr)
    }

    pub(super) fn build_non_strict_comparison(
        &mut self,
        lhs: Expr,
        rhs: Expr,
        build_strict: fn(Box<Expr>, Box<Expr>) -> Expr,
    ) -> Result<Expr, ParseError> {
        let lhs_slot = self.allocate_hidden_local()?;
        let rhs_slot = self.allocate_hidden_local()?;
        let line = self.last_line();
        let lhs_var = Expr::Var(lhs_slot);
        let rhs_var = Expr::Var(rhs_slot);
        Ok(Expr::Block {
            stmts: vec![
                Stmt::Let {
                    index: lhs_slot,
                    expr: lhs,
                    line,
                },
                Stmt::Let {
                    index: rhs_slot,
                    expr: rhs,
                    line,
                },
            ],
            expr: Box::new(Expr::Or(
                Box::new(build_strict(
                    Box::new(lhs_var.clone()),
                    Box::new(rhs_var.clone()),
                )),
                Box::new(Expr::Eq(Box::new(lhs_var), Box::new(rhs_var))),
            )),
        })
    }

    pub(super) fn parse_term(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_factor()?;
        loop {
            if self.match_kind(&TokenKind::Plus) {
                let rhs = self.parse_factor()?;
                expr = Expr::Add(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::Minus) {
                let rhs = self.parse_factor()?;
                expr = Expr::Sub(Box::new(expr), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    pub(super) fn parse_factor(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_unary()?;
        loop {
            if self.match_kind(&TokenKind::Star) {
                let rhs = self.parse_unary()?;
                expr = Expr::Mul(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::Slash) {
                let rhs = self.parse_unary()?;
                expr = Expr::Div(Box::new(expr), Box::new(rhs));
            } else if self.match_kind(&TokenKind::Percent) {
                let rhs = self.parse_unary()?;
                expr = Expr::Mod(Box::new(expr), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    pub(super) fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::Ampersand) {
            if self.match_ident_literal("mut") {
                let inner = self.parse_unary()?;
                self.require_mut_borrow_target(&inner)?;
                self.require_mut_borrow_binding_mutable(&inner)?;
                return Ok(Expr::BorrowMut(Box::new(inner)));
            }
            let inner = self.parse_unary()?;
            return Ok(Expr::Borrow(Box::new(inner)));
        }
        if self.dialect.allow_typeof_operator() && self.match_ident_literal("typeof") {
            let inner = self.parse_unary()?;
            return self.build_builtin_call_expr(BuiltinFunction::TypeOf, vec![inner]);
        }
        if self.match_kind(&TokenKind::Minus) {
            if let Some(text) = self.match_int_min_magnitude() {
                let _ = text;
                return Ok(Expr::Int(i64::MIN));
            }
            let inner = self.parse_unary()?;
            Ok(Expr::Neg(Box::new(inner)))
        } else if self.match_kind(&TokenKind::Bang) {
            let inner = self.parse_unary()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }

    pub(super) fn require_mut_borrow_target(&self, expr: &Expr) -> Result<(), ParseError> {
        if self.is_mut_borrow_target(expr) {
            return Ok(());
        }
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "mutable borrow target must be a local, local field, or local index"
                .to_string(),
        })
    }

    pub(super) fn is_mut_borrow_target(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Var(_) => true,
            Expr::Call(index, args) => {
                if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Get)
                    || args.len() != 2
                {
                    return false;
                }
                matches!(args.first(), Some(Expr::Var(_)))
            }
            _ => false,
        }
    }

    pub(super) fn require_mut_borrow_binding_mutable(&self, expr: &Expr) -> Result<(), ParseError> {
        let Some(root_slot) = self.extract_mut_borrow_root_slot(expr) else {
            return Ok(());
        };
        self.require_local_mutable_for_operation(
            root_slot,
            None,
            self.current_line_u32(),
            "take a mutable borrow of",
        )
    }

    pub(super) fn extract_mut_borrow_root_slot(&self, expr: &Expr) -> Option<LocalSlot> {
        match expr {
            Expr::Var(slot) => Some(*slot),
            Expr::Call(index, args) => {
                if BuiltinFunction::from_call_index(*index) != Some(BuiltinFunction::Get)
                    || args.len() != 2
                {
                    return None;
                }
                match args.first() {
                    Some(Expr::Var(slot)) => Some(*slot),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(super) fn require_local_mutable_for_operation(
        &self,
        index: LocalSlot,
        name_hint: Option<&str>,
        line: u32,
        action: &str,
    ) -> Result<(), ParseError> {
        if !self.enforce_mutable_bindings || self.is_local_slot_mutable(index) {
            return Ok(());
        }
        let display = name_hint
            .map(str::to_string)
            .or_else(|| self.find_local_name_by_slot(index))
            .unwrap_or_else(|| format!("#{index}"));
        Err(ParseError {
            span: None,
            code: Some("E_IMMUTABLE_LOCAL".to_string()),
            line: line as usize,
            message: format!(
                "cannot {action} immutable local '{display}'; declare it as 'let mut {display} = ...'"
            ),
        })
    }

    pub(super) fn apply_let_binding_mutability(
        &mut self,
        index: LocalSlot,
        declared_mutable: bool,
        created: bool,
    ) {
        if !self.enforce_mutable_bindings {
            return;
        }
        if created {
            self.set_local_slot_mutable(index, declared_mutable);
            return;
        }
        if declared_mutable {
            self.set_local_slot_mutable(index, true);
        }
    }

    pub(super) fn is_local_slot_mutable(&self, index: LocalSlot) -> bool {
        self.mutable_locals
            .get(index as usize)
            .copied()
            .unwrap_or(true)
    }

    pub(super) fn set_local_slot_mutable(&mut self, index: LocalSlot, is_mutable: bool) {
        let slot = index as usize;
        if slot >= self.mutable_locals.len() {
            self.mutable_locals.resize(slot + 1, true);
        }
        self.mutable_locals[slot] = is_mutable;
    }

    pub(super) fn find_local_name_by_slot(&self, index: LocalSlot) -> Option<String> {
        for scope in self.closure_scopes.iter().rev() {
            if let Some((name, _)) = scope.iter().find(|(_, slot)| **slot == index) {
                return Some(name.clone());
            }
        }
        self.locals
            .iter()
            .find(|(_, slot)| **slot == index)
            .map(|(name, _)| name.clone())
    }

    pub(super) fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::If) {
            return self.parse_if_expr();
        }
        if self.match_kind(&TokenKind::Match) {
            return self.parse_match_expr();
        }
        if self.match_kind(&TokenKind::True) {
            return Ok(Expr::Bool(true));
        }
        if self.match_kind(&TokenKind::False) {
            return Ok(Expr::Bool(false));
        }
        if self.match_kind(&TokenKind::Null) {
            return Ok(Expr::Null);
        }
        self.reject_out_of_range_int_literal()?;
        if let Some(value) = self.match_int() {
            return Ok(Expr::Int(value));
        }
        if let Some(value) = self.match_float() {
            return Ok(Expr::Float(value));
        }
        if let Some(value) = self.match_string() {
            return Ok(Expr::String(value));
        }
        if self.match_kind(&TokenKind::Pipe) {
            return self.parse_closure_literal();
        }
        if self.match_kind(&TokenKind::PipePipe) {
            return self.parse_closure_expr_with_params(Vec::new());
        }
        if self.dialect.allow_arrow_closure() && self.check_parenthesized_arrow_closure_start() {
            return self.parse_parenthesized_arrow_closure();
        }
        if self.dialect.allow_arrow_closure() && self.check_single_param_arrow_closure_start() {
            return self.parse_single_param_arrow_closure();
        }
        if let Some(name) = self.match_ident() {
            if self.dialect.allow_dotted_call()
                && let Some(expr) = self.try_parse_js_dotted_call(&name)?
            {
                return Ok(expr);
            }
            if !self.dialect.allow_namespace_path_separator() && self.check_path_separator() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "namespace calls use '.' in this language (for example 'json.encode(...)'), not '::'"
                        .to_string(),
                });
            }
            if self.dialect.allow_namespace_path_separator() && self.match_path_separator() {
                let mut path_segments = Vec::new();
                path_segments
                    .push(self.expect_namespace_segment("expected function name after '::'")?);
                while self.match_path_separator() {
                    path_segments
                        .push(self.expect_namespace_segment("expected function name after '::'")?);
                }
                self.expect(
                    &TokenKind::LParen,
                    "expected '(' after namespaced function name",
                )?;
                let args = self.parse_call_args()?;
                let member = path_segments
                    .first()
                    .ok_or_else(|| ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "expected function name after '::'".to_string(),
                    })?
                    .clone();
                let subpath = path_segments
                    .get(1..)
                    .map(|tail| tail.to_vec())
                    .unwrap_or_default();
                if let Some((builtin_namespace, builtin_member)) =
                    self.resolve_builtins_call_path(&name, &member, &subpath)
                {
                    let builtin_namespace = builtin_namespace.to_string();
                    let builtin_member = builtin_member.to_string();
                    if let Some(builtin) =
                        resolve_builtin_namespace_call(&builtin_namespace, &builtin_member)
                    {
                        let expr = self.build_builtin_call_expr(builtin, args)?;
                        return Ok(expr);
                    }
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: format!(
                            "unknown builtin function '{}::{}'",
                            builtin_namespace, builtin_member
                        ),
                    });
                }
                let host_name = self
                    .resolve_host_namespace_call_target(&name, &member, &subpath)
                    .ok_or_else(|| ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: format!(
                            "unknown namespace call '{}::{}'; import builtin namespaces first ({}), and for host calls import the matching host namespace or alias",
                            name,
                            path_segments.join("::"),
                            builtin_namespace_hint()
                        ),
                    })?;
                let expr = self.build_host_call_expr(&host_name, args)?;
                return Ok(expr);
            }

            let mut expr = if self.dialect.allow_macro_calls() && self.match_kind(&TokenKind::Bang)
            {
                self.parse_macro_call(&name)?
            } else if self.match_kind(&TokenKind::LParen) {
                let args = self.parse_call_args()?;
                if self.has_local_binding(&name) {
                    let local = self.get_local(&name)?;
                    Expr::LocalCall(local, args)
                } else if self.functions.contains_key(&name) {
                    let builtin_alias_call = if matches!(name.as_str(), "print" | "println") {
                        self.functions
                            .get(&name)
                            .map(|decl| !self.function_impls.contains_key(&decl.index))
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    if builtin_alias_call {
                        if let Some(expr) = self.try_build_language_builtin_call(&name, &args)? {
                            expr
                        } else {
                            let decl = self.resolve_function_for_call(&name, args.len())?;
                            Expr::Call(decl.index, args)
                        }
                    } else {
                        let decl = self.resolve_function_for_call(&name, args.len())?;
                        Expr::Call(decl.index, args)
                    }
                } else if let Some(expr) = self.try_build_language_builtin_call(&name, &args)? {
                    expr
                } else if let Some(host_name) = self.resolve_direct_host_call_target(&name) {
                    self.build_host_call_expr(&host_name, args)?
                } else {
                    let decl = self.resolve_function_for_call(&name, args.len())?;
                    Expr::Call(decl.index, args)
                }
            } else if self.has_local_binding(&name) {
                let index = self.get_local(&name)?;
                Expr::Var(index)
            } else if let Some(decl) = self.functions.get(&name) {
                Expr::FunctionRef(decl.index)
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("unknown local '{name}'"),
                });
            };
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }
        if self.match_kind(&TokenKind::LParen) {
            let mut expr = self.parse_expr()?;
            self.expect(&TokenKind::RParen, "expected ')' after expression")?;
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }
        if self.match_kind(&TokenKind::LBracket) {
            let mut expr = self.parse_array_literal()?;
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }
        if self.match_kind(&TokenKind::LBrace) {
            let mut expr = self.parse_brace_literal()?;
            expr = self.parse_postfix_access(expr)?;
            return Ok(expr);
        }

        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "expected expression".to_string(),
        })
    }

    pub(super) fn parse_if_expr(&mut self) -> Result<Expr, ParseError> {
        let condition = self.parse_expr()?;
        self.expect(
            &TokenKind::FatArrow,
            "expected '=>' after if condition in expression form",
        )?;
        let then_expr = self.parse_if_expr_branch()?;
        self.expect(&TokenKind::Else, "if expression requires an else branch")?;
        let else_expr = if self.match_kind(&TokenKind::If) {
            self.parse_if_expr()?
        } else {
            self.expect(
                &TokenKind::FatArrow,
                "expected '=>' after else in expression form",
            )?;
            self.parse_if_expr_branch()?
        };
        Ok(Expr::IfElse {
            condition: Box::new(condition),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        })
    }

    pub(super) fn parse_if_expr_branch(&mut self) -> Result<Expr, ParseError> {
        self.expect(
            &TokenKind::LBrace,
            "expected '{' after '=>' in if expression branch",
        )?;

        let mut stmts = Vec::<Stmt>::new();
        let mut trailing_expr: Option<Expr> = None;
        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unexpected end of input in if expression branch".to_string(),
                });
            }

            if self.starts_trailing_expr_block_statement() {
                stmts.push(self.parse_stmt()?);
                continue;
            }

            let line = self.current_line_u32();
            let expr = self.parse_expr()?;
            if self.check(&TokenKind::RBrace) {
                trailing_expr = Some(expr);
                break;
            }
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after expression in if expression branch",
            )?;
            stmts.push(Stmt::Expr { expr, line });
        }

        self.expect(
            &TokenKind::RBrace,
            "expected '}' to close if expression branch",
        )?;

        let expr = if let Some(expr) = trailing_expr {
            expr
        } else {
            let Some(last_stmt) = stmts.pop() else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "if expression branch must end with an expression".to_string(),
                });
            };
            if let Stmt::Expr { expr, .. } = last_stmt {
                expr
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "if expression branch must end with an expression".to_string(),
                });
            }
        };

        if stmts.is_empty() {
            Ok(expr)
        } else {
            Ok(Expr::Block {
                stmts,
                expr: Box::new(expr),
            })
        }
    }

    pub(super) fn parse_match_expr(&mut self) -> Result<Expr, ParseError> {
        let value = self.parse_expr()?;
        self.expect(&TokenKind::LBrace, "expected '{' after match value")?;

        let value_slot = self.allocate_hidden_local()?;
        let result_slot = self.allocate_hidden_local()?;
        let mut arms = Vec::<(MatchPattern, Expr)>::new();
        let mut default: Option<Expr> = None;

        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unexpected end of input in match expression".to_string(),
                });
            }

            let pattern_token_line = self.current_line();
            let pattern = self.parse_match_pattern()?;
            self.expect(&TokenKind::FatArrow, "expected '=>' in match arm")?;
            let arm_expr = self.parse_expr()?;

            match pattern {
                Some(pattern) => {
                    if default.is_some() {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: pattern_token_line,
                            message: "non-wildcard match arm cannot appear after '_' arm"
                                .to_string(),
                        });
                    }
                    arms.push((pattern, arm_expr));
                }
                None => {
                    if default.is_some() {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: pattern_token_line,
                            message: "duplicate '_' match arm".to_string(),
                        });
                    }
                    default = Some(arm_expr);
                }
            }

            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            if !self.check(&TokenKind::RBrace) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "expected ',' or '}' after match arm".to_string(),
                });
            }
        }
        self.expect(&TokenKind::RBrace, "expected '}' after match expression")?;

        let default = default.ok_or_else(|| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "match expression requires a wildcard arm '_ => ...'".to_string(),
        })?;

        Ok(Expr::Match {
            value_slot,
            result_slot,
            value: Box::new(value),
            arms,
            default: Box::new(default),
        })
    }

    pub(super) fn parse_match_pattern(&mut self) -> Result<Option<MatchPattern>, ParseError> {
        if self.match_kind(&TokenKind::LParen) {
            let pattern = self.parse_match_pattern()?;
            self.expect(
                &TokenKind::RParen,
                "expected ')' after parenthesized match pattern",
            )?;
            return Ok(pattern);
        }
        if let Some(value) = self.match_int() {
            return Ok(Some(MatchPattern::Int(value)));
        }
        self.reject_out_of_range_int_literal()?;
        if let Some(value) = self.match_string() {
            return Ok(Some(MatchPattern::String(value)));
        }
        if self.match_kind(&TokenKind::Null) {
            return Ok(Some(MatchPattern::Null));
        }
        if let Some(name) = self.match_ident() {
            if name == "_" {
                return Ok(None);
            }
            if let Some(type_pattern) = self.parse_match_type_constructor_pattern(&name)? {
                return Ok(Some(MatchPattern::Type(type_pattern)));
            }
        }
        Err(ParseError { span: None, code: None,
            line: self.current_line(),
            message:
                "match patterns currently support int/string/null literals, type patterns via Some(TypeName), and '_'"
                    .to_string(),
        })
    }

    pub(super) fn parse_match_type_constructor_pattern(
        &mut self,
        head: &str,
    ) -> Result<Option<MatchTypePattern>, ParseError> {
        if head == "Some" {
            return self.parse_some_type_pattern();
        }
        Ok(None)
    }

    pub(super) fn parse_some_type_pattern(
        &mut self,
    ) -> Result<Option<MatchTypePattern>, ParseError> {
        self.expect(
            &TokenKind::LParen,
            "expected '(' after Some in match type pattern",
        )?;
        let type_name = self.expect_ident("expected type name inside Some(...)")?;
        let type_pattern = match_type_pattern_from_ident(&type_name).ok_or_else(|| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message:
                "unknown match type pattern; expected one of Int/Float/Number/Bool/String/Array/Map"
                    .to_string(),
        })?;
        self.expect(
            &TokenKind::RParen,
            "expected ')' after Some(TypeName) match pattern",
        )?;
        Ok(Some(type_pattern))
    }

    pub(super) fn parse_postfix_access(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        loop {
            if self.match_kind(&TokenKind::LBracket) {
                if self.match_kind(&TokenKind::Colon) {
                    let end = if self.check(&TokenKind::RBracket) {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect(&TokenKind::RBracket, "expected ']' after slice expression")?;
                    expr = self.build_slice_access_expr(expr, None, end)?;
                    continue;
                }

                let first = self.parse_expr()?;
                if self.match_kind(&TokenKind::Colon) {
                    let end = if self.check(&TokenKind::RBracket) {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect(&TokenKind::RBracket, "expected ']' after slice expression")?;
                    expr = self.build_slice_access_expr(expr, Some(first), end)?;
                    continue;
                }

                self.expect(&TokenKind::RBracket, "expected ']' after index expression")?;
                expr = self.build_builtin_call_expr(BuiltinFunction::Get, vec![expr, first])?;
                continue;
            }
            if self.match_kind(&TokenKind::Dot) {
                let member = self.expect_namespace_segment("expected member name after '.'")?;
                if member == "copy" {
                    self.expect(
                        &TokenKind::LParen,
                        "expected '(' after '.copy' (use '.copy()')",
                    )?;
                    self.expect(
                        &TokenKind::RParen,
                        "copy does not take arguments; use '.copy()'",
                    )?;
                    expr = Expr::ToOwned(Box::new(expr));
                } else if member == "length" {
                    expr = self.build_builtin_call_expr(BuiltinFunction::Len, vec![expr])?;
                } else if member == "keys" {
                    expr = self.build_builtin_call_expr(BuiltinFunction::Keys, vec![expr])?;
                } else {
                    expr = self.build_builtin_call_expr(
                        BuiltinFunction::Get,
                        vec![expr, Expr::String(member)],
                    )?;
                }
                continue;
            }
            if self.match_kind(&TokenKind::Question) {
                self.expect(&TokenKind::Dot, "expected '.' after '?' in optional access")?;
                if self.match_kind(&TokenKind::LBracket) {
                    let key = self.parse_expr()?;
                    self.expect(
                        &TokenKind::RBracket,
                        "expected ']' after optional index expression",
                    )?;
                    expr = self.build_optional_get_expr(expr, key)?;
                    continue;
                }
                let member = self.expect_namespace_segment("expected member name after '?.'")?;
                expr = self.build_optional_get_expr(expr, Expr::String(member))?;
                continue;
            }
            break;
        }
        Ok(expr)
    }

    pub(super) fn build_slice_access_expr(
        &mut self,
        container: Expr,
        start: Option<Expr>,
        end: Option<Expr>,
    ) -> Result<Expr, ParseError> {
        let (container_slot, container_bind) = match container {
            Expr::Var(slot) => (slot, None),
            other => (self.allocate_hidden_local()?, Some(other)),
        };
        let start_slot = self.allocate_hidden_local()?;
        let start_expr = start.unwrap_or(Expr::Int(0));

        let slice_len = if let Some(end_expr) = end {
            let end_slot = self.allocate_hidden_local()?;
            let end_var = Expr::Var(end_slot);
            let end_is_negative = Expr::Lt(Box::new(end_var.clone()), Box::new(Expr::Int(0)));
            let len_expr = self
                .build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
            let adjusted_end = Expr::IfElse {
                condition: Box::new(end_is_negative),
                then_expr: Box::new(Expr::Add(Box::new(len_expr), Box::new(end_var.clone()))),
                else_expr: Box::new(end_var),
            };
            let slice_len = Expr::Sub(Box::new(adjusted_end), Box::new(Expr::Var(start_slot)));
            let slice_expr = self.build_builtin_call_expr(
                BuiltinFunction::Slice,
                vec![Expr::Var(container_slot), Expr::Var(start_slot), slice_len],
            )?;
            let with_end = self.bind_hidden_local_expr(end_slot, end_expr, slice_expr)?;
            self.bind_hidden_local_expr(start_slot, start_expr, with_end)?
        } else {
            let end_expr = self
                .build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
            let slice_len = Expr::Sub(Box::new(end_expr), Box::new(Expr::Var(start_slot)));
            let slice_expr = self.build_builtin_call_expr(
                BuiltinFunction::Slice,
                vec![Expr::Var(container_slot), Expr::Var(start_slot), slice_len],
            )?;
            self.bind_hidden_local_expr(start_slot, start_expr, slice_expr)?
        };
        if let Some(container_expr) = container_bind {
            self.bind_hidden_local_expr(container_slot, container_expr, slice_len)
        } else {
            Ok(slice_len)
        }
    }

    pub(super) fn build_optional_get_expr(
        &mut self,
        container: Expr,
        key: Expr,
    ) -> Result<Expr, ParseError> {
        let container_slot = self.allocate_hidden_local()?;
        let key_slot = self.allocate_hidden_local()?;

        let is_null = self.build_type_check_expr(Expr::Var(container_slot), "null")?;
        let is_map = self.build_type_check_expr(Expr::Var(container_slot), "map")?;
        let is_array = self.build_type_check_expr(Expr::Var(container_slot), "array")?;
        let is_string = self.build_type_check_expr(Expr::Var(container_slot), "string")?;
        let map_lookup = self.build_optional_map_lookup_expr(container_slot, key_slot)?;
        let array_lookup = self.build_optional_index_lookup_expr(container_slot, key_slot)?;
        let string_lookup = self.build_optional_index_lookup_expr(container_slot, key_slot)?;
        let typed_lookup = Expr::IfElse {
            condition: Box::new(is_map),
            then_expr: Box::new(map_lookup),
            else_expr: Box::new(Expr::IfElse {
                condition: Box::new(is_array),
                then_expr: Box::new(array_lookup),
                else_expr: Box::new(Expr::IfElse {
                    condition: Box::new(is_string),
                    then_expr: Box::new(string_lookup),
                    else_expr: Box::new(Expr::Null),
                }),
            }),
        };
        let guarded = Expr::IfElse {
            condition: Box::new(is_null),
            then_expr: Box::new(Expr::Null),
            else_expr: Box::new(typed_lookup),
        };

        let key_bound = self.bind_hidden_local_expr(key_slot, key, guarded)?;
        self.bind_hidden_local_expr(container_slot, container, key_bound)
    }

    pub(super) fn build_type_check_expr(
        &mut self,
        value: Expr,
        expected: &str,
    ) -> Result<Expr, ParseError> {
        let value_type = self.build_builtin_call_expr(BuiltinFunction::TypeOf, vec![value])?;
        Ok(Expr::Eq(
            Box::new(value_type),
            Box::new(Expr::String(expected.to_string())),
        ))
    }

    pub(super) fn build_optional_map_lookup_expr(
        &mut self,
        container_slot: LocalSlot,
        key_slot: LocalSlot,
    ) -> Result<Expr, ParseError> {
        let set_probe = self.build_builtin_call_expr(
            BuiltinFunction::Set,
            vec![Expr::Var(container_slot), Expr::Var(key_slot), Expr::Null],
        )?;
        let set_probe_len = self.build_builtin_call_expr(BuiltinFunction::Len, vec![set_probe])?;
        let container_len =
            self.build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
        let key_present = Expr::Eq(Box::new(set_probe_len), Box::new(container_len));
        let value = self.build_builtin_call_expr(
            BuiltinFunction::Get,
            vec![Expr::Var(container_slot), Expr::Var(key_slot)],
        )?;
        Ok(Expr::IfElse {
            condition: Box::new(key_present),
            then_expr: Box::new(value),
            else_expr: Box::new(Expr::Null),
        })
    }

    pub(super) fn build_optional_index_lookup_expr(
        &mut self,
        container_slot: LocalSlot,
        key_slot: LocalSlot,
    ) -> Result<Expr, ParseError> {
        let key_is_int = self.build_type_check_expr(Expr::Var(key_slot), "int")?;
        let key_is_negative = Expr::Lt(Box::new(Expr::Var(key_slot)), Box::new(Expr::Int(0)));
        let container_len =
            self.build_builtin_call_expr(BuiltinFunction::Len, vec![Expr::Var(container_slot)])?;
        let key_in_bounds = Expr::Lt(Box::new(Expr::Var(key_slot)), Box::new(container_len));
        let value = self.build_builtin_call_expr(
            BuiltinFunction::Get,
            vec![Expr::Var(container_slot), Expr::Var(key_slot)],
        )?;
        let in_range_value = Expr::IfElse {
            condition: Box::new(key_in_bounds),
            then_expr: Box::new(value),
            else_expr: Box::new(Expr::Null),
        };
        let non_negative_value = Expr::IfElse {
            condition: Box::new(key_is_negative),
            then_expr: Box::new(Expr::Null),
            else_expr: Box::new(in_range_value),
        };
        Ok(Expr::IfElse {
            condition: Box::new(key_is_int),
            then_expr: Box::new(non_negative_value),
            else_expr: Box::new(Expr::Null),
        })
    }

    pub(super) fn bind_hidden_local_expr(
        &mut self,
        value_slot: LocalSlot,
        value: Expr,
        body: Expr,
    ) -> Result<Expr, ParseError> {
        let result_slot = self.allocate_hidden_local()?;
        Ok(Expr::Match {
            value_slot,
            result_slot,
            value: Box::new(value),
            arms: Vec::new(),
            default: Box::new(body),
        })
    }

    pub(super) fn parse_array_literal(&mut self) -> Result<Expr, ParseError> {
        let mut out = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
        if !self.check(&TokenKind::RBracket) {
            loop {
                let value = self.parse_expr()?;
                out = self.build_builtin_call_expr(BuiltinFunction::ArrayPush, vec![out, value])?;
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RBracket, "expected ']' after array literal")?;
        Ok(out)
    }

    pub(super) fn parse_brace_literal(&mut self) -> Result<Expr, ParseError> {
        if self.match_kind(&TokenKind::RBrace) {
            return self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new());
        }

        enum BraceLiteralEntry {
            Array(Expr),
            Map { key: Expr, value: Expr },
        }

        let mut entries = Vec::<BraceLiteralEntry>::new();
        let mut has_array_entries = false;
        let mut has_map_entries = false;

        loop {
            let is_map_entry = self.check_map_entry_start();
            if is_map_entry {
                has_map_entries = true;
                let key = if self.match_kind(&TokenKind::LBracket) {
                    let expr = self.parse_expr()?;
                    self.expect(
                        &TokenKind::RBracket,
                        "expected ']' after map key expression",
                    )?;
                    expr
                } else {
                    self.parse_map_key_literal()?
                };
                if !(self.match_kind(&TokenKind::Colon) || self.match_kind(&TokenKind::Equal)) {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "expected ':' or '=' after map key".to_string(),
                    });
                }
                let value = self.parse_expr()?;
                entries.push(BraceLiteralEntry::Map { key, value });
            } else {
                has_array_entries = true;
                let value = self.parse_expr()?;
                entries.push(BraceLiteralEntry::Array(value));
            }

            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }

        self.expect(&TokenKind::RBrace, "expected '}' after brace literal")?;

        if has_map_entries {
            let mut out = self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new())?;
            let mut next_array_index = 0i64;
            for entry in entries {
                match entry {
                    BraceLiteralEntry::Array(value) => {
                        out = self.build_builtin_call_expr(
                            BuiltinFunction::Set,
                            vec![out, Expr::Int(next_array_index), value],
                        )?;
                        next_array_index = next_array_index.saturating_add(1);
                    }
                    BraceLiteralEntry::Map { key, value } => {
                        out = self
                            .build_builtin_call_expr(BuiltinFunction::Set, vec![out, key, value])?;
                    }
                }
            }
            Ok(out)
        } else if has_array_entries {
            let mut out = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
            for entry in entries {
                if let BraceLiteralEntry::Array(value) = entry {
                    out =
                        self.build_builtin_call_expr(BuiltinFunction::ArrayPush, vec![out, value])?;
                }
            }
            Ok(out)
        } else {
            self.build_builtin_call_expr(BuiltinFunction::MapNew, Vec::new())
        }
    }

    pub(super) fn parse_map_key_literal(&mut self) -> Result<Expr, ParseError> {
        if let Some(name) = self.match_ident() {
            return Ok(Expr::String(name));
        }
        if let Some(value) = self.match_string() {
            return Ok(Expr::String(value));
        }
        if let Some(value) = self.match_int() {
            return Ok(Expr::Int(value));
        }
        self.reject_out_of_range_int_literal()?;
        if let Some(value) = self.match_float() {
            return Ok(Expr::Float(value));
        }
        if self.match_kind(&TokenKind::True) {
            return Ok(Expr::Bool(true));
        }
        if self.match_kind(&TokenKind::False) {
            return Ok(Expr::Bool(false));
        }
        if self.match_kind(&TokenKind::Null) {
            return Ok(Expr::Null);
        }
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "map keys must be identifier/string/int/float/bool/null literals".to_string(),
        })
    }

    pub(super) fn check_map_entry_start(&self) -> bool {
        let Some(current) = self.tokens.get(self.pos) else {
            return false;
        };
        if matches!(current.kind, TokenKind::LBracket) {
            let mut depth = 0usize;
            let mut cursor = self.pos;
            while let Some(token) = self.tokens.get(cursor) {
                match token.kind {
                    TokenKind::LBracket => depth += 1,
                    TokenKind::RBracket => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return matches!(
                                self.tokens.get(cursor + 1),
                                Some(Token {
                                    kind: TokenKind::Colon | TokenKind::Equal,
                                    ..
                                })
                            );
                        }
                    }
                    TokenKind::Eof => return false,
                    _ => {}
                }
                cursor += 1;
            }
            return false;
        }
        let Some(next) = self.tokens.get(self.pos + 1) else {
            return false;
        };
        let is_key = matches!(
            current.kind,
            TokenKind::Ident(_)
                | TokenKind::String(_)
                | TokenKind::Int(_)
                | TokenKind::IntMinMagnitude(_)
                | TokenKind::Float(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
        );
        let is_delim = matches!(next.kind, TokenKind::Colon | TokenKind::Equal);
        is_key && is_delim
    }

    pub(super) fn build_builtin_call_expr(
        &mut self,
        builtin: BuiltinFunction,
        mut args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        if !builtin.accepts_arity(arity) {
            if args.len() == usize::from(builtin.arity()) + 1
                && Self::rewrite_regex_flags_arg_into_pattern(builtin, &mut args)
            {
                return Ok(Expr::Call(builtin.call_index(), args));
            }
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "function '{}' expects {} arguments",
                    builtin.name(),
                    builtin.arity()
                ),
            });
        }
        Ok(Expr::Call(builtin.call_index(), args))
    }

    pub(super) fn rewrite_regex_flags_arg_into_pattern(
        builtin: BuiltinFunction,
        args: &mut Vec<Expr>,
    ) -> bool {
        match builtin {
            BuiltinFunction::ReMatch
            | BuiltinFunction::ReFind
            | BuiltinFunction::ReReplace
            | BuiltinFunction::ReSplit
            | BuiltinFunction::ReCaptures => {
                let Some(flags) = args.pop() else {
                    return false;
                };
                let Some(pattern) = args.first().cloned() else {
                    return false;
                };
                args[0] = Self::build_regex_flags_pattern_expr(pattern, flags);
                true
            }
            _ => false,
        }
    }

    pub(super) fn build_regex_flags_pattern_expr(pattern: Expr, flags: Expr) -> Expr {
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

    pub(super) fn try_build_language_builtin_call(
        &mut self,
        name: &str,
        args: &[Expr],
    ) -> Result<Option<Expr>, ParseError> {
        match name {
            "print" if self.dialect.allow_macro_calls() => {
                Ok(Some(self.lower_print_call(args.to_vec())?))
            }
            "print" => Ok(Some(self.lower_plain_print_call(args.to_vec())?)),
            "println" if self.dialect.allow_macro_calls() => {
                Ok(Some(self.lower_println_call(args.to_vec())?))
            }
            "println" => Ok(Some(self.lower_plain_println_call(args.to_vec())?)),
            "type" => {
                if args.len() != 1 {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "type expects exactly one argument".to_string(),
                    });
                }
                Ok(Some(self.build_builtin_call_expr(
                    BuiltinFunction::TypeOf,
                    args.to_vec(),
                )?))
            }
            "assert" => Ok(Some(
                self.build_builtin_call_expr(BuiltinFunction::Assert, args.to_vec())?,
            )),
            _ => Ok(None),
        }
    }

    pub(super) fn parse_macro_call(&mut self, name: &str) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LParen, "expected '(' after macro name")?;
        let _args = self.parse_call_args()?;
        Err(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("unknown macro '{name}!'"),
        })
    }

    pub(super) fn lower_print_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = match args.as_slice() {
            [] => {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "print expects at least one argument".to_string(),
                });
            }
            [value] => value.clone(),
            [format_expr, format_args @ ..] => {
                let format_literal = self.expect_format_literal("print", format_expr)?;
                self.build_ruststyle_format_expr("print", format_literal, format_args.to_vec())?
            }
        };
        self.build_print_call_expr(rendered)
    }

    pub(super) fn lower_plain_print_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = self.render_plain_print_args(args)?;
        self.build_print_call_expr(rendered)
    }

    pub(super) fn lower_println_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = match args.as_slice() {
            [] => Expr::String("\n".to_string()),
            [value] => {
                let value = self.build_to_string_expr(value.clone())?;
                self.append_newline_expr(value)
            }
            [format_expr, format_args @ ..] => {
                let format_literal = self.expect_format_literal("println", format_expr)?;
                let rendered = self.build_ruststyle_format_expr(
                    "println",
                    format_literal,
                    format_args.to_vec(),
                )?;
                self.append_newline_expr(rendered)
            }
        };
        self.build_print_call_expr(rendered)
    }

    pub(super) fn lower_plain_println_call(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        let rendered = match args.is_empty() {
            true => Expr::String("\n".to_string()),
            false => {
                let rendered = self.render_plain_print_args(args)?;
                let value = self.build_to_string_expr(rendered)?;
                self.append_newline_expr(value)
            }
        };
        self.build_print_call_expr(rendered)
    }

    pub(super) fn render_plain_print_args(&mut self, args: Vec<Expr>) -> Result<Expr, ParseError> {
        match args.len() {
            0 => Ok(Expr::String(String::new())),
            1 => Ok(args
                .into_iter()
                .next()
                .expect("single print arg should exist")),
            _ => {
                let mut args = args.into_iter();
                let mut rendered =
                    self.build_to_string_expr(args.next().expect("first print arg should exist"))?;
                for arg in args {
                    rendered =
                        Expr::Add(Box::new(rendered), Box::new(Expr::String(" ".to_string())));
                    rendered = Expr::Add(
                        Box::new(rendered),
                        Box::new(self.build_to_string_expr(arg)?),
                    );
                }
                Ok(rendered)
            }
        }
    }

    pub(super) fn expect_format_literal<'a>(
        &self,
        callee: &str,
        format_expr: &'a Expr,
    ) -> Result<&'a str, ParseError> {
        match format_expr {
            Expr::String(value) => Ok(value.as_str()),
            _ => Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "{callee} formatting requires a string literal as the first argument"
                ),
            }),
        }
    }

    pub(super) fn build_ruststyle_format_expr(
        &mut self,
        callee: &str,
        format_literal: &str,
        args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        self.validate_ruststyle_format_template(callee, format_literal, args.len())?;
        let positional_args = self.build_array_expr(args)?;
        self.build_builtin_call_expr(
            BuiltinFunction::FormatTemplate,
            vec![Expr::String(format_literal.to_string()), positional_args],
        )
    }

    pub(super) fn validate_ruststyle_format_template(
        &self,
        callee: &str,
        format_literal: &str,
        arg_count: usize,
    ) -> Result<(), ParseError> {
        let template_args = vec![ParserFormatArg; arg_count];
        ParsedFormat::parse(format_literal, template_args.as_slice(), &NoNamedArguments)
            .map(|_| ())
            .map_err(|offset| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("{callee} format string is invalid near byte {offset}"),
            })
    }

    pub(super) fn build_array_expr(&mut self, values: Vec<Expr>) -> Result<Expr, ParseError> {
        let mut out = self.build_builtin_call_expr(BuiltinFunction::ArrayNew, Vec::new())?;
        for value in values {
            out = self.build_builtin_call_expr(BuiltinFunction::ArrayPush, vec![out, value])?;
        }
        Ok(out)
    }

    pub(super) fn build_print_call_expr(&mut self, argument: Expr) -> Result<Expr, ParseError> {
        let decl = self.resolve_function_for_call(STDLIB_PRINT_NAME, 1)?;
        Ok(Expr::Call(decl.index, vec![argument]))
    }

    pub(super) fn build_to_string_expr(&mut self, value: Expr) -> Result<Expr, ParseError> {
        self.build_builtin_call_expr(BuiltinFunction::ToString, vec![value])
    }

    pub(super) fn append_newline_expr(&self, value: Expr) -> Expr {
        Expr::Add(Box::new(value), Box::new(Expr::String("\n".to_string())))
    }

    pub(super) fn build_host_call_expr(
        &mut self,
        host_name: &str,
        args: Vec<Expr>,
    ) -> Result<Expr, ParseError> {
        let arity = u8::try_from(args.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        let decl = self.define_host_function(host_name, arity)?;
        Ok(Expr::Call(decl.index, args))
    }

    pub(super) fn resolve_direct_host_call_target(&self, name: &str) -> Option<String> {
        if let Some(mapped) = self.direct_host_call_aliases.get(name) {
            return Some(mapped.clone());
        }
        if self.direct_host_wildcard_imports.len() == 1
            && let Some(namespace) = self.direct_host_wildcard_imports.iter().next()
        {
            return Some(format!("{namespace}::{name}"));
        }
        None
    }

    pub(super) fn resolve_host_namespace_call_target(
        &self,
        namespace: &str,
        member: &str,
        subpath: &[String],
    ) -> Option<String> {
        let mut host_name = if let Some(host_root) = self.host_namespace_aliases.get(namespace) {
            if member.is_empty() {
                host_root.clone()
            } else {
                format!("{host_root}::{member}")
            }
        } else {
            return None;
        };

        for segment in subpath {
            host_name.push_str("::");
            host_name.push_str(segment);
        }
        Some(host_name)
    }

    pub(super) fn resolve_imported_builtin_namespace<'a>(
        &'a self,
        namespace: &'a str,
    ) -> Option<&'a str> {
        let root = self.host_namespace_aliases.get(namespace)?.as_str();
        if is_builtin_namespace(root) {
            Some(root)
        } else {
            None
        }
    }

    pub(super) fn resolve_builtins_call_path<'a>(
        &'a self,
        namespace: &'a str,
        member: &'a str,
        subpath: &'a [String],
    ) -> Option<(&'a str, &'a str)> {
        if subpath.is_empty()
            && let Some(imported_root) = self.resolve_imported_builtin_namespace(namespace)
        {
            return Some((imported_root, member));
        }

        None
    }

    pub(super) fn parse_index_assign_with_terminator(
        &mut self,
        expect_terminator: bool,
    ) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let name = self.expect_ident("expected identifier before indexed assignment")?;
        let key = if self.match_kind(&TokenKind::LBracket) {
            let key = self.parse_expr()?;
            self.expect(&TokenKind::RBracket, "expected ']' after assignment index")?;
            key
        } else if self.match_kind(&TokenKind::Dot) {
            Expr::String(self.expect_ident("expected member name after '.'")?)
        } else {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "expected '[' or '.' in indexed assignment".to_string(),
            });
        };
        self.expect(
            &TokenKind::Equal,
            "expected '=' after indexed assignment target",
        )?;
        let value = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after indexed assignment")?;
        }

        let index = self.get_local(&name)?;
        self.require_local_mutable_for_operation(index, Some(name.as_str()), line, "mutate")?;
        let expr =
            self.build_builtin_call_expr(BuiltinFunction::Set, vec![Expr::Var(index), key, value])?;
        Ok(Stmt::Assign { index, expr, line })
    }

    pub(super) fn check_index_assignment_start(&self) -> bool {
        let Some(Token {
            kind: TokenKind::Ident(_),
            ..
        }) = self.tokens.get(self.pos)
        else {
            return false;
        };

        if matches!(
            (
                self.tokens.get(self.pos + 1),
                self.tokens.get(self.pos + 2),
                self.tokens.get(self.pos + 3),
            ),
            (
                Some(Token {
                    kind: TokenKind::Dot,
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Ident(_),
                    ..
                }),
                Some(Token {
                    kind: TokenKind::Equal,
                    ..
                })
            )
        ) {
            return true;
        }

        if !matches!(
            self.tokens.get(self.pos + 1),
            Some(Token {
                kind: TokenKind::LBracket,
                ..
            })
        ) {
            return false;
        }

        let mut depth = 0usize;
        let mut cursor = self.pos + 1;
        while let Some(token) = self.tokens.get(cursor) {
            match token.kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(cursor + 1),
                            Some(Token {
                                kind: TokenKind::Equal,
                                ..
                            })
                        );
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            cursor += 1;
        }
        false
    }

    pub(super) fn check_parenthesized_arrow_closure_start(&self) -> bool {
        if !self.check(&TokenKind::LParen) {
            return false;
        }
        let mut cursor = self.pos + 1;
        if self.check_kind_at(cursor, &TokenKind::RParen) {
            return self.check_kind_at(cursor + 1, &TokenKind::FatArrow);
        }

        let mut expect_ident = true;
        while cursor < self.tokens.len() {
            if expect_ident {
                if !self.check_ident_at(cursor) {
                    return false;
                }
                expect_ident = false;
                cursor += 1;
                continue;
            }
            if self.check_kind_at(cursor, &TokenKind::Comma) {
                expect_ident = true;
                cursor += 1;
                continue;
            }
            if self.check_kind_at(cursor, &TokenKind::RParen) {
                return self.check_kind_at(cursor + 1, &TokenKind::FatArrow);
            }
            return false;
        }
        false
    }

    pub(super) fn check_single_param_arrow_closure_start(&self) -> bool {
        self.check_ident_at(self.pos) && self.check_kind_at(self.pos + 1, &TokenKind::FatArrow)
    }

    pub(super) fn parse_parenthesized_arrow_closure(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LParen, "expected '(' to start arrow parameters")?;
        let mut params = Vec::<String>::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                params.push(self.expect_ident("expected arrow parameter name")?);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after arrow parameters")?;
        self.expect(&TokenKind::FatArrow, "expected '=>' after arrow parameters")?;
        if self.check(&TokenKind::LBrace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "arrow closures with block bodies are not supported in this subset"
                    .to_string(),
            });
        }
        self.parse_closure_expr_with_params(params)
    }

    pub(super) fn parse_single_param_arrow_closure(&mut self) -> Result<Expr, ParseError> {
        let param = self.expect_ident("expected arrow parameter name")?;
        self.expect(&TokenKind::FatArrow, "expected '=>' after arrow parameter")?;
        if self.check(&TokenKind::LBrace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "arrow closures with block bodies are not supported in this subset"
                    .to_string(),
            });
        }
        self.parse_closure_expr_with_params(vec![param])
    }

    pub(super) fn try_parse_js_dotted_call(
        &mut self,
        base: &str,
    ) -> Result<Option<Expr>, ParseError> {
        let save_pos = self.pos;
        if !self.match_kind(&TokenKind::Dot) {
            return Ok(None);
        }

        let mut segments = Vec::<String>::new();
        loop {
            let member = self.expect_namespace_segment("expected member name after '.'")?;
            segments.push(member);
            if self.match_kind(&TokenKind::Dot) {
                continue;
            }
            break;
        }

        if !self.match_kind(&TokenKind::LParen) {
            self.pos = save_pos;
            return Ok(None);
        }
        let mut args = self.parse_call_args()?;

        if base == "console" && segments.len() == 1 && segments[0] == "log" {
            return Ok(Some(
                self.lower_plain_print_call(std::mem::take(&mut args))?,
            ));
        }

        if segments.is_empty() {
            self.pos = save_pos;
            return Ok(None);
        }

        if let Some(imported_root) = self.resolve_imported_builtin_namespace(base) {
            let imported_root = imported_root.to_string();
            if segments.len() != 1 {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "unknown builtin function '{}::{}'",
                        imported_root,
                        segments.join("::")
                    ),
                });
            }
            let member = segments[0].as_str();
            if let Some(builtin) = resolve_builtin_namespace_call(&imported_root, member) {
                return Ok(Some(self.build_builtin_call_expr(builtin, args)?));
            }
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("unknown builtin function '{}::{}'", imported_root, member),
            });
        }

        let member = segments[0].clone();
        let subpath = segments.into_iter().skip(1).collect::<Vec<_>>();
        if let Some(host_name) = self.resolve_host_namespace_call_target(base, &member, &subpath) {
            return Ok(Some(self.build_host_call_expr(&host_name, args)?));
        }

        self.pos = save_pos;
        Ok(None)
    }

    pub(super) fn parse_closure_literal(&mut self) -> Result<Expr, ParseError> {
        let mut params = Vec::<String>::new();
        if !self.check(&TokenKind::Pipe) {
            loop {
                params.push(self.expect_ident("expected closure parameter name")?);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::Pipe, "expected '|' after closure parameters")?;
        self.parse_closure_expr_with_params(params)
    }

    pub(super) fn parse_closure_expr_with_params(
        &mut self,
        params: Vec<String>,
    ) -> Result<Expr, ParseError> {
        let mut param_slots = Vec::new();
        let mut param_scope = HashMap::new();
        for param_name in &params {
            if param_scope.contains_key(param_name) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("duplicate closure parameter '{param_name}'"),
                });
            }
            let slot = self.allocate_hidden_local()?;
            param_scope.insert(param_name.clone(), slot);
            param_slots.push(slot);
        }

        self.closure_scopes.push(param_scope);
        self.closure_capture_contexts.push(ClosureCaptureContext {
            by_name: HashMap::new(),
            capture_copies: Vec::new(),
        });
        let body = self.parse_expr()?;
        let capture_context = self
            .closure_capture_contexts
            .pop()
            .ok_or_else(|| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "internal closure capture state error".to_string(),
            })?;
        self.closure_scopes.pop();
        Ok(Expr::Closure(ClosureExpr {
            param_slots,
            capture_copies: capture_context.capture_copies,
            body: Box::new(body),
        }))
    }

    pub(super) fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after arguments")?;
        Ok(args)
    }
}
