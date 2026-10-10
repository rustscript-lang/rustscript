#[path = "frontend_plugin_fixtures/schema_matrix.rs"]
mod schema_matrix;

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use vm::{
    CallOutcome, CallReturn, CompileError, CompileSourceFileOptions, FrontendId, FrontendIr,
    FrontendTypedIr, FrontendTypingPolicy, HostApiBuilder, HostApiCatalog, HostFunctionRegistry,
    HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema, HostTypeSchema,
    ParseError, ParserDialect, SharedParserOptions, SourceFlavor, SourceFrontendContext,
    SourceFrontendDescriptor, SourcePlugin, Stmt, TypeSchema, Value, Vm, VmStatus,
    analyze_source_from_string_with_options, collect_inferred_local_type_hints_with_options,
    compile_source_with_flavor_and_options, parse_source_with_dialect,
};

struct SyntheticDialect;

fn point_struct() -> HostStructSchema {
    HostStructSchema::new(
        "Point",
        vec![
            HostStructField::new("x", HostTypeSchema::Int),
            HostStructField::new("y", HostTypeSchema::Int),
        ],
    )
}

fn point_catalog() -> Arc<HostApiCatalog> {
    let point = point_struct();
    let mut builder = HostApiBuilder::new();
    builder.named_struct(point.clone());
    builder.function(HostFunctionSchema::with_return(
        "geo::origin",
        vec![],
        point.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "geo::take_point",
        vec![HostParamSchema::value("point", point.as_type())],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "geo::through",
        vec![
            HostParamSchema::value(
                "callback",
                HostTypeSchema::Callable {
                    params: vec![point.as_type()],
                    result: Box::new(point.as_type()),
                },
            ),
            HostParamSchema::value("point", point.as_type()),
        ],
        point.as_type(),
    ));
    Arc::new(builder.build().expect("Point catalog must build"))
}

static TEST_POINT_CATALOG: LazyLock<Arc<HostApiCatalog>> = LazyLock::new(point_catalog);

fn point_origin_host(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    let point = Value::map(vec![
        (Value::string("x"), Value::Int(3)),
        (Value::string("y"), Value::Int(4)),
    ]);
    Ok(CallOutcome::Return(CallReturn::One(point)))
}

fn point_through_host(_vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    let point = args
        .get(1)
        .cloned()
        .ok_or(vm::VmError::TypeMismatch("Point"))?;
    Ok(CallOutcome::Return(CallReturn::One(point)))
}

fn point_sum_host(_vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    let Some(Value::Map(point)) = args.first() else {
        return Err(vm::VmError::TypeMismatch("Point"));
    };
    let Some(Value::Int(x)) = point.get(&Value::string("x")) else {
        return Err(vm::VmError::TypeMismatch("Point.x"));
    };
    let Some(Value::Int(y)) = point.get(&Value::string("y")) else {
        return Err(vm::VmError::TypeMismatch("Point.y"));
    };
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(x + y))))
}

fn register_point_host(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    function: fn(&mut Vm, &[Value]) -> vm::VmResult<CallOutcome>,
) {
    let schema = vm::catalog_import_schemas_into(registry, catalog, name)
        .expect("named struct schemas should install")
        .into_iter()
        .next()
        .expect("registered catalog schema");
    registry
        .register_exact_static(name, schema.params.len() as u8, schema, function)
        .expect("matching host adapter should register");
}

fn bind_point_hosts(registry: &mut HostFunctionRegistry, catalog: &HostApiCatalog) {
    register_point_host(registry, catalog, "geo::origin", point_origin_host);
    register_point_host(registry, catalog, "geo::through", point_through_host);
    register_point_host(registry, catalog, "geo::take_point", point_sum_host);
}

fn named_hint<'a>(
    hints: &'a [vm::InferredLocalTypeHint],
    name: &str,
) -> &'a vm::InferredLocalTypeHint {
    hints
        .iter()
        .find(|hint| hint.name == name)
        .unwrap_or_else(|| panic!("missing hint for {name}"))
}

fn run_point_source(source: &str, catalog: Arc<HostApiCatalog>) -> Vec<Value> {
    let options = CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
    let compiled =
        compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options)
            .expect("typed host source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction should succeed");
    let mut registry = HostFunctionRegistry::empty();
    bind_point_hosts(&mut registry, catalog.as_ref());
    registry
        .bind_vm_cached(&mut vm)
        .expect("matching catalog adapters should bind");
    loop {
        match vm.run().expect("typed host source should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_) => panic!("point host functions should not wait"),
        }
    }
    vm.stack().to_vec()
}

impl ParserDialect for SyntheticDialect {}

static SYNTHETIC_DIALECT: SyntheticDialect = SyntheticDialect;
static VALIDATION_TEST_LOCK: Mutex<()> = Mutex::new(());
type SchemaObservation = (Option<TypeSchema>, Option<TypeSchema>, Option<TypeSchema>);
type SchemaObservations = Mutex<Vec<SchemaObservation>>;
static VALIDATION_OBSERVATIONS: SchemaObservations = Mutex::new(Vec::new());
static NAMED_VALIDATION_OBSERVATIONS: SchemaObservations = Mutex::new(Vec::new());
static VALIDATION_FAILURES: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

struct SyntheticSourcePlugin {
    id: &'static str,
    extensions: &'static [&'static str],
    typing_policy: FrontendTypingPolicy,
    requires_boolean_result: bool,
    observations: &'static SchemaObservations,
}

impl SyntheticSourcePlugin {
    fn flavor(&self) -> SourceFlavor {
        SourceFlavor::Plugin(FrontendId::new(self.id))
    }

    fn options(&'static self) -> CompileSourceFileOptions {
        let options = CompileSourceFileOptions::new().with_source_plugin(self);
        if self.requires_boolean_result {
            options.with_host_api_catalog(Arc::clone(&TEST_POINT_CATALOG))
        } else {
            options
        }
    }

    fn analyze(&'static self, source: &str) -> Result<vm::SemanticModel, vm::SourcePathError> {
        analyze_source_from_string_with_options(
            format!("synthetic.{}", self.extensions[0]),
            source,
            self.options(),
        )
    }
}

fn local_initializer_schema(typed_ir: &FrontendTypedIr<'_>, name: &str) -> Option<TypeSchema> {
    let (_, slot) = typed_ir
        .ir
        .local_bindings
        .iter()
        .find(|(local_name, _)| local_name == name)?;
    typed_ir.ir.stmts.iter().find_map(|stmt| match stmt {
        Stmt::Let { index, expr, .. } if index == slot => typed_ir.schema_of_expr(expr),
        _ => None,
    })
}

impl SourcePlugin for SyntheticSourcePlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.id),
            extensions: self.extensions,
            typing_policy: self.typing_policy,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        parse_source_with_dialect(
            source,
            &SYNTHETIC_DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                ..SharedParserOptions::default()
            },
        )
    }

    fn validate_typed_ir(&self, typed_ir: &FrontendTypedIr<'_>) -> Result<(), CompileError> {
        if !self.requires_boolean_result {
            return Ok(());
        }
        let Some((_, result_slot)) = typed_ir
            .ir
            .local_bindings
            .iter()
            .find(|(name, _)| name == "result")
        else {
            return Err(CompileError::StrictTypingRequired {
                line: Some(1),
                source_name: None,
                detail: "synthetic predicate result binding is missing".to_string(),
                span: None,
            });
        };
        let local_schema = typed_ir
            .local_schemas
            .get(usize::from(*result_slot))
            .cloned()
            .flatten();
        let expression_schema = typed_ir.ir.stmts.iter().find_map(|stmt| match stmt {
            Stmt::Let { index, expr, .. } if index == result_slot => typed_ir.schema_of_expr(expr),
            _ => None,
        });
        let function_schema = typed_ir
            .ir
            .functions
            .iter()
            .find(|decl| decl.name == "predicate")
            .and_then(|decl| typed_ir.schema_of_function(decl.index));
        let direct_expression_schema = local_initializer_schema(typed_ir, "direct");
        let wrapper_expression_schema = local_initializer_schema(typed_ir, "through");
        let wrapper_function_schema = typed_ir
            .ir
            .functions
            .iter()
            .find(|decl| decl.name == "wrap")
            .and_then(|decl| typed_ir.schema_of_function(decl.index));
        let caller_catalog_matches = typed_ir
            .host_api_catalog
            .map(|catalog| catalog.fingerprint() == TEST_POINT_CATALOG.fingerprint())
            .unwrap_or(false);
        NAMED_VALIDATION_OBSERVATIONS.lock().unwrap().push((
            direct_expression_schema
                .as_ref()
                .map(|schema| typed_ir.ir.source_schema(schema, 0)),
            wrapper_expression_schema
                .as_ref()
                .map(|schema| typed_ir.ir.source_schema(schema, 0)),
            wrapper_function_schema
                .as_ref()
                .map(|schema| typed_ir.ir.source_schema(schema, 0)),
        ));
        self.observations.lock().unwrap().push((
            local_schema.clone(),
            expression_schema.clone(),
            function_schema.clone(),
        ));
        let provenance = typed_ir
            .ir
            .parsed_semantic_index
            .as_ref()
            .unwrap()
            .type_provenance
            .iter()
            .find(|entry| {
                entry.source_name == "LocalPoint"
                    && entry.source_id == 0
                    && entry.declaration_span.is_some()
            })
            .expect("canonical guest type retains its original declaration");
        let declaration_span = provenance.declaration_span.unwrap();
        assert_eq!(
            typed_ir.source_map.unwrap().span_text(declaration_span),
            Some("LocalPoint")
        );
        let expected_point = TypeSchema::Named(provenance.canonical_name.clone(), vec![]);
        assert_eq!(
            typed_ir.ir.source_schema(&expected_point, 0),
            expected_point_schema()
        );
        let declaration = &typed_ir.ir.struct_schemas[&provenance.canonical_name];
        assert_eq!(
            declaration.body_schema,
            TypeSchema::Object(std::collections::HashMap::from([
                ("x".to_string(), TypeSchema::Int),
                ("y".to_string(), TypeSchema::Int)
            ]))
        );
        let expected_wrapper = TypeSchema::Callable {
            params: vec![expected_point.clone()],
            result: Box::new(expected_point.clone()),
        };
        let expected_function = TypeSchema::Callable {
            params: vec![TypeSchema::Int],
            result: Box::new(TypeSchema::Bool),
        };
        if expression_schema == Some(TypeSchema::Bool)
            && function_schema == Some(expected_function)
            && direct_expression_schema == Some(expected_point.clone())
            && wrapper_expression_schema == Some(expected_point)
            && wrapper_function_schema == Some(expected_wrapper)
            && caller_catalog_matches
        {
            return Ok(());
        }
        Err(CompileError::StrictTypingRequired {
            line: Some(1),
            source_name: None,
            detail: format!(
                "synthetic checked schemas must preserve bool and LocalPoint identities: local={local_schema:?}, expression={expression_schema:?}, function={function_schema:?}, direct={direct_expression_schema:?}, wrapper={wrapper_expression_schema:?}, wrapper_function={wrapper_function_schema:?}, caller_catalog_matches={caller_catalog_matches}"
            ),
            span: None,
        })
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(Vec::new())
    }
}

static STRICT_UNKNOWN_PLUGIN: SyntheticSourcePlugin = SyntheticSourcePlugin {
    id: "policy-strict",
    extensions: &["plug-strict"],
    typing_policy: FrontendTypingPolicy::Strict,
    requires_boolean_result: false,
    observations: &VALIDATION_OBSERVATIONS,
};
static DYNAMIC_UNKNOWN_PLUGIN: SyntheticSourcePlugin = SyntheticSourcePlugin {
    id: "policy-dynamic",
    extensions: &["plug-dynamic"],
    typing_policy: FrontendTypingPolicy::Dynamic,
    requires_boolean_result: false,
    observations: &VALIDATION_OBSERVATIONS,
};
static VALIDATION_PLUGIN: SyntheticSourcePlugin = SyntheticSourcePlugin {
    id: "policy-validator",
    extensions: &["plug-check"],
    typing_policy: FrontendTypingPolicy::Strict,
    requires_boolean_result: true,
    observations: &VALIDATION_OBSERVATIONS,
};

fn source_path_error_inner(error: vm::SourcePathError) -> String {
    match error {
        vm::SourcePathError::Source(error) | vm::SourcePathError::SourceWithMap { error, .. } => {
            error.to_string()
        }
        other => other.to_string(),
    }
}

#[test]
fn strict_unknown_schema_is_rejected_across_checked_entries_but_dynamic_source_runs() {
    let source = "fn identity(value: unknown) { 7 }\nlet item = 7;\nitem;\n";
    let strict_options = STRICT_UNKNOWN_PLUGIN.options();
    let strict_flavor = STRICT_UNKNOWN_PLUGIN.flavor();
    let compile_error =
        match compile_source_with_flavor_and_options(source, strict_flavor, strict_options.clone())
        {
            Ok(_) => panic!("strict plugin compilation must reject the unresolved schema"),
            Err(error) => source_path_error_inner(error),
        };
    let analyze_error = match STRICT_UNKNOWN_PLUGIN.analyze(source) {
        Ok(_) => panic!("strict plugin analysis must reject the unresolved schema"),
        Err(error) => source_path_error_inner(error),
    };
    let strict_failure = "strict frontends require concrete compile-time types; 'unknown' annotations are not allowed";
    assert!(compile_error.contains(strict_failure));
    assert!(analyze_error.contains(strict_failure));
    let hints_error =
        collect_inferred_local_type_hints_with_options(source, strict_flavor, strict_options)
            .expect_err("strict plugin hints must reject the unresolved schema");
    assert!(source_path_error_inner(hints_error).contains(strict_failure));

    let dynamic_options = DYNAMIC_UNKNOWN_PLUGIN.options();
    let dynamic_flavor = DYNAMIC_UNKNOWN_PLUGIN.flavor();
    let compiled =
        compile_source_with_flavor_and_options(source, dynamic_flavor, dynamic_options.clone())
            .expect("dynamic policy should preserve its parsed-source behavior");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    assert!(DYNAMIC_UNKNOWN_PLUGIN.analyze(source).is_ok());
    let hints =
        collect_inferred_local_type_hints_with_options(source, dynamic_flavor, dynamic_options)
            .expect("dynamic hints should remain available");
    let item = named_hint(&hints, "item");
    assert_eq!(item.schema, Some(TypeSchema::Int));
    let item_span = item
        .span
        .expect("dynamic source local retains parser provenance");
    assert_eq!(&source[item_span.lo..item_span.hi], "item");
}

fn reset_validation_observations() {
    VALIDATION_OBSERVATIONS.lock().unwrap().clear();
    NAMED_VALIDATION_OBSERVATIONS.lock().unwrap().clear();
}

fn expected_point_schema() -> TypeSchema {
    TypeSchema::Named("LocalPoint".to_string(), vec![])
}

fn expected_wrapper_schema() -> TypeSchema {
    TypeSchema::Callable {
        params: vec![expected_point_schema()],
        result: Box::new(expected_point_schema()),
    }
}

fn expected_predicate_schema() -> TypeSchema {
    TypeSchema::Callable {
        params: vec![TypeSchema::Int],
        result: Box::new(TypeSchema::Bool),
    }
}

fn assert_validated_once(source: &str, entry: &'static str) {
    let observations = VALIDATION_OBSERVATIONS.lock().unwrap().clone();
    assert_eq!(
        observations,
        vec![(
            Some(TypeSchema::Bool),
            Some(TypeSchema::Bool),
            Some(expected_predicate_schema()),
        )],
        "{entry} must pass actual local, expression, and callable schemas to validation"
    );
    assert_eq!(
        NAMED_VALIDATION_OBSERVATIONS.lock().unwrap().clone(),
        vec![(
            Some(expected_point_schema()),
            Some(expected_point_schema()),
            Some(expected_wrapper_schema()),
        )],
        "{entry} must pass named local and wrapper identities to the typed queries"
    );
    reset_validation_observations();
    let invalid = source.replace("predicate(1)", "1 + 2");
    let validation_error = match entry {
        "compile" => source_path_error_inner(
            match compile_source_with_flavor_and_options(
                &invalid,
                VALIDATION_PLUGIN.flavor(),
                VALIDATION_PLUGIN.options(),
            ) {
                Ok(_) => panic!("the finalized validation hook must reject the integer result"),
                Err(err) => err,
            },
        ),
        "analyze" => source_path_error_inner(match VALIDATION_PLUGIN.analyze(&invalid) {
            Ok(_) => panic!("analysis must propagate finalized validation failure"),
            Err(err) => err,
        }),
        "hints" => source_path_error_inner(
            match collect_inferred_local_type_hints_with_options(
                &invalid,
                VALIDATION_PLUGIN.flavor(),
                VALIDATION_PLUGIN.options(),
            ) {
                Ok(_) => panic!("hints must propagate finalized validation failure"),
                Err(err) => err,
            },
        ),
        _ => unreachable!(),
    };
    assert!(
        validation_error
            .contains("synthetic checked schemas must preserve bool and LocalPoint identities")
    );
    VALIDATION_FAILURES
        .lock()
        .unwrap()
        .push((entry, validation_error));
    assert_eq!(
        VALIDATION_OBSERVATIONS.lock().unwrap().clone(),
        vec![(
            None,
            Some(TypeSchema::Int),
            Some(expected_predicate_schema()),
        )],
        "{entry} validation must observe the inferred integer expression and complete function type"
    );
    assert_eq!(
        NAMED_VALIDATION_OBSERVATIONS.lock().unwrap().clone(),
        vec![(
            Some(expected_point_schema()),
            Some(expected_point_schema()),
            Some(expected_wrapper_schema()),
        )],
        "{entry} failure must preserve host and wrapper schemas"
    );
}

struct LogicalSchemaPlugin;
static LOGICAL_SCHEMA_PLUGIN: LogicalSchemaPlugin = LogicalSchemaPlugin;
impl SourcePlugin for LogicalSchemaPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("neutral-logical-schemas"),
            extensions: &["logical"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        parse_source_with_dialect(
            source,
            &SYNTHETIC_DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                import_scan_mode: true,
                ..SharedParserOptions::default()
            },
        )
    }
    fn validate_typed_ir(&self, typed: &FrontendTypedIr<'_>) -> Result<(), CompileError> {
        let optional = TypeSchema::Optional(Box::new(TypeSchema::Int));
        for (name, slot) in &typed.ir.local_bindings {
            let expected = match name.as_str() {
                "optional" => optional.clone(),
                "f" => TypeSchema::Callable {
                    params: vec![],
                    result: Box::new(TypeSchema::Int),
                },
                "g" => TypeSchema::Callable {
                    params: vec![optional.clone()],
                    result: Box::new(optional.clone()),
                },
                _ => continue,
            };
            assert_eq!(
                typed.local_schemas[usize::from(*slot)],
                Some(expected.clone()),
                "checked local {name}"
            );
            if let Some(expr) = typed.ir.stmts.iter().find_map(|statement| match statement {
                Stmt::Let { index, expr, .. } if index == slot => Some(expr),
                _ => None,
            }) {
                assert_eq!(
                    typed.schema_of_expr(&vm::compiler::Expr::Var(*slot)),
                    Some(expected.clone()),
                    "variable query {name}"
                );
                if name != "optional" {
                    assert_eq!(
                        typed.schema_of_expr(expr),
                        Some(expected),
                        "initializer query {name}"
                    );
                }
            }
        }
        if let Some(function) = typed
            .ir
            .functions
            .iter()
            .find(|function| function.name == "maybe")
        {
            assert_eq!(
                typed.schema_of_function(function.index),
                Some(TypeSchema::Callable {
                    params: vec![optional.clone()],
                    result: Box::new(optional)
                })
            );
        }
        Ok(())
    }
}

#[test]
fn inferred_function_reference_schema_agrees_with_runtime_hint_and_semantic_query() {
    let source = "fn answer() { 42 }\nlet f = answer;\nf();\n";
    let options = || CompileSourceFileOptions::new().with_source_plugin(&LOGICAL_SCHEMA_PLUGIN);
    let flavor = SourceFlavor::Plugin(FrontendId::new("neutral-logical-schemas"));
    let expected = TypeSchema::Callable {
        params: vec![],
        result: Box::new(TypeSchema::Int),
    };
    let compiled = compile_source_with_flavor_and_options(source, flavor, options()).unwrap();
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    let model = analyze_source_from_string_with_options("main.logical", source, options()).unwrap();
    assert_eq!(
        model.inferred_schema_at(vm::SourcePosition::new(
            0,
            source.find("let f").unwrap() + 4
        )),
        Some(expected.clone())
    );
    assert_eq!(
        model.inferred_schema_at(vm::SourcePosition::new(
            0,
            source.find("= answer").unwrap() + 2
        )),
        Some(expected.clone())
    );
    let hints = collect_inferred_local_type_hints_with_options(source, flavor, options()).unwrap();
    assert_eq!(named_hint(&hints, "f").schema, Some(expected));
}

#[test]
fn logical_optional_schema_is_complete_in_hints_and_validation_queries() {
    let source =
        "fn maybe(value: int?) -> int? { value }\nlet optional: int? = null;\nlet g = maybe;\n";
    let options = || CompileSourceFileOptions::new().with_source_plugin(&LOGICAL_SCHEMA_PLUGIN);
    let flavor = SourceFlavor::Plugin(FrontendId::new("neutral-logical-schemas"));
    compile_source_with_flavor_and_options(source, flavor, options()).unwrap();
    analyze_source_from_string_with_options("main.logical", source, options()).unwrap();
    let hints = collect_inferred_local_type_hints_with_options(source, flavor, options()).unwrap();
    assert_eq!(
        named_hint(&hints, "optional").schema,
        Some(TypeSchema::Optional(Box::new(TypeSchema::Int)))
    );
    assert_eq!(
        named_hint(&hints, "g").schema,
        Some(TypeSchema::Callable {
            params: vec![TypeSchema::Optional(Box::new(TypeSchema::Int))],
            result: Box::new(TypeSchema::Optional(Box::new(TypeSchema::Int)))
        })
    );
}

#[test]
fn finalized_validation_hook_checks_real_source_for_compile_analyze_and_hints() {
    let _guard = VALIDATION_TEST_LOCK.lock().unwrap();
    VALIDATION_FAILURES.lock().unwrap().clear();
    let source = "struct LocalPoint { x: int, y: int }\nfn wrap(point: LocalPoint) -> LocalPoint { point }\nfn predicate(x: int) -> bool { x > 0 }\nlet direct = wrap({ x: 1, y: 2 });\nlet through = wrap(direct);\nlet callback = wrap;\nlet called = callback(through);\nlet result = predicate(1);\n";

    reset_validation_observations();
    compile_source_with_flavor_and_options(
        source,
        VALIDATION_PLUGIN.flavor(),
        VALIDATION_PLUGIN.options(),
    )
    .expect("valid source should compile");
    assert_validated_once(source, "compile");

    reset_validation_observations();
    VALIDATION_PLUGIN
        .analyze(source)
        .expect("valid source should analyze");
    assert_validated_once(source, "analyze");

    reset_validation_observations();
    collect_inferred_local_type_hints_with_options(
        source,
        VALIDATION_PLUGIN.flavor(),
        VALIDATION_PLUGIN.options(),
    )
    .expect("valid source should produce hints");
    assert_validated_once(source, "hints");

    let failures = VALIDATION_FAILURES.lock().unwrap();
    let failure = |entry| {
        failures
            .iter()
            .find(|(name, _)| *name == entry)
            .map(|(_, error)| error)
            .unwrap_or_else(|| panic!("missing validation error for {entry}"))
    };
    assert_eq!(failure("compile"), failure("analyze"));
    assert_eq!(failure("analyze"), failure("hints"));
}

#[test]
fn logical_hints_retain_nested_shadow_declarations_and_spans() {
    let source = "let value = 1;\nif true { let value = 1; value; }\nvalue;\n";
    let hints = collect_inferred_local_type_hints_with_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .expect("same-typed nested shadow should produce logical hints");
    let outer = hints
        .iter()
        .find(|hint| hint.name == "value" && hint.declared_line == Some(1))
        .expect("outer lexical declaration keeps its hint");
    let inner = hints
        .iter()
        .find(|hint| hint.name == "value" && hint.declared_line == Some(2))
        .expect("nested lexical declaration keeps its hint");
    assert_eq!(outer.schema, Some(TypeSchema::Int));
    assert_eq!(inner.schema, Some(TypeSchema::Int));
    let outer_span = outer.span.expect("outer declaration keeps its source span");
    let inner_span = inner.span.expect("inner declaration keeps its source span");
    assert_ne!(outer_span.lo, inner_span.lo);
    assert_eq!(&source[outer_span.lo..outer_span.hi], "value");
    assert_eq!(&source[inner_span.lo..inner_span.hi], "value");
    assert_ne!(outer.declared_line, inner.declared_line);
}

#[test]
fn native_host_named_schema_flows_through_wrappers_collections_and_callback_calls() {
    let source = r#"
use geo;
fn wrap(point: Point) -> Point { point }
fn relay(pass: fn(Point) -> Point, value: Point) { pass(value) }
let direct = geo::origin();
let alias = direct;
let wrapped = wrap(alias);
let relayed = relay(wrap, wrapped);
let points = [relayed];
let object = { point: points[0] };
let callback = wrap;
let applied = geo::through(callback, object.point);
let contextual = geo::through(|host_parameter| host_parameter, applied);
let score = geo::take_point(contextual);
score;
"#;
    let catalog = point_catalog();
    assert_eq!(
        run_point_source(source, Arc::clone(&catalog)),
        vec![Value::Int(7)],
        "registered host functions should return and consume the real Point map"
    );

    let options = CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
    analyze_source_from_string_with_options("point.rss", source, options.clone())
        .expect("native host source should analyze");
    let hints =
        collect_inferred_local_type_hints_with_options(source, SourceFlavor::RustScript, options)
            .expect("native host source should produce structured hints");
    let point = TypeSchema::Named("Point".to_string(), vec![]);
    for name in [
        "direct",
        "alias",
        "wrapped",
        "relayed",
        "applied",
        "contextual",
        "host_parameter",
    ] {
        assert_eq!(
            named_hint(&hints, name).schema,
            Some(point.clone()),
            "{name}"
        );
    }
    match named_hint(&hints, "points").schema.as_ref() {
        Some(TypeSchema::Array(inner)) => assert_eq!(inner.as_ref(), &point),
        Some(TypeSchema::ArrayTuple(items)) => assert_eq!(items, std::slice::from_ref(&point)),
        other => panic!("array schema should retain Point, got {other:?}"),
    }
    match named_hint(&hints, "object").schema.as_ref() {
        Some(TypeSchema::Object(fields)) => assert_eq!(fields.get("point"), Some(&point)),
        other => panic!("object schema should retain named Point field, got {other:?}"),
    }
    let callback = named_hint(&hints, "callback");
    assert_eq!(
        callback.schema,
        Some(TypeSchema::Callable {
            params: vec![point.clone()],
            result: Box::new(point.clone()),
        })
    );
    assert_eq!(callback.inferred_type, "fn(Point) -> Point");
    assert_eq!(named_hint(&hints, "point").schema, Some(point));

    let wrong_source = "use geo;\ngeo::take_point(7);\n";
    let compile_error = match compile_source_with_flavor_and_options(
        wrong_source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    ) {
        Ok(_) => panic!("wrong host argument must fail compilation"),
        Err(error) => error.to_string(),
    };
    let rejected_model = analyze_source_from_string_with_options(
        "wrong.rss",
        wrong_source,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("semantic API retains its recoverable host-resolution diagnostic contract");
    let diagnostics = rejected_model.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_deref(), Some("E001"));
    assert!(compile_error.contains(&diagnostics[0].message));
    let span = diagnostics[0].span.unwrap();
    assert_eq!(
        rejected_model.sources().span_text(span),
        Some("geo::take_point")
    );
    let hints_error = match collect_inferred_local_type_hints_with_options(
        wrong_source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    ) {
        Ok(_) => panic!("wrong host argument must fail hint preparation"),
        Err(error) => error.to_string(),
    };
    assert_eq!(compile_error, hints_error);
}
