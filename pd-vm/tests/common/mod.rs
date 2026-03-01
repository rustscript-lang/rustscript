#![allow(dead_code, unused_imports)]

pub use vm::{
    Assembler, BytecodeBuilder, CallOutcome, CompileSourceFileOptions, Compiler, Expr,
    HostFunction, HostFunctionRegistry, Program, SourceFlavor, Stmt, Value, Vm, VmStatus,
    assemble, compile_source, compile_source_file, compile_source_file_with_options,
    compile_source_with_flavor,
};

pub struct YieldOnce {
    pub yielded: bool,
}

impl HostFunction for YieldOnce {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        if !self.yielded {
            self.yielded = true;
            Ok(CallOutcome::Yield)
        } else {
            Ok(CallOutcome::Return(vec![Value::Int(42)]))
        }
    }
}

pub struct AddOne;
pub struct EchoString;
pub struct PrintBuiltin;

impl HostFunction for AddOne {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        let value = match args.first() {
            Some(Value::Int(value)) => *value,
            _ => 0,
        };
        Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
    }
}

impl HostFunction for EchoString {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        let value = match args.first() {
            Some(Value::String(value)) => value.clone(),
            _ => return Err(vm::VmError::TypeMismatch("string")),
        };
        Ok(CallOutcome::Return(vec![Value::String(value)]))
    }
}

impl HostFunction for PrintBuiltin {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(args.to_vec()))
    }
}

pub fn static_add_one(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let value = match args.first() {
        Some(Value::Int(value)) => *value,
        _ => 0,
    };
    Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
}
