use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vm::arg;
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceMut, ResourceOwned, ResourceRef,
};
use vm::{
    HostApiBuilder, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
    ResourceTypeKey, ResourceTypeSchema,
};
static CALLS: AtomicUsize = AtomicUsize::new(0);
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Handle(i64);
impl HostResource for Handle {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("neutral.handle").unwrap())
    }
    fn begin_close(
        &mut self,
        _: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}
impl vm::HostResourceType for Handle {
    const KEY: &'static str = "neutral.handle";
    const DESCRIPTION: &'static str = "neutral handle";
}
mod adapters {
    use super::*;
    /// Execute a typed neutral resource operation.
    #[pd_host_function::pd_host_function(name = "neutral::read")]
    pub fn read(handle: ResourceRef<'_, Handle>) -> VmResult<i64> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(handle.get().0)
    }
    /// Execute a typed neutral resource operation.
    #[pd_host_function::pd_host_function(name = "neutral::change")]
    pub fn change(mut handle: ResourceMut<'_, Handle>) -> VmResult<i64> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        handle.get().0 += 1;
        Ok(handle.get().0)
    }
    /// Execute a typed neutral resource operation.
    #[pd_host_function::pd_host_function(name = "neutral::close")]
    pub fn close(handle: ResourceOwned<Handle>) -> VmResult<i64> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(handle.into_inner().0)
    }
}
struct ResourceProducer;
struct ResourceDialect;
impl ParserDialect for ResourceDialect {
    fn allow_let_mut_binding(&self) -> bool {
        true
    }
    fn allow_arrow_closure(&self) -> bool {
        true
    }
}
static RESOURCE_DIALECT: ResourceDialect = ResourceDialect;
static PRODUCER: ResourceProducer = ResourceProducer;
impl SourcePlugin for ResourceProducer {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("neutral-resource-producer"),
            extensions: &["owned"],
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
        let mut ir = parse_source_with_dialect(
            source,
            &RESOURCE_DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                import_scan_mode: true,
                enforce_mutable_bindings: true,
                ..SharedParserOptions::default()
            },
        )?;
        let placeholders = ir
            .functions
            .iter()
            .filter(|decl| ir.implicit_extern_names.contains(&decl.name))
            .cloned()
            .collect::<Vec<_>>();
        ir.functions
            .retain(|decl| !placeholders.iter().any(|old| old.index == decl.index));
        ir.implicit_extern_names.clear();
        let mut indexes = std::collections::HashMap::new();
        for declaration in placeholders {
            let name = match declaration.name.as_str() {
                "open_handle" => "neutral::open",
                "read_handle" => "neutral::read",
                "change_handle" => "neutral::change",
                "close_handle" => "neutral::close",
                _ => return Err(ParseError::new("unexpected resource alias")),
            };
            let installed = context.install_host_function(&mut ir, name, declaration.arity)?;
            indexes.insert(declaration.index, installed);
        }
        for stmt in &mut ir.stmts {
            remap_stmt(stmt, &indexes);
        }
        for implementation in ir.function_impls.values_mut() {
            for stmt in &mut implementation.body_stmts {
                remap_stmt(stmt, &indexes);
            }
            remap_expr(&mut implementation.body_expr, &indexes);
        }
        Ok(ir)
    }
}
fn remap_stmt(stmt: &mut vm::Stmt, indexes: &std::collections::HashMap<u16, u16>) {
    match stmt {
        vm::Stmt::Let { expr, .. } | vm::Stmt::Expr { expr, .. } => remap_expr(expr, indexes),
        vm::Stmt::ClosureLet { closure, .. } => remap_expr(&mut closure.body, indexes),
        vm::Stmt::FuncDecl { .. } | vm::Stmt::Noop { .. } => {}
        _ => panic!("unsupported resource fixture statement"),
    }
}
fn remap_expr(expr: &mut vm::Expr, indexes: &std::collections::HashMap<u16, u16>) {
    match expr {
        vm::Expr::Call(index, _, args, ..) => {
            if let Some(installed) = indexes.get(index) {
                *index = *installed;
            }
            for arg in args {
                remap_expr(arg, indexes);
            }
        }
        vm::Expr::Closure(closure) => remap_expr(&mut closure.body, indexes),
        vm::Expr::Block { stmts, expr } => {
            for stmt in stmts {
                remap_stmt(stmt, indexes);
            }
            remap_expr(expr, indexes);
        }
        vm::Expr::Borrow(expr) | vm::Expr::BorrowMut(expr) => remap_expr(expr, indexes),
        vm::Expr::LocalCall(_, _, args, _) => {
            for arg in args {
                remap_expr(arg, indexes);
            }
        }
        vm::Expr::Int(_) | vm::Expr::Var(_) | vm::Expr::Null => {}
        _ => panic!("unsupported resource fixture expression"),
    }
}
fn catalog() -> Arc<vm::HostApiCatalog> {
    let key = ResourceTypeKey::new("neutral.handle").unwrap();
    let mut b = HostApiBuilder::new();
    b.resource(ResourceTypeSchema::new(key.clone(), "neutral handle"));
    b.function(HostFunctionSchema::with_return(
        "neutral::open",
        vec![],
        HostTypeSchema::Resource(key.clone()),
    ));
    for (name, passing) in [
        ("neutral::read", HostParamPassing::Borrow),
        ("neutral::change", HostParamPassing::BorrowMut),
        ("neutral::close", HostParamPassing::TakeOwned),
    ] {
        b.function(HostFunctionSchema::with_return(
            name,
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(key.clone()),
                passing,
            )],
            HostTypeSchema::Int,
        ));
    }
    Arc::new(b.build().unwrap())
}
fn options(catalog: &Arc<vm::HostApiCatalog>) -> CompileSourceFileOptions {
    CompileSourceFileOptions::new()
        .with_source_plugin(&PRODUCER)
        .with_host_api_catalog(catalog.clone())
}
fn run(program: vm::Program, catalog: &vm::HostApiCatalog) -> Vm {
    let mut registry = vm::HostFunctionRegistry::new();
    for schema in vm::catalog_import_schemas(catalog, "neutral::open") {
        fn open(machine: &mut Vm, _: &[Value]) -> vm::VmResult<vm::CallOutcome> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            let token = machine.host_context().push_resource(Handle(41)).unwrap();
            Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(
                token.handle().raw() as i64,
            ))))
        }
        registry
            .register_exact_static("neutral::open", 0, schema, open)
            .unwrap();
    }
    fn read(machine: &mut Vm, args: &[Value]) -> VmResult<vm::CallOutcome> {
        adapters::read(machine, args)
            .map(|result| vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(result))))
    }
    fn change(machine: &mut Vm, args: &[Value]) -> VmResult<vm::CallOutcome> {
        adapters::change(machine, args)
            .map(|result| vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(result))))
    }
    fn close(machine: &mut Vm, args: &[Value]) -> VmResult<vm::CallOutcome> {
        adapters::close(machine, args)
            .map(|result| vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(result))))
    }
    for (name, adapter) in [
        (
            "neutral::read",
            read as fn(&mut Vm, &[Value]) -> VmResult<vm::CallOutcome>,
        ),
        ("neutral::change", change),
        ("neutral::close", close),
    ] {
        for schema in vm::catalog_import_schemas(catalog, name) {
            registry
                .register_exact_static(name, 1, schema, adapter)
                .unwrap();
        }
    }
    let mut machine = Vm::new(program);
    registry.bind_vm_cached(&mut machine).unwrap();
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    machine
}
#[test]
fn enabled_resource_policy_repeated_borrow_mutation_close_and_rejection_before_host_execution() {
    let _lock = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CALLS.store(0, Ordering::SeqCst);
    let catalog = catalog();
    let source = "let mut handle = open_handle();\nlet first = read_handle(handle);\nlet second = read_handle(handle);\nlet changed = change_handle(&mut handle);\nclose_handle(handle);\n";
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::Plugin(FrontendId::new("neutral-resource-producer")),
        options(&catalog),
    )
    .unwrap();
    let mut machine = run(compiled.program, &catalog);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    assert_eq!(CALLS.load(Ordering::SeqCst), 5);
    assert_eq!(machine.host_context().resource_count(), 0);
    for invalid in [
        "let handle = open_handle(); close_handle(handle); read_handle(handle);",
        "let handle = open_handle(); let escape = || read_handle(handle); escape();",
    ] {
        CALLS.store(0, Ordering::SeqCst);
        let error = vm::compile_source_with_flavor_and_options(
            invalid,
            vm::SourceFlavor::Plugin(FrontendId::new("neutral-resource-producer")),
            options(&catalog),
        )
        .err()
        .expect("resource misuse must fail before execution");
        assert!(
            error.to_string().contains("moved") || error.to_string().contains("borrow"),
            "{error}"
        );
        let analysis =
            vm::analyze_source_from_string_with_options("main.owned", invalid, options(&catalog))
                .err()
                .expect("analysis rejects resource misuse");
        let hints = vm::collect_inferred_local_type_hints_with_options(
            invalid,
            vm::SourceFlavor::Plugin(FrontendId::new("neutral-resource-producer")),
            options(&catalog),
        )
        .expect_err("hints reject resource misuse");
        assert_eq!(error.to_string(), analysis.to_string());
        assert_eq!(error.to_string(), hints.to_string());
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn semantic_only_resource_producer_retains_type_provenance_without_runtime_authority() {
    let _lock = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = temp_root("semantic-resource");
    let catalog = catalog();
    let library = "struct Box { value: int }\nlet handle = open_handle();\nread_handle(handle);\nclose_handle(handle);\npub fn done() -> int { 42 }\n";
    write_source(&root.join("lib.owned"), library);
    let entry = root.join("main.graph");
    let configured = || options(&catalog).with_source_plugin(&graph_format::GRAPH_FORMAT);
    let source = "bind type Box Item ./lib.owned\nlet value: Item = { value: 42 };\nvalue.value;\n";
    write_source(&entry, source);
    CALLS.store(0, Ordering::SeqCst);
    let compiled = compile_source_file_with_options(&entry, configured()).unwrap();
    assert!(compiled.functions.is_empty());
    assert!(
        !compiled
            .program
            .imports
            .iter()
            .any(|import| import.name.starts_with("neutral::"))
    );
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    let model = vm::compiler::analyze_source_file_with_options(&entry, configured()).unwrap();
    let provenance = &model
        .ir()
        .parsed_semantic_index
        .as_ref()
        .unwrap()
        .type_provenance;
    let original = provenance
        .iter()
        .find(|site| site.source_id == 1 && site.source_name == "Box")
        .expect("type-only producer declaration provenance");
    assert_eq!(
        &library[original.declaration_span.unwrap().lo..original.declaration_span.unwrap().hi],
        "Box"
    );
    let source = "bind type Box Item ./lib.owned\nbind value done done ./lib.owned\nlet value: Item = { value: 42 };\ndone();\n";
    write_source(&entry, source);
    CALLS.store(0, Ordering::SeqCst);
    let compiled = compile_source_file_with_options(&entry, configured()).unwrap();
    let machine = run(compiled.program, &catalog);
    assert_eq!(machine.stack().last(), Some(&Value::Int(42)));
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        3,
        "value promotion initializes once"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn enabled_dependency_resource_policy_survives_ownership_disabled_root() {
    let _lock = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CALLS.store(0, Ordering::SeqCst);
    let root = temp_root("owned-dependency");
    let catalog = catalog();
    let library = "let handle = open_handle();\nread_handle(handle);\nread_handle(handle);\nclose_handle(handle);\npub fn done() -> int { 42 }\n";
    write_source(&root.join("lib.owned"), library);
    let entry = root.join("main.graph");
    write_source(&entry, "bind value done done ./lib.owned\ndone();\n");
    // TypedFormat's descriptor disables ownership, while its dependency enables it.
    let entry = root.join("main.fmt-two");
    write_source(&entry, "use \"./lib.owned\" as done;\ncall done()");
    let configured = || options(&catalog).with_source_plugin(&FORMAT_TWO);
    let compiled = compile_source_file_with_options(&entry, configured())
        .expect("enabled dependency borrowing must survive disabled root");
    let machine = run(compiled.program, &catalog);
    assert_eq!(machine.stack().last(), Some(&Value::Int(42)));
    assert_eq!(CALLS.load(Ordering::SeqCst), 4);
    write_source(
        &root.join("lib.owned"),
        "let handle = open_handle(); close_handle(handle); read_handle(handle); pub fn done() -> int { 42 }",
    );
    CALLS.store(0, Ordering::SeqCst);
    let compiled_error = compile_source_file_with_options(&entry, configured())
        .err()
        .unwrap();
    let analyze_error = vm::compiler::analyze_source_file_with_options(&entry, configured())
        .err()
        .unwrap();
    let source = std::fs::read_to_string(&entry).unwrap();
    let hint_error = vm::collect_inferred_local_type_hints_at_path_with_options(
        &entry,
        &source,
        vm::SourceFlavor::Plugin(FrontendId::new(FORMAT_TWO.id)),
        configured(),
    )
    .err()
    .unwrap();
    assert_eq!(compiled_error.to_string(), analyze_error.to_string());
    assert_eq!(compiled_error.to_string(), hint_error.to_string());
    assert!(
        compiled_error.to_string().contains("moved"),
        "{compiled_error}"
    );
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(root).unwrap();
}
