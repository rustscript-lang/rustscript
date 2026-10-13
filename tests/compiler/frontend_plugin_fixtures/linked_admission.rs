use super::*;
use std::sync::{Arc, Mutex};
static HOOKS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
struct LinkedProducer;
static PRODUCER: LinkedProducer = LinkedProducer;
impl SourcePlugin for LinkedProducer {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("neutral-linked-policy"),
            extensions: &["linked"],
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
    fn module_type_exports(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
        ir: &FrontendIr,
    ) -> Result<Vec<vm::FrontendTypeExport>, ParseError> {
        graph_format::GRAPH_FORMAT.module_type_exports(source, context, ir)
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
            for stmt in &mut ir.stmts {
                resource_policy::remap_stmt(
                    stmt,
                    &std::collections::HashMap::from([(old.index, installed)]),
                );
            }
        }
        Ok(ir)
    }
    fn validate_typed_ir(&self, typed: &vm::FrontendTypedIr<'_>) -> Result<(), vm::CompileError> {
        let owner = typed.source_id.unwrap();
        HOOKS.lock().unwrap().push(owner);
        for decl in typed
            .ir
            .parsed_semantic_index
            .as_ref()
            .unwrap()
            .local_decls
            .iter()
            .filter(|decl| decl.ident_span.source_id == owner)
        {
            let initializer = typed
                .ir
                .stmts
                .iter()
                .find_map(|stmt| match stmt {
                    vm::Stmt::Let { index, expr, .. } if *index == decl.slot => Some(expr),
                    _ => None,
                })
                .unwrap();
            let schema = typed.schema_of_expr(initializer);
            if schema != Some(vm::TypeSchema::Int)
                || typed.schema_of_expr(&vm::Expr::Var(decl.slot)) != schema
            {
                return Err(vm::CompileError::StrictTypingRequired {
                    line: Some(1),
                    source_name: None,
                    detail: format!("integer initializer policy rejected {schema:?}"),
                    span: Some(decl.ident_span),
                });
            }
            if decl.name == "checked" {
                assert!(matches!(initializer, vm::Expr::Call(_, _, _, Some(_), _)));
            }
        }
        Ok(())
    }
}
#[test]
fn semantic_only_imported_guest_and_host_facts_are_linked_before_owned_hook() {
    let root = temp_root("linked-admission");
    let entry = root.join("main.graph");
    let library = root.join("lib.linked");
    let source =
        "bind type Box Item ./lib.linked\nlet value: Item = { value: 42 };\nvalue.value;\n";
    write_source(&entry, source);
    write_source(
        &root.join("value.fmt-one"),
        "export fn plus(x: int) -> int = x + 1",
    );
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "neutral::probe",
        vec![vm::HostParamSchema::value("value", vm::HostTypeSchema::Int)],
        vm::HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().unwrap());
    let configured = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&graph_format::GRAPH_FORMAT)
            .with_source_plugin(&PRODUCER)
            .with_source_plugin(&FORMAT_ONE)
            .with_host_api_catalog(catalog.clone())
    };
    let library_source = "bind value plus plus ./value.fmt-one\nstruct Box { value: int }\nlet n = plus(41);\nlet checked = probe(41);\npub fn done() -> int { 42 }\n";
    write_source(&library, library_source);
    for operation in 0..3 {
        HOOKS.lock().unwrap().clear();
        match operation {
            0 => {
                let compiled = compile_source_file_with_options(&entry, configured()).unwrap();
                assert!(compiled.functions.is_empty());
                assert!(compiled.program.callable_prototypes.is_empty());
                assert!(
                    !compiled
                        .program
                        .imports
                        .iter()
                        .any(|import| import.name == "neutral::probe")
                );
                let baseline = vm::compile_source_with_flavor_and_options(
                    "let value = { value: 42 };\nvalue.value;\n",
                    vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
                    configured(),
                )
                .unwrap();
                assert_eq!(compiled.program.local_count, baseline.program.local_count);
                let mut machine = Vm::new(compiled.program);
                assert_eq!(machine.run().unwrap(), VmStatus::Halted);
                assert_eq!(machine.stack(), &[Value::Int(42)]);
            }
            1 => {
                let model =
                    vm::compiler::analyze_source_file_with_options(&entry, configured()).unwrap();
                let n = model
                    .ir()
                    .parsed_semantic_index
                    .as_ref()
                    .unwrap()
                    .local_decls
                    .iter()
                    .find(|decl| decl.name == "n")
                    .unwrap();
                assert_eq!(
                    model.ir().semantic_index.as_ref().unwrap().slot_schemas[usize::from(n.slot)],
                    Some(vm::TypeSchema::Int)
                );
            }
            _ => {
                let hints = vm::collect_inferred_local_type_hints_at_path_with_options(
                    &entry,
                    source,
                    vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
                    configured(),
                )
                .unwrap();
                assert_eq!(
                    hints.iter().find(|hint| hint.name == "n").unwrap().schema,
                    Some(vm::TypeSchema::Int)
                );
            }
        }
        assert_eq!(*HOOKS.lock().unwrap(), vec![1]);
    }
    for invalid in [
        library_source.replace("plus(41)", "plus(\"wrong\")"),
        library_source.replace("let n = plus(41);", "plus(\"wrong\");"),
        library_source.replace("let n = plus(41);", "let n = true;"),
    ] {
        write_source(&library, &invalid);
        for operation in 0..3 {
            HOOKS.lock().unwrap().clear();
            let error = match operation {
                0 => compile_source_file_with_options(&entry, configured()).map(|_| ()),
                1 => {
                    vm::compiler::analyze_source_file_with_options(&entry, configured()).map(|_| ())
                }
                _ => vm::collect_inferred_local_type_hints_at_path_with_options(
                    &entry,
                    source,
                    vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
                    configured(),
                )
                .map(|_| ()),
            }
            .expect_err("semantic-only guest argument or real hook policy must reject");
            if invalid.contains("let n = true") {
                assert!(
                    error.to_string().contains("integer initializer policy"),
                    "{error}"
                );
                assert_eq!(*HOOKS.lock().unwrap(), vec![1]);
            } else {
                assert!(error.to_string().contains("argument"), "{error}");
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
