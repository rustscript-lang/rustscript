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
        Ok(CallOutcome::Return(args.to_vec()))
    }
}

impl HostFunction for AddOneFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            _ => return Err(vm::VmError::TypeMismatch("int")),
        };
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
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
            other => panic!("unknown function '{other}'"),
        }
    }
}

fn run_compiled_file(path: &Path) -> Vec<Value> {
    let compiled = compile_source_file(path).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let mut jit_config = vm.jit_config().clone();
    jit_config.enabled = false;
    vm.set_jit_config(jit_config);
    register_functions(&mut vm, &compiled.functions);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    vm.stack().to_vec()
}

fn run_compiled_source(flavor: SourceFlavor, source: &str) -> Vec<Value> {
    let compiled = compile_source_with_flavor(source, flavor).expect("compile should succeed");
    let mut vm = Vm::with_locals(compiled.program, compiled.locals);
    let mut jit_config = vm.jit_config().clone();
    jit_config.enabled = false;
    vm.set_jit_config(jit_config);
    register_functions(&mut vm, &compiled.functions);
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    vm.stack().to_vec()
}

#[test]
fn examples_run() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");

    let stack = run_compiled_file(&root.join("example.rss"));
    assert_eq!(stack, vec![Value::Int(6)]);

    let stack = run_compiled_file(&root.join("example.js"));
    assert_eq!(stack, vec![Value::Int(6)]);

    let stack = run_compiled_file(&root.join("example.lua"));
    assert_eq!(stack, vec![Value::Int(6)]);

    let stack = run_compiled_file(&root.join("example.scm"));
    assert_eq!(stack, vec![Value::Int(6)]);

    // Feature examples for each frontend flavor.
    let stack = run_compiled_file(&root.join("example_complex.rss"));
    assert_eq!(stack, vec![Value::Int(12)]);

    let stack = run_compiled_file(&root.join("example_complex.js"));
    assert_eq!(stack, vec![Value::Int(12)]);

    let stack = run_compiled_file(&root.join("example_complex.lua"));
    assert_eq!(stack, vec![Value::Int(12)]);

    let stack = run_compiled_file(&root.join("example_complex.scm"));
    assert_eq!(stack, vec![Value::Int(12)]);

    // AES fixture should also be consumable as a module from another RSS program.
    let stack = run_compiled_file(&root.join("aes_128_cbc_usage.rss"));
    assert_eq!(
        stack,
        vec![Value::String(
            "7649abac8119b246cee98e9b12e9197d".to_string()
        )]
    );
}

#[test]
fn nullable_chain_maps_in_all_frontends() {
    let rss_source = r#"
let a = { b: { c: 7 } };
a?.b?.c;
let m = { b: {} };
m?.b?.c;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, rss_source),
        vec![Value::Int(7), Value::Null]
    );

    let js_source = r#"
const a = { b: { c: 7 } };
a?.b?.c;
const m = { b: {} };
m?.b?.c;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::JavaScript, js_source),
        vec![Value::Int(7), Value::Null]
    );

    let lua_source = r#"
local a = { b = { c = 7 } }
a?.b?.c
local m = { b = {} }
m?.b?.c
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::Lua, lua_source),
        vec![Value::Int(7), Value::Null]
    );

    let scheme_source = r#"
(define a (hash (b (hash (c 7)))))
a?.b?.c
(define m (hash (b (hash))))
m?.b?.c
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::Scheme, scheme_source),
        vec![Value::Int(7), Value::Null]
    );
}

#[test]
fn nullable_chain_handles_array_and_string_indexes() {
    let rss_source = r#"
let arr = [10, 20];
arr?.[1];
arr?.[2];
arr?.["x"];
let text = "abc";
text?.[1];
text?.[5];
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, rss_source),
        vec![
            Value::Int(20),
            Value::Null,
            Value::Null,
            Value::String("b".to_string()),
            Value::Null,
        ]
    );

    let js_source = r#"
const arr = [10, 20];
arr?.[1];
arr?.[2];
arr?.["x"];
const text = "abc";
text?.[1];
text?.[5];
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::JavaScript, js_source),
        vec![
            Value::Int(20),
            Value::Null,
            Value::Null,
            Value::String("b".to_string()),
            Value::Null,
        ]
    );

    let lua_source = r#"
local arr = {10, 20}
arr?.[1]
arr?.[2]
arr?.["x"]
local text = "abc"
text?.[1]
text?.[5]
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::Lua, lua_source),
        vec![
            Value::Int(20),
            Value::Null,
            Value::Null,
            Value::String("b".to_string()),
            Value::Null,
        ]
    );
}

#[test]
fn nullable_map_lookup_does_not_mutate_source_map() {
    let source = r#"
let m = { present: 1 };
m?.missing;
m.length;
m.present;
"#;
    assert_eq!(
        run_compiled_source(SourceFlavor::RustScript, source),
        vec![Value::Null, Value::Int(1), Value::Int(1)]
    );
}
