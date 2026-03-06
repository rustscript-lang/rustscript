use std::sync::{Arc, Mutex};

use vm::{
    CallOutcome, FunctionDecl, HostFunction, PrintHostFunction, PrintlnHostFunction, SourceFlavor,
    SourcePathError, Value, Vm, VmError, VmStatus, compile_source_with_flavor_and_options,
    format_value, render_vm_error,
};

use crate::analyzer::{LintDiagnostic, lint_source_with_flavor};
use crate::stdlib::embedded_stdlib_compile_options;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaygroundHostFunctionSpec {
    pub name: &'static str,
    pub arity: usize,
    pub docs: &'static str,
}

const HOST_FUNCTION_SPECS: &[PlaygroundHostFunctionSpec] = &[
    PlaygroundHostFunctionSpec {
        name: "print",
        arity: 1,
        docs: "Writes a value to playground print output.",
    },
    PlaygroundHostFunctionSpec {
        name: "add_one",
        arity: 1,
        docs: "Returns input integer + 1.",
    },
    PlaygroundHostFunctionSpec {
        name: "echo",
        arity: 1,
        docs: "Returns the first argument unchanged.",
    },
];

pub(crate) fn host_function_specs() -> &'static [PlaygroundHostFunctionSpec] {
    HOST_FUNCTION_SPECS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub diagnostics: Vec<LintDiagnostic>,
    pub output: Vec<String>,
    pub stack: Vec<String>,
    pub error: Option<String>,
}

impl RunReport {
    pub fn success(output: Vec<String>, stack: Vec<String>) -> Self {
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: None,
        }
    }

    pub fn source_error(source: &str, flavor: SourceFlavor, err: SourcePathError) -> Self {
        let diagnostics = lint_source_with_flavor(source, flavor).diagnostics;
        Self {
            diagnostics,
            output: Vec::new(),
            stack: Vec::new(),
            error: Some(err.to_string()),
        }
    }

    pub fn runtime_error(message: String, output: Vec<String>, stack: Vec<String>) -> Self {
        Self {
            diagnostics: Vec::new(),
            output,
            stack,
            error: Some(message),
        }
    }
}

pub fn run_source_with_flavor(source: &str, flavor: SourceFlavor) -> RunReport {
    let compiled = match compile_source_with_flavor_and_options(
        source,
        flavor,
        embedded_stdlib_compile_options(),
    ) {
        Ok(compiled) => compiled,
        Err(err) => return RunReport::source_error(source, flavor, err),
    };

    let output_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    if let Err(err) = register_functions(&mut vm, &compiled.functions, &output_lines) {
        return RunReport::runtime_error(err, Vec::new(), Vec::new());
    }

    loop {
        let status = match vm.run() {
            Ok(status) => status,
            Err(err) => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::runtime_error(render_vm_error(&vm, &err), output, stack);
            }
        };
        match status {
            VmStatus::Halted => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::success(output, stack);
            }
            VmStatus::Yielded => {}
            VmStatus::Waiting(op_id) => {
                let output = drain_output(&output_lines);
                let stack = vm.stack().iter().map(format_value).collect::<Vec<_>>();
                return RunReport::runtime_error(
                    format!(
                        "vm is waiting on host op {op_id}; asynchronous host ops are unavailable in the wasm playground runtime"
                    ),
                    output,
                    stack,
                );
            }
        }
    }
}

fn drain_output(lines: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    match lines.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => Vec::new(),
    }
}

fn register_functions(
    vm: &mut Vm,
    functions: &[FunctionDecl],
    print_output: &Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    for decl in functions {
        register_named_function(vm, &decl.name, print_output)?;
    }
    Ok(())
}

fn register_named_function(
    vm: &mut Vm,
    name: &str,
    print_output: &Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    match name {
        "print" => {
            let lines = Arc::clone(print_output);
            vm.bind_function(
                "print",
                Box::new(PrintHostFunction::new(move |rendered| {
                    push_output_line(&lines, rendered);
                })),
            );
        }
        "println" => {
            let lines = Arc::clone(print_output);
            vm.bind_function(
                "println",
                Box::new(PrintlnHostFunction::new(move |rendered| {
                    push_output_line(&lines, rendered);
                })),
            );
        }
        "add_one" => vm.bind_function("add_one", Box::new(AddOneFunction)),
        "echo" => vm.bind_function("echo", Box::new(EchoFunction)),
        other => {
            return Err(format!("no host binding for function '{other}'"));
        }
    }
    Ok(())
}

fn push_output_line(lines: &Arc<Mutex<Vec<String>>>, rendered: String) {
    let normalized = rendered.trim_end_matches('\n').to_string();
    if let Ok(mut guard) = lines.lock() {
        guard.push(normalized);
    }
}

struct AddOneFunction;

impl HostFunction for AddOneFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            _ => return Err(VmError::TypeMismatch("int")),
        };
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
    }
}

struct EchoFunction;

impl HostFunction for EchoFunction {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
        let value = args.first().cloned().ok_or(VmError::StackUnderflow)?;
        Ok(CallOutcome::Return(vec![value]))
    }
}
