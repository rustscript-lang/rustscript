use super::*;
use serde_json::{Value, json};

fn option_schema(inner: Value) -> Value {
    json!({"kind": "option", "inner": inner})
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
fn option_public_basic_hints_keep_option_and_unit_schemas() {
    let source = "fn relay(value: Option<int>) -> Option<int> { match value { Option::Some(v) => Option::Some(v + 1), Option::None => Option::None } }\n\
         let some: Option<int> = Option::Some(3);\n\
         let none: Option<int> = Option::None;\n\
         let unit: () = ();\n\
         let relay_fn = relay;\n";
    let payload = response(local_type_hints_json_response(source, "rss"));
    println!("OPTION_OBSERVATION basic-hints={payload}");
    let int = json!({"kind":"int"});
    let option_int = option_schema(int.clone());
    assert_eq!(hint(&payload, "some")["schema"], option_int);
    assert_eq!(hint(&payload, "none")["schema"], option_int);
    assert_eq!(hint(&payload, "unit")["schema"], json!({"kind":"unit"}));
    assert_eq!(hint(&payload, "some")["inferred_type"], "Option<int>");
    assert_eq!(hint(&payload, "unit")["inferred_type"], "()");
    assert_eq!(
        hint(&payload, "relay_fn")["schema"],
        json!({"kind":"callable", "params":[option_int.clone()], "result":option_int})
    );
}

#[test]
fn option_public_hints_preserve_nested_result_and_resource_free_units() {
    let source = "let nested: Option<Result<int, string>> = Option::Some(Result::Ok(1));\n\
         let inner_unit: Option<()> = Option::Some(());\n\
         let empty: Option<string> = Option::Some(\"\");\n\
         let value: int = match nested { Option::Some(result) => match result { Result::Ok(v) => v, Result::Err(e) => -1 }, Option::None => -1 };\n";
    let payload = response(local_type_hints_json_response(source, "rss"));
    println!("OPTION_OBSERVATION nested-hints={payload}");
    let int = json!({"kind":"int"});
    let string = json!({"kind":"string"});
    let result = json!({"kind":"result", "ok":int, "err":string});
    assert_eq!(hint(&payload, "nested")["schema"], option_schema(result));
    assert_eq!(
        hint(&payload, "inner_unit")["schema"],
        option_schema(json!({"kind":"unit"}))
    );
    assert_eq!(hint(&payload, "empty")["schema"], option_schema(string));
    assert_eq!(hint(&payload, "value")["schema"], int);
}

#[test]
fn option_public_format_is_idempotent_and_preserves_unit_spelling() {
    let source = "fn relay(value:Option<int>)->Option<int>{match value{Option::Some(v)=>Option::Some(v+1),Option::None=>Option::None}}\n\
         let value:Option<()> =Option::Some(());\nlet result=relay(Option::Some(1));\n";
    let formatted = response(format_source_json_response(source, "rss"));
    println!("OPTION_OBSERVATION format={formatted}");
    assert_eq!(formatted["ok"], true);
    let text = formatted["formatted"].as_str().unwrap();
    assert!(text.contains("Option<int>"), "{text}");
    assert!(text.contains("Option<()>"), "{text}");
    assert!(text.contains("Option::None"), "{text}");
    assert_eq!(
        response(format_source_json_response(text, "rss")),
        formatted
    );
}

#[cfg(feature = "runtime")]
#[test]
fn option_native_runtime_response_json_keeps_all_states() {
    let source = r#"
fn relay(value: Option<int>) -> Option<int> { match value { Option::Some(v) => Option::Some(v + 1), Option::None => Option::None } }
let some = relay(Option::Some(7));
let none = relay(Option::None);
match some { Option::Some(value) => print("some:" + value), Option::None => print("some:none"), };
match none { Option::Some(value) => print("none:some"), Option::None => print("none:none"), };
let unit: () = ();
print("unit");
"#;
    let payload = response(run_source_json_response(
        source,
        "rss",
        Some(r#"{"fuel":100000,"fuelCheckInterval":1}"#),
    ));
    println!("OPTION_OBSERVATION native-runtime={payload}");
    assert_eq!(payload["ok"], true, "{payload}");
    assert!(payload["error"].is_null(), "{payload}");
    assert_eq!(payload["halted"], true);
    assert_eq!(payload["output"], json!(["some:8", "none:none", "unit"]));
}

#[test]
fn option_schema_json_recurses_through_option_nested_in_containers() {
    use vm::TypeSchema as S;
    let int = json!({"kind":"int"});
    let option = S::Option(Box::new(S::Int));
    let expected = option_schema(int.clone());
    assert_eq!(type_schema_to_json(&option), expected);
    assert_eq!(type_schema_to_json(&S::Unit), json!({"kind":"unit"}));
    let nested = S::Option(Box::new(S::Result {
        ok: Box::new(S::Array(Box::new(option.clone()))),
        err: Box::new(S::Callable {
            params: vec![S::Unit],
            result: Box::new(option.clone()),
        }),
    }));
    assert_eq!(
        type_schema_to_json(&nested),
        option_schema(json!({
            "kind":"result",
            "ok":{"kind":"array", "item":expected},
            "err":{"kind":"callable", "params":[{"kind":"unit"}], "result":expected}
        }))
    );
}
