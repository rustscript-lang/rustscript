#![cfg(feature = "runtime")]
use std::path::Path;

use vm::{
    CallOutcome, FunctionDecl, HostFunction, SourceFlavor, Value, Vm, VmStatus,
    compile_source_file, compile_source_with_flavor,
};

struct PrintFunction;
struct AddOneFunction;

impl HostFunction for PrintFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(args.to_vec().into()))
    }
}

impl HostFunction for AddOneFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            _ => return Err(vm::VmError::TypeMismatch("int")),
        };
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)].into()))
    }
}

fn register_functions(vm: &mut Vm, functions: &[FunctionDecl]) {
    for decl in functions {
        match decl.name.as_str() {
            "print" => {
                vm.bind_function("print", Box::new(PrintFunction));
            }
            "add_one" => {
                vm.bind_function("add_one", Box::new(AddOneFunction));
            }
            "runtime::sleep" | "runtime::exit" => {}
            other => panic!("unknown function '{other}'"),
        }
    }
}

fn run_vm_until_halted(vm: &mut Vm) {
    loop {
        match vm.run().expect("vm should run") {
            VmStatus::Halted => break,
            VmStatus::Yielded => continue,
            VmStatus::Waiting(_op_id) => vm
                .wait_for_host_op_blocking()
                .expect("vm should complete host operation"),
        }
    }
}

fn run_compiled_file(path: &Path) -> Vec<Value> {
    let compiled = compile_source_file(path).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let mut jit_config = *vm.jit_config();
    jit_config.enabled = false;
    vm.set_jit_config(jit_config);
    register_functions(&mut vm, &compiled.functions);
    run_vm_until_halted(&mut vm);
    vm.stack().to_vec()
}

fn run_compiled_source(flavor: SourceFlavor, source: &str) -> Vec<Value> {
    let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
    let mut vm = Vm::new(compiled.program);
    let mut jit_config = *vm.jit_config();
    jit_config.enabled = false;
    vm.set_jit_config(jit_config);
    register_functions(&mut vm, &compiled.functions);
    run_vm_until_halted(&mut vm);
    vm.stack().to_vec()
}

#[test]
fn ifft_math_example_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let stack = run_compiled_file(&root.join("ifft_math.rss"));
    assert_eq!(
        stack,
        vec![
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(3.0),
            Value::Float(4.0),
        ]
    );
}

#[test]
fn rustscript_optional_chain_uses_declared_schema_and_handling_runs() {
    let rss_source = r#"
struct Inner { c: int }
struct Outer { b: Inner }

let present: Outer = { b: { c: 7 } };
let missing: Outer? = null;

present?.b?.c.unwrap_or(0);
missing?.b?.c.unwrap_or(0);
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, rss_source),
        vec![Value::Int(7), Value::Int(0)]
    );
}

#[test]
fn rustscript_optional_chain_handles_declared_array_and_string_indexes() {
    let rss_source = r#"
struct Data {
    arr: [int],
    text: string,
}

let data: Data = { arr: [10, 20], text: "abc" };
data?.arr?.[1].unwrap_or(0);
data?.arr?.[2].unwrap_or(0);
data?.text?.[1].unwrap_or("");
data?.text?.[5].unwrap_or("");
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, rss_source),
        vec![
            Value::Int(20),
            Value::Int(0),
            Value::string("b"),
            Value::string(""),
        ]
    );
}

#[test]
fn rustscript_borrowed_map_for_in_iterates_entries_without_keys_array() {
    let source = r#"
let values: map<int> = {a: 1, b: 2, c: 3};
let mut sum: int = 0;
let mut names: string = "";
for (key: string, value: int) in &values {
    names = names + key;
    sum = sum + value;
}
[sum, names.length];
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, source),
        vec![Value::array(vec![Value::Int(6), Value::Int(3)])]
    );
}

#[test]
fn rustscript_borrowed_map_for_in_rejects_mutation() {
    let source = r#"
let mut values: map<int> = {a: 1};
for (key: string, value: int) in &values {
    values["b"] = value;
}
values;
"#;
    let err = match compile_source_with_flavor(source, SourceFlavor::RustScript) {
        Ok(_) => panic!("borrowed map mutation should fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("borrowed by a map iterator"));
}

#[test]
fn rustscript_borrowed_map_bindings_do_not_alias_source_or_each_other() {
    // Source alias through the key binding.
    let key_alias = r#"
let values: map<int> = {a: 1};
for (values: string, value: int) in &values {
    value;
}
"#;
    let err = match compile_source_with_flavor(key_alias, SourceFlavor::RustScript) {
        Ok(_) => panic!("map iterator key should not shadow the source"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("shadows the borrowed source"));

    // Source alias through the value binding.
    let value_alias = r#"
let values: map<int> = {a: 1};
for (key: string, values: int) in &values {
    key;
}
"#;
    let err = match compile_source_with_flavor(value_alias, SourceFlavor::RustScript) {
        Ok(_) => panic!("map iterator value should not shadow the source"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("shadows the borrowed source"));

    // Duplicate binding names.
    let duplicate = r#"
let values: map<int> = {a: 1};
for (item: string, item: int) in &values {
    item;
}
"#;
    let err = match compile_source_with_flavor(duplicate, SourceFlavor::RustScript) {
        Ok(_) => panic!("duplicate map iterator bindings should fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("duplicate map iterator binding"));
}

#[test]
fn rustscript_borrowed_map_bindings_restore_outer_locals() {
    let source = r#"
let mut key: int = 7;
let values: map<int> = {a: 1};
for (key: string, value: int) in &values {
    let observed: int = value;
}
key = 9;
key;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, source),
        vec![Value::Int(9)]
    );
}

#[test]
fn rustscript_borrowed_map_for_in_rejects_source_rebinding() {
    let source = r#"
let values: map<int> = {a: 1};
for (key: string, value: int) in &values {
    let values: map<int> = {};
    key;
}
"#;
    let err = match compile_source_with_flavor(source, SourceFlavor::RustScript) {
        Ok(_) => panic!("borrowed map source rebinding should fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("borrowed by a map iterator"));
}

#[test]
fn rustscript_borrowed_map_for_in_validates_binding_schemas() {
    let bad_key = r#"
let values: map<int> = {a: 1};
for (key: int, value: int) in &values {
    value;
}
"#;
    let err = match compile_source_with_flavor(bad_key, SourceFlavor::RustScript) {
        Ok(_) => panic!("map iterator keys must be strings"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("map iterator key binding"));

    let bad_value = r#"
let values: map<int> = {a: 1};
for (key: string, value: string) in &values {
    key;
}
"#;
    let err = match compile_source_with_flavor(bad_value, SourceFlavor::RustScript) {
        Ok(_) => panic!("map iterator value schema must match the map"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("map iterator value binding"));
}

#[test]
fn rustscript_borrowed_map_iterator_ids_survive_local_compaction() {
    let source = r#"
let a0: int = 0;
let a1: int = 1;
let a2: int = 2;
let a3: int = 3;
let a4: int = 4;
let a5: int = 5;
let a6: int = 6;
let a7: int = 7;
let a8: int = 8;
let values: map<int> = {a: 1, b: 2};
let mut total: int = 0;
for (key: string, value: int) in &values {
    total += value;
}
total;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, source),
        vec![Value::Int(3)]
    );

    let nested = r#"
let a0: int = 0;
let a1: int = 1;
let a2: int = 2;
let a3: int = 3;
let a4: int = 4;
let a5: int = 5;
let a6: int = 6;
let a7: int = 7;
let a8: int = 8;
let outer: map<int> = {a: 1, b: 2};
let inner: map<int> = {x: 10, y: 20};
let mut total: int = 0;
for (outer_key: string, outer_value: int) in &outer {
    for (inner_key: string, inner_value: int) in &inner {
        total += outer_value + inner_value;
    }
}
total;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, nested),
        vec![Value::Int(66)]
    );
}

#[test]
fn rustscript_function_map_iteration_enforces_borrow_and_schema() {
    let rebind = r#"
fn probe() -> int {
    let values: map<int> = {a: 1};
    for (key: string, value: int) in &values {
        let values: map<int> = {};
    }
    values.length
}
probe();
"#;
    let err = match compile_source_with_flavor(rebind, SourceFlavor::RustScript) {
        Ok(_) => panic!("function-local borrowed source rebinding should fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("borrowed by a map iterator"));

    let mismatch = r#"
fn probe() -> int {
    let values: map<int> = {a: 1};
    for (key: string, value: string) in &values {}
    values.length
}
probe();
"#;
    let err = match compile_source_with_flavor(mismatch, SourceFlavor::RustScript) {
        Ok(_) => panic!("function-local iterator schema mismatch should fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("map iterator value binding"));
}

#[test]
fn rustscript_explicit_iterator_value_schema_requires_typed_source() {
    let source = r#"
let values = {a: 1};
for (key: string, value: int) in &values {}
"#;
    let err = match compile_source_with_flavor(source, SourceFlavor::RustScript) {
        Ok(_) => panic!("explicit iterator schema over an untyped map should fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("source map has no declared map<T> schema")
    );
}

#[test]
fn rustscript_borrowed_map_iteration_rejects_non_string_keys() {
    let source = r#"
let values: map<int> = {1: 2};
for (key: string, value: int) in &values {}
"#;
    let compiled = compile_source_with_flavor(source, SourceFlavor::RustScript)
        .expect("typed source should compile");
    let mut vm = Vm::new(compiled.program);
    let err = vm
        .run()
        .expect_err("non-string map keys should fail at iterator init");
    assert!(err.to_string().contains("requires string keys"));
}

#[test]
fn rustscript_untyped_rebinding_does_not_reuse_stale_map_schema() {
    let source = r#"
let values: map<int> = {a: 1};
let values = {a: "x"};
for (key: string, value: int) in &values { value; }
"#;
    let err = match compile_source_with_flavor(source, SourceFlavor::RustScript) {
        Ok(_) => panic!("untyped rebinding must clear stale map schema"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("map<T> schema"));
}

#[test]
fn rustscript_function_parameter_map_type_is_visible_to_iterators() {
    let source = r#"
fn sum(values: map<int>) -> int {
    let mut total: int = 0;
    for (key: string, value: int) in &values { total += value; }
    total
}
sum({a: 1, b: 2});
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, source),
        vec![Value::Int(3)]
    );
}

#[test]
fn rustscript_borrowed_map_iterator_propagates_nested_binding_schema() {
    let source = r#"
let groups: map<map<int>> = {first: {a: 1, b: 2}};
let mut total: int = 0;
for (group_key: string, group: map<int>) in &groups {
    for (item_key: string, item: int) in &group { total += item; }
}
total;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, source),
        vec![Value::Int(3)]
    );
}
