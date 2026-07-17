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
        if self.check_increment_start() {
            return self.parse_increment_with_terminator(true);
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
        let type_params = self.parse_type_params("struct", &name)?;
        self.push_active_type_params(&type_params);
        self.expect(&TokenKind::LBrace, "expected '{' after struct name")?;
        let fields = self.parse_object_type_schema_fields()?;
        self.pop_active_type_params();
        self.expect(&TokenKind::RBrace, "expected '}' after struct body")?;
        if self
            .struct_schemas
            .insert(
                name.clone(),
                StructDecl {
                    name: name.clone(),
                    type_params,
                    body_schema: TypeSchema::Object(fields),
                },
            )
            .is_some()
        {
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
        let type_params = self.parse_type_params("function", &name)?;
        self.push_active_type_params(&type_params);
        self.expect(&TokenKind::LParen, "expected '(' after function name")?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let param = self.expect_ident("expected parameter name")?;
                let schema = if self.match_kind(&TokenKind::Colon) {
                    Some(self.parse_declared_type_schema()?)
                } else {
                    None
                };
                params.push(FunctionParam {
                    name: param,
                    schema,
                });
                if self.match_kind(&TokenKind::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&TokenKind::RParen, "expected ')' after parameters")?;
        let return_schema = self.parse_optional_declared_return_schema()?;
        self.pop_active_type_params();
        let return_type = return_schema
            .as_ref()
            .map(TypeSchema::coarse_value_type)
            .unwrap_or(ValueType::Unknown);
        let param_names = Self::function_param_names(&params);
        let arg_schemas = params
            .iter()
            .map(|param| param.schema.clone())
            .collect::<Vec<_>>();

        let arity = u8::try_from(param_names.len()).map_err(|_| ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: "function arity too large".to_string(),
        })?;
        let predeclared = self.functions.get(&name).cloned().ok_or(ParseError {
            span: None,
            code: None,
            line: self.current_line(),
            message: format!("function '{name}' was not predeclared"),
        })?;
        if self.parsed_function_decls.contains(&predeclared.index) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("duplicate function '{name}'"),
            });
        }
        if predeclared.arity != arity || predeclared.type_params.len() != type_params.len() {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: format!("function header changed while parsing '{name}'"),
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
        let index = predeclared.index;
        let decl = FunctionDecl {
            name: name.clone(),
            arity,
            index,
            args: param_names.clone(),
            arg_schemas,
            return_schema,
            type_params: type_params.clone(),
            exported,
            return_type,
        };
        self.functions.insert(name.clone(), decl.clone());
        let current_line = self.current_line();
        let list_entry = self
            .function_list
            .iter_mut()
            .find(|candidate| candidate.index == index)
            .ok_or(ParseError {
                span: None,
                code: None,
                line: current_line,
                message: format!("function list entry missing for '{name}'"),
            })?;
        *list_entry = decl.clone();
        self.parsed_function_decls.insert(index);

        self.push_active_type_params(&type_params);
        let has_impl = if self.match_kind(&TokenKind::Equal) {
            let function_impl = self.parse_function_impl_expr(&params)?;
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after function definition",
            )?;
            self.function_impls.insert(index, function_impl);
            true
        } else if self.match_kind(&TokenKind::LBrace) {
            let function_impl = self.parse_function_impl_block(&params)?;
            self.expect(&TokenKind::RBrace, "expected '}' after function body")?;
            self.function_impls.insert(index, function_impl);
            // Optional trailing semicolon for compatibility.
            self.match_kind(&TokenKind::Semicolon);
            true
        } else {
            self.expect(
                &TokenKind::Semicolon,
                "expected ';' after function declaration",
            )?;
            false
        };
        self.pop_active_type_params();

        Ok(Stmt::FuncDecl {
            name,
            index,
            arity,
            args: param_names,
            exported,
            has_impl,
            line,
        })
    }

    pub(super) fn parse_function_impl_expr(
        &mut self,
        params: &[crate::compiler::ir::FunctionParam],
    ) -> Result<FunctionImpl, ParseError> {
        self.parse_function_impl(params, |parser| {
            let body_expr_line = parser.current_line_u32();
            Ok((Vec::new(), parser.parse_expr()?, body_expr_line))
        })
    }

    pub(super) fn parse_optional_declared_return_schema(
        &mut self,
    ) -> Result<Option<TypeSchema>, ParseError> {
        if !self.match_return_type_arrow() {
            return Ok(None);
        }

        let mut schema = self.parse_declared_type_schema()?;
        if self.match_kind(&TokenKind::Question) {
            schema = TypeSchema::Optional(Box::new(schema));
        }
        Ok(Some(schema))
    }

    fn parse_object_type_schema_fields(
        &mut self,
    ) -> Result<HashMap<String, TypeSchema>, ParseError> {
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
        let mut schema = if self.match_kind(&TokenKind::LBracket) {
            let mut elements = Vec::new();
            let mut rest = None;
            if self.check(&TokenKind::RBracket) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "expected at least one item in array type schema".to_string(),
                });
            }
            loop {
                let element = self.parse_declared_type_schema()?;
                if self.match_kind(&TokenKind::Ellipsis) {
                    rest = Some(element);
                    if self.match_kind(&TokenKind::Comma) && !self.check(&TokenKind::RBracket) {
                        return Err(ParseError {
                            span: None,
                            code: None,
                            line: self.current_line(),
                            message: "variadic array schema entry must be the final item"
                                .to_string(),
                        });
                    }
                    break;
                }
                elements.push(element);
                if self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RBracket) {
                        break;
                    }
                    continue;
                }
                break;
            }
            self.expect(&TokenKind::RBracket, "expected ']' after array type schema")?;
            match (elements.len(), rest) {
                (0, Some(rest)) => TypeSchema::Array(Box::new(rest)),
                (1, None) => TypeSchema::Array(Box::new(elements.pop().unwrap())),
                (_, Some(rest)) => TypeSchema::ArrayTupleRest {
                    prefix: elements,
                    rest: Box::new(rest),
                },
                _ => TypeSchema::ArrayTuple(elements),
            }
        } else if self.match_kind(&TokenKind::LBrace) {
            return Err(ParseError {
                span: None,
                code: None,
                line: self.current_line(),
                message: "inline object type schema is not supported; declare a struct and reference it by name"
                    .to_string(),
            });
        } else if self.match_kind(&TokenKind::Fn) {
            self.expect(&TokenKind::LParen, "expected '(' after callable type 'fn'")?;
            let mut params = Vec::new();
            if !self.check(&TokenKind::RParen) {
                loop {
                    params.push(self.parse_declared_type_schema()?);
                    if self.match_kind(&TokenKind::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(
                &TokenKind::RParen,
                "expected ')' after callable type parameters",
            )?;
            if !self.match_return_type_arrow() {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: "callable type schema requires '-> <return_type>'".to_string(),
                });
            }
            let result = self.parse_declared_type_schema()?;
            TypeSchema::Callable {
                params,
                result: Box::new(result),
            }
        } else if self.match_kind(&TokenKind::Null) {
            TypeSchema::Null
        } else {
            let span = self.current_span();
            let name = self.expect_ident("expected type schema")?;
            match name.as_str() {
                "unknown" => {
                    self.unknown_type_spans.push(span);
                    TypeSchema::Unknown
                }
                "int" => TypeSchema::Int,
                "float" => TypeSchema::Float,
                "number" => TypeSchema::Number,
                "bool" => TypeSchema::Bool,
                "string" => TypeSchema::String,
                "bytes" => TypeSchema::Bytes,
                "array" => {
                    if self.match_kind(&TokenKind::Less) {
                        let element = self.parse_declared_type_schema()?;
                        self.expect(
                            &TokenKind::Greater,
                            "expected '>' after array type argument",
                        )?;
                        TypeSchema::Array(Box::new(element))
                    } else {
                        TypeSchema::Array(Box::new(TypeSchema::Unknown))
                    }
                }
                "map" => {
                    if self.match_kind(&TokenKind::Less) {
                        let value = self.parse_declared_type_schema()?;
                        self.expect(&TokenKind::Greater, "expected '>' after map value type")?;
                        TypeSchema::Map(Box::new(value))
                    } else {
                        TypeSchema::Map(Box::new(TypeSchema::Unknown))
                    }
                }
                other => {
                    let type_args = if self.check(&TokenKind::Less) {
                        self.expect(&TokenKind::Less, "expected '<' before type arguments")?;
                        let mut type_args = Vec::new();
                        loop {
                            type_args.push(self.parse_declared_type_schema()?);
                            if self.match_kind(&TokenKind::Comma) {
                                continue;
                            }
                            break;
                        }
                        self.expect(&TokenKind::Greater, "expected '>' after type arguments")?;
                        type_args
                    } else {
                        Vec::new()
                    };
                    if self.is_active_type_param(other) {
                        if !type_args.is_empty() {
                            return Err(ParseError {
                                span: Some(span),
                                code: None,
                                line: self.current_line(),
                                message: format!(
                                    "generic parameter '{other}' cannot be instantiated with type arguments"
                                ),
                            });
                        }
                        TypeSchema::GenericParam(other.to_string())
                    } else {
                        self.schema_reference_sites.push((
                            other.to_string(),
                            type_args.len(),
                            self.current_line(),
                            span,
                        ));
                        TypeSchema::Named(other.to_string(), type_args)
                    }
                }
            }
        };

        while self.match_kind(&TokenKind::LBracket) {
            self.expect(
                &TokenKind::RBracket,
                "expected ']' after array schema alias",
            )?;
            schema = TypeSchema::Array(Box::new(schema));
        }

        if self.match_kind(&TokenKind::Question) {
            schema = TypeSchema::Optional(Box::new(schema));
        }

        Ok(schema)
    }

    pub(super) fn parse_function_impl_block(
        &mut self,
        params: &[crate::compiler::ir::FunctionParam],
    ) -> Result<FunctionImpl, ParseError> {
        self.parse_function_impl(params, |parser| {
            let mut body_stmts = Vec::new();
            let mut trailing_expr: Option<Expr> = None;
            let mut trailing_expr_line: Option<u32> = None;
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
                    trailing_expr_line = Some(line);
                    break;
                }
                parser.consume_stmt_terminator("expected ';' after expression")?;
                body_stmts.push(Stmt::Expr { expr, line });
            }

            let (body_expr, body_expr_line) = if let Some(expr) = trailing_expr {
                (
                    expr,
                    trailing_expr_line.expect("trailing expr should record a line"),
                )
            } else {
                let Some(last_stmt) = body_stmts.pop() else {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: parser.current_line(),
                        message: "function body must end with an expression statement".to_string(),
                    });
                };
                if let Stmt::Expr { expr, line } = last_stmt {
                    (expr, line)
                } else {
                    return Err(ParseError {
                        span: None,
                        code: None,
                        line: parser.current_line(),
                        message: "function body must end with an expression statement".to_string(),
                    });
                }
            };

            Ok((body_stmts, body_expr, body_expr_line))
        })
    }

    pub(super) fn parse_function_impl<F>(
        &mut self,
        params: &[crate::compiler::ir::FunctionParam],
        parse_body: F,
    ) -> Result<FunctionImpl, ParseError>
    where
        F: FnOnce(&mut Self) -> Result<(Vec<Stmt>, Expr, u32), ParseError>,
    {
        let mut param_scope = HashMap::new();
        let mut param_slots = Vec::new();
        for param in params {
            if param_scope.contains_key(&param.name) {
                return Err(ParseError {
                    span: None,
                    code: None,
                    line: self.current_line(),
                    message: format!("duplicate function parameter '{}'", param.name),
                });
            }
            let slot = self.allocate_hidden_local()?;
            param_scope.insert(param.name.clone(), slot);
            param_slots.push(slot);
            if let Some(schema) = &param.schema {
                self.local_schemas.insert(slot, schema.clone());
            }
        }
        self.closure_scopes.push(param_scope);
        self.closure_capture_contexts.push(ClosureCaptureContext {
            by_name: HashMap::new(),
            capture_copies: Vec::new(),
        });
        self.function_body_depth += 1;
        let (body_stmts, body_expr, body_expr_line) = parse_body(self)?;
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
        let mut capture_copies = capture_context.capture_copies;
        capture_copies.sort_unstable();
        capture_copies.dedup();
        Ok(FunctionImpl {
            param_slots,
            capture_copies,
            body_stmts,
            body_expr,
            body_expr_line,
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
        let declared_schema = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_declared_type_schema()?)
        } else {
            None
        };
        self.expect(&TokenKind::Equal, "expected '=' after identifier")?;
        let predeclared_closure_binding = if self.check(&TokenKind::Pipe) {
            Some(if !self.closure_scopes.is_empty() {
                if let Some(index) = self
                    .closure_scopes
                    .last()
                    .and_then(|scope| scope.get(&name))
                    .copied()
                {
                    (index, false)
                } else {
                    let index = self.allocate_hidden_local()?;
                    if let Some(scope) = self.closure_scopes.last_mut() {
                        scope.insert(name.clone(), index);
                    }
                    self.named_local_bindings.push((name.clone(), index));
                    (index, true)
                }
            } else {
                self.get_or_assign_local(&name)?
            })
        } else {
            None
        };
        let mut expr = self.parse_expr()?;
        if let Some(expected) = declared_schema.as_ref() {
            self.contextualize_function_value(&mut expr, expected)?;
        }
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after let")?;
        }

        let (index, created) = if let Some(predeclared) = predeclared_closure_binding {
            predeclared
        } else if !self.closure_scopes.is_empty() {
            if let Some(index) = self
                .closure_scopes
                .last()
                .and_then(|scope| scope.get(&name))
                .copied()
            {
                (index, false)
            } else {
                let index = self.allocate_hidden_local()?;
                if let Some(scope) = self.closure_scopes.last_mut() {
                    scope.insert(name.clone(), index);
                }
                self.named_local_bindings.push((name.clone(), index));
                (index, true)
            }
        } else {
            self.get_or_assign_local(&name)?
        };
        if !self.borrowed_map_iter_locals.is_empty()
            && self.borrowed_map_iter_locals.contains(&index)
        {
            let display = self
                .find_local_name_by_slot(index)
                .unwrap_or_else(|| format!("#{index}"));
            return Err(ParseError {
                span: None,
                code: Some("E_BORROW_CONFLICT".to_string()),
                line: line as usize,
                message: format!(
                    "cannot rebind local '{display}' while it is borrowed by a map iterator"
                ),
            });
        }
        if let Some(declared_schema) = &declared_schema {
            self.local_schemas.insert(index, declared_schema.clone());
        } else if !created {
            self.local_schemas.remove(&index);
        }
        self.apply_let_binding_mutability(index, declared_mutable, created);
        Ok(Stmt::Let {
            index,
            declared_schema,
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
        let index = self.get_local(&name)?;
        self.require_local_mutable_for_operation(index, Some(name.as_str()), line, "assign to")?;

        let (kind, expr) = if self.match_kind(&TokenKind::Equal) {
            (AssignmentKind::Set, self.parse_expr()?)
        } else if self.match_kind(&TokenKind::PlusEqual) {
            let rhs = self.parse_expr()?;
            (
                AssignmentKind::Add,
                self.build_numeric_addition_expr(index, rhs),
            )
        } else {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: "expected '=' or '+=' after identifier".to_string(),
            });
        };
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after assignment")?;
        }

        Ok(Stmt::Assign {
            kind,
            index,
            expr,
            line,
        })
    }

    pub(super) fn parse_increment_with_terminator(
        &mut self,
        expect_terminator: bool,
    ) -> Result<Stmt, ParseError> {
        let line = self.current_line_u32();
        let name = if self.match_kind(&TokenKind::PlusPlus) {
            self.expect_ident("expected identifier after '++'")?
        } else {
            let name = self.expect_ident("expected identifier before '++'")?;
            self.expect(&TokenKind::PlusPlus, "expected '++' after identifier")?;
            name
        };
        let index = self.get_local(&name)?;
        self.require_local_mutable_for_operation(index, Some(name.as_str()), line, "increment")?;
        if expect_terminator {
            self.consume_stmt_terminator("expected ';' after increment")?;
        }
        Ok(Stmt::Assign {
            kind: AssignmentKind::Increment,
            index,
            expr: self.build_numeric_addition_expr(index, Expr::Int(1)),
            line,
        })
    }

    pub(super) fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let line = self.last_line();
        if self.dialect.allow_for_in_loop() {
            return self.parse_for_in(line);
        }
        self.parse_c_style_for(line)
    }

    fn parse_for_in(&mut self, line: u32) -> Result<Stmt, ParseError> {
        if self.check(&TokenKind::LParen) {
            if matches!(
                self.tokens.get(self.pos + 1).map(|token| &token.kind),
                Some(TokenKind::Ident(_))
            ) {
                return self.parse_map_for_in(line);
            }
            self.match_kind(&TokenKind::LParen);
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.last_line() as usize,
                message: "expected Rust-style for-in loop; write `for i in 0..n { ... }`"
                    .to_string(),
            });
        }
        if self.match_kind(&TokenKind::Let) {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.last_line() as usize,
                message: "expected Rust-style for-in loop; write `for i in 0..n { ... }`"
                    .to_string(),
            });
        }

        let declared_mutable =
            self.dialect.allow_let_mut_binding() && self.match_ident_literal("mut");
        let name = if declared_mutable {
            self.expect_ident("expected identifier after 'for mut'")?
        } else {
            self.expect_ident("expected identifier after 'for'")?
        };
        if !self.match_ident_literal("in") {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: "expected Rust-style for-in loop; write `for i in 0..n { ... }`"
                    .to_string(),
            });
        }

        let start = self.parse_expr()?;
        let inclusive = if self.match_kind(&TokenKind::DotDot) {
            false
        } else if self.match_kind(&TokenKind::DotDotEqual) {
            true
        } else {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: "expected range expression after 'in'; write `for i in 0..n { ... }`"
                    .to_string(),
            });
        };
        let end = self.parse_expr()?;

        let (index, _) = self.bind_for_loop_local(&name)?;
        if self.enforce_mutable_bindings {
            self.set_local_slot_mutable(index, declared_mutable);
        }

        self.loop_depth += 1;
        let body = self.parse_block("expected '{' after for range")?;
        self.loop_depth -= 1;

        let loop_var = Expr::Var(index);
        let condition = if inclusive {
            self.build_non_strict_comparison(loop_var, end, Expr::Lt)?
        } else {
            Expr::Lt(Box::new(loop_var), Box::new(end))
        };
        Ok(Stmt::For {
            init: Box::new(Stmt::Let {
                index,
                declared_schema: None,
                expr: start,
                line,
            }),
            condition,
            post: Box::new(Stmt::Assign {
                kind: AssignmentKind::Set,
                index,
                expr: self.build_numeric_addition_expr(index, Expr::Int(1)),
                line,
            }),
            body,
            line,
        })
    }

    fn parse_map_for_in(&mut self, line: u32) -> Result<Stmt, ParseError> {
        self.expect(&TokenKind::LParen, "expected '(' after 'for'")?;
        let key_name = self.expect_ident("expected map key binding")?;
        let key_schema = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_declared_type_schema()?)
        } else {
            None
        };
        self.expect(
            &TokenKind::Comma,
            "expected ',' between map iterator bindings",
        )?;
        let value_name = self.expect_ident("expected map value binding")?;
        if value_name == key_name {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: format!(
                    "duplicate map iterator binding '{key_name}'; each binding must have a unique name"
                ),
            });
        }
        let value_schema = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_declared_type_schema()?)
        } else {
            None
        };
        self.expect(
            &TokenKind::RParen,
            "expected ')' after map iterator bindings",
        )?;
        if !self.match_ident_literal("in") {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: "expected 'in' after map iterator bindings".to_string(),
            });
        }
        let map = self.parse_expr()?;
        let map_slot = if let Expr::Borrow(inner) = &map
            && let Expr::Var(slot) = inner.as_ref()
        {
            *slot
        } else {
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: "map iteration requires an immutable local borrow; write `for (key, value) in &map { ... }`".to_string(),
            });
        };

        // Prevent binding aliasing with the borrowed source or each other.
        if let (Some(map_name), Some(key_name_str)) = (
            self.find_local_name_by_slot(map_slot),
            Some(key_name.as_str()),
        ) {
            if key_name_str == map_name {
                return Err(ParseError {
                    span: Some(self.current_span()),
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "map iterator key binding '{key_name_str}' shadows the borrowed source '{map_name}'"
                    ),
                });
            }
            if value_name == map_name {
                return Err(ParseError {
                    span: Some(self.current_span()),
                    code: None,
                    line: self.current_line(),
                    message: format!(
                        "map iterator value binding '{value_name}' shadows the borrowed source '{map_name}'"
                    ),
                });
            }
        }

        let key_slot = self.allocate_hidden_local()?;
        let value_slot = self.allocate_hidden_local()?;
        if self.enforce_mutable_bindings {
            self.set_local_slot_mutable(key_slot, false);
            self.set_local_slot_mutable(value_slot, false);
        }
        if let Some(schema) = &key_schema {
            self.local_schemas.insert(key_slot, schema.clone());
        }
        if let Some(schema) = &value_schema {
            self.local_schemas.insert(value_slot, schema.clone());
        }
        let iterator_slot = self.allocate_hidden_local()?;
        let iterator_id = Expr::Int(i64::from(iterator_slot));

        // Enforce map iterator binding schemas: keys must be strings and
        // value schema, if declared, must match the map element type.
        if matches!(key_schema.as_ref(), Some(schema) if !matches!(schema, TypeSchema::String)) {
            let label = key_schema
                .as_ref()
                .map(|schema| format!("{schema:?}"))
                .unwrap_or_default();
            return Err(ParseError {
                span: Some(self.current_span()),
                code: None,
                line: self.current_line(),
                message: format!("map iterator key binding must be 'string', found '{label}'"),
            });
        }
        if let Some(value_schema) = value_schema.as_ref() {
            let line_context = self.current_line();
            let Some((map_element_schema, found_label)) =
                self.infer_map_element_schema(map_slot)?
            else {
                return Err(ParseError {
                    span: Some(self.current_span()),
                    code: None,
                    line: line_context,
                    message: "cannot validate an explicit map iterator value schema because the source map has no declared map<T> schema"
                        .to_string(),
                });
            };
            let declared_label = format!("{value_schema:?}");
            if map_element_schema != *value_schema {
                return Err(ParseError {
                    span: Some(self.current_span()),
                    code: None,
                    line: line_context,
                    message: format!(
                        "map iterator value binding expects '{found_label}', found '{declared_label}'"
                    ),
                });
            }
        }

        let previous_key_slot = self.replace_current_local_binding(&key_name, key_slot);
        let previous_value_slot = self.replace_current_local_binding(&value_name, value_slot);
        let previous_key_schema = self.local_schemas.get(&key_slot).cloned();
        let previous_value_schema = self.local_schemas.get(&value_slot).cloned();
        if let Some(schema) = key_schema.as_ref() {
            self.local_schemas.insert(key_slot, schema.clone());
        }
        if let Some(schema) = value_schema.as_ref() {
            self.local_schemas.insert(value_slot, schema.clone());
        }
        self.borrowed_map_iter_locals.push(map_slot);
        self.loop_depth += 1;
        let body_result = self.parse_block("expected '{' after borrowed map");
        self.loop_depth -= 1;
        self.borrowed_map_iter_locals.pop();
        self.restore_local_schema(value_slot, previous_value_schema);
        self.restore_local_schema(key_slot, previous_key_schema);
        self.restore_current_local_binding(&value_name, previous_value_slot);
        self.restore_current_local_binding(&key_name, previous_key_slot);
        let mut body = body_result?;
        body.insert(
            0,
            Stmt::Let {
                index: value_slot,
                declared_schema: value_schema,
                expr: self.build_builtin_call_expr(
                    BuiltinFunction::MapIterTakeValue,
                    vec![iterator_id.clone()],
                )?,
                line,
            },
        );
        body.insert(
            0,
            Stmt::Let {
                index: key_slot,
                declared_schema: key_schema,
                expr: self.build_builtin_call_expr(
                    BuiltinFunction::MapIterTakeKey,
                    vec![iterator_id.clone()],
                )?,
                line,
            },
        );

        let loop_stmt = Stmt::For {
            init: Box::new(Stmt::Assign {
                kind: AssignmentKind::Set,
                index: map_slot,
                expr: self.build_builtin_call_expr(
                    BuiltinFunction::MapIterInit,
                    vec![map, iterator_id.clone()],
                )?,
                line,
            }),
            condition: self
                .build_builtin_call_expr(BuiltinFunction::MapIterNext, vec![iterator_id.clone()])?,
            post: Box::new(Stmt::Noop { line }),
            body,
            line,
        };
        let close_stmt = Stmt::Assign {
            kind: AssignmentKind::Set,
            index: map_slot,
            expr: self.build_builtin_call_expr(
                BuiltinFunction::MapIterClose,
                vec![Expr::Var(map_slot), iterator_id],
            )?,
            line,
        };
        Ok(Stmt::IfElse {
            condition: Expr::Bool(true),
            then_branch: vec![loop_stmt, close_stmt],
            else_branch: Vec::new(),
            line,
        })
    }

    fn replace_current_local_binding(&mut self, name: &str, slot: LocalSlot) -> Option<LocalSlot> {
        if let Some(scope) = self.closure_scopes.last_mut() {
            scope.insert(name.to_string(), slot)
        } else {
            self.locals.insert(name.to_string(), slot)
        }
    }

    fn restore_current_local_binding(&mut self, name: &str, previous: Option<LocalSlot>) {
        let bindings = if let Some(scope) = self.closure_scopes.last_mut() {
            scope
        } else {
            &mut self.locals
        };
        if let Some(slot) = previous {
            bindings.insert(name.to_string(), slot);
        } else {
            bindings.remove(name);
        }
    }

    fn restore_local_schema(&mut self, slot: LocalSlot, previous: Option<TypeSchema>) {
        if let Some(schema) = previous {
            self.local_schemas.insert(slot, schema);
        } else {
            self.local_schemas.remove(&slot);
        }
    }

    fn bind_for_loop_local(&mut self, name: &str) -> Result<(LocalSlot, bool), ParseError> {
        if !self.closure_scopes.is_empty() {
            if let Some(index) = self
                .closure_scopes
                .last()
                .and_then(|scope| scope.get(name))
                .copied()
            {
                return Ok((index, false));
            }
            let index = self.allocate_hidden_local()?;
            if let Some(scope) = self.closure_scopes.last_mut() {
                scope.insert(name.to_string(), index);
            }
            self.named_local_bindings.push((name.to_string(), index));
            return Ok((index, true));
        }
        self.get_or_assign_local(name)
    }

    fn parse_c_style_for(&mut self, line: u32) -> Result<Stmt, ParseError> {
        let parenthesized = if self.match_kind(&TokenKind::LParen) {
            if !self.dialect.allow_parenthesized_for_loop() {
                return Err(ParseError {
                    span: Some(self.current_span()),
                    code: None,
                    line: self.last_line() as usize,
                    message: "expected Rust-style for-in loop; write `for i in 0..n { ... }`"
                        .to_string(),
                });
            }
            true
        } else {
            false
        };

        let init = if self.match_kind(&TokenKind::Let) {
            self.parse_let_with_terminator(false)?
        } else if self.check_increment_start() {
            self.parse_increment_with_terminator(false)?
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

        let post = if self.check_increment_start() {
            self.parse_increment_with_terminator(false)?
        } else if self.check_assignment_start() {
            self.parse_assign_with_terminator(false)?
        } else {
            let post_line = self.current_line_u32();
            let expr = self.parse_expr()?;
            Stmt::Expr {
                expr,
                line: post_line,
            }
        };
        if parenthesized {
            self.expect(&TokenKind::RParen, "expected ')' after for clauses")?;
        }
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

    fn infer_map_element_schema(
        &self,
        slot: LocalSlot,
    ) -> Result<Option<(TypeSchema, String)>, ParseError> {
        if let Some(schema) = self.local_schemas.get(&slot) {
            let element = match schema {
                TypeSchema::Map(inner) => inner.as_ref().clone(),
                other => return Ok(Some((other.clone(), format!("{other:?}")))),
            };
            return Ok(Some((element.clone(), format!("{element:?}"))));
        }
        Ok(None)
    }
}
