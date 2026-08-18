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
