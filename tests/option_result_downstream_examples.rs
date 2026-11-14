#![cfg(feature = "runtime")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use vm::{CompileSourceFileOptions, Value, Vm, VmStatus, compile_source_file_with_options};

mod common;

const STDLIB_MODULES: &[&str] = &[
    "bytes",
    "collections",
    "io",
    "iter",
    "lrucache",
    "math",
    "parse",
    "path",
    "strings",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn example_path(name: &str) -> PathBuf {
    workspace_root()
        .join("pd-vm-wasm")
        .join("examples")
        .join(name)
}

fn stdlib_options() -> CompileSourceFileOptions {
    let mut options = CompileSourceFileOptions::new();
    for module in STDLIB_MODULES {
        let spec = format!("stdlib/rss/{module}.rss");
        let source = std::fs::read_to_string(workspace_root().join(&spec)).unwrap_or_else(|err| {
            panic!(
                "bundled stdlib module '{spec}' should be readable from the Core checkout: {err}"
            )
        });
        options.set_module_override_source(spec, source);
    }
    options
}

struct ExampleRun {
    lines: Vec<String>,
    stack: Vec<Value>,
}

/// Compiles one canonical playground example and executes it with the print
/// sink captured, so the assertions read the example's named output lines.
fn run_example(name: &str) -> ExampleRun {
    let path = example_path(name);
    let compiled = compile_source_file_with_options(&path, stdlib_options())
        .unwrap_or_else(|err| panic!("example '{name}' should compile: {err}"));

    let output = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink_output = Arc::clone(&output);
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_runtime_print_sink(move |rendered| {
        sink_output
            .lock()
            .expect("print sink should lock")
            .push(rendered);
    });
    for decl in &compiled.functions {
        if decl.name == "runtime::sleep" {
            vm.bind_function("runtime::sleep", common::make_runtime_sleep());
        }
    }

    loop {
        match vm.run().expect("example should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("example host operation should complete"),
        }
    }

    let rendered = output.lock().expect("print sink should lock").clone();
    let lines = rendered
        .iter()
        .flat_map(|entry| entry.split('\n'))
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect();

    ExampleRun {
        lines,
        stack: vm.stack().to_vec(),
    }
}

fn named_output(lines: &[String]) -> BTreeMap<&str, &str> {
    lines
        .iter()
        .map(|line| {
            line.split_once('=')
                .unwrap_or_else(|| panic!("example output line '{line}' should be named"))
        })
        .collect()
}

/// Every named line the canonical Unit/Option/Result example must print.
const OPTION_RESULT_EXPECTED: &[(&str, &str)] = &[
    ("unit_literal", "()"),
    ("unit_fn_returns_unit", "()"),
    ("unit_explicit_annotation", "()"),
    ("unit_roundtrip", "()"),
    ("option_some_zero", "0"),
    ("option_none", "-1"),
    ("option_some_false", "false"),
    ("option_some_empty_is_empty", "true"),
    ("option_match_none", "-2"),
    ("option_map", "1"),
    ("option_and_then", "2"),
    ("option_or_else", "5"),
    ("option_unwrap_or_else", "4"),
    ("option_relay_some", "2"),
    ("option_relay_none", "-1"),
    ("result_is_ok", "true"),
    ("result_is_err", "true"),
    ("result_match_ok", "21"),
    ("result_match_err", "bad input"),
    ("result_chain_ok", "9"),
    ("result_chain_err", "7"),
    ("result_unwrap_or_ok", "11"),
    ("result_unwrap_or_err", "30"),
    ("result_unwrap_or_else_err", "42"),
    ("result_forward_ok", "0"),
    ("result_forward_err", "9"),
    ("result_map_err_then_try", "7"),
    ("result_ok_to_option", "21"),
    ("result_err_to_option", "bad input"),
    ("result_ok_err_is_none", "true"),
    ("project_field", "crab"),
    ("project_absent_key", "missing"),
    ("project_present_key", "one"),
    ("project_array_index", "20"),
    ("project_string_index", "b"),
    ("project_missing_string_index", "missing"),
];

#[test]
fn option_result_example_prints_every_named_contract_line() {
    let run = run_example("rss-option-result-example.rss");
    assert_eq!(
        run.lines.len(),
        OPTION_RESULT_EXPECTED.len(),
        "the canonical example must print exactly the named contract lines, got {:?}",
        run.lines
    );

    let actual = named_output(&run.lines);
    for (name, expected) in OPTION_RESULT_EXPECTED {
        let observed = actual
            .get(name)
            .unwrap_or_else(|| panic!("missing named output line '{name}' in {:?}", run.lines));
        assert_eq!(
            observed, expected,
            "named output line '{name}' should report '{expected}'"
        );
    }

    assert_eq!(
        run.stack,
        vec![
            Value::string("unit_literal=()\n"),
            Value::string("unit_fn_returns_unit=()\n"),
            Value::string("unit_explicit_annotation=()\n"),
            Value::string("unit_roundtrip=()\n"),
            Value::string("option_some_zero=0\n"),
            Value::string("option_none=-1\n"),
            Value::string("option_some_false=false\n"),
            Value::string("option_some_empty_is_empty=true\n"),
            Value::string("option_match_none=-2\n"),
            Value::string("option_map=1\n"),
            Value::string("option_and_then=2\n"),
            Value::string("option_or_else=5\n"),
            Value::string("option_unwrap_or_else=4\n"),
            Value::string("option_relay_some=2\n"),
            Value::string("option_relay_none=-1\n"),
            Value::string("result_is_ok=true\n"),
            Value::string("result_is_err=true\n"),
            Value::string("result_match_ok=21\n"),
            Value::string("result_match_err=bad input\n"),
            Value::string("result_chain_ok=9\n"),
            Value::string("result_chain_err=7\n"),
            Value::string("result_unwrap_or_ok=11\n"),
            Value::string("result_unwrap_or_err=30\n"),
            Value::string("result_unwrap_or_else_err=42\n"),
            Value::string("result_forward_ok=0\n"),
            Value::string("result_forward_err=9\n"),
            Value::string("result_map_err_then_try=7\n"),
            Value::string("result_ok_to_option=21\n"),
            Value::string("result_err_to_option=bad input\n"),
            Value::string("result_ok_err_is_none=true\n"),
            Value::string("project_field=crab\n"),
            Value::string("project_absent_key=missing\n"),
            Value::string("project_present_key=one\n"),
            Value::string("project_array_index=20\n"),
            Value::string("project_string_index=b\n"),
            Value::string("project_missing_string_index=missing\n"),
        ],
        "the canonical example must keep its print order"
    );
}

#[test]
fn lru_example_reports_option_shaped_entry_output() {
    let run = run_example("rss-lrucache-example.rss");
    assert_eq!(
        run.lines,
        vec![
            "{\"newest_key\":\"build-ops\",\"oldest_key\":\"world-news\",\"order\":[\"world-news\",\"breaking-alerts\",\"build-ops\"],\"total_visible_articles\":18,\"visible_topics\":[\"world news\",\"breaking alerts\",\"build ops\"]}".to_string()
        ],
        "the LRU example must keep its report shape and values"
    );
    assert_eq!(
        run.stack,
        vec![Value::string(
            "{\"newest_key\":\"build-ops\",\"oldest_key\":\"world-news\",\"order\":[\"world-news\",\"breaking-alerts\",\"build-ops\"],\"total_visible_articles\":18,\"visible_topics\":[\"world news\",\"breaking alerts\",\"build ops\"]}"
        )],
        "the LRU example must return the encoded report"
    );
}

#[test]
fn lru_example_has_no_legacy_found_wrapper() {
    let source = std::fs::read_to_string(example_path("rss-lrucache-example.rss"))
        .expect("LRU example should be readable");
    for legacy in ["LruPeekResult", "LruEntryReport", "found:"] {
        assert!(
            !source.contains(legacy),
            "the LRU example must not keep the legacy '{legacy}' shape"
        );
    }
    for required in [
        "pub fn peek<K, V>(cache: LruCacheState<K, V>, key: K) -> Option<V>",
        "pub fn oldest<K, V>(cache: LruCacheState<K, V>) -> Option<LruEntryRow<K, V>>",
        "pub fn newest<K, V>(cache: LruCacheState<K, V>) -> Option<LruEntryRow<K, V>>",
    ] {
        assert!(
            source.contains(required),
            "the LRU example must expose '{required}'"
        );
    }
}

#[test]
fn complex_example_treats_runtime_sleep_as_a_unit_statement() {
    let source = std::fs::read_to_string(example_path("rss-complex-example.rss"))
        .expect("complex example should be readable");
    assert!(
        source.contains("\nruntime::sleep(100);\n"),
        "runtime::sleep must stay an unbound Unit statement"
    );
    assert!(
        !source.contains("= runtime::sleep"),
        "no local may depend on the runtime::sleep return value"
    );

    let run = run_example("rss-complex-example.rss");
    assert_eq!(
        run.lines,
        vec!["closure_value is  12".to_string()],
        "the complex example must keep its printed output"
    );
    assert!(
        run.stack.contains(&Value::Null),
        "the Unit sleep result must reach the stack as the Null carrier, got {:?}",
        run.stack
    );
}
