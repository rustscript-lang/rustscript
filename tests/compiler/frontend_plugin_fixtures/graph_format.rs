use super::*;

pub(super) struct GraphFormat;
pub(super) static GRAPH_FORMAT: GraphFormat = GraphFormat;
impl SourcePlugin for GraphFormat {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("neutral-graph-format"),
            extensions: &["graph"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_module_imports(
        &self,
        source: &str,
        _path: &Path,
        context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<ModuleImport>, vm::SourcePathError> {
        let mut imports = Vec::new();
        let mut offset = 0;
        for (line, text) in source.split_inclusive('\n').enumerate() {
            if text.starts_with("bind ") {
                let parts: Vec<_> = text.split_whitespace().collect();
                if parts.len() != 5 {
                    let lo = offset + text.find("bind").unwrap();
                    return Err(vm::SourcePathError::Source(vm::SourceError::Parse(
                        ParseError {
                            line: line + 1,
                            message:
                                "graph binding requires kind, imported name, local name and module"
                                    .to_string(),
                            span: Some(vm::Span {
                                source_id: context.source_id,
                                lo,
                                hi: lo + 4,
                            }),
                            code: None,
                        },
                    )));
                }
                let clause = match parts[1] {
                    "type" => ImportClause::Named(vec![NamedImport::type_only(parts[2], parts[3])]),
                    "value" => ImportClause::Named(vec![NamedImport::value(parts[2], parts[3])]),
                    "namespace" | "type-namespace" => ImportClause::Namespace(parts[3].to_string()),
                    _ => panic!("invalid graph binding"),
                };
                let mut import = ModuleImport::value(parts[4], clause, line + 1);
                if parts[1] == "type-namespace" {
                    import = import.type_only();
                }
                import.span = Some(vm::Span {
                    source_id: context.source_id,
                    lo: offset,
                    hi: offset + text.trim_end().len(),
                });
                imports.push(import);
            }
            offset += text.len();
        }
        Ok(imports)
    }
    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let lowered = source
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with("bind ") || line.starts_with("alias ") {
                    line.chars()
                        .map(|c| if c == '\r' || c == '\n' { c } else { ' ' })
                        .collect::<String>()
                } else {
                    line.to_string()
                }
            })
            .collect::<String>();
        parse_source_with_dialect(
            &lowered,
            &DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                import_scan_mode: true,
                ..SharedParserOptions::default()
            },
        )
    }
    fn module_type_exports(
        &self,
        source: &str,
        _context: &SourceFrontendContext<'_>,
        ir: &FrontendIr,
    ) -> Result<Vec<vm::compiler::FrontendTypeExport>, ParseError> {
        let mut exports = ir
            .struct_schemas
            .values()
            .filter(|decl| decl.origin == vm::compiler::StructDeclOrigin::Guest)
            .map(|decl| vm::compiler::FrontendTypeExport {
                source_name: decl.name.clone(),
                schema: vm::TypeSchema::Named(decl.name.clone(), vec![]),
                declaration: Some(decl.clone()),
            })
            .collect::<Vec<_>>();
        for line in source.lines().filter(|line| line.starts_with("alias ")) {
            let parts: Vec<_> = line.split_whitespace().collect();
            let decl = ir.struct_schemas.get(parts[2]).expect("alias declaration");
            exports.push(vm::compiler::FrontendTypeExport {
                source_name: parts[1].to_string(),
                schema: vm::TypeSchema::Named(decl.name.clone(), vec![]),
                declaration: Some(decl.clone()),
            });
        }
        Ok(exports)
    }
}

#[test]
fn public_consumer_can_query_independent_declaration_namespaces() {
    use vm::DeclarationNamespace;
    let mut graph = vm::ModuleGraph::new();
    let module = graph.add_node(
        PathBuf::from("neutral.module"),
        "neutral.module".to_string(),
        vec![],
    );
    let value = graph
        .add_declaration_in_namespace(module, DeclarationNamespace::Value, "Item", true)
        .unwrap();
    let typ = graph
        .add_declaration_in_namespace(module, DeclarationNamespace::Type, "Item", true)
        .unwrap();
    assert_ne!(value, typ);
    assert_eq!(
        graph.declaration_symbol_in_namespace(module, DeclarationNamespace::Value, "Item"),
        Some(value)
    );
    assert_eq!(
        graph.declaration_symbol_in_namespace(module, DeclarationNamespace::Type, "Item"),
        Some(typ)
    );
}

#[test]
fn import_diagnostics_keep_exact_utf8_byte_span_in_root_and_dependency() {
    let root = temp_root("import-span");
    let malformed = "// 中文\r\n\r\nbind type\r\n";
    for dependency in [false, true] {
        let entry = root.join("main.graph");
        if dependency {
            write_source(&root.join("lib.graph"), malformed);
            write_source(&entry, "bind namespace _ lib ./lib.graph\n42;\n");
        } else {
            write_source(&entry, malformed);
        }
        let error = compile_source_file_with_options(
            &entry,
            CompileSourceFileOptions::new().with_source_plugin(&GRAPH_FORMAT),
        )
        .err()
        .unwrap();
        let vm::SourcePathError::SourceWithMap {
            error: vm::SourceError::Parse(parse),
            sources,
        } = error
        else {
            panic!("expected source-owned import diagnostic");
        };
        let span = parse.span.unwrap();
        assert_eq!(span.source_id, u32::from(dependency));
        assert_eq!(span.lo, malformed.find("bind").unwrap());
        assert_eq!(span.hi, span.lo + 4);
        assert_eq!(
            &sources.source(span.source_id).unwrap()[span.lo..span.hi],
            "bind"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn type_and_value_imports_with_same_local_spelling_resolve_independently() {
    let root = temp_root("namespaces");
    write_source(&root.join("type.fmt-type"), "export type Box = int");
    write_source(
        &root.join("value.fmt-one"),
        "export fn plus(value: int) -> int = value + 1",
    );
    let entry = root.join("main.graph");
    write_source(
        &entry,
        "bind type Box Item ./type.fmt-type\nbind value plus Item ./value.fmt-one\nItem(41);",
    );
    let compiled = compile_source_file_with_options(
        &entry,
        CompileSourceFileOptions::new()
            .with_source_plugin(&GRAPH_FORMAT)
            .with_source_plugin(&FORMAT_ONE)
            .with_source_plugin(&FORMAT_TYPE_EXPORT),
    )
    .expect("independent type/value names must not be ambiguous calls");
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn type_namespace_does_not_authorize_value_call() {
    let root = temp_root("type-namespace");
    write_source(
        &root.join("lib.graph"),
        "struct Box { value: int } pub fn hidden() -> int { 99 }",
    );
    let entry = root.join("main.graph");
    write_source(
        &entry,
        "bind type-namespace _ types ./lib.graph\ntypes::hidden();",
    );
    let error = compile_source_file_with_options(
        &entry,
        CompileSourceFileOptions::new().with_source_plugin(&GRAPH_FORMAT),
    )
    .err()
    .expect("type namespace cannot provide executable functions");
    assert!(error.to_string().contains("hidden"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn independent_generic_nominal_modules_keep_identity_and_arity() {
    let root = temp_root("generic-nominal");
    for directory in ["left", "right"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    write_source(
        &root.join("left/lib.graph"),
        "struct Box<T> { value: T }\nalias Alias Box\n",
    );
    write_source(
        &root.join("right/lib.graph"),
        "struct Box<T> { value: T }\n",
    );
    let entry = root.join("main.graph");
    let source = "bind type Alias Left ./left/lib.graph\nbind type Box Right ./right/lib.graph\nlet left: Left<int> = { value: 42 };\nlet right: Right<string> = { value: \"other\" };\nleft.value;\n";
    write_source(&entry, source);
    let configured = || CompileSourceFileOptions::new().with_source_plugin(&GRAPH_FORMAT);
    let compiled = compile_source_file_with_options(&entry, configured()).unwrap();
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    let hints = vm::collect_inferred_local_type_hints_at_path_with_options(
        &entry,
        source,
        vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
        configured(),
    )
    .unwrap();
    let left = hints.iter().find(|hint| hint.name == "left").unwrap();
    let right = hints.iter().find(|hint| hint.name == "right").unwrap();
    let (
        Some(vm::TypeSchema::Named(left_name, left_args)),
        Some(vm::TypeSchema::Named(right_name, right_args)),
    ) = (&left.schema, &right.schema)
    else {
        panic!("nominal schemas required");
    };
    assert_ne!(left_name, right_name);
    assert_eq!(left_args, &[vm::TypeSchema::Int]);
    assert_eq!(right_args, &[vm::TypeSchema::String]);
    assert_eq!(left.inferred_type, "Left<int>");
    assert_eq!(right.inferred_type, "Right<string>");
    write_source(
        &entry,
        "bind type Alias Left ./left/lib.graph\nlet value: Left<int, string> = { value: 42 };\n",
    );
    assert!(
        compile_source_file_with_options(&entry, configured()).is_err(),
        "generic arity is checked after alias canonicalization"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn nominal_export_alias_uses_underlying_declaration_identity() {
    let root = temp_root("nominal-alias");
    write_source(
        &root.join("lib.graph"),
        "struct Box { value: int }\nalias Alias Box\n",
    );
    let entry = root.join("main.graph");
    write_source(
        &entry,
        "bind type Alias Item ./lib.graph\nlet value: Item = { value: 42 };\nvalue.value;",
    );
    let compiled = compile_source_file_with_options(
        &entry,
        CompileSourceFileOptions::new().with_source_plugin(&GRAPH_FORMAT),
    )
    .expect("export spelling must not mint a different nominal type");
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn nominal_hint_keeps_source_alias_label_and_declaration_span() {
    let root = temp_root("nominal-hint");
    write_source(
        &root.join("lib.graph"),
        "struct Box { value: int }\nalias Alias Box\n",
    );
    let entry = root.join("main.graph");
    let source = "// 中文\r\nbind type Alias Item ./lib.graph\r\nlet value: Item = { value: 42 };\r\nvalue.value;\r\n";
    write_source(&entry, source);
    let hints = vm::collect_inferred_local_type_hints_at_path_with_options(
        &entry,
        source,
        vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
        CompileSourceFileOptions::new().with_source_plugin(&GRAPH_FORMAT),
    )
    .unwrap();
    let hint = hints
        .iter()
        .find(|hint| hint.name == "value")
        .expect("root value hint");
    assert_eq!(
        hint.inferred_type, "Item",
        "logical label must retain the source alias"
    );
    let span = hint.span.expect("logical declaration span");
    assert_eq!(span.source_id, 0);
    assert_eq!(&source[span.lo..span.hi], "value");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn conflicting_type_alias_imports_report_owned_ambiguity() {
    let root = temp_root("duplicate-types");
    for name in ["one", "two"] {
        write_source(
            &root.join(format!("{name}.fmt-type")),
            "export type Box = int",
        );
    }
    let entry = root.join("main.graph");
    write_source(
        &entry,
        "bind type Box Item ./one.fmt-type\nbind type Box Item ./two.fmt-type\n42;",
    );
    let error = compile_source_file_with_options(
        &entry,
        CompileSourceFileOptions::new()
            .with_source_plugin(&GRAPH_FORMAT)
            .with_source_plugin(&FORMAT_TYPE_EXPORT),
    )
    .err()
    .expect("different type symbol imports with one local name are ambiguous");
    assert!(error.to_string().contains("Item"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}
