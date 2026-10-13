use super::*;
#[test]
fn coherent_schema_json_preserves_optional_callable_named_and_module_byte_identity() {
    let source = "// 中文\r\nuse self::lib as lib;\r\nlet optional: int? = null;\r\nlet value = lib::make();\r\nfn factory() { lib::make() }\r\nlet function = factory;\r\n";
    let library = "struct Box { value: int }\npub fn make() -> Box { { value: 42 } }\nlet local: int? = null;\n";
    let overrides = serde_json::json!([{"path": "./lib.rss", "source": library}]).to_string();
    let response: serde_json::Value =
        serde_json::from_slice(&local_type_hints_json_with_context_response(
            source,
            "rustscript",
            "/neutral/main.rss",
            &overrides,
        ))
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let hints = response["hints"].as_array().unwrap();
    let optional = hints
        .iter()
        .find(|hint| hint["name"] == "optional")
        .unwrap();
    assert_eq!(
        optional["schema"],
        serde_json::json!({"kind": "optional", "inner": {"kind": "int"}})
    );
    assert_eq!(optional["span"]["source_id"], 0);
    let lo = optional["span"]["lo"].as_u64().unwrap() as usize;
    let hi = optional["span"]["hi"].as_u64().unwrap() as usize;
    assert_eq!(&source[lo..hi], "optional");
    assert_eq!(optional["source_name"], "/neutral/main.rss");
    let named = hints.iter().find(|hint| hint["name"] == "value").unwrap();
    assert_eq!(named["schema"]["kind"], "named");
    let function = hints
        .iter()
        .find(|hint| hint["name"] == "function")
        .unwrap();
    assert_eq!(function["schema"]["kind"], "callable");
    assert_eq!(function["schema"]["result"], named["schema"]);
    let dependency = hints.iter().find(|hint| hint["name"] == "local").unwrap();
    assert_eq!(dependency["span"]["source_id"], 1);
    let lo = dependency["span"]["lo"].as_u64().unwrap() as usize;
    let hi = dependency["span"]["hi"].as_u64().unwrap() as usize;
    assert_eq!(&library[lo..hi], "local");
    assert_eq!(dependency["schema"], optional["schema"]);
}
#[test]
fn coherent_recursive_json_keeps_distinct_module_nominals_and_instantiated_callable_contracts() {
    let source = "// 中文\r\nuse self::left::lib as left;\r\nuse self::right::lib as right;\r\nlet left_value = left::make();\r\nlet right_value = right::make();\r\nfn identity<T>(value: T) -> T { value }\r\nlet specialized = identity::<int>;\r\nlet nested: map<[int?]> = { \"one\": [null, 42] };\r\nlet tuple: [int?, fn(int) -> int] = [null, specialized];\r\n";
    let left = "struct Box<T> { value: T }\npub fn make() -> Box<int> { { value: 42 } }\n";
    let right =
        "struct Box<T> { value: T }\npub fn make() -> Box<string> { { value: \"label\" } }\n";
    let overrides = serde_json::json!([
        {"path": "./left/lib.rss", "source": left},
        {"path": "./right/lib.rss", "source": right},
    ])
    .to_string();
    let response: serde_json::Value =
        serde_json::from_slice(&local_type_hints_json_with_context_response(
            source,
            "rustscript",
            "/neutral/main.rss",
            &overrides,
        ))
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let hints = response["hints"].as_array().unwrap();
    let hint = |name| hints.iter().find(|hint| hint["name"] == name).unwrap();
    let left_schema = &hint("left_value")["schema"];
    let right_schema = &hint("right_value")["schema"];
    assert_eq!(left_schema["kind"], "named");
    assert_eq!(right_schema["kind"], "named");
    assert_ne!(left_schema["name"], right_schema["name"]);
    assert_eq!(left_schema["args"], serde_json::json!([{"kind": "int"}]));
    assert_eq!(
        right_schema["args"],
        serde_json::json!([{"kind": "string"}])
    );
    let callable = serde_json::json!({"kind": "callable", "params": [{"kind": "int"}], "result": {"kind": "int"}});
    assert_eq!(hint("specialized")["schema"], callable);
    assert_eq!(
        hint("nested")["schema"],
        serde_json::json!({"kind": "map", "item": {"kind": "array", "item": {"kind": "optional", "inner": {"kind": "int"}}}})
    );
    assert_eq!(
        hint("tuple")["schema"],
        serde_json::json!({"kind": "tuple", "items": [{"kind": "optional", "inner": {"kind": "int"}}, callable]})
    );
    for name in [
        "left_value",
        "right_value",
        "specialized",
        "nested",
        "tuple",
    ] {
        let span = &hint(name)["span"];
        assert_eq!(span["source_id"], 0);
        assert_eq!(
            &source[span["lo"].as_u64().unwrap() as usize..span["hi"].as_u64().unwrap() as usize],
            name
        );
    }
}

#[test]
fn coherent_inferred_callable_json_keeps_optional_results_and_per_call_generics() {
    let source = "// 中文\r\nfn maybe(value: int?) { value }\r\nlet g = maybe;\r\nlet closure: fn(int?) -> int? = |value| value;\r\nfn answer() { 42 }\r\nlet f = answer;\r\nfn identity<T>(value: T) -> T { value }\r\nlet output = identity::<int>(41);\r\ng(null);\r\n";
    let response: serde_json::Value =
        serde_json::from_slice(&local_type_hints_json_with_context_response(
            source,
            "rustscript",
            "/neutral/main.rss",
            "[]",
        ))
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    let hints = response["hints"].as_array().unwrap();
    let hint = |name| hints.iter().find(|hint| hint["name"] == name).unwrap();
    let optional = serde_json::json!({"kind": "optional", "inner": {"kind": "int"}});
    let callable =
        serde_json::json!({"kind": "callable", "params": [optional.clone()], "result": optional});
    assert_eq!(hint("g")["schema"], callable);
    assert_eq!(hint("closure")["schema"], callable);
    assert_eq!(
        hint("f")["schema"],
        serde_json::json!({"kind":"callable", "params":[], "result":{"kind":"int"}})
    );
    assert_eq!(hint("output")["schema"], serde_json::json!({"kind":"int"}));
    for name in ["g", "closure", "f", "output"] {
        let span = &hint(name)["span"];
        assert_eq!(span["source_id"], 0);
        assert_eq!(
            &source[span["lo"].as_u64().unwrap() as usize..span["hi"].as_u64().unwrap() as usize],
            name
        );
        assert_eq!(hint(name)["source_name"], "/neutral/main.rss");
    }
}

#[test]
fn coherent_typehint_json_preserves_errors_and_existing_scalar_fields() {
    let response: serde_json::Value = serde_json::from_slice(&local_type_hints_json_response(
        "let value = 42;",
        "rustscript",
    ))
    .unwrap();
    let hint = &response["hints"][0];
    assert_eq!(hint["name"], "value");
    assert_eq!(hint["inferred_type"], "int");
    assert_eq!(hint["declared_line"], 1);
    let response: serde_json::Value = serde_json::from_slice(&local_type_hints_json_response(
        "let value: unknown = 42;",
        "rustscript",
    ))
    .unwrap();
    assert_eq!(response["hints"], serde_json::json!([]));
    assert!(response["error"].is_string());
}
