#![cfg(feature = "runtime")]

use vm::compile_source;

fn untyped_json_rejection(source: &str) -> (String, Option<vm::Span>) {
    match compile_source(source) {
        Ok(_) => panic!("untyped public JSON must not remain a RustScript source route: {source}"),
        Err(vm::SourceError::Compile(vm::CompileError::StrictTypingRequired {
            detail,
            span,
            ..
        })) => (detail, span),
        Err(error) => panic!("unexpected request rejection shape: {error}"),
    }
}

#[test]
fn rss_rejects_untyped_public_json_decode_with_call_span() {
    let source = "use json; json::decode(\"null\");";
    let (detail, span) = untyped_json_rejection(source);
    assert!(
        detail.contains("typed JSON target"),
        "untyped decode must fail at the typed boundary: {detail}"
    );
    let span = span.expect("untyped decode must keep its parser-origin call span");
    assert_eq!(&source[span.lo..span.hi], "json::decode");
}

#[test]
fn rss_untyped_decode_null_cannot_reach_language_print() {
    let source = "use json; print(json::decode(\"null\"));";
    let (detail, span) = untyped_json_rejection(source);
    assert!(
        detail.contains("typed JSON target"),
        "raw decode must not flow into print: {detail}"
    );
    let span = span.expect("untyped decode must keep its parser-origin call span");
    assert_eq!(&source[span.lo..span.hi], "json::decode");
}

#[test]
fn rss_untyped_public_json_encode_without_concrete_target_fails_explicitly() {
    let source = "use json; let identity = |value| value; json::encode(identity);";
    let error = match compile_source(source) {
        Ok(_) => panic!("untyped encode without a concrete target must fail explicitly: {source}"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("cannot encode this value"),
        "untyped encode without a concrete target must fail explicitly: {error}"
    );
}

#[test]
fn rss_untyped_encode_of_dynamic_runtime_map_is_rejected_at_the_typed_boundary() {
    let source = "use json; let payload: map = {}; json::encode(payload);";
    let (detail, span) = untyped_json_rejection(source);
    assert!(
        detail.contains("typed JSON target"),
        "untyped runtime-map encode must fail at the typed boundary: {detail}"
    );
    let span = span.expect("untyped encode must keep its parser-origin call span");
    assert_eq!(&source[span.lo..span.hi], "json::encode");
}

#[test]
fn typed_rss_decode_and_print_keep_logical_unit() {
    let source = "use json;\nprint(json::decode::<()>(\"null\"));\n";
    let compiled = compile_source(source)
        .unwrap_or_else(|error| panic!("typed decode should compile: {source}\n{error}"));
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    assert_eq!(
        machine.run().expect("typed decode run"),
        vm::VmStatus::Halted
    );
    assert_eq!(lines.lock().expect("sink").as_slice(), ["()".to_string()]);
}
