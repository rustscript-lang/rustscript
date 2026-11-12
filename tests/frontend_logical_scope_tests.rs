#![cfg(feature = "runtime")]

use vm::compile_source;

#[path = "support/legacy_nullable_plugin.rs"]
mod legacy_nullable_plugin;

fn plugin_print_sink(source: &str) -> Vec<String> {
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        legacy_nullable_plugin::legacy_nullable_flavor(),
        legacy_nullable_plugin::legacy_nullable_options(vm::CompileSourceFileOptions::default()),
    )
    .unwrap_or_else(|error| panic!("plugin source should compile: {source}\n{error}"));
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    assert_eq!(
        machine.run().expect("plugin source run"),
        vm::VmStatus::Halted
    );
    lines.lock().expect("sink").clone()
}

#[test]
fn plugin_root_print_keeps_plain_legacy_null_display() {
    assert_eq!(plugin_print_sink("print(());"), ["null".to_string()]);
}

#[test]
fn rss_root_print_unit_keeps_logical_display() {
    let source = "print(());";
    let compiled = compile_source(source).expect("RSS unit print should compile");
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&lines);
    machine.set_runtime_print_sink(move |rendered| {
        recorded.lock().expect("sink").push(rendered);
    });
    assert_eq!(machine.run().expect("RSS unit print"), vm::VmStatus::Halted);
    assert_eq!(lines.lock().expect("sink").as_slice(), ["()".to_string()]);
}

#[test]
fn plugin_root_keeps_legacy_typed_json_decode_route() {
    let source = "use json;\nlet decoded: int? = json::decode::<int?>(\"null\");\ndecoded;\n";
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        legacy_nullable_plugin::legacy_nullable_flavor(),
        legacy_nullable_plugin::legacy_nullable_options(vm::CompileSourceFileOptions::default()),
    )
    .unwrap_or_else(|error| {
        panic!("plugin legacy nullable decode should compile: {source}\n{error}")
    });
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(
        machine.run().expect("plugin legacy decode should run"),
        vm::VmStatus::Halted
    );
    assert_eq!(machine.stack().last(), Some(&vm::Value::Null));
}
