#![cfg(feature = "runtime")]

use std::path::PathBuf;
use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SemanticModel, SourcePosition, analyze_source_file_with_options,
};
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};

fn key(name: &str) -> ResourceTypeKey {
    ResourceTypeKey::new(name).expect("test resource key")
}

fn overload_catalog() -> Arc<HostApiCatalog> {
    let file_key = key("adapter.file");
    let database_key = key("adapter.database");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file_key.clone(), "an adapter file"));
    builder.resource(ResourceTypeSchema::new(
        database_key.clone(),
        "an adapter database",
    ));
    builder.function(
        HostFunctionSchema::with_return(
            "adapter::make_file",
            Vec::new(),
            HostTypeSchema::Resource(file_key.clone()),
        )
        .with_description("create an adapter file"),
    );
    builder.function(
        HostFunctionSchema::with_return(
            "adapter::make_database",
            Vec::new(),
            HostTypeSchema::Resource(database_key.clone()),
        )
        .with_description("create an adapter database"),
    );
    builder.function(
        HostFunctionSchema::with_return(
            "adapter::close",
            vec![HostParamSchema::with_passing(
                "handle",
                HostTypeSchema::Resource(file_key),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Bool,
        )
        .with_description("close the adapter file"),
    );
    builder.function(
        HostFunctionSchema::with_return(
            "adapter::close",
            vec![HostParamSchema::with_passing(
                "connection",
                HostTypeSchema::Resource(database_key),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Null,
        )
        .with_description("close the adapter database"),
    );
    Arc::new(builder.build().expect("overload catalog must be valid"))
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rustscript_semantic_host_overload_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temporary root must be created");
    root
}

fn analyze_with_overloads(source: &str) -> SemanticModel {
    let root = temp_root();
    let path = root.join("main.rss");
    std::fs::write(&path, source).expect("source must be written");
    let options = CompileSourceFileOptions::new().with_host_api_catalog(overload_catalog());
    let result = analyze_source_file_with_options(&path, options);
    let _ = std::fs::remove_dir_all(root);
    result.expect("overload source must analyze")
}

#[test]
fn semantic_host_metadata_selects_documentation_for_exact_overload() {
    let source = "use adapter;\nlet file = adapter::make_file();\nlet db = adapter::make_database();\nadapter::close(file);\nadapter::close(db);\n";
    let model = analyze_with_overloads(source);
    let first = source
        .find("adapter::close(file)")
        .expect("file close call");
    let second = source
        .find("adapter::close(db)")
        .expect("database close call");

    let file_signature = model
        .callable_signature_at(SourcePosition::new(0, first + 2))
        .expect("file close signature");
    assert_eq!(file_signature.description, "close the adapter file");
    assert_eq!(file_signature.return_type, HostTypeSchema::Bool);
    assert_eq!(file_signature.params[0].name, "handle");
    assert_eq!(
        file_signature.params[0].passing,
        HostParamPassing::TakeOwned
    );
    assert_eq!(
        file_signature.params[0].ty,
        HostTypeSchema::Resource(key("adapter.file"))
    );

    let database_signature = model
        .callable_signature_at(SourcePosition::new(0, second + 2))
        .expect("database close signature");
    assert_eq!(database_signature.description, "close the adapter database");
    assert_eq!(database_signature.return_type, HostTypeSchema::Null);
    assert_eq!(database_signature.params[0].name, "connection");
    assert_eq!(
        database_signature.params[0].passing,
        HostParamPassing::TakeOwned
    );
    assert_eq!(
        database_signature.params[0].ty,
        HostTypeSchema::Resource(key("adapter.database"))
    );

    let file_definition = model
        .definition_at(SourcePosition::new(0, first + 2))
        .expect("file close definition");
    let database_definition = model
        .definition_at(SourcePosition::new(0, second + 2))
        .expect("database close definition");
    assert!(file_definition.label.contains("close the adapter file"));
    assert!(
        database_definition
            .label
            .contains("close the adapter database")
    );
    assert_ne!(file_definition.label, database_definition.label);
}
