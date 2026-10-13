//! Field ownership is independent of the source frontend.

use vm::{
    CompileSourceFileOptions, FrontendId, FrontendIr, FrontendTypingPolicy, ParseError,
    ParserDialect, SharedParserOptions, SourceFlavor, SourceFrontendContext,
    SourceFrontendDescriptor, SourcePlugin, Value, Vm, VmStatus,
    analyze_source_from_string_with_options, collect_inferred_local_type_hints_with_options,
    compile_source_with_flavor_and_options, parse_source_with_dialect,
};

struct ProjectionDialect;
impl ParserDialect for ProjectionDialect {
    fn allow_let_mut_binding(&self) -> bool {
        true
    }
}

struct ProjectionPlugin(&'static str, &'static [&'static str]);
impl SourcePlugin for ProjectionPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.0),
            extensions: self.1,
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }

    fn parse_module_imports(
        &self,
        source: &str,
        _path: &std::path::Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        // This fixture grammar has one explicit namespace module directive.
        Ok(if source.lines().any(|line| line == "use dep;") {
            vec![vm::ModuleImport::value(
                format!("./dep.{}", self.1[0]),
                vm::ImportClause::Namespace("dep".to_string()),
                1,
            )]
        } else {
            vec![]
        })
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = parse_source_with_dialect(
            source,
            &ProjectionDialect,
            SharedParserOptions {
                source_id: context.source_id,
                import_scan_mode: true,
                allow_implicit_externs: true,
                ..SharedParserOptions::default()
            },
        )?;
        let mut host_indices = std::collections::HashMap::new();
        if let Some(catalog) = context.host_api_catalog {
            let declarations = ir.functions.clone();
            for declaration in declarations {
                if catalog.function(&declaration.name).is_some() {
                    ir.functions
                        .retain(|function| function.index != declaration.index);
                    ir.implicit_extern_names
                        .retain(|name| name != &declaration.name);
                    let new_index = context.install_host_function(
                        &mut ir,
                        &declaration.name,
                        declaration.arity,
                    )?;
                    host_indices.insert(declaration.index, new_index);
                }
            }
        }
        for stmt in &mut ir.stmts {
            project_stmt(stmt, &host_indices);
        }
        for function in ir.function_impls.values_mut() {
            for stmt in &mut function.body_stmts {
                project_stmt(stmt, &host_indices);
            }
            project_expr(&mut function.body_expr, &host_indices);
        }
        Ok(ir)
    }
}

fn project_stmt(stmt: &mut vm::Stmt, hosts: &std::collections::HashMap<u16, u16>) {
    match stmt {
        vm::Stmt::Let {
            expr: vm::Expr::Var(slot),
            ..
        } => {
            // The synthetic producer spells direct root transfer explicitly.
            let root = *slot;
            if let vm::Stmt::Let { expr, .. } = stmt {
                *expr = vm::Expr::MoveVar(root);
            }
        }
        vm::Stmt::Let { expr, .. }
        | vm::Stmt::Assign { expr, .. }
        | vm::Stmt::Expr { expr, .. } => project_expr(expr, hosts),
        _ => {}
    }
}

fn project_expr(expr: &mut vm::Expr, hosts: &std::collections::HashMap<u16, u16>) {
    use vm::Expr;
    match expr {
        Expr::OptionalGet {
            field_projection,
            container,
            key,
            ..
        } => {
            *field_projection = true;
            project_expr(container, hosts);
            project_expr(key, hosts);
        }
        Expr::OptionUnwrapOr {
            value, fallback, ..
        } => {
            project_expr(value, hosts);
            project_expr(fallback, hosts);
        }
        Expr::ToOwned(inner) | Expr::Borrow(inner) | Expr::BorrowMut(inner) => {
            project_expr(inner, hosts)
        }
        Expr::Call(index, _, args, _, _) => {
            if let Some(new_index) = hosts.get(index) {
                *index = *new_index;
            }
            for arg in args {
                project_expr(arg, hosts);
            }
        }
        Expr::IfElse {
            condition,
            then_expr,
            else_expr,
        } => {
            project_expr(condition, hosts);
            project_expr(then_expr, hosts);
            project_expr(else_expr, hosts);
        }
        Expr::Block { stmts, expr } => {
            for stmt in stmts {
                project_stmt(stmt, hosts);
            }
            project_expr(expr, hosts);
        }
        Expr::ModuleCall(_, _, args, _) => {
            for arg in args {
                project_expr(arg, hosts);
            }
        }
        _ => {}
    }
}

static A: ProjectionPlugin = ProjectionPlugin("opaque-projection-a", &["projection-a"]);
static B: ProjectionPlugin = ProjectionPlugin("opaque-projection-b", &["projection-b"]);

const SOURCE: &str = "struct Entry { name: string, note: string? }\nlet entry: Entry = { name: \"first\" };\nlet name = entry.name;\nlet note = entry?.note;\nnote;\n";

fn assert_run(source: &str, expected: Value, plugin: &'static ProjectionPlugin) {
    let options = || CompileSourceFileOptions::new().with_source_plugin(plugin);
    let flavor = SourceFlavor::Plugin(FrontendId::new(plugin.0));
    let compiled = compile_source_with_flavor_and_options(source, flavor, options())
        .unwrap_or_else(|error| panic!("strict sibling projection must compile: {error}"));
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[expected]);
    let model =
        analyze_source_from_string_with_options(format!("main.{}", plugin.1[0]), source, options())
            .unwrap();
    assert!(model.diagnostics().is_empty());
    let hints = collect_inferred_local_type_hints_with_options(source, flavor, options()).unwrap();
    let note = hints.iter().find(|hint| hint.name == "note").unwrap();
    assert!(note.schema.is_some());
    let span = note.span.unwrap();
    assert_eq!(&source[span.lo..span.hi], "note");
    assert_eq!(
        model.inferred_schema_at(vm::SourcePosition::new(0, span.lo)),
        note.schema
    );
    let parsed = model.ir().parsed_semantic_index.as_ref().unwrap();
    let site = parsed
        .call_sites
        .iter()
        .find(|site| source.get(site.expr_span.lo..site.expr_span.hi) == Some("entry?.note"))
        .unwrap();
    assert_eq!(&source[site.expr_span.lo..site.expr_span.hi], "entry?.note");
    assert!(
        model
            .ir()
            .semantic_index
            .as_ref()
            .unwrap()
            .resolved_calls
            .contains_key(&site.id)
    );
}

#[test]
fn opaque_a_available_optional_sibling_after_normal_projection() {
    assert_run(SOURCE, Value::Null, &A);
}

#[test]
fn opaque_b_available_optional_sibling_after_normal_projection() {
    assert_run(SOURCE, Value::Null, &B);
}

#[test]
fn optional_projection_null_and_present_results() {
    for plugin in [&A, &B] {
        for (field, expected) in [("null", Value::Null), ("\"ready\"", Value::string("ready"))] {
            assert_run(
                &SOURCE.replace(
                    "{ name: \"first\" }",
                    &format!("{{ name: \"first\", note: {field} }}"),
                ),
                expected,
                plugin,
            );
        }
    }
}

#[test]
fn nested_block_optional_projection_preserves_copyable_child() {
    let source = "struct Entry { note: int? }\nlet mut entry: Entry = {};\nlet note: int? = (if true => { entry = { note: 42 }; let first = entry?.note; entry?.note } else => { entry?.note }).copy();\nnote.unwrap_or(0);";
    for plugin in [&A, &B] {
        assert_run(source, Value::Int(42), plugin);
    }
}

#[test]
fn named_generic_projection_keeps_callable_and_semantic_facts() {
    let source = "struct Entry<T> { name: string, note: T? }\nfn preserve<T>(value: T) -> T { value }\nlet entry: Entry<string> = { name: \"first\" };\nlet preserved = preserve::<Entry<string>>(entry);\nlet name = entry.name;\nlet note = entry?.note;\nnote;\n";
    for plugin in [&A, &B] {
        assert_run(source, Value::Null, plugin);
        let compiled = compile_source_with_flavor_and_options(
            source,
            SourceFlavor::Plugin(FrontendId::new(plugin.0)),
            CompileSourceFileOptions::new().with_source_plugin(plugin),
        )
        .unwrap();
        let entry = vm::TypeSchema::Named("Entry".to_string(), vec![vm::TypeSchema::String]);
        let expected = vm::TypeSchema::Callable {
            params: vec![entry.clone()],
            result: Box::new(entry),
        };
        let model = analyze_source_from_string_with_options(
            format!("main.{}", plugin.1[0]),
            source,
            CompileSourceFileOptions::new().with_source_plugin(plugin),
        )
        .unwrap();
        assert!(
            compiled
                .program
                .callable_prototypes
                .iter()
                .filter_map(|prototype| prototype.schema.as_ref())
                .any(|schema| model.ir().source_schema(schema, 0) == expected),
            "{:?}",
            compiled.program.callable_prototypes
        );
    }
}

fn assert_rejected(source: &str, detail: &str) {
    for plugin in [&A, &B] {
        let options = || {
            CompileSourceFileOptions::new()
                .with_source_plugin(plugin)
                .with_host_api_catalog(std::sync::Arc::new(resource_catalog()))
        };
        let flavor = SourceFlavor::Plugin(FrontendId::new(plugin.0));
        let compile = compile_source_with_flavor_and_options(source, flavor, options())
            .err()
            .expect("checked compile must reject");
        let analysis = analyze_source_from_string_with_options(
            format!("main.{}", plugin.1[0]),
            source,
            options(),
        )
        .err()
        .expect("checked analysis must reject");
        let hints = collect_inferred_local_type_hints_with_options(source, flavor, options())
            .expect_err("checked hints must reject");
        for error in [compile, analysis, hints] {
            assert!(
                error.to_string().contains(detail),
                "expected {detail}: {error}"
            );
        }
    }
}

#[test]
fn whole_record_after_noncopy_field_remains_rejected() {
    assert_rejected(
        &SOURCE.replace("\nnote;\n", "\nentry;\n"),
        "partially moved",
    );
}

#[test]
fn repeated_noncopy_optional_field_remains_rejected() {
    assert_rejected(
        &SOURCE.replace("\nnote;\n", "\nentry?.note;\n"),
        "was moved earlier",
    );
}

#[test]
fn optional_projection_cannot_read_a_normally_moved_field() {
    assert_rejected(
        &SOURCE.replace("entry?.note", "entry?.name"),
        "was moved earlier",
    );
}

#[test]
fn already_moved_root_remains_rejected() {
    let source = "use neutral;\nstruct Entry { name: string, note: resource<neutral.projection>? }\nlet entry: Entry = { name: \"first\", note: neutral::make() };\nlet moved = entry;\nentry?.note;";
    assert_rejected(source, "moved");
}

#[test]
fn literal_copyable_field_can_be_projected_repeatedly() {
    let source = "struct Entry { name: string, note: int? }\nlet entry: Entry = { name: \"first\", note: 42 };\nlet name = entry.name;\nlet first = entry?.note;\nlet note = entry?.note;\nnote;\n";
    for plugin in [&A, &B] {
        assert_run(source, Value::Int(42), plugin);
    }
}

#[test]
fn explicit_copy_optional_field_keeps_the_original_available() {
    let source = SOURCE.replace(
        "let note = entry?.note;",
        "let first = (entry?.note).copy();\nlet note = entry?.note;",
    );
    for plugin in [&A, &B] {
        assert_run(&source, Value::Null, plugin);
    }
}

#[test]
fn projection_requires_literal_key_and_direct_unborrowed_local() {
    for access in [
        "entry?.[key]",
        "(&entry)?.note",
        "(&mut entry)?.note",
        "make()?.note",
    ] {
        let source = format!(
            "fn make() -> Entry {{ {{ name: \"first\" }} }}\nlet key: string = \"note\";\n{}",
            SOURCE
                .replace("let entry:", "let mut entry:")
                .replace("entry?.note", access)
        );
        assert_rejected(&source, "requires a direct local and a literal key");
    }
}

#[test]
fn optional_value_receiver_requires_unwrap_before_field_projection() {
    let source = SOURCE
        .replace("entry: Entry = { name: \"first\" }", "entry: Entry? = null")
        .replace("let name = entry.name;", "");
    assert_rejected(&source, "unwrapped before field projection receiver");
}

#[test]
fn missing_required_and_wrong_present_fields_remain_rejected() {
    assert_rejected(
        &SOURCE.replace("{ name: \"first\" }", "{ note: null }"),
        "field 'name' is required",
    );
    assert_rejected(
        &SOURCE.replace("{ name: \"first\" }", "{ name: \"first\", note: 7 }"),
        "field 'note' is declared",
    );
}

#[test]
fn independent_strict_unknown_stays_rejected() {
    assert_rejected(
        &format!("fn invalid(value: unknown) {{ 1 }}\n{SOURCE}"),
        "'unknown' annotations are not allowed",
    );
}

fn resource_catalog() -> vm::HostApiCatalog {
    use vm::{
        HostApiBuilder, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
        ResourceTypeKey, ResourceTypeSchema,
    };
    let key = ResourceTypeKey::new("neutral.projection").unwrap();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key.clone(), "projection resource"));
    builder.function(HostFunctionSchema::with_return(
        "neutral::make",
        vec![],
        HostTypeSchema::Resource(key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "neutral::close",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(key),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    builder.build().unwrap()
}

#[test]
fn optional_resource_field_cannot_be_reprojected_or_copied() {
    let source = "use neutral;\nstruct Entry { name: string, note: resource<neutral.projection>? }\nlet entry: Entry = { name: \"first\", note: neutral::make() };\nlet name = entry.name;\nlet note = entry?.note;\n";
    assert_rejected(&format!("{source}entry?.note;"), "was moved earlier");
    assert_rejected(
        &source.replace("entry?.note", "(entry?.note).copy()"),
        "resource",
    );
    assert_rejected(&format!("{source}entry;"), "partially moved");
}

#[test]
fn resource_record_borrow_and_mutable_borrow_cannot_escape() {
    let source = "use neutral;\nstruct Entry { name: string, note: resource<neutral.projection>? }\nlet mut entry: Entry = { name: \"first\", note: neutral::make() };\nlet note = entry?.note;\n";
    for receiver in ["(&entry)", "(&mut entry)"] {
        assert_rejected(
            &source.replace("entry?.note", &format!("{receiver}?.note")),
            "requires a direct local and a literal key",
        );
    }
}

#[test]
fn native_value_lookup_still_permits_repeated_required_field_reads() {
    let source = "struct Entry { name: string }\nlet entry: Entry = { name: \"first\" };\nentry?.name;\nentry?.name;";
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::new(),
    )
    .unwrap();
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(
        machine.stack(),
        &[Value::string("first"), Value::string("first")]
    );
}

struct EventCall {
    events: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    name: &'static str,
    result: Value,
}
impl vm::HostFunction for EventCall {
    fn call(&mut self, _machine: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        self.events.lock().unwrap().push(self.name);
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(
            self.result.clone(),
        )))
    }
}

#[test]
fn native_temporary_dynamic_key_value_lookup_evaluates_each_once() {
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "neutral::tick",
        vec![],
        vm::HostTypeSchema::Null,
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "neutral::key",
        vec![],
        vm::HostTypeSchema::String,
    ));
    let source = "use neutral;\nlet entry: map<int> = { note: 42 };\n(if true => { let ignored = neutral::tick(); entry } else => { entry })?.[neutral::key()];";
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::new()
            .with_host_api_catalog(std::sync::Arc::new(builder.build().unwrap())),
    )
    .unwrap();
    let events = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let mut machine = Vm::new(compiled.program);
    machine.bind_function(
        "neutral::tick",
        Box::new(EventCall {
            events: events.clone(),
            name: "receiver",
            result: Value::Null,
        }),
    );
    machine.bind_function(
        "neutral::key",
        Box::new(EventCall {
            events: events.clone(),
            name: "key",
            result: Value::string("note"),
        }),
    );
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    assert_eq!(*events.lock().unwrap(), ["receiver", "key"]);
}

struct ProjectionResource(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl vm::resource::HostResource for ProjectionResource {
    fn resource_type_key() -> Option<vm::ResourceTypeKey> {
        Some(vm::ResourceTypeKey::new("neutral.projection").unwrap())
    }
    fn begin_close(
        &mut self,
        _reason: vm::resource::ResourceCloseReason,
    ) -> vm::resource::ResourceResult<vm::resource::CloseProgress> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vm::resource::CloseProgress::Ready)
    }
}
struct MakeResource {
    creates: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    closes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl vm::HostFunction for MakeResource {
    fn call(&mut self, machine: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        self.creates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let token = machine
            .host_context()
            .push_resource(ProjectionResource(self.closes.clone()))
            .map_err(|error| vm::VmError::HostError(error.to_string()))?;
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(
            token.handle().raw() as i64,
        ))))
    }
}
struct CloseResource;
impl vm::HostFunction for CloseResource {
    fn call(&mut self, machine: &mut Vm, args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
        let Value::Int(raw) = args[0] else {
            return Err(vm::VmError::TypeMismatch("resource handle"));
        };
        let handle = vm::resource::ResourceHandle::from_raw(raw as u64)
            .map_err(|error| vm::VmError::HostError(error.to_string()))?;
        machine
            .execution_scope()
            .close_resource::<ProjectionResource>(
                handle,
                vm::resource::ResourceCloseReason::Requested,
            )
            .map_err(|error| vm::VmError::HostError(error.to_string()))?;
        Ok(vm::CallOutcome::Return(vm::CallReturn::none()))
    }
}

#[test]
fn optional_resource_projection_transfers_one_handle_without_record_clone() {
    let source = "use neutral;\nstruct Entry { name: string, note: resource<neutral.projection>? }\nlet entry: Entry = { name: \"first\", note: neutral::make() };\nlet name = entry.name;\nlet note = entry?.note;\nlet handle = note.unwrap_or(neutral::make());\nneutral::close(handle);\n42;";
    for plugin in [&A, &B] {
        let compiled = compile_source_with_flavor_and_options(
            source,
            SourceFlavor::Plugin(FrontendId::new(plugin.0)),
            CompileSourceFileOptions::new()
                .with_source_plugin(plugin)
                .with_host_api_catalog(std::sync::Arc::new(resource_catalog())),
        )
        .unwrap();
        let creates = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let closes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut machine = Vm::new(compiled.program);
        machine.bind_function(
            "neutral::make",
            Box::new(MakeResource {
                creates: creates.clone(),
                closes: closes.clone(),
            }),
        );
        machine.bind_function("neutral::close", Box::new(CloseResource));
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack(), &[Value::Int(42)]);
        assert_eq!(creates.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(closes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[test]
fn integer_literal_projection_keeps_other_indexes_available() {
    for plugin in [&A, &B] {
        let source = "let entry: [string] = [\"first\", \"second\"];\nlet first = entry[0];\nlet note = entry?.[1];\nnote;";
        let compiled = compile_source_with_flavor_and_options(
            source,
            SourceFlavor::Plugin(FrontendId::new(plugin.0)),
            CompileSourceFileOptions::new().with_source_plugin(plugin),
        )
        .unwrap();
        let mut machine = Vm::new(compiled.program);
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack(), &[Value::string("second")]);
        assert_rejected(
            &source.replace("entry?.[1]", "entry?.[0]"),
            "was moved earlier",
        );
    }
}

#[test]
fn imported_projection_preserves_intent_through_linking_and_slot_compaction() {
    for plugin in [&A, &B] {
        let source = "use dep;\nlet note = dep::read();\nnote;";
        let dependency = "struct Entry<T> { name: string, note: T? }\npub fn read() -> string? {\nlet a: int = 1; let b: int = 2; let c: int = 3; let d: int = 4; let e: int = 5; let f: int = 6; let g: int = 7; let h: int = 8;\nlet entry: Entry<string> = { name: \"first\", note: \"ready\" };\nlet name = entry.name;\nlet note = entry?.note;\nnote\n}";
        let options = || {
            CompileSourceFileOptions::new()
                .with_source_plugin(plugin)
                .with_module_override_source(format!("dep.{}", plugin.1[0]), dependency)
        };
        let flavor = SourceFlavor::Plugin(FrontendId::new(plugin.0));
        let compiled = vm::compiler::compile_source_at_path_with_flavor_and_options(
            format!("main.{}", plugin.1[0]),
            source,
            flavor,
            options(),
        )
        .unwrap();
        assert!(
            compiled
                .program
                .callable_prototypes
                .iter()
                .any(|prototype| prototype.schema
                    == Some(vm::TypeSchema::Callable {
                        params: vec![],
                        result: Box::new(vm::TypeSchema::Optional(Box::new(
                            vm::TypeSchema::String
                        )))
                    }))
        );
        let mut machine = Vm::new(compiled.program);
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack(), &[Value::string("ready")]);
        let model = analyze_source_from_string_with_options(
            format!("main.{}", plugin.1[0]),
            source,
            options(),
        )
        .unwrap();
        assert!(model.diagnostics().is_empty());
        let hints =
            collect_inferred_local_type_hints_with_options(source, flavor, options()).unwrap();
        assert!(
            hints
                .iter()
                .any(|hint| hint.name == "entry" && hint.span.unwrap().source_id != 0)
        );
    }
}

#[test]
fn native_borrowed_resource_receiver_still_rejects_escape_in_value_mode() {
    for receiver in ["(&entry)", "(&mut entry)"] {
        let source = format!(
            "use neutral;\nstruct Entry {{ note: resource<neutral.projection>? }}\nlet mut entry: Entry = {{ note: neutral::make() }};\n{receiver}?.note;"
        );
        let error = compile_source_with_flavor_and_options(
            &source,
            SourceFlavor::RustScript,
            CompileSourceFileOptions::new()
                .with_host_api_catalog(std::sync::Arc::new(resource_catalog())),
        )
        .err()
        .expect("borrow escape must reject");
        assert!(error.to_string().contains("borrow"), "{error}");
    }
}
