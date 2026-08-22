use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::{
    HostApiBuilder, HostFunctionSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
    ResourceTypeKey, ResourceTypeSchema, compile_source_with_flavor_and_options, decode_program,
    disassemble_program, encode_program,
};

fn catalog() -> Arc<vm::HostApiCatalog> {
    let file = ResourceTypeKey::new("io.file").expect("valid io.file key");
    let connection =
        ResourceTypeKey::new("sqlite.connection").expect("valid sqlite.connection key");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file.clone(), "file"));
    builder.resource(ResourceTypeSchema::new(connection.clone(), "connection"));
    builder.function(HostFunctionSchema::with_return(
        "acme::open_file",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(file.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::open_db",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(connection.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::forward",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(file),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::forward",
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(connection),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::String,
    ));
    Arc::new(builder.build().expect("catalog must build"))
}

fn compile_catalog_program() -> vm::CompiledProgram {
    let catalog = catalog();
    compile_source_with_flavor_and_options(
        r#"
use acme;
let file = acme::open_file("file");
let db = acme::open_db("db");
acme::forward(file);
acme::forward(db);
"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("catalog source should compile")
}

#[test]
fn compiler_splits_same_flat_host_index_into_exact_schema_imports() {
    let fingerprint = catalog().fingerprint();
    let compiled = compile_catalog_program();

    let forward = compiled
        .program
        .imports
        .iter()
        .enumerate()
        .filter(|(_, import)| import.name == "acme::forward")
        .collect::<Vec<_>>();
    assert_eq!(forward.len(), 2, "each exact overload needs its own import");

    let mut params = forward
        .iter()
        .map(|(_, import)| {
            let schema = import.schema.as_ref().expect("resolved import schema");
            assert_eq!(schema.fingerprint, fingerprint);
            assert_eq!(schema.params.len(), 1);
            assert_eq!(schema.params[0].passing, HostParamPassing::TakeOwned);
            schema.params[0].schema.clone()
        })
        .collect::<Vec<_>>();
    params.sort_by_key(|schema| format!("{schema:?}"));
    assert_eq!(
        params,
        vec![
            TypeSchema::Resource(ResourceTypeKey::new("io.file").unwrap()),
            TypeSchema::Resource(ResourceTypeKey::new("sqlite.connection").unwrap()),
        ]
    );

    let disassembly = disassemble_program(&compiled.program);
    for (index, _) in forward {
        assert!(
            disassembly.contains(&format!("call {index} 1")),
            "exact import {index} is never referenced:\n{disassembly}"
        );
    }
}

#[test]
fn vmbc_roundtrip_preserves_resolved_host_import_schema() {
    let compiled = compile_catalog_program();
    let bytes = encode_program(&compiled.program).expect("resolved imports should encode");
    let decoded = decode_program(&bytes).expect("resolved imports should decode");

    assert_eq!(decoded.imports, compiled.program.imports);
    assert!(
        decoded
            .imports
            .iter()
            .filter(|import| import.name == "acme::forward")
            .all(|import| import.schema.is_some())
    );
}

#[test]
fn type_schema_hash_is_independent_of_object_insertion_order() {
    let lhs = TypeSchema::Object(HashMap::from([
        ("alpha".to_string(), TypeSchema::Int),
        ("beta".to_string(), TypeSchema::String),
    ]));
    let rhs = TypeSchema::Object(HashMap::from([
        ("beta".to_string(), TypeSchema::String),
        ("alpha".to_string(), TypeSchema::Int),
    ]));
    let digest = |schema: &TypeSchema| {
        let mut hasher = DefaultHasher::new();
        schema.hash(&mut hasher);
        hasher.finish()
    };

    assert_eq!(lhs, rhs);
    assert_eq!(digest(&lhs), digest(&rhs));
}

fn callable_catalog() -> Arc<vm::HostApiCatalog> {
    let map = || HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown));
    let callback = HostTypeSchema::Callable {
        params: vec![map()],
        result: Box::new(map()),
    };
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "acme::consume",
        vec![HostParamSchema::value("callback", callback)],
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("callable catalog must build"))
}

fn compile_with_catalog(
    source: &str,
    catalog: Arc<vm::HostApiCatalog>,
) -> Result<vm::CompiledProgram, vm::SourcePathError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
}

fn compile_callable_catalog(source: &str) -> Result<vm::CompiledProgram, vm::SourcePathError> {
    compile_with_catalog(source, callable_catalog())
}

#[test]
fn catalog_callable_schema_rejects_wrong_inline_callback_return() {
    let error = match compile_callable_catalog(r#"use acme; acme::consume(|item| 1);"#) {
        Ok(_) => panic!("catalog callable result schema must reject an int-returning closure"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("callable body result"),
        "diagnostic should identify the callback return mismatch: {error}"
    );
}

#[test]
fn catalog_callable_schema_validates_inline_and_named_callbacks() {
    if let Err(error) =
        compile_callable_catalog(r#"use acme; acme::consume(|item| { action: "continue" });"#)
    {
        panic!("a map-returning inline callback should compile: {error}");
    }

    if let Err(error) = compile_callable_catalog(
        r#"
        use acme;
        fn callback(item: map) -> map { { action: "continue" } }
        acme::consume(callback);
        "#,
    ) {
        panic!("a map-returning named callback should compile: {error}");
    }

    let error = match compile_callable_catalog(
        r#"
        use acme;
        fn callback(item: map) -> int { 1 }
        acme::consume(callback);
        "#,
    ) {
        Ok(_) => panic!("a named int-returning callback must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("found fn(map") && error.to_string().contains("-> int"),
        "diagnostic should identify the incompatible named callback: {error}"
    );

    let error = match compile_callable_catalog(
        r#"
        use acme;
        fn callback(item: int) -> map { { action: "continue" } }
        acme::consume(callback);
        "#,
    ) {
        Ok(_) => panic!("a named int-parameter callback must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("found fn(int") && error.to_string().contains("-> map"),
        "diagnostic should identify the incompatible callback parameter: {error}"
    );
}

fn overloaded_callable_catalog() -> Arc<vm::HostApiCatalog> {
    let map = || HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown));
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "acme::choose",
        vec![HostParamSchema::value(
            "callback",
            HostTypeSchema::Callable {
                params: vec![map()],
                result: Box::new(map()),
            },
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::choose",
        vec![HostParamSchema::value(
            "callback",
            HostTypeSchema::Callable {
                params: vec![map()],
                result: Box::new(HostTypeSchema::Int),
            },
        )],
        HostTypeSchema::Int,
    ));
    Arc::new(
        builder
            .build()
            .expect("overloaded callable catalog must build"),
    )
}

#[test]
fn catalog_callable_schema_drives_overload_selection() {
    if let Err(error) = compile_with_catalog(
        r#"use acme; acme::choose(|item| { action: "continue" });"#,
        overloaded_callable_catalog(),
    ) {
        panic!("the map-returning overload should be selected: {error}");
    }
    if let Err(error) = compile_with_catalog(
        r#"use acme; acme::choose(|item| 1);"#,
        overloaded_callable_catalog(),
    ) {
        panic!("the int-returning overload should be selected: {error}");
    }
}
