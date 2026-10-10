use super::*;

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
        graph_format::GRAPH_FORMAT.parse_source(source, context)
    }
    fn validate_typed_ir(&self, typed: &vm::FrontendTypedIr<'_>) -> Result<(), vm::CompileError> {
        let owner = typed.source_id.unwrap();
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
