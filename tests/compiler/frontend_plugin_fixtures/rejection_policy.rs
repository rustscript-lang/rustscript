use super::*;
static HOOKS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
static MIXED_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct RejectionPolicy {
    id: &'static str,
    extensions: &'static [&'static str],
    required: vm::TypeSchema,
}
static ROOT_POLICY: RejectionPolicy = RejectionPolicy {
    id: "neutral-predicate-policy",
    extensions: &["predicate"],
    required: vm::TypeSchema::Bool,
};
static DEPENDENCY_POLICY: RejectionPolicy = RejectionPolicy {
    id: "neutral-label-policy",
    extensions: &["label"],
    required: vm::TypeSchema::String,
};
impl SourcePlugin for RejectionPolicy {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.id),
            extensions: self.extensions,
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_module_imports(
        &self,
        source: &str,
        path: &Path,
        context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<ModuleImport>, vm::SourcePathError> {
        graph_format::GRAPH_FORMAT.parse_module_imports(source, path, context)
    }
    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = graph_format::GRAPH_FORMAT.parse_source(source, context)?;
        if let Some(old) = ir
            .functions
            .iter()
            .find(|decl| decl.name == "probe")
            .cloned()
        {
            ir.functions.retain(|decl| decl.index != old.index);
            ir.implicit_extern_names.retain(|name| name != "probe");
            let installed = context.install_host_function(&mut ir, "neutral::probe", 1)?;
            let indexes = std::collections::HashMap::from([(old.index, installed)]);
            for stmt in &mut ir.stmts {
                resource_policy::remap_stmt(stmt, &indexes);
            }
        }
        Ok(ir)
    }
    fn validate_typed_ir(&self, typed: &vm::FrontendTypedIr<'_>) -> Result<(), vm::CompileError> {
        let owner = typed.source_id.unwrap();
        if typed
            .host_api_catalog
            .is_some_and(|catalog| catalog.function("neutral::probe").is_some())
        {
            let checked = typed
                .ir
                .parsed_semantic_index
                .as_ref()
                .unwrap()
                .local_decls
                .iter()
                .find(|decl| decl.name == "checked" && decl.ident_span.source_id == owner)
                .unwrap();
            let initializer = typed
                .ir
                .stmts
                .iter()
                .find_map(|stmt| match stmt {
                    vm::Stmt::Let { index, expr, .. } if *index == checked.slot => Some(expr),
                    _ => None,
                })
                .unwrap();
            assert!(matches!(initializer, vm::Expr::Call(_, _, _, Some(_), _)));
            assert_eq!(typed.schema_of_expr(initializer), Some(vm::TypeSchema::Int));
            assert_eq!(
                typed.schema_of_expr(&vm::Expr::Var(checked.slot)),
                Some(vm::TypeSchema::Int)
            );
            HOOKS.lock().unwrap().push(owner);
        }
        let declaration = typed
            .ir
            .parsed_semantic_index
            .as_ref()
            .unwrap()
            .local_decls
            .iter()
            .find(|decl| decl.name == "result" && decl.ident_span.source_id == owner)
            .expect("validator must query its own result declaration");
        let schema = typed.schema_of_expr(&vm::Expr::Var(declaration.slot));
        if schema == Some(self.required.clone()) {
            return Ok(());
        }
        Err(vm::CompileError::StrictTypingRequired {
            line: Some(2),
            source_name: None,
            detail: format!(
                "{} requires {:?}, observed {:?}",
                self.id, self.required, schema
            ),
            span: Some(declaration.ident_span),
        })
    }
}

#[test]
fn distinct_root_and_dependency_semantic_rejections_preserve_owner_across_checked_entries() {
    let root = temp_root("semantic-rejections");
    let entry = root.join("main.predicate");
    let library = root.join("lib.label");
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&ROOT_POLICY)
            .with_source_plugin(&DEPENDENCY_POLICY)
    };
    let valid_root = "bind value answer answer ./lib.label\nlet result = true;\nanswer();\n";
    let valid_dependency =
        "// 中文\r\nlet result = \"label\";\r\npub fn answer() -> int { 42 }\r\n";
    for invalid_owner in [None, Some(0), Some(1)] {
        let source = if invalid_owner == Some(0) {
            valid_root.replace("true", "1")
        } else {
            valid_root.to_string()
        };
        let dependency = if invalid_owner == Some(1) {
            valid_dependency.replace("\"label\"", "1")
        } else {
            valid_dependency.to_string()
        };
        write_source(&entry, &source);
        write_source(&library, &dependency);
        for operation in 0..3 {
            let result = match operation {
                0 => compile_source_file_with_options(&entry, options()).map(|compiled| {
                    let mut machine = Vm::new(compiled.program);
                    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
                    assert_eq!(machine.stack().last(), Some(&Value::Int(42)));
                }),
                1 => vm::compiler::analyze_source_file_with_options(&entry, options()).map(|_| ()),
                _ => vm::collect_inferred_local_type_hints_at_path_with_options(
                    &entry,
                    &source,
                    vm::SourceFlavor::Plugin(FrontendId::new(ROOT_POLICY.id)),
                    options(),
                )
                .map(|_| ()),
            };
            if let Some(owner) = invalid_owner {
                let error =
                    result.expect_err("semantic policy rejects the actual inferred integer");
                let expected_policy = if owner == 0 {
                    ROOT_POLICY.id
                } else {
                    DEPENDENCY_POLICY.id
                };
                assert!(error.to_string().contains(expected_policy), "{error}");
                assert!(error.to_string().contains("Some(Int)"), "{error}");
                let vm::SourcePathError::SourceWithMap {
                    error:
                        vm::SourceError::Compile(vm::CompileError::StrictTypingRequired {
                            span: Some(span),
                            ..
                        }),
                    sources,
                } = error
                else {
                    panic!("source-owned validation error required")
                };
                assert_eq!(span.source_id, owner);
                assert_eq!(sources.span_text(span), Some("result"));
            } else {
                result.unwrap();
            }
        }
    }
    // A native root still invokes the dependency's semantic checker.
    let native = root.join("main.rss");
    write_source(&native, "use self::lib as lib;\nlib::answer();\n");
    let error = compile_source_file_with_options(
        &native,
        options().with_module_override_path("./lib.rss", &library),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains(DEPENDENCY_POLICY.id), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unresolved_host_result_recovery_keeps_independent_parameter_admission() {
    let _lock = MIXED_SERIAL.lock().unwrap();
    let root = temp_root("host-result-admission");
    let entry = root.join("main.predicate");
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "neutral::probe",
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Int,
    ));
    let catalog = std::sync::Arc::new(builder.build().unwrap());
    let configured = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&ROOT_POLICY)
            .with_host_api_catalog(catalog.clone())
    };
    let source = "let result = true;\nlet checked = probe(41);\nlet failed = probe(\"wrong\");\n";
    write_source(&entry, source);
    HOOKS.lock().unwrap().clear();
    let model = vm::compiler::analyze_source_file_with_options(&entry, configured())
        .expect("host-only failure retains diagnostic model despite unresolved result local");
    assert_eq!(model.diagnostics().len(), 1);
    let span = model.diagnostics()[0].span.unwrap();
    assert_eq!(model.sources().span_text(span), Some("probe"));
    assert_eq!(*HOOKS.lock().unwrap(), vec![0]);
    assert!(compile_source_file_with_options(&entry, configured()).is_err());
    assert!(
        vm::collect_inferred_local_type_hints_at_path_with_options(
            &entry,
            source,
            vm::SourceFlavor::Plugin(FrontendId::new(ROOT_POLICY.id)),
            configured(),
        )
        .is_err()
    );
    let source = format!("{source}fn unresolved(value) -> int {{ 42 }}\n");
    write_source(&entry, &source);
    let error = vm::compiler::analyze_source_file_with_options(&entry, configured())
        .err()
        .expect("independent untyped parameter must remain rejected");
    let vm::SourcePathError::SourceWithMap {
        error:
            vm::SourceError::Compile(vm::CompileError::StrictTypingRequired {
                span: Some(span),
                detail,
                ..
            }),
        sources,
    } = error
    else {
        panic!("independent strict parameter diagnostic required")
    };
    assert!(detail.contains("parameter"), "{detail}");
    assert_eq!(sources.span_text(span), Some("value"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn host_recovery_retains_mixed_owner_hooks_and_independent_strict_errors() {
    let _lock = MIXED_SERIAL.lock().unwrap();
    let root = temp_root("mixed-admission");
    let entry = root.join("main.predicate");
    let library = root.join("lib.label");
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "neutral::probe",
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Int,
    ));
    let catalog = std::sync::Arc::new(builder.build().unwrap());
    let configured = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&ROOT_POLICY)
            .with_source_plugin(&DEPENDENCY_POLICY)
            .with_host_api_catalog(catalog.clone())
    };
    for rejected in [0, 1, 2] {
        let root_result = if rejected == 1 { "true" } else { "1" };
        let dependency_result = if rejected == 0 { "\"label\"" } else { "1" };
        let source = format!(
            "bind value answer answer ./lib.label\nlet result = {root_result};\nlet checked = probe(41);\nprobe(\"wrong\");\nanswer();\n"
        );
        write_source(&entry, &source);
        write_source(
            &library,
            &format!(
                "let result = {dependency_result};\nlet checked = probe(41);\npub fn answer() -> int {{ 42 }}\n"
            ),
        );
        for operation in 0..3 {
            HOOKS.lock().unwrap().clear();
            let error = match operation {
                0 => compile_source_file_with_options(&entry, configured()).map(|_| ()),
                1 => {
                    vm::compiler::analyze_source_file_with_options(&entry, configured()).map(|_| ())
                }
                _ => vm::collect_inferred_local_type_hints_at_path_with_options(
                    &entry,
                    &source,
                    vm::SourceFlavor::Plugin(FrontendId::new(ROOT_POLICY.id)),
                    configured(),
                )
                .map(|_| ()),
            }
            .expect_err("real semantic rejection must survive wrong installed host call");
            let hooks = HOOKS.lock().unwrap().clone();
            assert!(!hooks.is_empty());
            assert_eq!(
                hooks
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                hooks.len()
            );
            let owner = if rejected == 2 {
                *hooks.last().unwrap()
            } else {
                rejected
            };
            assert_eq!(hooks.last(), Some(&owner));
            let vm::SourcePathError::SourceWithMap {
                error:
                    vm::SourceError::Compile(vm::CompileError::StrictTypingRequired {
                        span: Some(span),
                        ..
                    }),
                sources,
            } = error
            else {
                panic!("owner policy error required")
            };
            assert_eq!(span.source_id, owner);
            assert_eq!(sources.span_text(span), Some("result"));
        }
    }
    let source = "bind value answer answer ./lib.label\nlet result = true;\nlet checked = probe(41);\nlet wrong: int = \"bad\";\nprobe(\"wrong\");\nanswer();\n";
    write_source(&entry, source);
    write_source(
        &library,
        "let result = \"label\";\nlet checked = probe(41);\npub fn answer() -> int { 42 }\n",
    );
    let error = vm::compiler::analyze_source_file_with_options(&entry, configured())
        .err()
        .unwrap();
    assert!(!matches!(
        &error,
        vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Compile(vm::CompileError::HostCallResolve { .. }),
            ..
        }
    ));
    assert!(
        error.to_string().contains("schema") || error.to_string().contains("type"),
        "{error}"
    );
    std::fs::remove_dir_all(root).unwrap();
}
