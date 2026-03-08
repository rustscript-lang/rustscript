#![allow(unused_imports)]

pub use vm::{
    Assembler, BytecodeBuilder, CallOutcome, CompileSourceFileOptions, Compiler, Expr,
    HostFunction, HostFunctionRegistry, Program, SourceFlavor, Stmt, Store, Value, Vm, VmStatus,
    assemble, compile_source, compile_source_file, compile_source_file_with_options,
    compile_source_with_flavor,
};

pub struct RuntimeCase<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub flavor: SourceFlavor,
    pub expected_stack: Vec<Value>,
    pub expected_locals: Option<usize>,
}

pub type HostFactory = fn() -> Box<dyn HostFunction>;

pub struct HostBindingCase<'a> {
    pub name: &'a str,
    pub factory: HostFactory,
}

pub struct ParseErrorCase<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub flavor: SourceFlavor,
    pub expected_contains_all: &'a [&'a str],
}

pub fn run_runtime_case(case: &RuntimeCase<'_>) {
    run_runtime_case_with_bindings(case, &[]);
}

pub fn run_runtime_case_with_bindings(case: &RuntimeCase<'_>, bindings: &[HostBindingCase<'_>]) {
    let compiled =
        compile_source_with_flavor(case.source, case.flavor).expect("compile should succeed");
    if let Some(expected_locals) = case.expected_locals {
        assert_eq!(
            compiled.locals, expected_locals,
            "unexpected local count for case '{}'",
            case.name
        );
    }
    let mut vm = Vm::new(compiled.program);
    for binding in bindings {
        vm.bind_function(binding.name, (binding.factory)());
    }
    let status = vm.run().expect("vm should run");
    assert_eq!(
        status,
        VmStatus::Halted,
        "vm did not halt for case '{}'",
        case.name
    );
    assert_eq!(
        vm.stack(),
        case.expected_stack.as_slice(),
        "unexpected stack for case '{}'",
        case.name
    );
}

pub fn run_runtime_cases(cases: &[RuntimeCase<'_>]) {
    for case in cases {
        run_runtime_case(case);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompileErrorKind {
    Assembler,
    CallArityOverflow,
    ClosureUsedAsValue,
    CallableUsedAsValue,
    NonCallableLocal,
    LocalSlotOverflow,
    CallableArityMismatch,
    BreakOutsideLoop,
    ContinueOutsideLoop,
    InlineFunctionRecursion,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SourceErrorKind {
    Parse,
    CompileAny,
    Compile(CompileErrorKind),
}

pub struct SourceErrorCase<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub flavor: SourceFlavor,
    pub expected_kind: SourceErrorKind,
    pub expected_contains_all: &'a [&'a str],
}

fn compile_error_kind(err: &vm::CompileError) -> CompileErrorKind {
    match err {
        vm::CompileError::Assembler(_) => CompileErrorKind::Assembler,
        vm::CompileError::CallArityOverflow => CompileErrorKind::CallArityOverflow,
        vm::CompileError::ClosureUsedAsValue => CompileErrorKind::ClosureUsedAsValue,
        vm::CompileError::CallableUsedAsValue => CompileErrorKind::CallableUsedAsValue,
        vm::CompileError::NonCallableLocal(_) => CompileErrorKind::NonCallableLocal,
        vm::CompileError::LocalSlotOverflow(_) => CompileErrorKind::LocalSlotOverflow,
        vm::CompileError::CallableArityMismatch { .. } => CompileErrorKind::CallableArityMismatch,
        vm::CompileError::BreakOutsideLoop => CompileErrorKind::BreakOutsideLoop,
        vm::CompileError::ContinueOutsideLoop => CompileErrorKind::ContinueOutsideLoop,
        vm::CompileError::InlineFunctionRecursion(_) => CompileErrorKind::InlineFunctionRecursion,
    }
}

pub fn expect_source_error_case(case: &SourceErrorCase<'_>) {
    let err = match compile_source_with_flavor(case.source, case.flavor) {
        Ok(_) => panic!("case '{}' should fail to compile", case.name),
        Err(err) => err,
    };

    match case.expected_kind {
        SourceErrorKind::Parse => match err {
            vm::SourceError::Parse(parse) => {
                for needle in case.expected_contains_all {
                    assert!(
                        parse.message.contains(needle),
                        "case '{}': parse error '{}' did not contain '{}'",
                        case.name,
                        parse.message,
                        needle
                    );
                }
            }
            other => panic!("case '{}': expected parse error, got {other}", case.name),
        },
        SourceErrorKind::CompileAny => match err {
            vm::SourceError::Compile(compile_err) => {
                let debug = format!("{compile_err:?}");
                for needle in case.expected_contains_all {
                    assert!(
                        debug.contains(needle),
                        "case '{}': compile error '{debug}' did not contain '{}'",
                        case.name,
                        needle
                    );
                }
            }
            other => panic!("case '{}': expected compile error, got {other}", case.name),
        },
        SourceErrorKind::Compile(expected_kind) => match err {
            vm::SourceError::Compile(compile_err) => {
                let actual_kind = compile_error_kind(&compile_err);
                assert_eq!(
                    actual_kind, expected_kind,
                    "case '{}': expected compile kind {:?}, got {:?}",
                    case.name, expected_kind, actual_kind
                );
                let debug = format!("{compile_err:?}");
                for needle in case.expected_contains_all {
                    assert!(
                        debug.contains(needle),
                        "case '{}': compile error '{debug}' did not contain '{}'",
                        case.name,
                        needle
                    );
                }
            }
            other => panic!("case '{}': expected compile error, got {other}", case.name),
        },
    }
}

pub fn expect_parse_error_case(case: &ParseErrorCase<'_>) {
    let source_case = SourceErrorCase {
        name: case.name,
        source: case.source,
        flavor: case.flavor,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: case.expected_contains_all,
    };
    expect_source_error_case(&source_case);
}

pub fn expect_parse_error_contains_any_with_flavor(
    source: &str,
    flavor: SourceFlavor,
    expected_any: &[&str],
) {
    expect_parse_error_contains_any_case("unnamed", source, flavor, expected_any);
}

pub fn expect_parse_error_contains_any_case(
    case_name: &str,
    source: &str,
    flavor: SourceFlavor,
    expected_any: &[&str],
) {
    let err = match compile_source_with_flavor(source, flavor) {
        Ok(_) => panic!("case '{case_name}' should fail to compile"),
        Err(err) => err,
    };
    match err {
        vm::SourceError::Parse(parse) => {
            assert!(
                expected_any
                    .iter()
                    .any(|needle| parse.message.contains(needle)),
                "case '{}': parse error '{}' did not contain any expected substring: {:?}",
                case_name,
                parse.message,
                expected_any
            );
        }
        other => panic!("case '{case_name}': unexpected error: {other}"),
    }
}

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
pub struct AlwaysAllow;
pub struct RuntimeSleep;

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

impl HostFunction for AlwaysAllow {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![Value::Bool(true)]))
    }
}

impl HostFunction for RuntimeSleep {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        Ok(CallOutcome::Return(vec![Value::Bool(true)]))
    }
}

pub fn static_add_one(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, vm::VmError> {
    let value = match args.first() {
        Some(Value::Int(value)) => *value,
        _ => 0,
    };
    Ok(CallOutcome::Return(vec![Value::Int(value + 1)]))
}

pub fn make_add_one() -> Box<dyn HostFunction> {
    Box::new(AddOne)
}

pub fn make_echo_string() -> Box<dyn HostFunction> {
    Box::new(EchoString)
}

pub fn make_print_builtin() -> Box<dyn HostFunction> {
    Box::new(PrintBuiltin)
}

pub fn make_always_allow() -> Box<dyn HostFunction> {
    Box::new(AlwaysAllow)
}

pub fn make_runtime_sleep() -> Box<dyn HostFunction> {
    Box::new(RuntimeSleep)
}

#[test]
fn common_helpers_are_referenced() {
    let _runtime_case = RuntimeCase {
        name: "smoke",
        source: "1;",
        flavor: SourceFlavor::RustScript,
        expected_stack: Vec::new(),
        expected_locals: None,
    };
    let _parse_case = ParseErrorCase {
        name: "smoke",
        source: "let =",
        flavor: SourceFlavor::RustScript,
        expected_contains_all: &[],
    };
    let _source_case = SourceErrorCase {
        name: "smoke",
        source: "let =",
        flavor: SourceFlavor::RustScript,
        expected_kind: SourceErrorKind::Parse,
        expected_contains_all: &[],
    };
    let _host_binding = HostBindingCase {
        name: "x",
        factory: make_add_one,
    };
    let _host_factory: HostFactory = make_add_one;

    let _ = run_runtime_case as for<'a> fn(&RuntimeCase<'a>);
    let _ =
        run_runtime_case_with_bindings as for<'a, 'b> fn(&RuntimeCase<'a>, &[HostBindingCase<'b>]);
    let _ = run_runtime_cases as for<'a> fn(&[RuntimeCase<'a>]);
    let _ = compile_error_kind as fn(&vm::CompileError) -> CompileErrorKind;
    let _ = expect_source_error_case as for<'a> fn(&SourceErrorCase<'a>);
    let _ = expect_parse_error_case as for<'a> fn(&ParseErrorCase<'a>);
    let _ = expect_parse_error_contains_any_with_flavor as fn(&str, SourceFlavor, &[&str]);
    let _ = expect_parse_error_contains_any_case as fn(&str, &str, SourceFlavor, &[&str]);
    let _ = static_add_one as fn(&mut Vm, &[Value]) -> Result<CallOutcome, vm::VmError>;
    let _ = make_add_one as fn() -> Box<dyn HostFunction>;
    let _ = make_echo_string as fn() -> Box<dyn HostFunction>;
    let _ = make_print_builtin as fn() -> Box<dyn HostFunction>;
    let _ = make_always_allow as fn() -> Box<dyn HostFunction>;
    let _ = make_runtime_sleep as fn() -> Box<dyn HostFunction>;

    let _ = SourceErrorKind::CompileAny;
    let _ = SourceErrorKind::Compile(CompileErrorKind::Assembler);

    let _ = YieldOnce { yielded: false };
    let _ = AddOne;
    let _ = EchoString;
    let _ = PrintBuiltin;
    let _ = AlwaysAllow;
}
