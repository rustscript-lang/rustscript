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
