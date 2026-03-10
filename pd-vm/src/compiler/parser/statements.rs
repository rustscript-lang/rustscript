use super::*;

impl Parser {
    pub(super) fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        if self.match_kind(&TokenKind::Pub) {
            if self.match_kind(&TokenKind::Fn) {
                return self.parse_fn_decl(true);
            }
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "expected 'fn' after 'pub'".to_string(),
            });
        }
        if self.match_kind(&TokenKind::Use) {
            return self.parse_use_stmt();
        }
        if self.dialect.allow_import_stmt() && self.match_kind(&TokenKind::Import) {
            return self.parse_js_import_stmt();
        }
        if self.dialect.allow_return_stmt() && self.check_ident_literal("return") {
            return self.parse_return_stmt();
        }
        if self.match_kind(&TokenKind::Fn) {
            return self.parse_fn_decl(false);
        }
        if self.match_kind(&TokenKind::Struct) {
            return self.parse_struct_decl();
        }
        if self.match_kind(&TokenKind::Let) {
            if self.dialect.allow_require_declaration() && self.check_js_require_declaration_start()
            {
                return self.parse_js_require_declaration_after_let();
            }
            return self.parse_let_with_terminator(true);
        }
        if self.match_kind(&TokenKind::For) {
            return self.parse_for();
        }
        if self.match_kind(&TokenKind::If) {
            return self.parse_if();
        }
        if self.match_kind(&TokenKind::While) {
            return self.parse_while();
        }
        if self.match_kind(&TokenKind::Break) {
            return self.parse_loop_control_stmt(true);
        }
        if self.match_kind(&TokenKind::Continue) {
            return self.parse_loop_control_stmt(false);
        }
        if self.check_index_assignment_start() {
            return self.parse_index_assign_with_terminator(true);
        }
        if self.check_assignment_start() {
            return self.parse_assign_with_terminator(true);
        }

        let line = self.current_line_u32();
        let expr = self.parse_expr()?;
        self.consume_stmt_terminator("expected ';' after expression")?;
        Ok(Stmt::Expr { expr, line })
    }

    pub(super) fn parse_loop_control_stmt(&mut self, is_break: bool) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        if self.loop_depth == 0 {
            return Err(ParseError {
                span: None,
                code: None,
                line: line as usize,
                message: if is_break {
                    "'break' is only allowed inside loops".to_string()
                } else {
                    "'continue' is only allowed inside loops".to_string()
                },
            });
        }
        self.consume_stmt_terminator(if is_break {
            "expected ';' after break"
        } else {
            "expected ';' after continue"
        })?;
        Ok(if is_break {
            Stmt::Break { line }
        } else {
            Stmt::Continue { line }
        })
    }

    pub(super) fn parse_use_stmt(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let namespace = self.expect_ident("expected namespace after 'use'")?;
        if self.match_kind(&TokenKind::Semicolon) {
            self.host_namespace_aliases
                .insert(namespace.clone(), namespace);
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::As) {
            let alias = self.expect_ident("expected namespace alias after 'as'")?;
            self.expect(&TokenKind::Semicolon, "expected ';' after use alias")?;
            self.host_namespace_aliases.insert(alias, namespace);
            return Ok(Stmt::Noop { line });
        }

        if !self.match_path_separator() {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "unsupported use syntax for host namespace '{namespace}'; expected ';', 'as <alias>', or '::{{...}}'"
                ),
            });
        }

        if is_builtin_namespace(&namespace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!(
                    "unsupported use list for builtin namespace '{namespace}'; import the namespace and call members through it"
                ),
            });
        }

        if self.match_kind(&TokenKind::Star) {
            self.direct_host_wildcard_imports.insert(namespace);
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after host wildcard import",
            )?;
            return Ok(Stmt::Noop { line });
        }

        self.expect(&TokenKind::LBrace, "expected '{' after host import path")?;
        if self.match_kind(&TokenKind::Star) {
            self.direct_host_wildcard_imports.insert(namespace);
            self.expect(&TokenKind::RBrace, "expected '}' after '*'")?;
            self.expect(&TokenKind::Semicolon, "expected ';' after use list")?;
            return Ok(Stmt::Noop { line });
        }

        loop {
            let imported = self.expect_ident("expected host function name in use list")?;
            let local = if self.match_kind(&TokenKind::As) {
                self.expect_ident("expected local alias after 'as'")?
            } else {
                imported.clone()
            };
            let target = format!("{namespace}::{imported}");
            if let Some(existing) = self.direct_host_call_aliases.get(&local)
                && existing != &target
            {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "host import alias '{local}' already maps to '{existing}', cannot remap to '{target}'"
                    ),
                });
            }
            self.direct_host_call_aliases.insert(local, target);

            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect(&TokenKind::RBrace, "expected '}' after use list")?;
        self.expect(&TokenKind::Semicolon, "expected ';' after use list")?;
        Ok(Stmt::Noop { line })
    }

    pub(super) fn parse_js_import_stmt(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();

        if self.match_string().is_some() {
            self.consume_stmt_terminator("expected ';' after import statement")?;
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::Star) {
            self.expect(&TokenKind::As, "expected 'as' in namespace import")?;
            let alias = self.expect_ident("expected namespace alias in import")?;
            self.expect(&TokenKind::From, "expected 'from' in import statement")?;
            let spec = self.expect_string_literal("expected module string after 'from'")?;
            self.consume_stmt_terminator("expected ';' after import statement")?;
            if is_builtin_namespace(&spec) || is_virtual_host_namespace_spec(&spec) {
                self.host_namespace_aliases.insert(alias, spec);
            }
            return Ok(Stmt::Noop { line });
        }

        if self.match_kind(&TokenKind::LBrace) {
            let named = self.parse_js_named_import_list()?;
            self.expect(&TokenKind::RBrace, "expected '}' after import list")?;
            self.expect(&TokenKind::From, "expected 'from' in import statement")?;
            let spec = self.expect_string_literal("expected module string after 'from'")?;
            self.consume_stmt_terminator("expected ';' after import statement")?;
            if is_virtual_host_namespace_spec(&spec) && !is_builtin_namespace(&spec) {
                for (imported, local) in named {
                    self.direct_host_call_aliases
                        .insert(local, format!("{spec}::{imported}"));
                }
            }
            return Ok(Stmt::Noop { line });
        }

        let default_ident = self.expect_ident("expected import clause after 'import'")?;
        if self.match_kind(&TokenKind::Comma) {
            if self.match_kind(&TokenKind::Star) {
                self.expect(&TokenKind::As, "expected 'as' in namespace import")?;
                let _alias = self.expect_ident("expected namespace alias in import")?;
            } else if self.match_kind(&TokenKind::LBrace) {
                let _ = self.parse_js_named_import_list()?;
                self.expect(&TokenKind::RBrace, "expected '}' after import list")?;
            } else {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unsupported import clause after ','".to_string(),
                });
            }
        }
        self.expect(&TokenKind::From, "expected 'from' in import statement")?;
        let spec = self.expect_string_literal("expected module string after 'from'")?;
        self.consume_stmt_terminator("expected ';' after import statement")?;
        if is_builtin_namespace(&spec) || is_virtual_host_namespace_spec(&spec) {
            self.host_namespace_aliases.insert(default_ident, spec);
        }
        Ok(Stmt::Noop { line })
    }

    pub(super) fn parse_js_named_import_list(
        &mut self,
    ) -> Result<Vec<(String, String)>, ParseError> {
        let mut named = Vec::<(String, String)>::new();
        if self.check(&TokenKind::RBrace) {
            return Ok(named);
        }
        loop {
            let imported = self.expect_ident("expected imported symbol in import list")?;
            let local = if self.match_kind(&TokenKind::As) {
                self.expect_ident("expected local alias after 'as'")?
            } else {
                imported.clone()
            };
            named.push((imported, local));
            if self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        Ok(named)
    }

    pub(super) fn parse_struct_decl(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let name = self.expect_ident("expected struct name after 'struct'")?;
        self.expect(&TokenKind::LBrace, "expected '{' after struct name")?;
        let fields = self.parse_object_type_schema_fields()?;
        self.expect(&TokenKind::RBrace, "expected '}' after struct body")?;
        if self.struct_schemas.insert(name.clone(), TypeSchema::Object(fields)).is_some() {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("duplicate struct schema '{name}'"),
            });
        }
        Ok(Stmt::Noop { line })
    }

    pub(super) fn parse_return_stmt(&mut self) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let _ = self.match_ident_literal("return");

        if self.function_body_depth == 0 {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "'return' is only valid inside function bodies".to_string(),
            });
        }

        let expr = if self.match_kind(&TokenKind::Semicolon)
            || self.check(&TokenKind::RBrace)
            || self.check(&TokenKind::Eof)
        {
            Expr::Null
        } else {
            let expr = self.parse_expr()?;
            self.consume_stmt_terminator("expected ';' after return value")?;
            expr
        };
        Ok(Stmt::Expr { expr, line })
    }

    pub(super) fn check_js_require_declaration_start(&self) -> bool {
        if self.check(&TokenKind::LBrace) {
            let mut cursor = self.pos + 1;
            while cursor < self.tokens.len() {
                match self.tokens[cursor].kind {
                    TokenKind::RBrace => {
                        return self.check_kind_at(cursor + 1, &TokenKind::Equal)
                            && self.check_ident_literal_at(cursor + 2, "require")
                            && self.check_kind_at(cursor + 3, &TokenKind::LParen)
                            && self.check_string_at(cursor + 4)
                            && self.check_kind_at(cursor + 5, &TokenKind::RParen);
                    }
                    TokenKind::Eof => return false,
                    _ => cursor += 1,
                }
            }
            return false;
        }

        self.check_ident_at(self.pos)
            && self.check_kind_at(self.pos + 1, &TokenKind::Equal)
            && self.check_ident_literal_at(self.pos + 2, "require")
            && self.check_kind_at(self.pos + 3, &TokenKind::LParen)
            && self.check_string_at(self.pos + 4)
            && self.check_kind_at(self.pos + 5, &TokenKind::RParen)
    }

    pub(super) fn parse_js_require_declaration_after_let(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();

        if self.match_kind(&TokenKind::LBrace) {
            while !self.check(&TokenKind::RBrace) {
                if self.check(&TokenKind::Eof) {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: "unexpected end of input in require destructuring".to_string(),
                    });
                }
                self.pos += 1;
            }
            self.expect(&TokenKind::RBrace, "expected '}' in require destructuring")?;
            self.expect(
                &TokenKind::Equal,
                "expected '=' after destructuring in require declaration",
            )?;
            let _spec = self.parse_js_require_call()?;
            self.consume_stmt_terminator("expected ';' after require declaration")?;
            return Ok(Stmt::Noop { line });
        }

        let alias = self.expect_ident("expected identifier after 'let'")?;
        self.expect(
            &TokenKind::Equal,
            "expected '=' in require declaration after identifier",
        )?;
        let spec = self.parse_js_require_call()?;
        self.consume_stmt_terminator("expected ';' after require declaration")?;
        if is_builtin_namespace(&spec) || is_virtual_host_namespace_spec(&spec) {
            self.host_namespace_aliases.insert(alias, spec);
        }
        Ok(Stmt::Noop { line })
    }

    pub(super) fn parse_js_require_call(&mut self) -> Result<String, ParseError> {
        if !self.match_ident_literal("require") {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "expected 'require(...)' in declaration".to_string(),
            });
        }
        self.expect(&TokenKind::LParen, "expected '(' after require")?;
        let spec = self.expect_string_literal("expected module string in require")?;
        self.expect(
            &TokenKind::RParen,
            "expected ')' after require module string",
        )?;
        Ok(spec)
    }

    pub(super) fn parse_fn_decl(&mut self, exported: bool) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let name = self.expect_ident("expected function name after 'fn'")?;
        self.expect(&TokenKind::LParen, "expected '(' after function name")?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let param = self.expect_ident("expected parameter name")?;
                params.push(param);
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after parameters")?;
        let return_type = self.parse_optional_declared_return_type()?;

        let arity = u8::try_from(params.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        if self.functions.contains_key(&name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("duplicate function '{name}'"),
            });
        }
        if self.locals.contains_key(&name) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("name '{name}' already used by a local binding"),
            });
        }
        let index = self.next_function;
        self.next_function = self.next_function.checked_add(1).ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function index overflow".to_string(),
        })?;
        let decl = FunctionDecl {
            name: name.clone(),
            arity,
            index,
            args: params.clone(),
            exported,
            return_type,
        };
        self.functions.insert(name.clone(), decl.clone());
        self.function_list.push(decl.clone());

        if self.match_kind(&TokenKind::Equal) {
            let function_impl = self.parse_function_impl_expr(&params)?;
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after function definition",
            )?;
            self.function_impls.insert(index, function_impl);
        } else if self.match_kind(&TokenKind::LBrace) {
            let function_impl = self.parse_function_impl_block(&params)?;
            self.expect(&TokenKind::RBrace, "expected '}' after function body")?;
            self.function_impls.insert(index, function_impl);
            // Optional trailing semicolon for compatibility.
            self.match_kind(&TokenKind::Semicolon);
        } else {
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after function declaration",
            )?;
        }

        Ok(Stmt::FuncDecl {
            name,
            index,
            arity,
            args: params,
            exported,
            line,
        })
    }

    pub(super) fn parse_function_impl_expr(
        &mut self,
        params: &[String],
    ) -> Result<FunctionImpl, ParseError> {
        self.parse_function_impl(params, |parser| Ok((Vec::new(), parser.parse_expr()?)))
    }

    pub(super) fn parse_optional_declared_return_type(&mut self) -> Result<ValueType, ParseError> {
        if !self.match_return_type_arrow() {
            return Ok(ValueType::Unknown);
        }

        let ty = if self.match_kind(&TokenKind::Null) {
            ValueType::Null
        } else {
            let span = self.current_span();
            let name = self.expect_ident("expected return type after '->'")?;
            match name.as_str() {
                "unknown" => {
                    self.unknown_type_spans.push(span);
                    ValueType::Unknown
                }
                "int" => ValueType::Int,
                "float" => ValueType::Float,
                "bool" => ValueType::Bool,
                "string" => ValueType::String,
                "array" => ValueType::Array,
                "map" => ValueType::Map,
                other => {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: self.current_line(),
                        message: format!(
                            "unknown declared return type '{other}', expected unknown/null/int/float/bool/string/array/map"
                        ),
                    });
                }
            }
        };

        Ok(ty)
    }

    fn parse_object_type_schema_fields(&mut self) -> Result<HashMap<String, TypeSchema>, ParseError> {
        let mut fields = HashMap::new();
        if !self.check(&TokenKind::RBrace) {
            loop {
                let field = self.expect_ident("expected field name in object type schema")?;
                self.expect(&TokenKind::Colon, "expected ':' after schema field name")?;
                let field_schema = self.parse_declared_type_schema()?;
                fields.insert(field, field_schema);
                if self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RBrace) {
                        break;
                    }
                    continue;
                }
                break;
            }
        }
        Ok(fields)
    }

    pub(super) fn parse_declared_type_schema(&mut self) -> Result<TypeSchema, ParseError> {
        if self.match_kind(&TokenKind::LBracket) {
            let element = self.parse_declared_type_schema()?;
            self.expect(&TokenKind::RBracket, "expected ']' after array type schema")?;
            return Ok(TypeSchema::Array(Box::new(element)));
        }

        if self.match_kind(&TokenKind::LBrace) {
            let fields = self.parse_object_type_schema_fields()?;
            self.expect(&TokenKind::RBrace, "expected '}' after object type schema")?;
            return Ok(TypeSchema::Object(fields));
        }

        if self.match_kind(&TokenKind::Null) {
            return Ok(TypeSchema::Null);
        }

        let span = self.current_span();
        let name = self.expect_ident("expected type schema")?;
        match name.as_str() {
            "unknown" => {
                self.unknown_type_spans.push(span);
                Ok(TypeSchema::Unknown)
            }
            "int" => Ok(TypeSchema::Int),
            "float" => Ok(TypeSchema::Float),
            "bool" => Ok(TypeSchema::Bool),
            "string" => Ok(TypeSchema::String),
            "array" => Ok(TypeSchema::Array(Box::new(TypeSchema::Unknown))),
            "map" => Ok(TypeSchema::Map(Box::new(TypeSchema::Unknown))),
            other => {
                self.schema_reference_sites
                    .push((other.to_string(), self.current_line(), span));
                Ok(TypeSchema::Named(other.to_string()))
            }
        }
    }

    pub(super) fn parse_declared_struct_name(&mut self) -> Result<String, ParseError> {
        let span = self.current_span();
        let name = self.expect_ident("expected struct name after ':'")?;
        self.schema_reference_sites
            .push((name.clone(), self.current_line(), span));
        Ok(name)
    }

    pub(super) fn parse_function_impl_block(
        &mut self,
        params: &[String],
    ) -> Result<FunctionImpl, ParseError> {
        self.parse_function_impl(params, |parser| {
            let mut body_stmts = Vec::new();
            let mut trailing_expr: Option<Expr> = None;
            while !parser.check(&TokenKind::RBrace) {
                if parser.check(&TokenKind::Eof) {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: parser.current_line(),
                        message: "unexpected end of input in function body".to_string(),
                    });
                }

                if parser.starts_trailing_expr_block_statement() {
                    body_stmts.push(parser.parse_stmt()?);
                    continue;
                }

                let line = parser.current_line_u32();
                let expr = parser.parse_expr()?;
                if parser.check(&TokenKind::RBrace) {
                    trailing_expr = Some(expr);
                    break;
                }
                parser.consume_stmt_terminator("expected ';' after expression")?;
                body_stmts.push(Stmt::Expr { expr, line });
            }

            let body_expr = if let Some(expr) = trailing_expr {
                expr
            } else {
                let Some(last_stmt) = body_stmts.pop() else {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: parser.current_line(),
                        message: "function body must end with an expression statement".to_string(),
                    });
                };
                if let Stmt::Expr { expr, .. } = last_stmt {
                    expr
                } else {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: parser.current_line(),
                        message: "function body must end with an expression statement".to_string(),
                    });
                }
            };

            Ok((body_stmts, body_expr))
        })
    }

    pub(super) fn parse_function_impl<F>(
        &mut self,
        params: &[String],
        parse_body: F,
    ) -> Result<FunctionImpl, ParseError>
    where
        F: FnOnce(&mut Self) -> Result<(Vec<Stmt>, Expr), ParseError>,
    {
        let mut param_scope = HashMap::new();
        let mut param_slots = Vec::new();
        for param in params {
            if param_scope.contains_key(param) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("duplicate function parameter '{param}'"),
                });
            }
            let slot = self.allocate_hidden_local()?;
            param_scope.insert(param.clone(), slot);
            param_slots.push(slot);
        }
        self.closure_scopes.push(param_scope);
        self.closure_capture_contexts.push(ClosureCaptureContext {
            by_name: HashMap::new(),
            capture_copies: Vec::new(),
        });
        self.function_body_depth += 1;
        let (body_stmts, body_expr) = parse_body(self)?;
        self.function_body_depth = self.function_body_depth.saturating_sub(1);
        let capture_context = self
            .closure_capture_contexts
            .pop()
            .ok_or_else(|| ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "internal function capture state error".to_string(),
            })?;
        self.closure_scopes.pop();
        Ok(FunctionImpl {
            param_slots,
            capture_copies: capture_context.capture_copies,
            body_stmts,
            body_expr,
        })
    }

    pub(super) fn parse_let_with_terminator(
        &mut self,
        expect_terminator: bool,
    ) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let declared_mutable =
            self.dialect.allow_let_mut_binding() && self.match_ident_literal("mut");
        let name = if declared_mutable {
            self.expect_ident("expected identifier after 'let mut'")?
        } else {
            self.expect_ident("expected identifier after 'let'")?
        };
        let declared_struct = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_declared_struct_name()?)
        } else {
            None
        };
        self.expect(&TokenKind::Equal, "expected '=' after identifier")?;
        let expr = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after let")?;
        }

        if !self.closure_scopes.is_empty() {
            if let Some(index) = self
                .closure_scopes
                .last()
                .and_then(|scope| scope.get(&name))
                .copied()
            {
                self.apply_let_binding_mutability(index, declared_mutable, false);
                return Ok(Stmt::Let {
                    index,
                    declared_struct,
                    expr,
                    line,
                });
            }
            let index = self.allocate_hidden_local()?;
            if let Some(scope) = self.closure_scopes.last_mut() {
                scope.insert(name, index);
            }
            self.apply_let_binding_mutability(index, declared_mutable, true);
            return Ok(Stmt::Let {
                index,
                declared_struct,
                expr,
                line,
            });
        }

        let (index, created) = self.get_or_assign_local(&name)?;
        self.apply_let_binding_mutability(index, declared_mutable, created);
        Ok(Stmt::Let {
            index,
            declared_struct,
            expr,
            line,
        })
    }

    pub(super) fn parse_assign_with_terminator(
        &mut self,
        expect_terminator: bool,
    ) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let name = self.expect_ident("expected identifier before '='")?;
        self.expect(&TokenKind::Equal, "expected '=' after identifier")?;
        let expr = self.parse_expr()?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after assignment")?;
        }

        let index = self.get_local(&name)?;
        self.require_local_mutable_for_operation(index, Some(name.as_str()), line, "assign to")?;
        Ok(Stmt::Assign { index, expr, line })
    }

    pub(super) fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        self.expect(&TokenKind::LParen, "expected '(' after 'for'")?;

        let init = if self.match_kind(&TokenKind::Let) {
            self.parse_let_with_terminator(false)?
        } else if self.check_assignment_start() {
            self.parse_assign_with_terminator(false)?
        } else {
            let init_line = self.current_line_u32();
            let expr = self.parse_expr()?;
            Stmt::Expr {
                expr,
                line: init_line,
            }
        };
        self.expect(&TokenKind::Semicolon, "expected ';' after for initializer")?;

        let condition = self.parse_expr()?;
        self.expect(&TokenKind::Semicolon, "expected ';' after for condition")?;

        let post = if self.check_assignment_start() {
            self.parse_assign_with_terminator(false)?
        } else {
            let post_line = self.current_line_u32();
            let expr = self.parse_expr()?;
            Stmt::Expr {
                expr,
                line: post_line,
            }
        };
        self.expect(&TokenKind::RParen, "expected ')' after for clauses")?;
        self.loop_depth += 1;
        let body = self.parse_block("expected '{' after for clauses")?;
        self.loop_depth -= 1;
        Ok(Stmt::For {
            init: Box::new(init),
            condition,
            post: Box::new(post),
            body,
            line,
        })
    }

    pub(super) fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let condition = self.parse_expr()?;
        let then_branch = self.parse_block("expected '{' after if condition")?;
        let else_branch = if self.match_kind(&TokenKind::Else) {
            if self.match_kind(&TokenKind::If) {
                vec![self.parse_if()?]
            } else {
                self.parse_block("expected '{' after else")?
            }
        } else {
            Vec::new()
        };
        Ok(Stmt::IfElse {
            condition,
            then_branch,
            else_branch,
            line,
        })
    }

    pub(super) fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        let condition = self.parse_expr()?;
        self.loop_depth += 1;
        let body = self.parse_block("expected '{' after while condition")?;
        self.loop_depth -= 1;
        Ok(Stmt::While {
            condition,
            body,
            line,
        })
    }

    pub(super) fn parse_block(&mut self, message: &str) -> Result<Vec<Stmt>, ParseError> {
        self.expect(&TokenKind::LBrace, message)?;
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Eof) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "unexpected end of input in block".to_string(),
                });
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect(&TokenKind::RBrace, "expected '}' to close block")?;
        Ok(stmts)
    }
}
