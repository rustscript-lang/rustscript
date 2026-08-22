#![cfg(not(feature = "runtime"))]

use std::sync::Arc;

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamSchema, HostTypeSchema,
    compile_source_with_flavor_and_options,
};

fn custom_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "x::f",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("custom catalog must build"))
}

#[test]
fn no_runtime_explicit_catalog_emits_exact_import_schema() {
    let custom = custom_catalog();
    let compiled = compile_source_with_flavor_and_options(
        "use x; x::f(1);",
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&custom)),
    )
    .expect("no-runtime custom catalog source should compile");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "x::f")
        .expect("custom host import should be present");
    let schema = import
        .schema
        .as_ref()
        .expect("custom no-runtime import must carry exact schema");
    assert_eq!(schema.fingerprint, custom.fingerprint());
    assert_eq!(schema.params[0].name, "value");
    assert_eq!(schema.params[0].schema, TypeSchema::Int);
    assert_eq!(schema.return_type, TypeSchema::Int);
}
