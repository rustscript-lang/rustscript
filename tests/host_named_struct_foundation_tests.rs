//! Named host-struct schema foundation: catalog mapping, field access,
//! object-literal call compatibility, and display/hover labels.
//!
//! Runtime values remain maps; `HostTypeSchema::Map` dynamic semantics are
//! unchanged. HTTP/SQLite/JIT catalogs are not migrated here.

use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, TypeSchema, compile_source_with_flavor_and_options,
};
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostStructField, HostStructSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};
use vm::{
    CallOutcome, CallReturn, CompiledProgram, HostExtension, HostFunctionRegistry, HostImportParam,
    HostImportSchema, NamedStructSchema, SourcePathError, SourcePosition, Vm,
    analyze_source_from_string_with_options,
};

fn point_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("x", HostTypeSchema::Int),
        HostStructField::new("y", HostTypeSchema::Int),
    ]
}

fn point_struct() -> HostStructSchema {
    HostStructSchema::new("Point", point_fields())
}

fn point_type() -> HostTypeSchema {
    point_struct().as_type()
}

fn point_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.named_struct(point_struct());
    builder.function(HostFunctionSchema::with_return(
        "geo::origin",
        vec![],
        point_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "geo::take_point",
        vec![HostParamSchema::value("p", point_type())],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "geo::open_map",
        vec![],
        HostTypeSchema::Map(Box::new(HostTypeSchema::Int)),
    ));
    Arc::new(builder.build().expect("point catalog"))
}

fn compile(source: &str, catalog: Arc<HostApiCatalog>) -> Result<CompiledProgram, SourcePathError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
}

fn compile_err(source: &str, catalog: Arc<HostApiCatalog>) -> String {
    match compile(source, catalog) {
        Ok(_) => panic!("expected compile error"),
        Err(err) => err.to_string(),
    }
}

#[test]
fn named_struct_return_allows_field_access() {
    let compiled = compile(
        r#"
        use geo;
        let p = geo::origin();
        let s = p.x + p.y;
        "#,
        point_catalog(),
    )
    .expect("field access on named host struct should compile");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .any(|import| import.name == "geo::origin")
    );
}

#[test]
fn object_literal_is_compatible_with_named_struct_param() {
    compile(
        r#"
        use geo;
        geo::take_point({ x: 1, y: 2 });
        "#,
        point_catalog(),
    )
    .expect("object literal should match named struct param");
}

#[test]
fn object_literal_missing_field_is_rejected() {
    let message = compile_err(
        r#"
        use geo;
        geo::take_point({ x: 1 });
        "#,
        point_catalog(),
    );
    assert!(
        message.contains("Point") || message.contains("field") || message.contains("match"),
        "diagnostic should mention the mismatch, got {message}"
    );
}

#[test]
fn dynamic_map_is_not_a_named_struct() {
    let message = compile_err(
        r#"
        use geo;
        let m = geo::open_map();
        geo::take_point(m);
        "#,
        point_catalog(),
    );
    assert!(
        message.contains("Point") || message.contains("map") || message.contains("match"),
        "diagnostic should distinguish map from named struct, got {message}"
    );
}

#[test]
fn hover_label_uses_named_struct_name() {
    let source = "use geo;\nlet p = geo::origin();\n";
    let model = analyze_source_from_string_with_options(
        "named_struct.rss",
        source,
        CompileSourceFileOptions::default().with_host_api_catalog(point_catalog()),
    )
    .expect("analyze named struct program");
    let offset = source.find("let p").expect("binding") + 4;
    let schema = model
        .inferred_schema_at(SourcePosition::new(0, offset))
        .expect("hover on named struct local");
    assert_eq!(schema, TypeSchema::Named("Point".to_string(), vec![]));
}

#[test]
fn nested_resource_field_is_preserved_in_compiler_schema() {
    let file = ResourceTypeKey::new("io.file").expect("key");
    let handle = HostStructSchema::new(
        "HandleBox",
        vec![HostStructField::new(
            "file",
            HostTypeSchema::Resource(file.clone()),
        )],
    );
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file.clone(), "file"));
    builder.named_struct(handle.clone());
    builder.function(HostFunctionSchema::with_return(
        "handles::open",
        vec![],
        handle.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "handles::borrow",
        vec![HostParamSchema::with_passing(
            "h",
            handle.as_type(),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Null,
    ));
    let catalog = Arc::new(builder.build().expect("handle catalog"));
    let compiled = compile(
        r#"
        use handles;
        let h = handles::open();
        h.file;
        "#,
        catalog,
    )
    .expect("nested resource field access should compile");
    let origin = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "handles::open")
        .expect("handles::open import");
    let schema = origin.schema.as_ref().expect("exact schema");
    assert_eq!(
        schema.return_type,
        TypeSchema::Named("HandleBox".to_string(), vec![])
    );
}

fn handle_catalog() -> Arc<HostApiCatalog> {
    let file = ResourceTypeKey::new("io.file").expect("key");
    let handle = HostStructSchema::new(
        "HandleBox",
        vec![HostStructField::new(
            "file",
            HostTypeSchema::Resource(file.clone()),
        )],
    );
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file.clone(), "file"));
    builder.named_struct(handle.clone());
    builder.function(HostFunctionSchema::with_return(
        "handles::open",
        vec![],
        handle.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "handles::borrow",
        vec![HostParamSchema::with_passing(
            "h",
            handle.as_type(),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Null,
    ));
    builder.function(HostFunctionSchema::with_return(
        "handles::take",
        vec![HostParamSchema::with_passing(
            "h",
            handle.as_type(),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    Arc::new(builder.build().expect("handle catalog"))
}

fn empty_vm() -> Vm {
    let compiled = compile("0;", point_catalog()).expect("empty program");
    Vm::try_new(compiled.program).expect("test VM construction must not fail")
}

fn noop_host(_vm: &mut Vm, _args: &[vm::Value]) -> vm::VmResult<CallOutcome> {
    Ok(CallOutcome::Return(CallReturn::None))
}

struct NamedHandleExtension {
    catalog: Arc<HostApiCatalog>,
    name: &'static str,
    arity: u8,
}

impl HostExtension for NamedHandleExtension {
    fn catalog(&self) -> Option<&HostApiCatalog> {
        Some(self.catalog.as_ref())
    }

    fn register(&self, registry: &mut HostFunctionRegistry) -> vm::VmResult<()> {
        for schema in vm::catalog_import_schemas(self.catalog.as_ref(), self.name) {
            registry.register_exact_static(self.name, self.arity, schema, noop_host)?;
        }
        Ok(())
    }
}

fn assert_nested_resource_rejection(err: &vm::VmError, kind: &str) {
    let text = err.to_string();
    assert!(
        !text.contains("contains no resource"),
        "{kind} must not mis-detect Named as resource-free: {text}"
    );
    assert!(
        text.contains("not directly")
            || text.contains("addressable")
            || text.contains("nested")
            || text.contains("aggregate"),
        "{kind} expected nested-resource rejection, got {text}"
    );
}

#[test]
fn catalog_import_schema_preserves_named_identity_for_resource_struct() {
    let catalog = handle_catalog();
    let schemas = vm::catalog_import_schemas(&catalog, "handles::borrow");
    assert_eq!(
        schemas[0].params[0].schema,
        TypeSchema::Named("HandleBox".to_string(), vec![])
    );
    let schemas = vm::catalog_import_schemas(&catalog, "handles::open");
    assert_eq!(
        schemas[0].return_type,
        TypeSchema::Named("HandleBox".to_string(), vec![])
    );
}

#[test]
fn resource_bearing_named_param_is_detected_at_exact_registration() {
    let catalog = handle_catalog();
    let schema = vm::catalog_import_schemas(&catalog, "handles::borrow")
        .into_iter()
        .next()
        .expect("borrow schema");
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.install_named_struct_schemas(vm::catalog_named_struct_schemas(&catalog));
    let err = registry
        .register_exact_static("handles::borrow", 1, schema, |_, _| {
            Ok(vm::CallOutcome::Return(vm::CallReturn::None))
        })
        .expect_err("nested resource in named struct is not handle-addressable");
    let text = err.to_string();
    assert!(
        text.contains("not directly") || text.contains("addressable"),
        "expected nested-resource addressability rejection, got {text}"
    );
    assert!(
        !text.contains("contains no resource"),
        "must not mis-detect Named as resource-free: {text}"
    );
}

#[test]
fn resource_bearing_named_return_is_rejected_as_nested_at_exact_registration() {
    let catalog = handle_catalog();
    let schema = vm::catalog_import_schemas(&catalog, "handles::open")
        .into_iter()
        .next()
        .expect("open schema");
    assert_eq!(
        schema.return_type,
        TypeSchema::Named("HandleBox".to_string(), vec![])
    );
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.install_named_struct_schemas(vm::catalog_named_struct_schemas(&catalog));
    let err = registry
        .register_exact_static("handles::open", 0, schema, |_, _| {
            Ok(vm::CallOutcome::Return(vm::CallReturn::None))
        })
        .expect_err("named struct return with nested resource must be rejected");
    let text = err.to_string();
    assert!(
        text.contains("nested") || text.contains("aggregate"),
        "got {text}"
    );
}

#[test]
fn install_extension_rejects_resource_bearing_named_borrow_without_manual_schema_install() {
    let mut vm = empty_vm();
    let err = vm
        .install_extension(&NamedHandleExtension {
            catalog: handle_catalog(),
            name: "handles::borrow",
            arity: 1,
        })
        .expect_err("nested resource in named Borrow must be rejected");
    assert_nested_resource_rejection(&err, "install_extension Borrow");
}

#[test]
fn install_extension_rejects_resource_bearing_named_take_owned_without_manual_schema_install() {
    let mut vm = empty_vm();
    let err = vm
        .install_extension(&NamedHandleExtension {
            catalog: handle_catalog(),
            name: "handles::take",
            arity: 1,
        })
        .expect_err("nested resource in named TakeOwned must be rejected");
    assert_nested_resource_rejection(&err, "install_extension TakeOwned");
}

#[test]
fn install_extension_rejects_resource_bearing_named_return_without_manual_schema_install() {
    let mut vm = empty_vm();
    let err = vm
        .install_extension(&NamedHandleExtension {
            catalog: handle_catalog(),
            name: "handles::open",
            arity: 0,
        })
        .expect_err("named struct return with nested resource must be rejected");
    assert_nested_resource_rejection(&err, "install_extension return");
}

#[test]
fn catalog_import_schemas_into_installs_named_struct_bodies() {
    let catalog = handle_catalog();
    let mut registry = HostFunctionRegistry::empty();
    let schema = vm::catalog_import_schemas_into(&mut registry, &catalog, "handles::borrow")
        .into_iter()
        .next()
        .expect("borrow schema");
    let err = registry
        .register_exact_static("handles::borrow", 1, schema, noop_host)
        .expect_err("nested resource must be detected after catalog import");
    assert_nested_resource_rejection(&err, "catalog_import_schemas_into Borrow");
}

#[test]
fn missing_named_struct_body_is_a_precise_registration_error() {
    let catalog = handle_catalog();
    let schema = vm::catalog_import_schemas(&catalog, "handles::borrow")
        .into_iter()
        .next()
        .expect("borrow schema");
    let mut registry = HostFunctionRegistry::empty();
    let err = registry
        .register_exact_static("handles::borrow", 1, schema, noop_host)
        .expect_err("missing Named body must not be silent");
    let text = err.to_string();
    assert!(
        !text.contains("contains no resource"),
        "must not mis-detect missing Named body as resource-free: {text}"
    );
    assert!(
        text.contains("HandleBox")
            || text.contains("named struct")
            || text.contains("installed")
            || text.contains("unknown"),
        "expected precise missing-body registration error, got {text}"
    );
}

#[test]
fn failed_named_struct_instantiation_is_a_precise_registration_error() {
    let file = ResourceTypeKey::new("io.file").expect("key");
    let mut fields = std::collections::HashMap::new();
    fields.insert("file".to_string(), TypeSchema::Resource(file.clone()));
    let mut registry = HostFunctionRegistry::empty();
    registry.install_named_struct_schemas(std::collections::HashMap::from([(
        "Box".to_string(),
        NamedStructSchema {
            type_params: vec!["T".to_string()],
            body_schema: TypeSchema::Object(fields),
        },
    )]));
    let schema = HostImportSchema {
        params: vec![HostImportParam {
            name: "h".into(),
            schema: TypeSchema::Named("Box".into(), Vec::new()),
            passing: HostParamPassing::Borrow,
        }],
        return_type: TypeSchema::Null,
        fingerprint: HostApiCatalog::default().fingerprint(),
    };
    let err = registry
        .register_exact_static("handles::box", 1, schema, noop_host)
        .expect_err("failed Named instantiation must not be silent");
    let text = err.to_string();
    assert!(
        !text.contains("contains no resource"),
        "must not mis-detect failed instantiate as resource-free: {text}"
    );
    assert!(
        text.contains("Box")
            || text.contains("instantiat")
            || text.contains("type argument")
            || text.contains("named struct"),
        "expected precise instantiation error, got {text}"
    );
}
