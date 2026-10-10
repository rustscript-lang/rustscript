use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vm::compiler::{
    CompileSourceFileOptions, Expr, FrontendId, FrontendIr, FrontendTypingPolicy, ParseError,
    ParserDialect, SharedParserOptions, SourceFlavor, SourcePosition, Stmt,
    analyze_source_from_string_with_options, collect_inferred_local_type_hints_with_options,
    compile_source_with_flavor_and_options, parse_source_with_dialect,
};
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostStructField, HostStructSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};
use vm::{
    CallOutcome, CallReturn, HostExtension, HostFunctionRegistry, SourceFrontendContext,
    SourceFrontendDescriptor, SourcePlugin, Value, Vm, VmResult,
};

static HOST_CALLS: AtomicUsize = AtomicUsize::new(0);
static CAPTURE: Mutex<Option<BridgeCapture>> = Mutex::new(None);

#[derive(Clone, Debug)]
struct BridgeCapture {
    candidates: Vec<HostFunctionSchema>,
    profile_origin: vm::compiler::StructDeclOrigin,
    address_origin: vm::compiler::StructDeclOrigin,
    returned_type: vm::TypeSchema,
}

struct NativeGrammar;
impl ParserDialect for NativeGrammar {
    fn allow_arrow_closure(&self) -> bool {
        true
    }
}
static NATIVE_GRAMMAR: NativeGrammar = NativeGrammar;

struct CatalogFrontend;
static CATALOG_FRONTEND: CatalogFrontend = CatalogFrontend;

impl SourcePlugin for CatalogFrontend {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("synthetic-catalog-bridge"),
            extensions: &["plug"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = parse_source_with_dialect(
            source,
            &NATIVE_GRAMMAR,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: true,
            },
        )?;
        let returned_type = context.install_host_type(&mut ir, "Profile")?;
        let alias = "load_profile";
        let declaration = ir
            .functions
            .iter()
            .find(|function| function.name == alias)
            .cloned()
            .ok_or_else(|| ParseError::new("synthetic source did not parse load_profile"))?;
        ir.functions
            .retain(|function| function.index != declaration.index);
        ir.implicit_extern_names.retain(|name| name != alias);
        let function_index =
            context.install_host_function(&mut ir, "profile::load", declaration.arity)?;
        for stmt in &mut ir.stmts {
            if let Stmt::Let {
                expr: Expr::Call(index, ..),
                ..
            } = stmt
                && *index == declaration.index
            {
                *index = function_index;
            }
        }
        let metadata = ir
            .host_api_metadata
            .as_ref()
            .ok_or_else(|| ParseError::new("catalog helper did not bind host metadata"))?;
        let candidates = metadata
            .candidates(function_index)
            .ok_or_else(|| ParseError::new("catalog helper did not retain candidates"))?
            .to_vec();
        let capture = BridgeCapture {
            candidates,
            profile_origin: ir
                .struct_schemas
                .get("Profile")
                .ok_or_else(|| ParseError::new("Profile schema was not installed"))?
                .origin,
            address_origin: ir
                .struct_schemas
                .get("Address")
                .ok_or_else(|| ParseError::new("nested Address schema was not installed"))?
                .origin,
            returned_type,
        };
        *CAPTURE.lock().expect("capture lock") = Some(capture);
        Ok(ir)
    }
}

fn profile_catalog() -> Arc<HostApiCatalog> {
    let address = HostStructSchema::new(
        "Address",
        vec![HostStructField::new("city", HostTypeSchema::String)],
    );
    let profile = HostStructSchema::new(
        "Profile",
        vec![
            HostStructField::new("name", HostTypeSchema::String),
            HostStructField::new("address", address.as_type()),
        ],
    );
    let mut builder = HostApiBuilder::new();
    builder.named_struct(address);
    builder.named_struct(profile.clone());
    builder.function(HostFunctionSchema::with_return(
        "profile::load",
        vec![HostParamSchema::value("name", HostTypeSchema::String)],
        profile.as_type(),
    ));
    Arc::new(builder.build().expect("profile catalog"))
}

struct OverloadFrontend;
static OVERLOAD_FRONTEND: OverloadFrontend = OverloadFrontend;

impl SourcePlugin for OverloadFrontend {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("synthetic-catalog-overloads"),
            extensions: &["plug"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = parse_source_with_dialect(
            source,
            &NATIVE_GRAMMAR,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: true,
            },
        )?;
        let implicit_names = ir.implicit_extern_names.clone();
        let placeholders: Vec<_> = ir
            .functions
            .iter()
            .filter(|function| implicit_names.contains(&function.name))
            .cloned()
            .collect();
        let placeholder_indices: std::collections::HashSet<_> =
            placeholders.iter().map(|function| function.index).collect();
        ir.functions
            .retain(|function| !placeholder_indices.contains(&function.index));
        ir.implicit_extern_names.clear();

        let mut remap = HashMap::new();
        for placeholder in placeholders {
            let catalog_name = match placeholder.name.as_str() {
                "select_int" | "select_string" | "select_pair" | "select_callback"
                | "borrow_handle" => "matrix::select",
                "open_handle" => "matrix::open",
                "only_int" => "matrix::only_int",
                _ => {
                    return Err(ParseError::new(format!(
                        "unexpected synthetic host alias '{}'",
                        placeholder.name
                    )));
                }
            };
            let index = context.install_host_function(&mut ir, catalog_name, placeholder.arity)?;
            remap.insert(placeholder.index, index);
        }
        for statement in &mut ir.stmts {
            let expression = match statement {
                Stmt::Let { expr, .. } | Stmt::Expr { expr, .. } => expr,
                _ => continue,
            };
            if let Expr::Call(index, ..) = expression
                && let Some(bound_index) = remap.get(index)
            {
                *index = *bound_index;
            }
        }

        Ok(ir)
    }
}

fn overload_catalog() -> Arc<HostApiCatalog> {
    let resource = ResourceTypeKey::new("synthetic.handle").expect("resource key");
    let callback = HostTypeSchema::Callable {
        params: vec![HostTypeSchema::String],
        result: Box::new(HostTypeSchema::Bool),
    };
    let resource_type = HostTypeSchema::Resource(resource.clone());
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        resource.clone(),
        "synthetic handle",
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::select",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::select",
        vec![HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::String,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::select",
        vec![HostParamSchema::with_passing(
            "value",
            resource_type.clone(),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::select",
        vec![HostParamSchema::with_passing(
            "value",
            resource_type.clone(),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::select",
        vec![HostParamSchema::value("callback", callback)],
        HostTypeSchema::Bool,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::select",
        vec![
            HostParamSchema::value("left", HostTypeSchema::Int),
            HostParamSchema::value("right", HostTypeSchema::String),
        ],
        HostTypeSchema::String,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::open",
        Vec::new(),
        resource_type,
    ));
    builder.function(HostFunctionSchema::with_return(
        "matrix::only_int",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("overload catalog"))
}

struct ProfileExtension {
    catalog: Arc<HostApiCatalog>,
}

impl HostExtension for ProfileExtension {
    fn catalog(&self) -> Option<&HostApiCatalog> {
        Some(self.catalog.as_ref())
    }

    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        for schema in vm::catalog_import_schemas(&self.catalog, "profile::load") {
            registry.register_exact_static("profile::load", 1, schema, profile_load)?;
        }
        Ok(())
    }
}

fn profile_load(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
    HOST_CALLS.fetch_add(1, Ordering::SeqCst);
    let address = Value::map(vec![(Value::string("city"), Value::string("Paris"))]);
    let profile = Value::map(vec![
        (Value::string("name"), Value::string("Ada")),
        (Value::string("address"), address),
    ]);
    Ok(CallOutcome::Return(CallReturn::one(profile)))
}

#[test]
fn synthetic_plugin_binds_alias_to_catalog_identity_and_runs_named_result() {
    HOST_CALLS.store(0, Ordering::SeqCst);
    *CAPTURE.lock().expect("capture lock") = None;
    let catalog = profile_catalog();
    let source = "let profile = load_profile(\"Ada\");\nlet city = profile.address.city;\ncity;\n";
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-catalog-bridge"));
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&CATALOG_FRONTEND)
            .with_host_api_catalog(Arc::clone(&catalog))
    };

    let compiled = compile_source_with_flavor_and_options(source, flavor, options())
        .expect("catalog-backed plugin source should compile");
    let capture = CAPTURE
        .lock()
        .expect("capture lock")
        .clone()
        .expect("frontend should capture installed metadata");
    assert_eq!(
        capture.candidates,
        catalog
            .functions_named("profile::load")
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        capture.profile_origin,
        vm::compiler::StructDeclOrigin::Catalog
    );
    assert_eq!(
        capture.address_origin,
        vm::compiler::StructDeclOrigin::Catalog
    );
    assert_eq!(
        capture.returned_type,
        vm::TypeSchema::Named("Profile".to_string(), Vec::new())
    );
    assert_eq!(compiled.program.imports.len(), 1);
    assert_eq!(compiled.program.imports[0].name, "profile::load");
    assert_eq!(compiled.program.imports[0].arity, 1);
    let expected_host_schema = vm::catalog_import_schemas(&catalog, "profile::load");
    assert_eq!(
        compiled.program.host_import_schemas()[0].as_ref(),
        expected_host_schema.first()
    );
    let local_schemas = &compiled
        .program
        .type_map
        .as_ref()
        .expect("strict compile retains inferred schemas")
        .local_schemas;
    assert!(local_schemas.iter().any(|schema| {
        schema == &Some(vm::TypeSchema::Named("Profile".to_string(), Vec::new()))
    }));
    assert!(
        local_schemas
            .iter()
            .any(|schema| schema == &Some(vm::TypeSchema::String))
    );
    let hints = collect_inferred_local_type_hints_with_options(source, flavor, options())
        .expect("plugin type hints use the caller catalog");
    assert_eq!(
        hints
            .iter()
            .find(|hint| hint.name == "profile")
            .unwrap()
            .inferred_type,
        "Profile"
    );
    assert_eq!(
        hints
            .iter()
            .find(|hint| hint.name == "city")
            .unwrap()
            .inferred_type,
        "string"
    );

    let mut vm = Vm::try_new(compiled.program).expect("compiled program creates VM");
    let extension = ProfileExtension { catalog };
    let mut registry = HostFunctionRegistry::empty();
    vm::register_host_extension(&mut registry, &extension).expect("register exact catalog host");
    registry
        .bind_vm_cached(&mut vm)
        .expect("bind exact compiled import");
    assert_eq!(
        vm.run().expect("host call and named field access run"),
        vm::VmStatus::Halted
    );
    assert_eq!(HOST_CALLS.load(Ordering::SeqCst), 1);
    assert!(vm.stack().contains(&Value::string("Paris")));
}

struct NameOnlyFrontend;
static NAME_ONLY_FRONTEND: NameOnlyFrontend = NameOnlyFrontend;

impl SourcePlugin for NameOnlyFrontend {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("synthetic-name-only"),
            extensions: &["plug"],
            typing_policy: FrontendTypingPolicy::Dynamic,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = parse_source_with_dialect(
            source,
            &NATIVE_GRAMMAR,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: true,
            },
        )?;
        let declaration = ir
            .functions
            .iter_mut()
            .find(|function| function.name == "load_profile")
            .ok_or_else(|| ParseError::new("synthetic source did not parse load_profile"))?;
        declaration.name = "profile::load".to_string();
        Ok(ir)
    }
}

#[test]
fn catalog_installation_preserves_existing_source_function_identities() {
    for source in [
        "print(41);",
        "collide(41);",
        "pub fn collide(value: int) -> int { value + 1 } collide(41);",
    ] {
        let mut ir = parse_source_with_dialect(
            source,
            &NATIVE_GRAMMAR,
            SharedParserOptions {
                allow_implicit_externs: true,
                ..SharedParserOptions::default()
            },
        )
        .expect("collision source parses");
        let before = ir.functions[0].clone();
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            &before.name,
            vec![HostParamSchema::value("value", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        ));
        let catalog = builder.build().expect("collision catalog");
        let context = SourceFrontendContext {
            source_id: 0,
            original_path: None,
            phase: vm::compiler::ParsePhase::Standalone,
            host_api_catalog: Some(&catalog),
            imports: &[],
        };
        let installed = context
            .install_host_function(&mut ir, &before.name, 1)
            .expect("catalog installation allocates a safe slot");
        assert_ne!(
            installed, before.index,
            "existing source declaration must keep its identity: {source}"
        );
        let preserved = ir
            .functions
            .iter()
            .find(|f| f.index == before.index)
            .unwrap();
        assert_eq!(format!("{preserved:?}"), format!("{before:?}"));
        assert_eq!(
            context
                .install_host_function(&mut ir, &before.name, 1)
                .unwrap(),
            installed,
            "already exact-bound host identity is reusable"
        );
    }
}

#[test]
fn rejects_name_only_import_for_catalog_owned_function() {
    let catalog = profile_catalog();
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-name-only"));
    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&NAME_ONLY_FRONTEND)
        .with_host_api_catalog(catalog);
    let error =
        match compile_source_with_flavor_and_options("load_profile(\"Ada\");\n", flavor, options) {
            Ok(_) => panic!("catalog-owned imports require exact candidate metadata"),
            Err(error) => error.to_string(),
        };
    assert!(
        error.contains("catalog-bound host function") || error.contains("candidate metadata"),
        "expected fail-closed catalog binding diagnostic, got {error}"
    );
}

#[test]
fn binds_complete_arity_families_and_resolves_typed_resource_and_callable_calls() {
    let catalog = overload_catalog();
    let source = concat!(
        "let int_result = select_int(7);\n",
        "let string_result = select_string(\"hello\");\n",
        "let callback_result = select_callback((value) => value == \"hello\");\n",
    );
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-catalog-overloads"));
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&OVERLOAD_FRONTEND)
            .with_host_api_catalog(Arc::clone(&catalog))
    };
    let compiled = compile_source_with_flavor_and_options(source, flavor, options())
        .expect("all compatible overload calls compile");
    let mut select_import_arities: Vec<_> = compiled
        .program
        .imports
        .iter()
        .filter(|import| import.name == "matrix::select")
        .map(|import| import.arity)
        .collect();
    select_import_arities.sort_unstable();
    assert_eq!(select_import_arities, vec![1, 1, 1]);

    let model = analyze_source_from_string_with_options("overload.plug", source, options())
        .expect("analysis resolves the same catalog candidates");
    let signature_at = |alias: &str| {
        let offset = source.find(alias).expect("alias source offset") + 2;
        model
            .callable_signature_at(SourcePosition::new(0, offset))
            .expect("callable signature at alias")
    };
    let int_signature = signature_at("select_int");
    assert_eq!(int_signature.params[0].ty, HostTypeSchema::Int);
    assert_eq!(int_signature.return_type, HostTypeSchema::Int);
    assert_eq!(int_signature.params[0].passing, HostParamPassing::Value);
    let string_signature = signature_at("select_string");
    assert_eq!(string_signature.params[0].ty, HostTypeSchema::String);
    assert_eq!(string_signature.return_type, HostTypeSchema::String);
    assert_eq!(string_signature.params[0].passing, HostParamPassing::Value);
    let callback_signature = signature_at("select_callback");
    assert!(matches!(
        callback_signature.params[0].ty,
        HostTypeSchema::Callable { .. }
    ));
    assert_eq!(callback_signature.return_type, HostTypeSchema::Bool);
}

#[test]
fn separates_arity_indexes_while_resolving_calls_from_the_complete_same_arity_family() {
    let catalog = overload_catalog();
    let source = concat!(
        "let int_result = select_int(7);\n",
        "let string_result = select_string(\"hello\");\n",
        "let pair_result = select_pair(4, \"right\");\n",
        "let handle = open_handle();\n",
        "borrow_handle(&handle);\n",
    );
    let context = SourceFrontendContext {
        source_id: 0,
        original_path: None,
        phase: vm::compiler::ParsePhase::Standalone,
        host_api_catalog: Some(catalog.as_ref()),
        imports: &[],
    };
    let ir = OVERLOAD_FRONTEND
        .parse_source(source, &context)
        .expect("synthetic RSS source parses and binds catalog aliases");
    let call_indices: Vec<_> = ir
        .stmts
        .iter()
        .filter_map(|statement| match statement {
            Stmt::Let {
                expr: Expr::Call(index, ..),
                ..
            }
            | Stmt::Expr {
                expr: Expr::Call(index, ..),
                ..
            } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(call_indices.len(), 5);
    assert_eq!(call_indices[0], call_indices[1]);
    assert_ne!(call_indices[0], call_indices[2]);
    assert_ne!(call_indices[0], call_indices[3]);
    assert_eq!(call_indices[0], call_indices[4]);
    let metadata = ir
        .host_api_metadata
        .as_ref()
        .expect("helper binds candidates to caller catalog");
    assert_eq!(metadata.fingerprint(), catalog.fingerprint());
    let expected_one: Vec<_> = catalog
        .functions_named("matrix::select")
        .into_iter()
        .filter(|schema| schema.params.len() == 1)
        .cloned()
        .collect();
    let expected_two: Vec<_> = catalog
        .functions_named("matrix::select")
        .into_iter()
        .filter(|schema| schema.params.len() == 2)
        .cloned()
        .collect();
    assert_eq!(
        metadata.candidates(call_indices[0]),
        Some(expected_one.as_slice())
    );
    assert_eq!(
        metadata.candidates(call_indices[2]),
        Some(expected_two.as_slice())
    );
    assert!(
        expected_one
            .iter()
            .any(|candidate| { candidate.params[0].passing == HostParamPassing::Borrow })
    );
    assert!(
        expected_one
            .iter()
            .any(|candidate| { candidate.params[0].passing == HostParamPassing::TakeOwned })
    );
    assert!(
        expected_one
            .iter()
            .any(|candidate| { matches!(candidate.params[0].ty, HostTypeSchema::Callable { .. }) })
    );

    let resolver = vm::compiler::HostCallResolver::new(catalog.as_ref());
    let selected_int = resolver
        .resolve("matrix::select", &[vm::TypeSchema::Int])
        .expect("integer source call resolves");
    assert_eq!(selected_int.params[0].schema, vm::TypeSchema::Int);
    assert_eq!(selected_int.return_type, vm::TypeSchema::Int);
    assert_eq!(selected_int.passing, vec![HostParamPassing::Value]);
    let selected_pair = resolver
        .resolve(
            "matrix::select",
            &[vm::TypeSchema::Int, vm::TypeSchema::String],
        )
        .expect("two-argument source call resolves independently");
    assert_eq!(selected_pair.params.len(), 2);
    assert_eq!(selected_pair.params[0].schema, vm::TypeSchema::Int);
    assert_eq!(selected_pair.params[1].schema, vm::TypeSchema::String);
    assert_eq!(selected_pair.return_type, vm::TypeSchema::String);
    assert_eq!(selected_pair.passing, vec![HostParamPassing::Value; 2]);
    let resource_ambiguity = resolver
        .resolve(
            "matrix::select",
            &[vm::TypeSchema::Resource(
                ResourceTypeKey::new("synthetic.handle").unwrap(),
            )],
        )
        .expect_err("pass-only resource candidates remain ambiguous without explicit intent");
    assert!(
        resource_ambiguity
            .to_string()
            .contains("ambiguous host function")
    );
    assert!(resource_ambiguity.to_string().contains("equally-viable"));
}

#[test]
fn rejects_unsupported_arity_and_incompatible_argument_during_source_compilation() {
    let catalog = overload_catalog();
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-catalog-overloads"));
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&OVERLOAD_FRONTEND)
            .with_host_api_catalog(Arc::clone(&catalog))
    };
    for (source, expected_fragment) in [
        (
            "let result = select_int(1, 2, 3);\n",
            "no overload with 3 argument",
        ),
        (
            "let result = only_int(\"not an int\");\n",
            "no host function",
        ),
    ] {
        let error = match compile_source_with_flavor_and_options(source, flavor, options()) {
            Ok(_) => panic!("incompatible source must fail compilation: {source}"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(expected_fragment),
            "expected {expected_fragment:?} diagnostic, got {error}"
        );
    }
}

fn parse_native_source(
    source: &str,
    context: &SourceFrontendContext<'_>,
) -> Result<FrontendIr, ParseError> {
    parse_source_with_dialect(
        source,
        &NATIVE_GRAMMAR,
        SharedParserOptions {
            source_id: context.source_id,
            allow_implicit_externs: true,
            allow_implicit_semicolons: false,
            enforce_mutable_bindings: true,
            import_scan_mode: true,
        },
    )
}

struct TypeOnlyFrontend;
static TYPE_ONLY_FRONTEND: TypeOnlyFrontend = TypeOnlyFrontend;

impl SourcePlugin for TypeOnlyFrontend {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("synthetic-catalog-type-only"),
            extensions: &["plug"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = parse_native_source(source, context)?;
        context.install_host_type(&mut ir, "Profile")?;
        Ok(ir)
    }
}

struct GuestCollisionFrontend;
static GUEST_COLLISION_FRONTEND: GuestCollisionFrontend = GuestCollisionFrontend;

impl SourcePlugin for GuestCollisionFrontend {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("synthetic-guest-catalog-collision"),
            extensions: &["plug"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let mut ir = parse_native_source(source, context)?;
        context.install_host_type(&mut ir, "Profile")?;
        Ok(ir)
    }
}

struct StoredFrontend;
static STORED_FRONTEND: StoredFrontend = StoredFrontend;
static STORED_IR: Mutex<Option<(FrontendIr, u16)>> = Mutex::new(None);

impl SourcePlugin for StoredFrontend {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("synthetic-catalog-reuse"),
            extensions: &["plug"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        if source.contains("load_profile") {
            let ir = CATALOG_FRONTEND.parse_source(source, context)?;
            let index = ir
                .host_api_metadata
                .as_ref()
                .and_then(|metadata| metadata.function_indices().next())
                .ok_or_else(|| ParseError::new("stored source has no catalog function index"))?;
            *STORED_IR.lock().expect("stored IR lock") = Some((ir.clone(), index));
            return Ok(ir);
        }

        let (mut ir, original_index) = STORED_IR
            .lock()
            .expect("stored IR lock")
            .clone()
            .ok_or_else(|| ParseError::new("source reuse requested before catalog source"))?;
        context.install_host_type(&mut ir, "Profile")?;
        let index = context.install_host_function(&mut ir, "profile::load", 1)?;
        if index != original_index {
            return Err(ParseError::new(
                "compatible catalog reuse changed the flat index",
            ));
        }
        Ok(ir)
    }
}

fn foreign_profile_catalog() -> Arc<HostApiCatalog> {
    let profile = HostStructSchema::new(
        "Profile",
        vec![HostStructField::new("name", HostTypeSchema::Int)],
    );
    let mut builder = HostApiBuilder::new();
    builder.named_struct(profile.clone());
    builder.function(HostFunctionSchema::with_return(
        "profile::load",
        vec![HostParamSchema::value("name", HostTypeSchema::String)],
        profile.as_type(),
    ));
    Arc::new(builder.build().expect("foreign profile catalog"))
}

#[test]
fn type_only_catalog_installation_adds_no_executable_host_imports() {
    let catalog = profile_catalog();
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-catalog-type-only"));
    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&TYPE_ONLY_FRONTEND)
        .with_host_api_catalog(catalog);
    let compiled = compile_source_with_flavor_and_options("7;\n", flavor, options)
        .expect("type-only source compiles");
    assert!(compiled.program.imports.is_empty());
    assert!(compiled.program.named_struct_decls().is_empty());
}

#[test]
fn guest_named_type_collision_fails_in_source_compilation() {
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-guest-catalog-collision"));
    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&GUEST_COLLISION_FRONTEND)
        .with_host_api_catalog(profile_catalog());
    let error = match compile_source_with_flavor_and_options(
        "struct Profile { label: string }\n7;\n",
        flavor,
        options,
    ) {
        Ok(_) => panic!("catalog type must not replace the guest declaration"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("guest"),
        "unexpected collision error: {error}"
    );
    assert!(
        error.contains("Profile"),
        "unexpected collision error: {error}"
    );
}

#[test]
fn same_catalog_ir_reuse_succeeds_and_foreign_catalog_reuse_fails() {
    *STORED_IR.lock().expect("stored IR lock") = None;
    let catalog = profile_catalog();
    let flavor = SourceFlavor::Plugin(FrontendId::new("synthetic-catalog-reuse"));
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&STORED_FRONTEND)
            .with_host_api_catalog(Arc::clone(&catalog))
    };
    compile_source_with_flavor_and_options(
        "let profile = load_profile(\"Ada\");\nlet city = profile.address.city;\ncity;\n",
        flavor,
        options(),
    )
    .expect("initial source binds catalog metadata");
    let reused = compile_source_with_flavor_and_options("7;\n", flavor, options())
        .expect("same-catalog helper state is reusable");
    assert_eq!(reused.program.imports[0].name, "profile::load");

    *STORED_IR.lock().expect("stored IR lock") = None;
    compile_source_with_flavor_and_options(
        "let profile = load_profile(\"Ada\");\nlet city = profile.address.city;\ncity;\n",
        flavor,
        options(),
    )
    .expect("foreign-catalog fixture is prepared with its original catalog");
    let foreign_options = CompileSourceFileOptions::new()
        .with_source_plugin(&STORED_FRONTEND)
        .with_host_api_catalog(foreign_profile_catalog());
    let error = match compile_source_with_flavor_and_options("7;\n", flavor, foreign_options) {
        Ok(_) => panic!("IR metadata from another catalog must be rejected"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("different") || error.contains("identity"),
        "unexpected foreign-catalog diagnostic: {error}"
    );
}
