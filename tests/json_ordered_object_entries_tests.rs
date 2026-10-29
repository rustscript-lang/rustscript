//! Ordered JSON object-entry contract: real RSS/VM evidence.
//!
//! `Value::Map` stays an unordered hash map, so the only way a script can see a
//! decoded object's member order is the `json::decode_object_entries` /
//! `json::encode_object_entries` wire codec plus its named `JsonObjectEntry`
//! body. These tests drive that contract through the real compiler, the real
//! standard catalog, real host binding, and the real VM; the codec's Rust
//! functions are never called directly.
//!
//! Covered here:
//!
//! * the published typed schemas (two required string fields, `Value` passing,
//!   array of the named body) and that the new capability allocates **no**
//!   static builtin index;
//! * document order for both directions, per-entry values, nested objects,
//!   escaped keys/values, Unicode, empty containers, `null`/`true`/numbers,
//!   and a 2048-member object (no per-entry script-frame recursion);
//! * meaningful rejections for bad roots, duplicate keys, bad/trailing JSON,
//!   missing/wrong fields, and duplicate entry keys — with the VM still usable
//!   afterwards and no resource or pending-operation growth;
//! * the same-snapshot custom catalog / exact binding path, plus the
//!   fail-closed unbound path and the untouched plain `json` builtins;
//! * negative gates: corrupted order and corrupted values are genuinely
//!   different from the real output, so the comparisons above are load-bearing.

#![cfg(feature = "runtime")]

use std::sync::Arc;

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::host_api::{HostApiCatalog, HostParamPassing};
use vm::{
    CompiledProgram, HostApiBuilder, HostFunctionRegistry, HostFunctionSchema, HostParamSchema,
    HostStructField, HostStructSchema, HostTypeSchema, SourcePathError, StandardHostModule, Value,
    Vm, VmError, VmStatus, compile_source, register_json_builtin_module_from_catalog,
    standard_host_catalog, standard_host_modules,
};

const DECODE: &str = "json::decode_object_entries";
const ENCODE: &str = "json::encode_object_entries";
const ENTRY: &str = "JsonObjectEntry";

// ------------------------------------------------------------------- harness

fn json_module() -> &'static StandardHostModule {
    standard_host_modules()
        .iter()
        .find(|module| module.name == "json")
        .expect("the standard json host module must be composed")
}

/// The json module's own published catalog: the snapshot a guest compiled
/// against the standard catalog agrees with.
fn json_module_catalog() -> HostApiCatalog {
    json_module()
        .catalog_module()
        .expect("the json module must publish a catalog surface")
        .catalog()
        .expect("the json catalog surface must be valid")
}

/// A registry whose json adapters are installed from `catalog`.
fn registry_from(catalog: &HostApiCatalog) -> HostFunctionRegistry {
    let mut registry = HostFunctionRegistry::empty();
    register_json_builtin_module_from_catalog(&mut registry, catalog)
        .expect("the json entry adapters must install from the compile snapshot");
    registry
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
        Ok(_) => panic!("expected a compile error for:\n{source}"),
        Err(error) => error.to_string(),
    }
}

fn run_to_halt(vm: &mut Vm) {
    loop {
        match vm.run().expect("source must run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_) => panic!("the ordered entry codec must not pend"),
        }
    }
}

/// Compiles with the default (standard) catalog, binds the standard json
/// module from that same snapshot, and runs the program to halt.
fn run_standard(source: &str) -> Vec<Value> {
    let catalog = standard_host_catalog();
    let compiled = compile(source, Arc::clone(&catalog)).expect("source must compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry_from(catalog.as_ref())
        .bind_vm_cached(&mut vm)
        .expect("the standard json module must bind");
    run_to_halt(&mut vm);
    vm.stack().to_vec()
}

/// A halted VM whose exported callables can be invoked one at a time, so one
/// test can mix accepted and rejected calls on the same VM.
fn vm_for(source: &str) -> Vm {
    let catalog = standard_host_catalog();
    let compiled = compile(source, Arc::clone(&catalog)).expect("source must compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry_from(catalog.as_ref())
        .bind_vm_cached(&mut vm)
        .expect("the standard json module must bind");
    run_to_halt(&mut vm);
    vm
}

fn invoke(vm: &mut Vm, name: &str) -> Result<Value, String> {
    let callable = vm
        .resolve_exported_callable(name)
        .unwrap_or_else(|error| panic!("exported callable `{name}`: {error}"));
    vm.invoke_callable(callable, &[])
        .map_err(|error| match error {
            VmError::HostError(message) => message,
            other => format!("{other:?}"),
        })
}

fn strings(value: &Value) -> Vec<String> {
    let Value::Array(items) = value else {
        panic!("expected an array, got {value:?}");
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.as_str().to_string(),
            other => panic!("expected a string element, got {other:?}"),
        })
        .collect()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.as_str().to_string(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Reads one guest `JsonObjectEntry` carrier (a map with the two required
/// string fields).
fn entry_parts(value: &Value) -> (String, String) {
    let Value::Map(map) = value else {
        panic!("expected a JsonObjectEntry carrier map, got {value:?}");
    };
    let field = |name: &str| match map.get(&Value::string(name)) {
        Some(Value::String(text)) => text.as_str().to_string(),
        other => panic!("entry field `{name}`: {other:?}"),
    };
    (field("key"), field("value_json"))
}

fn entries(value: &Value) -> Vec<(String, String)> {
    let Value::Array(items) = value else {
        panic!("expected an entry array, got {value:?}");
    };
    items.iter().map(entry_parts).collect()
}

fn entry_keys(value: &Value) -> Vec<String> {
    entries(value).into_iter().map(|(key, _)| key).collect()
}

/// Asserts no host resource and no pending operation survived a call.
fn assert_vm_is_clean(vm: &mut Vm, context: &str) {
    assert_eq!(
        vm.host_context().resource_count(),
        0,
        "{context} must not leave a host resource behind"
    );
    assert!(
        vm.execution_scope().operations().is_empty(),
        "{context} must not leave a pending operation behind"
    );
}

/// One exported callable that returns the decoded entries of `text`.
fn decode_callable(name: &str, text: &str) -> String {
    format!("pub fn {name}() {{\n    json::decode_object_entries({text:?})\n}}\n")
}

/// One exported callable that encodes a guest-built entry array.
fn encode_callable(name: &str, entries_source: &str) -> String {
    format!("pub fn {name}() {{\n    json::encode_object_entries({entries_source})\n}}\n")
}

/// A JSON object text with `count` members whose keys sort in the opposite
/// order to their document order, so any re-sort is observable.
fn wide_object(count: usize) -> (String, Vec<String>) {
    let keys: Vec<String> = (0..count)
        .map(|index| format!("k{:06}", count - index))
        .collect();
    let mut text = String::from("{");
    for (position, key) in keys.iter().enumerate() {
        if position > 0 {
            text.push(',');
        }
        text.push_str(&format!("\"{key}\":{position}"));
    }
    text.push('}');
    (text, keys)
}

// ------------------------------------------------------------------- schemas

#[test]
fn standard_catalog_publishes_the_required_named_entry_body() {
    let catalog = standard_host_catalog();
    let body = catalog
        .struct_named(ENTRY)
        .expect("JsonObjectEntry must be declared by the standard catalog");
    let fields: Vec<(&str, &HostTypeSchema)> = body
        .fields
        .iter()
        .map(|field| (field.name.as_str(), &field.ty))
        .collect();
    assert_eq!(
        fields,
        [
            ("key", &HostTypeSchema::String),
            ("value_json", &HostTypeSchema::String)
        ],
        "JsonObjectEntry carries exactly the two required string fields"
    );
    assert!(
        body.fields
            .iter()
            .all(|field| !matches!(field.ty, HostTypeSchema::Optional(_))),
        "both JsonObjectEntry fields are required"
    );
}

#[test]
fn entry_functions_publish_typed_value_passing_schemas() {
    let catalog = standard_host_catalog();
    let entry_type = catalog
        .struct_named(ENTRY)
        .expect("JsonObjectEntry must be declared")
        .as_type();

    let decode = catalog.functions_named(DECODE);
    assert_eq!(decode.len(), 1, "one `{DECODE}` overload");
    let decode = &decode[0];
    assert_eq!(
        decode.return_type,
        HostTypeSchema::Array(Box::new(entry_type.clone()))
    );
    assert_eq!(decode.params.len(), 1);
    assert_eq!(decode.params[0].name, "text");
    assert_eq!(decode.params[0].ty, HostTypeSchema::String);
    assert_eq!(
        decode.params[0].passing,
        HostParamPassing::Value,
        "the ordered codec takes resource-free values"
    );

    let encode = catalog.functions_named(ENCODE);
    assert_eq!(encode.len(), 1, "one `{ENCODE}` overload");
    let encode = &encode[0];
    assert_eq!(encode.return_type, HostTypeSchema::String);
    assert_eq!(encode.params.len(), 1);
    assert_eq!(encode.params[0].name, "entries");
    assert_eq!(
        encode.params[0].ty,
        HostTypeSchema::Array(Box::new(entry_type)),
        "the encoder takes the named entry array"
    );
    assert_eq!(encode.params[0].passing, HostParamPassing::Value);

    // Neither direction may widen to a dynamic schema.
    for function in [decode, encode] {
        assert_ne!(function.return_type, HostTypeSchema::Unknown);
        for param in &function.params {
            assert_ne!(param.ty, HostTypeSchema::Unknown);
            assert_ne!(
                param.ty,
                HostTypeSchema::Map(Box::new(HostTypeSchema::Unknown))
            );
        }
    }
}

// ----------------------------------------------------------------------- ABI

/// The ordered entry codec is a typed host import, not a static builtin: the
/// frozen call-index table is untouched, so every existing VMBC `call` operand
/// keeps its meaning.
#[test]
fn ordered_entry_codec_allocates_no_static_builtin_index() {
    assert_eq!(vm::builtin_call_index(DECODE), None);
    assert_eq!(vm::builtin_call_index(ENCODE), None);
    assert!(
        vm::BUILTIN_CATALOG
            .iter()
            .all(|builtin| !builtin.name().contains("object_entries")),
        "no object-entry function may join the static builtin catalog"
    );
    assert_eq!(
        vm::builtin_call_index("json::encode"),
        Some(0xFFC2),
        "json::encode keeps its frozen static index"
    );
    assert_eq!(
        vm::builtin_call_index("json::decode"),
        Some(0xFFC4),
        "json::decode keeps its frozen static index"
    );
    // The optional SQLite namespace is filtered out of the generated catalog
    // when its feature is off, so the frozen count follows the same gate. The
    // json and io indices below are identical in both shapes.
    let expected_catalog_entries = if cfg!(feature = "sqlite") { 109 } else { 104 };
    assert_eq!(
        vm::BUILTIN_CATALOG.len(),
        expected_catalog_entries,
        "the frozen static catalog keeps its entry count"
    );
}

// ------------------------------------------------------------------ behavior

#[test]
fn decode_and_encode_keep_both_opposite_key_orders() {
    let first = r#"{"zzzzzzz9": 1, "0000000a": 2, "mmmmmmmm": 3, "zzzzzzz1": 4}"#;
    let second = r#"{"mmmmmmmm": 3, "zzzzzzz1": 4, "0000000a": 2, "zzzzzzz9": 1}"#;
    let source = format!(
        "use json;\n\
         pub fn first() {{\n    json::decode_object_entries({first:?})\n}}\n\
         pub fn second() {{\n    json::decode_object_entries({second:?})\n}}\n\
         pub fn round_trip_first() {{\n    \
         json::decode_object_entries(json::encode_object_entries(json::decode_object_entries({first:?})))\n}}\n"
    );
    let mut vm = vm_for(&source);

    let first_entries = entries(&invoke(&mut vm, "first").expect("first decode"));
    assert_eq!(
        first_entries
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["zzzzzzz9", "0000000a", "mmmmmmmm", "zzzzzzz1"],
        "document order is preserved, not sorted"
    );
    assert_eq!(
        first_entries
            .iter()
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>(),
        ["1", "2", "3", "4"],
        "each member keeps its own value"
    );

    let second_entries = entries(&invoke(&mut vm, "second").expect("second decode"));
    assert_eq!(
        second_entries
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["mmmmmmmm", "zzzzzzz1", "0000000a", "zzzzzzz9"],
        "the opposite order is its own answer"
    );
    assert_ne!(
        first_entries, second_entries,
        "the same key set in two orders must not collapse to one answer"
    );

    let round_tripped = entries(&invoke(&mut vm, "round_trip_first").expect("round trip"));
    assert_eq!(
        round_tripped, first_entries,
        "decode -> encode -> decode keeps both order and values"
    );
    assert_vm_is_clean(&mut vm, "the order round trip");
}

#[test]
fn guest_built_entry_array_encodes_in_input_order() {
    let entries_literal = r#"[
        { key: "zzzzzzz9", value_json: "1" },
        { key: "0000000a", value_json: "2" },
        { key: "mmmmmmmm", value_json: "3" },
        { key: "zzzzzzz1", value_json: "4" }
    ]"#;
    let reversed_literal = r#"[
        { key: "mmmmmmmm", value_json: "3" },
        { key: "zzzzzzz1", value_json: "4" },
        { key: "0000000a", value_json: "2" },
        { key: "zzzzzzz9", value_json: "1" }
    ]"#;
    let cookie_literal = r#"[
        { key: "nw", value_json: "\"1\"" },
        { key: "ipb_member_id", value_json: "\"17\"" },
        { key: "ipb_pass_hash", value_json: "\"abc\"" }
    ]"#;
    let source = format!(
        "use json;\n{}{}{}",
        encode_callable("forward", entries_literal),
        encode_callable("reverse", reversed_literal),
        encode_callable("cookie", cookie_literal)
    );
    let mut vm = vm_for(&source);

    let forward = text(&invoke(&mut vm, "forward").expect("forward encode"));
    assert_eq!(
        forward, r#"{"zzzzzzz9":1,"0000000a":2,"mmmmmmmm":3,"zzzzzzz1":4}"#,
        "the encoder writes the guest's order"
    );
    let reverse = text(&invoke(&mut vm, "reverse").expect("reverse encode"));
    assert_eq!(
        reverse, r#"{"mmmmmmmm":3,"zzzzzzz1":4,"0000000a":2,"zzzzzzz9":1}"#,
        "the opposite order produces the opposite document"
    );
    assert_ne!(forward, reverse);
    assert_eq!(
        text(&invoke(&mut vm, "cookie").expect("cookie encode")),
        r#"{"nw":"1","ipb_member_id":"17","ipb_pass_hash":"abc"}"#,
        "a cookie object keeps its header order"
    );
    assert_vm_is_clean(&mut vm, "guest-built encoding");
}

#[test]
fn nested_objects_arrays_and_scalars_survive_the_codec() {
    // `tasks` is an object of nested objects whose inner order matters; the
    // remaining members cover every JSON value kind plus escaped and Unicode
    // text.
    let source = format!(
        "use json;\n{}",
        decode_callable(
            "probe",
            r#"{
                "tasks": {"task_b": {"state": "paused", "n": 2}, "task_a": {"state": "ready", "n": 1}},
                "cookies": {"nw": "1", "ipb_pass_hash": "abc"},
                "escaped\":key": "quote \" and backslash \\ and tab \t",
                "unicode": "日本語 – 😀",
                "empty_object": {},
                "empty_array": [],
                "array": [3, 2, 1],
                "null_value": null,
                "true_value": true,
                "false_value": false,
                "float_value": 1.5,
                "int_value": -7,
                "string_value": "plain"
            }"#
        )
    );
    let mut vm = vm_for(&source);
    let decoded = entries(&invoke(&mut vm, "probe").expect("nested decode"));
    let keys: Vec<&str> = decoded.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "tasks",
            "cookies",
            r#"escaped":key"#,
            "unicode",
            "empty_object",
            "empty_array",
            "array",
            "null_value",
            "true_value",
            "false_value",
            "float_value",
            "int_value",
            "string_value",
        ],
        "top-level member order and escaped keys are preserved"
    );

    let by_key = |wanted: &str| {
        decoded
            .iter()
            .find(|(key, _)| key == wanted)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("missing member {wanted}"))
    };
    assert_eq!(by_key("empty_object"), "{}");
    assert_eq!(by_key("empty_array"), "[]");
    assert_eq!(by_key("null_value"), "null");
    assert_eq!(by_key("true_value"), "true");
    assert_eq!(by_key("false_value"), "false");
    assert_eq!(by_key("float_value"), "1.5");
    assert_eq!(by_key("int_value"), "-7");
    assert_eq!(by_key("array"), "[3, 2, 1]");
    assert_eq!(by_key("string_value"), "\"plain\"");
    assert_eq!(by_key("unicode"), "\"日本語 – 😀\"");
    assert_eq!(
        by_key(r#"escaped":key"#),
        r#""quote \" and backslash \\ and tab \t""#
    );

    // The nested `tasks` object keeps its inner order when the caller recurses
    // into the member's own JSON text.
    let tasks_source = format!("use json;\n{}", decode_callable("inner", &by_key("tasks")));
    let mut inner_vm = vm_for(&tasks_source);
    let task_entries = entries(&invoke(&mut inner_vm, "inner").expect("nested decode"));
    assert_eq!(
        task_entries
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["task_b", "task_a"],
        "second-level object order is preserved"
    );
    assert_eq!(task_entries[0].1, r#"{"state": "paused", "n": 2}"#);

    let cookie_source = format!(
        "use json;\n{}",
        decode_callable("cookie", &by_key("cookies"))
    );
    let mut cookie_vm = vm_for(&cookie_source);
    assert_eq!(
        entry_keys(&invoke(&mut cookie_vm, "cookie").expect("cookie decode")),
        ["nw", "ipb_pass_hash"]
    );
}

#[test]
fn empty_object_decodes_to_empty_array_and_empty_entries_encode_empty_object() {
    let source = format!(
        "use json;\n{}{}",
        decode_callable("empty_object", "{}"),
        encode_callable("empty_entries", "[]")
    );
    let mut vm = vm_for(&source);
    assert_eq!(
        entries(&invoke(&mut vm, "empty_object").expect("empty decode")),
        Vec::<(String, String)>::new(),
        "an empty object is an empty entry array, never null"
    );
    assert_eq!(
        text(&invoke(&mut vm, "empty_entries").expect("empty encode")),
        "{}"
    );
}

#[test]
fn null_and_empty_values_stay_entries() {
    let source = format!(
        "use json;\n{}{}",
        decode_callable("decoded", r#"{"null_member": null, "empty": ""}"#),
        encode_callable(
            "encoded",
            r#"[{ key: "null_member", value_json: "null" }, { key: "empty", value_json: "\"\"" }]"#
        )
    );
    let mut vm = vm_for(&source);
    let decoded = entries(&invoke(&mut vm, "decoded").expect("null member decode"));
    assert_eq!(
        decoded,
        vec![
            ("null_member".to_string(), "null".to_string()),
            ("empty".to_string(), "\"\"".to_string()),
        ],
        "a null value is a member, not a missing entry"
    );
    assert_eq!(
        text(&invoke(&mut vm, "encoded").expect("null member encode")),
        r#"{"null_member":null,"empty":""}"#
    );
}

#[test]
fn wide_object_keeps_order_without_script_frame_recursion() {
    let count = 2048;
    let (object, keys) = wide_object(count);
    let source = format!(
        "use json;\n\
         pub fn width() {{\n    let entries = json::decode_object_entries({object:?});\n    \
         entries.length\n}}\n\
         pub fn edges() {{\n    let entries = json::decode_object_entries({object:?});\n    \
         [entries[0].key, entries[{last}].key]\n}}\n\
         pub fn reencoded() {{\n    let entries = json::decode_object_entries({object:?});\n    \
         json::encode_object_entries(entries)\n}}\n",
        last = count - 1
    );
    let mut vm = vm_for(&source);
    assert_eq!(
        invoke(&mut vm, "width").expect("wide decode length"),
        Value::Int(count as i64)
    );
    assert_eq!(
        strings(&invoke(&mut vm, "edges").expect("wide decode edges")),
        vec![keys[0].clone(), keys[count - 1].clone()],
        "the first and last members keep their document positions"
    );
    let reencoded = text(&invoke(&mut vm, "reencoded").expect("wide re-encode"));
    assert!(
        reencoded.starts_with(&format!("{{\"{}\":0,", keys[0])),
        "the re-encoded object starts with the first member"
    );
    let tail = format!("\"{}\":{}", keys[count - 1], count - 1) + "}";
    assert!(
        reencoded.ends_with(&tail),
        "the re-encoded object ends with the last member"
    );
    assert_vm_is_clean(&mut vm, "the 2048-member object");
}

#[test]
fn depth_boundary_is_the_standard_parser_limit() {
    fn nested(depth: usize) -> String {
        let mut text = String::new();
        for _ in 0..depth {
            text.push_str("{\"a\":");
        }
        text.push('1');
        for _ in 0..depth {
            text.push('}');
        }
        text
    }
    let shallow = nested(64);
    let deep = nested(400);

    let source = format!(
        "use json;\n{}{}{}",
        decode_callable("shallow", &shallow),
        decode_callable("deep", &deep),
        encode_callable(
            "deep_value",
            &format!(
                "[{{ key: \"a\", value_json: {deep_value:?} }}]",
                deep_value = deep
            )
        )
    );
    let mut vm = vm_for(&source);
    let decoded = entries(&invoke(&mut vm, "shallow").expect("64-deep object decodes"));
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].1, nested(63), "the nested value is kept intact");

    let deep_error = invoke(&mut vm, "deep").expect_err("400-deep object must be rejected");
    assert!(
        deep_error.contains("recursion limit"),
        "the standard parser's depth limit is the boundary: {deep_error}"
    );
    let deep_value_error =
        invoke(&mut vm, "deep_value").expect_err("400-deep value_json must be rejected");
    assert!(
        deep_value_error.contains("recursion limit"),
        "the encoder applies the same limit: {deep_value_error}"
    );
    assert_vm_is_clean(&mut vm, "the depth boundary");
    assert!(
        invoke(&mut vm, "shallow").is_ok(),
        "the same VM still decodes after the rejections"
    );
}

#[test]
fn decode_rejections_are_meaningful_and_leave_the_vm_usable() {
    let cases: [(&str, &str, &str); 10] = [
        ("array_root", "[]", "root value must be a JSON object"),
        ("scalar_root", "42", "root value must be a JSON object"),
        ("null_root", "null", "root value must be a JSON object"),
        (
            "duplicate_root",
            r#"{"a": 1, "a": 2}"#,
            "duplicate object key",
        ),
        (
            "duplicate_nested",
            r#"{"a": {"b": 1, "b": 2}}"#,
            "duplicate object key",
        ),
        ("trailing", r#"{"a": 1} {"b": 2}"#, "trailing"),
        (
            "bad_escape",
            r#"{"a": "\q"}"#,
            "json_decode_object_entries failed",
        ),
        (
            "surrogate_value",
            r#"{"a": "\ud800"}"#,
            "json_decode_object_entries failed",
        ),
        (
            "surrogate_key",
            r#"{"\ud800": 1}"#,
            "json_decode_object_entries failed",
        ),
        ("nonfinite", r#"{"a": 1e400}"#, "number out of range"),
    ];
    let mut source = String::from("use json;\n");
    for (name, text, _) in cases {
        source.push_str(&decode_callable(name, text));
    }
    source.push_str(&decode_callable("healthy", r#"{"ok": 1}"#));
    let mut vm = vm_for(&source);

    for (name, _, expected) in cases {
        let error = invoke(&mut vm, name)
            .err()
            .unwrap_or_else(|| panic!("`{name}` must be rejected"));
        assert!(
            error.contains(expected),
            "`{name}` rejection should mention {expected:?}, got {error:?}"
        );
        assert!(
            !error.contains("zzzzzzz9"),
            "the rejection must not echo member values: {error:?}"
        );
        assert_vm_is_clean(&mut vm, name);
    }
    assert_eq!(
        entry_keys(&invoke(&mut vm, "healthy").expect("a healthy decode after six rejections")),
        ["ok"],
        "the same VM keeps working after the rejections"
    );
}

#[test]
fn encode_rejections_are_meaningful_and_leave_the_vm_usable() {
    let sources: [(&str, &str, &str); 6] = [
        (
            "duplicate_key",
            r#"[{ key: "a", value_json: "1" }, { key: "a", value_json: "2" }]"#,
            "duplicate entry key 'a'",
        ),
        (
            "missing_json",
            r#"[{ key: "b", value_json: "{" }]"#,
            "entry 0 value_json is not one valid JSON value",
        ),
        (
            "trailing_json",
            r#"[{ key: "c", value_json: "1 2" }]"#,
            "entry 0 value_json is not one valid JSON value",
        ),
        (
            "nested_duplicate",
            r#"[{ key: "d", value_json: "{\"n\": 1, \"n\": 2}" }]"#,
            "duplicate object key",
        ),
        (
            "empty_json",
            r#"[{ key: "e", value_json: "" }]"#,
            "entry 0 value_json is not one valid JSON value",
        ),
        (
            "bad_escape",
            r#"[{ key: "f", value_json: "\"\\q\"" }]"#,
            "entry 0 value_json is not one valid JSON value",
        ),
    ];
    let mut source = String::from("use json;\n");
    for (name, literal, _) in sources {
        source.push_str(&encode_callable(name, literal));
    }
    source.push_str(&encode_callable(
        "healthy",
        r#"[{ key: "ok", value_json: "1" }]"#,
    ));
    let mut vm = vm_for(&source);

    for (name, literal, expected) in sources {
        let error = invoke(&mut vm, name)
            .err()
            .unwrap_or_else(|| panic!("`{name}` must be rejected"));
        assert!(
            error.contains(expected),
            "`{name}` rejection should mention {expected:?}, got {error:?}"
        );
        assert!(
            !error.contains(literal),
            "the rejection must not echo the guest entry array: {error:?}"
        );
        assert_vm_is_clean(&mut vm, name);
    }
    assert_eq!(
        text(&invoke(&mut vm, "healthy").expect("a healthy encode after six rejections")),
        r#"{"ok":1}"#,
        "the same VM keeps working after the rejections"
    );
}

#[test]
fn secret_payload_is_not_echoed_by_a_value_rejection() {
    let source = format!(
        "use json;\n{}",
        encode_callable(
            "secret",
            r#"[{ key: "token", value_json: "{\"S3CRET\": \"S3CRET-value\"" }]"#
        )
    );
    let mut vm = vm_for(&source);
    let error = invoke(&mut vm, "secret").expect_err("a truncated object is not one JSON value");
    assert!(error.contains("value_json"), "{error}");
    assert!(
        !error.contains("S3CRET"),
        "host errors must not echo secret payload text: {error}"
    );
}

#[test]
fn entry_shape_mismatches_are_rejected_at_compile_time() {
    // A guest cannot type an incomplete or mistyped entry, so the named body is
    // the rejection: these are typed compile failures, not silent nulls.
    let catalog = standard_host_catalog();

    let missing = format!(
        "use json;\n{}",
        encode_callable("probe", r#"[{ key: "a" }]"#)
    );
    let error = compile_err(&missing, Arc::clone(&catalog));
    assert!(
        error.contains(ENTRY) && error.contains("key: string"),
        "missing field rejection should name JsonObjectEntry and the partial object: {error}"
    );

    let wrong_type = format!(
        "use json;\n{}",
        encode_callable("probe", r#"[{ key: 1, value_json: "1" }]"#)
    );
    let error = compile_err(&wrong_type, Arc::clone(&catalog));
    assert!(
        error.contains(ENTRY) || error.contains("string"),
        "wrong field type rejection should be typed: {error}"
    );

    let not_an_array = format!(
        "use json;\n{}",
        encode_callable("probe", r#"{ key: "a", value_json: "1" }"#)
    );
    let error = compile_err(&not_an_array, Arc::clone(&catalog));
    assert!(
        error.contains("array") || error.contains(ENTRY),
        "a bare entry is not an entry array: {error}"
    );

    let null_entries = format!("use json;\n{}", encode_callable("probe", "null"));
    let error = compile_err(&null_entries, catalog);
    assert!(
        error.contains("array") || error.contains("null") || error.contains(ENTRY),
        "null is not an entry array: {error}"
    );
}

// -------------------------------------------------------------- same snapshot

#[test]
fn compiled_entry_calls_survive_the_vmbc_wire() {
    // The ordered codec is reached through a typed host import, so its schema
    // travels in the program's host-import table. Encode/decode the program
    // through the VMBC wire and run the decoded program: the guest call still
    // binds to the same import and still keeps order.
    let source = format!(
        "use json;\n{}",
        decode_callable(
            "probe",
            r#"{"zzzzzzz9": 1, "0000000a": 2, "mmmmmmmm": 3, "zzzzzzz1": 4}"#
        )
    );
    let catalog = standard_host_catalog();
    let compiled = compile(&source, Arc::clone(&catalog)).expect("source must compile");

    let schema_names: Vec<String> = compiled
        .program
        .host_import_schemas()
        .iter()
        .filter_map(|schema| schema.as_ref().map(|schema| schema.name.clone()))
        .collect();
    assert_eq!(
        schema_names,
        vec![DECODE.to_string()],
        "the compiled program carries the typed import schema"
    );

    let bytes = vm::encode_program(&compiled.program).expect("the program must encode");
    let program = vm::decode_program(&bytes).expect("the program must decode");
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    registry_from(catalog.as_ref())
        .bind_vm_cached(&mut vm)
        .expect("the standard json module must bind");
    run_to_halt(&mut vm);
    assert_eq!(
        entry_keys(&invoke(&mut vm, "probe").expect("wire-decoded program must call the codec")),
        ["zzzzzzz9", "0000000a", "mmmmmmmm", "zzzzzzz1"]
    );
}

#[test]
fn custom_catalog_exact_binding_runs_the_codec() {
    let mut builder = HostApiBuilder::new();
    let body = HostStructSchema::new(
        ENTRY,
        vec![
            HostStructField::new("key", HostTypeSchema::String),
            HostStructField::new("value_json", HostTypeSchema::String),
        ],
    );
    let entry_type = body.as_type();
    builder.named_struct(body);
    builder.function(HostFunctionSchema::with_return(
        DECODE,
        vec![HostParamSchema::value("text", HostTypeSchema::String)],
        HostTypeSchema::Array(Box::new(entry_type.clone())),
    ));
    builder.function(HostFunctionSchema::with_return(
        ENCODE,
        vec![HostParamSchema::value(
            "entries",
            HostTypeSchema::Array(Box::new(entry_type)),
        )],
        HostTypeSchema::String,
    ));
    let catalog = Arc::new(builder.build().expect("the custom catalog must build"));
    assert_eq!(
        catalog.fingerprint(),
        json_module_catalog().fingerprint(),
        "the custom catalog must be the same snapshot the module publishes"
    );

    let source = format!(
        "use json;\n{}{}",
        decode_callable(
            "decode",
            r#"{"zzzzzzz9": 1, "0000000a": 2, "mmmmmmmm": 3, "zzzzzzz1": 4}"#
        ),
        encode_callable(
            "encode",
            r#"[{ key: "zzzzzzz9", value_json: "1" }, { key: "0000000a", value_json: "2" }]"#
        )
    );
    let compiled = compile(&source, Arc::clone(&catalog)).expect("custom catalog source compiles");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let mut registry = HostFunctionRegistry::restricted();
    register_json_builtin_module_from_catalog(&mut registry, catalog.as_ref())
        .expect("exact binding against the same snapshot");
    registry
        .bind_vm_cached(&mut vm)
        .expect("the exact adapters must bind");
    run_to_halt(&mut vm);

    assert_eq!(
        entry_keys(&invoke(&mut vm, "decode").expect("custom decode")),
        ["zzzzzzz9", "0000000a", "mmmmmmmm", "zzzzzzz1"]
    );
    assert_eq!(
        text(&invoke(&mut vm, "encode").expect("custom encode")),
        r#"{"zzzzzzz9":1,"0000000a":2}"#
    );
}

#[test]
fn unbound_registry_fails_closed_on_the_entry_imports() {
    let catalog = standard_host_catalog();
    let source = format!("use json;\n{}", decode_callable("probe", r#"{"a": 1}"#));
    let compiled = compile(&source, Arc::clone(&catalog)).expect("source must compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let registry = HostFunctionRegistry::empty();
    let error = match registry.bind_vm_cached(&mut vm) {
        Ok(()) => match vm.run() {
            Ok(_) => panic!("an unbound ordered entry import must not run"),
            Err(error) => error.to_string(),
        },
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains(DECODE) || error.contains("json"),
        "the unbound import must be rejected by name: {error}"
    );
}

#[test]
fn plain_json_builtins_still_need_no_registry_and_still_sort_maps() {
    // `json::encode` / `json::decode` keep their static builtin path: an
    // unbound VM runs them, and their map behaviour is unchanged (a map still
    // loses document order, which is exactly why the entry codec exists).
    let source = r#"
use json;
let source = "{\"zzzzzzz9\": 1, \"0000000a\": 2, \"mmmmmmmm\": 3, \"zzzzzzz1\": 4}";
let decoded: map = json::decode::<map>(source);
json::encode(decoded);
"#;
    let compiled = compile_source(source).expect("plain json source must compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    HostFunctionRegistry::empty()
        .bind_vm_cached(&mut vm)
        .expect("an unbound registry still binds");
    assert_eq!(vm.run().expect("plain json must run"), VmStatus::Halted);
    assert_eq!(
        vm.stack().to_vec(),
        vec![Value::string(
            r#"{"0000000a":2,"mmmmmmmm":3,"zzzzzzz1":4,"zzzzzzz9":1}"#
        )],
        "the plain map codec is untouched and still sorts"
    );
}

#[test]
fn ordered_entries_do_not_change_a_plain_json_round_trip() {
    // A guest that keeps using the plain codec must observe exactly the same
    // values as before: the ordered entry surface is additive.
    let source = r#"
use json;
let decoded: map = json::decode::<map>("{\"b\": 2, \"a\": 1}");
let text = json::encode(decoded);
[text, json::decode::<map>(text)];
"#;
    let values = run_standard(source);
    assert_eq!(
        values,
        vec![Value::array(vec![
            Value::string(r#"{"a":1,"b":2}"#),
            json_map(&[("a", 1), ("b", 2)]),
        ])]
    );
}

/// A guest map value with the given integer members, for equality checks.
fn json_map(entries: &[(&str, i64)]) -> Value {
    Value::map(
        entries
            .iter()
            .map(|(key, value)| (Value::string(*key), Value::Int(*value)))
            .collect(),
    )
}

// ------------------------------------------------------------ negative gates

#[test]
fn order_and_value_comparisons_reject_corruption() {
    let source = format!(
        "use json;\n{}{}",
        decode_callable(
            "decoded",
            r#"{"zzzzzzz9": 1, "0000000a": 2, "mmmmmmmm": 3, "zzzzzzz1": 4}"#
        ),
        encode_callable(
            "encoded",
            r#"[
                { key: "zzzzzzz9", value_json: "1" },
                { key: "0000000a", value_json: "2" },
                { key: "mmmmmmmm", value_json: "3" },
                { key: "zzzzzzz1", value_json: "4" }
            ]"#
        )
    );
    let mut vm = vm_for(&source);
    let decoded = entries(&invoke(&mut vm, "decoded").expect("decode"));
    let encoded = text(&invoke(&mut vm, "encoded").expect("encode"));

    let mut swapped = decoded.clone();
    swapped.swap(0, 3);
    assert_ne!(
        swapped, decoded,
        "a reordered entry array must not compare equal"
    );

    let mut retyped = decoded.clone();
    retyped[0].1 = "2".to_string();
    assert_ne!(retyped, decoded, "a changed value must not compare equal");

    let mut resorted = decoded.clone();
    resorted.sort();
    assert_ne!(
        resorted, decoded,
        "a sorted entry array must not compare equal to the document order"
    );

    assert_ne!(
        encoded.replace("\"0000000a\":2,", ""),
        encoded,
        "a dropped member must change the encoded text"
    );
    assert_eq!(
        encoded,
        r#"{"zzzzzzz9":1,"0000000a":2,"mmmmmmmm":3,"zzzzzzz1":4}"#
    );
}
