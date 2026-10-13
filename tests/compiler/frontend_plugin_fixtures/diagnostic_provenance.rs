use super::*;

struct DiagnosticPlugin {
    mode: usize,
}
static DIAGNOSTIC_PLUGINS: [DiagnosticPlugin; 6] = [
    DiagnosticPlugin { mode: 0 },
    DiagnosticPlugin { mode: 1 },
    DiagnosticPlugin { mode: 2 },
    DiagnosticPlugin { mode: 3 },
    DiagnosticPlugin { mode: 4 },
    DiagnosticPlugin { mode: 5 },
];
impl SourcePlugin for DiagnosticPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("neutral-diagnostic-producer"),
            extensions: &["diagnostic"],
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
        graph_format::GRAPH_FORMAT
            .parse_source(source, context)
            .map_err(|mut error| {
                // Retain the genuine syntax diagnostic; vary only the producer's supplied provenance.
                let exact = source.find(';').unwrap();
                error.span = match self.mode {
                    0 => Some(vm::Span {
                        source_id: context.source_id,
                        lo: exact,
                        hi: exact + 1,
                    }),
                    1 => None,
                    2 => Some(vm::Span {
                        source_id: context.source_id + 1,
                        lo: exact,
                        hi: exact + 1,
                    }),
                    3 => Some(vm::Span {
                        source_id: context.source_id,
                        lo: exact + 1,
                        hi: exact,
                    }),
                    4 => Some(vm::Span {
                        source_id: context.source_id,
                        lo: exact,
                        hi: source.len() + 1,
                    }),
                    _ => Some(vm::Span {
                        source_id: context.source_id,
                        lo: source.find('中').unwrap() + 1,
                        hi: exact,
                    }),
                };
                error
            })
    }
}
#[test]
fn genuine_parser_diagnostics_preserve_valid_spans_and_fallback_for_all_invalid_byte_ranges() {
    let root = temp_root("diagnostic-fallback");
    let entry = root.join("main.graph");
    let dependency = root.join("lib.diagnostic");
    let malformed = "// 中文\r\n\r\nlet value = ;\r\n";
    for plugin in &DIAGNOSTIC_PLUGINS {
        for nested in [false, true] {
            let entry = if nested {
                entry.clone()
            } else {
                root.join("main.diagnostic")
            };
            let source = if nested {
                "bind namespace _ lib ./lib.diagnostic\n42;\n"
            } else {
                malformed
            };
            write_source(&entry, source);
            write_source(&dependency, malformed);
            let configured = || {
                CompileSourceFileOptions::new()
                    .with_source_plugin(plugin)
                    .with_source_plugin(&graph_format::GRAPH_FORMAT)
            };
            for operation in 0..3 {
                let error = match operation {
                    0 => compile_source_file_with_options(&entry, configured())
                        .err()
                        .unwrap(),
                    1 => vm::compiler::analyze_source_file_with_options(&entry, configured())
                        .err()
                        .unwrap(),
                    _ => vm::collect_inferred_local_type_hints_at_path_with_options(
                        &entry,
                        source,
                        vm::SourceFlavor::Plugin(FrontendId::new(if nested {
                            "neutral-graph-format"
                        } else {
                            "neutral-diagnostic-producer"
                        })),
                        configured(),
                    )
                    .err()
                    .unwrap(),
                };
                let vm::SourcePathError::SourceWithMap {
                    error: vm::SourceError::Parse(parse),
                    sources,
                } = error
                else {
                    panic!("real parser error with sources required")
                };
                assert!(parse.message.contains("expected"), "{}", parse.message);
                let span = parse.span.unwrap();
                let owner = u32::from(nested);
                assert_eq!(span.source_id, owner);
                if plugin.mode == 0 {
                    assert_eq!(span.lo, malformed.find(';').unwrap());
                    assert_eq!(sources.span_text(span), Some(";"));
                } else {
                    assert_eq!(span, sources.line_span(owner, 3).unwrap());
                    assert_eq!(sources.span_text(span).unwrap().trim_end(), "let value = ;");
                }
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn parameter_hints_keep_optional_schema_and_exact_chinese_crlf_declaration_bytes() {
    let root = temp_root("parameter-provenance");
    let entry = root.join("main.graph");
    let source =
        "// 中文\r\nfn maybe(parameter: int?) -> int? { parameter }\r\nlet callback = maybe;\r\n";
    write_source(&entry, source);
    let hints = vm::collect_inferred_local_type_hints_at_path_with_options(
        &entry,
        source,
        vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
        CompileSourceFileOptions::new().with_source_plugin(&graph_format::GRAPH_FORMAT),
    )
    .unwrap();
    let parameter = hints.iter().find(|hint| hint.name == "parameter").unwrap();
    assert_eq!(
        parameter.schema,
        Some(vm::TypeSchema::Optional(Box::new(vm::TypeSchema::Int)))
    );
    let span = parameter.span.unwrap();
    assert_eq!(span.source_id, 0);
    assert_eq!(span.lo, source.find("parameter").unwrap());
    assert_eq!(&source[span.lo..span.hi], "parameter");
    std::fs::remove_dir_all(root).unwrap();
}
