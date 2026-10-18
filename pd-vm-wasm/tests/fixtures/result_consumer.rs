use super::*;
use serde_json::{Value, json};

fn result_schema(ok: Value, err: Value) -> Value {
    json!({"kind": "result", "ok": ok, "err": err})
}

fn response(bytes: Vec<u8>) -> Value {
    serde_json::from_slice(&bytes).expect("service response must be JSON")
}

fn hint<'a>(response: &'a Value, name: &str) -> &'a Value {
    assert!(response.get("error").is_none(), "{response}");
    response["hints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hint| hint["name"] == name)
        .unwrap_or_else(|| panic!("missing hint {name}: {response}"))
}

#[test]
fn result_public_basic_hints_keep_ok_err_and_callable_payload_schemas() {
    let source = "fn increment(value: int) -> int { value + 1 }\nlet success: Result<int, string> = Result::Ok(7);\nlet failure: Result<int, string> = Result::Err(\"bad\");\nlet function: Result<fn(int) -> int, Result<int, string>> = Result::Ok(increment);\n";
    let payload = response(local_type_hints_json_response(source, "rss"));
    println!("RESULT_OBSERVATION basic-hints={payload}");
    let int = json!({"kind":"int"});
    let basic = result_schema(int.clone(), json!({"kind":"string"}));
    assert_eq!(hint(&payload, "success")["schema"], basic);
    assert_eq!(hint(&payload, "failure")["schema"], basic);
    assert_eq!(
        hint(&payload, "function")["schema"],
        result_schema(
            json!({"kind":"callable", "params":[int], "result":int}),
            basic
        )
    );
}

#[test]
fn result_public_hints_preserve_recursive_types_module_identity_and_arm_bindings() {
    let source = "// 中文\r\nuse self::lib as lib;\r\nlet returned = lib::make();\r\nlet matched = match returned { Result::Ok(value) => value.value, Result::Err(error) => 0 };\r\nlet nested: Result<[Result<int, string>?], Result<string?, int>> = Result::Ok([null]);\r\nlet optional: Result<int, string>? = null;\r\nfn relay(input: Result<int, string>) -> Result<string, string> { let number = input?; Result::Ok(\"done\") }\r\nlet relay_fn = relay;\r\nlet propagated = relay(Result::Err(\"bad\"));\r\n";
    let library = "struct Box<T> { value: T }\npub fn make() -> Result<Box<int>, Box<string>> { Result::Ok({ value: 42 }) }\nlet failure: Result<int, string> = Result::Err(\"bad\");\n";
    let overrides = json!([{"path":"./lib.rss", "source":library}]).to_string();
    let payload = response(local_type_hints_json_with_context_response(
        source,
        "rss",
        "/neutral/main.rss",
        &overrides,
    ));
    println!("RESULT_OBSERVATION hints={payload}");
    let named = |arg| json!({"kind":"named", "name":hint(&payload, "value")["schema"]["name"], "args":[arg]});
    let int = json!({"kind":"int"});
    let string = json!({"kind":"string"});
    let basic = result_schema(int.clone(), string.clone());
    assert_eq!(
        hint(&payload, "returned")["schema"],
        result_schema(named(int.clone()), named(string.clone()))
    );
    assert_eq!(hint(&payload, "value")["schema"], named(int.clone()));
    assert_eq!(hint(&payload, "error")["schema"], named(string.clone()));
    assert_eq!(hint(&payload, "matched")["schema"], int);
    assert_eq!(
        hint(&payload, "nested")["schema"],
        result_schema(
            json!({"kind":"array", "item":{"kind":"optional", "inner":basic}}),
            result_schema(json!({"kind":"optional", "inner":string}), int.clone())
        )
    );
    assert_eq!(
        hint(&payload, "optional")["schema"],
        json!({"kind":"optional", "inner":basic})
    );
    assert_eq!(hint(&payload, "number")["schema"], int);
    let outer = result_schema(string.clone(), string.clone());
    assert_eq!(hint(&payload, "propagated")["schema"], outer);
    assert_eq!(
        hint(&payload, "relay_fn")["schema"],
        json!({"kind":"callable", "params":[basic], "result":outer})
    );
    assert_eq!(
        hint(&payload, "returned")["inferred_type"],
        "Result<Box<int>, Box<string>>"
    );
    assert_eq!(hint(&payload, "value")["inferred_type"], "Box<int>");
    assert_eq!(hint(&payload, "error")["inferred_type"], "Box<string>");
    assert_eq!(hint(&payload, "returned")["declared_line"], 3);
    assert_eq!(hint(&payload, "returned")["last_line"], 4);
    assert_eq!(
        hint(&payload, "propagated")["inferred_type"],
        "Result<string, string>"
    );
    for name in [
        "returned",
        "value",
        "error",
        "input",
        "nested",
        "optional",
        "relay_fn",
        "propagated",
        "number",
    ] {
        let item = hint(&payload, name);
        let span = &item["span"];
        assert_eq!(span["source_id"], 0);
        assert_eq!(item["source_name"], "/neutral/main.rss");
        assert_eq!(
            &source[span["lo"].as_u64().unwrap() as usize..span["hi"].as_u64().unwrap() as usize],
            name
        );
    }
    let item = hint(&payload, "failure");
    assert_eq!(item["schema"], basic);
    assert_eq!(item["span"]["source_id"], 1);
    let span = &item["span"];
    assert_eq!(
        &library[span["lo"].as_u64().unwrap() as usize..span["hi"].as_u64().unwrap() as usize],
        "failure"
    );
}

#[test]
fn result_public_format_preserves_try_and_is_idempotent() {
    let source = "fn fail()->Result<int,string>{Result::Err(\"bad\")}\nfn relay()->Result<string,string>{let number=fail()?;Result::Ok(\"done\")}\nlet result=relay();\n";
    let formatted = response(format_source_json_response(source, "rss"));
    println!("RESULT_OBSERVATION format={formatted}");
    assert_eq!(formatted["ok"], true);
    let text = formatted["formatted"].as_str().unwrap();
    // The shared formatter may wrap a long function body between tokens.
    // Whitespace must not change the postfix operator or constructor tokens.
    assert!(
        text.split_whitespace()
            .collect::<String>()
            .contains("fail()?"),
        "{text}"
    );
    assert!(text.contains("Result<string, string>"), "{text}");
    assert_eq!(
        response(format_source_json_response(text, "rss")),
        formatted
    );
}

#[test]
fn result_public_lint_preserves_try_and_typed_diagnostics() {
    let source = "fn fail() -> Result<int, string> {\n    Result::Err(\"bad\")\n}\nfn relay() -> Result<string, string> {\n    let number = fail()?;\n    Result::Ok(\"done\")\n}\nlet result = relay();\n";
    let formatted = response(format_source_json_response(source, "rss"));
    assert_eq!(formatted["ok"], true);
    let text = formatted["formatted"].as_str().unwrap();
    let lint = response(lint_source_json_response(text, "rss"));
    println!("RESULT_OBSERVATION lint={lint}");
    assert_eq!(lint["diagnostics"], json!([]));
    let wrong = "fn fail() -> Result<int, string> { Result::Err(\"bad\") }\nfn wrong() -> Result<int, int> {\n    let number = fail()?;\n    Result::Ok(number)\n}\n";
    let compile_error = vm::compile_source_with_flavor_and_options(
        wrong,
        SourceFlavor::RustScript,
        stdlib::embedded_stdlib_compile_options(),
    )
    .err()
    .expect("wrong E must fail compilation");
    match &compile_error {
        vm::SourcePathError::SourceWithMap { error, sources } => {
            println!("RESULT_ERROR_ORIGIN {error:?}");
            for id in 0..3 {
                println!(
                    "RESULT_ERROR_SOURCE {id} {:?}",
                    sources.file(id).map(|file| (&file.name, file.text.len()))
                );
            }
        }
        other => println!("RESULT_ERROR_ORIGIN {other:?}"),
    }
    let lint = response(lint_source_json_response(wrong, "rss"));
    println!("RESULT_OBSERVATION mismatch={lint}");
    let error = lint["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["severity"] == "error")
        .expect("wrong E must be rejected");
    assert!(
        error["message"].as_str().unwrap().contains("string"),
        "{lint}"
    );
    assert!(error["message"].as_str().unwrap().contains("int"), "{lint}");
    assert_eq!(error["line"], 3);
    assert_eq!(error["span"]["start_line"], 3);
    assert!(
        error["span"]["end_col"].as_u64().unwrap() > error["span"]["start_col"].as_u64().unwrap()
    );
    assert!(
        error["rendered"].as_str().unwrap().contains("fail()?"),
        "{lint}"
    );
    let rejected = response(local_type_hints_json_response(wrong, "rss"));
    assert_eq!(rejected["hints"], json!([]));
    assert!(rejected["error"].is_string());
}

#[test]
fn result_public_dependency_lint_keeps_original_owner_and_exact_try_extent() {
    let library = "// 変換\r\nfn fail() -> Result<int, string> { Result::Err(\"bad\") }\r\npub fn wrong() -> Result<int, int> {\r\n    let number = fail()?;\r\n    Result::Ok(number)\r\n}\r\n";
    let source = "use self::lib as lib;\nlib::wrong();\n";
    let overrides = json!([{"path":"./lib.rss", "source":library}]).to_string();
    for _ in 0..2 {
        let lint = response(lint_source_json_with_context_response(
            source,
            "rss",
            "/neutral/main.rss",
            &overrides,
        ));
        println!("RESULT_OBSERVATION dependency-mismatch={lint}");
        let error = lint["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["severity"] == "error")
            .expect("wrong E must fail");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("? error type string is not int")
        );
        assert_eq!(error["line"], 4);
        assert_eq!(error["span"]["start_line"], 4);
        assert_eq!(error["span"]["end_line"], 4);
        assert_eq!(error["span"]["start_col"], 24);
        assert_eq!(error["span"]["end_col"], 25);
        assert!(
            error["rendered"]
                .as_str()
                .unwrap()
                .contains("/neutral/lib.rss")
        );
        assert!(error["rendered"].as_str().unwrap().contains("fail()?"));
    }
}

#[cfg(feature = "runtime")]
#[test]
fn result_native_runtime_response_json_distinguishes_guest_branches_and_fatal_error() {
    for (constructor, expected) in [
        ("Result::Ok(7)", "ok:8"),
        ("Result::Err(\"bad\")", "err:bad"),
    ] {
        let source = format!(
            r#"
fn relay(input: Result<int, string>) -> Result<int, string> {{
    let number = input?;
    print("after-try");
    Result::Ok(number + 1)
}}
let returned = relay({constructor});
match returned {{
    Result::Ok(value) => print("ok:" + value),
    Result::Err(error) => print("err:" + error),
}};
print("caller-continues");
"#
        );
        let payload = response(run_source_json_response(
            &source,
            "rss",
            Some(r#"{"fuel":100000,"fuelCheckInterval":1}"#),
        ));
        println!("RESULT_OBSERVATION native-runtime-{constructor}={payload}");
        assert_eq!(payload["ok"], true, "{payload}");
        assert!(payload["error"].is_null(), "{payload}");
        assert_eq!(payload["halted"], true);
        assert_eq!(payload["yielded"], false);
        assert_eq!(payload["diagnostics"], json!([]));
        assert_eq!(payload["fuel"]["enabled"], true);
        let expected_output = if constructor.contains("::Ok") {
            json!(["after-try", expected, "caller-continues"])
        } else {
            json!([expected, "caller-continues"])
        };
        assert_eq!(payload["output"], expected_output);
    }
    let fatal = response(run_source_json_response(
        "assert(false);",
        "rss",
        Some(r#"{"fuel":100000,"fuelCheckInterval":1}"#),
    ));
    println!("RESULT_OBSERVATION native-runtime-fatal={fatal}");
    assert_eq!(fatal["ok"], false);
    assert!(fatal["error"].is_string());
    assert_eq!(fatal["halted"], true);
}

#[test]
fn result_schema_json_recurses_through_both_branches_and_containers() {
    use vm::TypeSchema as S;
    let result = S::Result {
        ok: Box::new(S::Int),
        err: Box::new(S::String),
    };
    let expected = result_schema(json!({"kind":"int"}), json!({"kind":"string"}));
    let nested = S::Result {
        ok: Box::new(S::Optional(Box::new(S::Array(Box::new(result.clone()))))),
        err: Box::new(S::Callable {
            params: vec![S::Named("Box".into(), vec![result.clone()])],
            result: Box::new(result.clone()),
        }),
    };
    assert_eq!(
        type_schema_to_json(&nested),
        result_schema(
            json!({"kind":"optional", "inner":{"kind":"array", "item":expected}}),
            json!({"kind":"callable", "params":[{"kind":"named", "name":"Box", "args":[expected]}], "result":expected}),
        )
    );
    for (schema, expected) in [
        (
            S::Map(Box::new(result.clone())),
            json!({"kind":"map", "item":expected}),
        ),
        (
            S::ArrayTuple(vec![result.clone()]),
            json!({"kind":"tuple", "items":[expected]}),
        ),
        (
            S::ArrayTupleRest {
                prefix: vec![result.clone()],
                rest: Box::new(result.clone()),
            },
            json!({"kind":"tuple_rest", "prefix":[expected], "rest":expected}),
        ),
        (
            S::Object(std::collections::HashMap::from([("value".into(), result)])),
            json!({"kind":"object", "fields":{"value":expected}}),
        ),
    ] {
        assert_eq!(type_schema_to_json(&schema), expected);
    }
}
