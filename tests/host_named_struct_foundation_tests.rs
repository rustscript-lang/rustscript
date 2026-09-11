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
    CompiledProgram, SourcePathError, SourcePosition, analyze_source_from_string_with_options,
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
    let origin_schema = compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|schema| schema.name == "handles::open")
        .expect("handles::open import schema");
    assert_eq!(origin_schema.return_type, handle.as_type());
    assert!(origin_schema.return_type.contains_resource());
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
    Arc::new(builder.build().expect("handle catalog"))
}

#[test]
fn catalog_import_schema_preserves_named_identity_for_resource_struct() {
    let catalog = handle_catalog();
    let schemas = vm::catalog_import_schemas(&catalog, "handles::borrow");
    assert_eq!(
        schemas[0].params[0].schema,
        HostTypeSchema::named_struct(
            "HandleBox",
            vec![HostStructField::new(
                "file",
                HostTypeSchema::Resource(ResourceTypeKey::new("io.file").expect("key")),
            )],
        )
    );
    assert!(schemas[0].params[0].schema.contains_resource());
    let schemas = vm::catalog_import_schemas(&catalog, "handles::open");
    assert_eq!(
        schemas[0].return_type,
        HostTypeSchema::named_struct(
            "HandleBox",
            vec![HostStructField::new(
                "file",
                HostTypeSchema::Resource(ResourceTypeKey::new("io.file").expect("key")),
            )],
        )
    );
    assert!(schemas[0].return_type.contains_resource());
}

#[test]
fn resource_bearing_named_param_is_detected_at_exact_registration() {
    let catalog = handle_catalog();
    let schema = vm::catalog_import_schemas(&catalog, "handles::borrow")
        .into_iter()
        .next()
        .expect("borrow schema");
    assert!(
        schema.params[0].schema.contains_resource(),
        "named struct with a nested resource must classify as resource-bearing"
    );
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.install_named_struct_schemas(vm::catalog_named_struct_schemas(&catalog));
    assert!(
        matches!(
            registry.named_struct_schemas().get("HandleBox"),
            Some(TypeSchema::Object(fields))
                if fields.get("file").is_some_and(|ty| matches!(ty, TypeSchema::Resource(_)))
        ),
        "installed named-struct body must expose the nested resource"
    );
    registry
        .register_exact_static("handles::borrow", 1, schema, |_, _| {
            Ok(vm::CallOutcome::Return(vm::CallReturn::None))
        })
        .expect("target registry accepts nested resources as named-struct maps");
}

#[test]
fn resource_bearing_named_return_is_classified_without_rejecting_registration() {
    let catalog = handle_catalog();
    let schema = vm::catalog_import_schemas(&catalog, "handles::open")
        .into_iter()
        .next()
        .expect("open schema");
    assert_eq!(
        schema.return_type,
        HostTypeSchema::named_struct(
            "HandleBox",
            vec![HostStructField::new(
                "file",
                HostTypeSchema::Resource(ResourceTypeKey::new("io.file").expect("key")),
            )],
        )
    );
    assert!(schema.return_type.contains_resource());
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.install_named_struct_schemas(vm::catalog_named_struct_schemas(&catalog));
    assert!(
        matches!(
            registry.named_struct_schemas().get("HandleBox"),
            Some(TypeSchema::Object(fields))
                if fields.get("file").is_some_and(|ty| matches!(ty, TypeSchema::Resource(_)))
        ),
        "installed named-struct body must expose the nested resource"
    );
    registry
        .register_exact_static("handles::open", 0, schema, |_, _| {
            Ok(vm::CallOutcome::Return(vm::CallReturn::None))
        })
        .expect("target registry accepts named-struct returns with nested resources");
}
