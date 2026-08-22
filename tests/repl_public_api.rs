use vm::compiler::TypeSchema;
use vm::{
    ReplLocalBinding, ReplLocalState, compile_source_for_repl_with_state, standard_host_catalog,
};

#[test]
fn public_repl_state_api_preserves_moved_local_semantics() {
    let binding = ReplLocalBinding {
        name: "message".to_string(),
        mutable: false,
        schema: Some(TypeSchema::String),
        optional: false,
    };

    let available = ReplLocalState {
        binding: binding.clone(),
        moved: false,
    };
    compile_source_for_repl_with_state("message;", &[available])
        .expect("available local should compile");

    let moved = ReplLocalState {
        binding,
        moved: true,
    };
    assert!(
        compile_source_for_repl_with_state("message;", &[moved]).is_err(),
        "moved local must be rejected"
    );
}

/// The REPL compile entry must attach the standard host catalog and emit
/// exact V13 `HostImport` schemas for standard host calls — never a
/// name-only fallback — identically to the file/at-path entries.
#[cfg(feature = "http-client")]
#[test]
fn repl_state_api_emits_exact_host_import_schemas() {
    let compiled = compile_source_for_repl_with_state(
        "use http; let _ = http::client::request({\"method\": \"GET\", \"url\": \"http://127.0.0.1:1/x\"});",
        &[],
    )
    .expect("repl snippet with a standard host call should compile");

    let http_import = compiled
        .compiled
        .program
        .imports
        .iter()
        .find(|i| i.name == "http::client::request")
        .expect("http::client::request must be a host import");
    assert!(
        http_import.schema.is_some(),
        "repl compile must emit exact schemas, got: {:?}",
        http_import.schema
    );
    assert_eq!(
        http_import.schema.as_ref().unwrap().fingerprint,
        standard_host_catalog().fingerprint(),
        "repl compile schema must carry the standard catalog fingerprint"
    );
}
