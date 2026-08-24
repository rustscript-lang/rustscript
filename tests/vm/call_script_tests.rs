//! Milestone 6: `CallScript` interpreter entry tests.
//!
//! These tests build raw `CallScript` bytecode (0x1A, prototype_id:u32 LE,
//! argc:u8) with hand-written callable metadata so the interpreter contract
//! is pinned independently of the compiler: frame entry, resume
//! continuation, operand stack cleanup, typed failures, depth limits, and
//! interruption ticks.
#[path = "../common/mod.rs"]
mod common;
use common::*;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use vm::{
    CallableKind, CallablePrototype, CallableTarget, FunctionRegion, Program, ScriptFunction,
    Value, VmError, VmStatus,
};

/// Build a program whose root body is `root_prefix` followed by
/// `CallScript(prototype_id, argc)` and `ret`; the callee body is supplied
/// as raw bytes. Callable metadata describes one prototype with the given
/// arity/target/captures/self slot.
#[allow(clippy::too_many_arguments)]
fn call_script_program(
    prototype_id: u32,
    argc: u8,
    arity: u8,
    target: CallableTarget,
    capture_slots: Vec<u16>,
    self_slot: Option<u16>,
    root_prefix: Vec<u8>,
    callee_body: Vec<u8>,
) -> Program {
    let mut code = root_prefix;
    code.push(0x1A);
    code.extend_from_slice(&prototype_id.to_le_bytes());
    code.push(argc);
    code.push(0x01); // ret
    let function_entry = code.len() as u32;
    code.extend_from_slice(&callee_body);
    let function_end = code.len() as u32;

    Program::new(vec![Value::Int(41), Value::Int(1)], code)
        .with_local_count(1)
        .with_callable_metadata(
            vec![ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![CallablePrototype {
                kind: CallableKind::FunctionItem,
                target,
                arity,
                frame_local_count: 1,
                parameter_slots: (0..arity).map(u16::from).collect(),
                capture_source_slots: Vec::new(),
                capture_slots,
                capture_modes: Vec::new(),
                self_slot,
                schema: None,
            }],
            vec![
                FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![],
        )
}

/// A program whose callee (prototype 0) recursively calls itself through
/// `CallScript` with no arguments until the depth limit stops it.
fn call_script_recursion_program() -> Program {
    // Root body: CallScript(0, 0), ret.
    let mut code = vec![0x1A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
    let function_entry = code.len() as u32;
    // Callee body: CallScript(0, 0), ret.
    code.extend_from_slice(&[0x1A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let function_end = code.len() as u32;

    Program::new(Vec::new(), code)
        .with_local_count(1)
        .with_callable_metadata(
            vec![ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![CallablePrototype {
                kind: CallableKind::FunctionItem,
                target: CallableTarget::ScriptFunction(0),
                arity: 0,
                frame_local_count: 1,
                parameter_slots: Vec::new(),
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![
                FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![],
        )
}

/// Callee body that returns `local 0 + 1` (parameter + 1).
fn callee_param_plus_one() -> Vec<u8> {
    vec![0x0F, 0x00, 0x02, 0x01, 0x00, 0x00, 0x00, 0x03, 0x01]
}

#[test]
fn call_script_enters_script_frame_and_resumes_caller() {
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00], // ldc 0 (41)
        callee_param_plus_one(),
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("script call should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert_eq!(vm.call_depth(), 0);
}

#[test]
fn call_script_preserves_caller_stack_below_operands() {
    // Root: ldc 0 (41), ldc 0 (41), CallScript(0, 1), ret. The first value
    // sits below the operand stack base and must survive the nested frame.
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00],
        callee_param_plus_one(),
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("script call should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(41), Value::Int(42)]);
    assert_eq!(vm.call_depth(), 0);
}

#[test]
fn call_script_rejects_stack_underflow() {
    // argc is 2 but only one value is pushed.
    let program = call_script_program(
        0,
        2,
        2,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(vm.run(), Err(VmError::StackUnderflow)));
}

#[test]
fn call_script_rejects_invalid_prototype_id() {
    let program = call_script_program(
        99,
        1,
        1,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(
        vm.run(),
        Err(VmError::InvalidCallablePrototype(99))
    ));
}

#[test]
fn call_script_rejects_invalid_script_function_id() {
    // The prototype exists, passes the environment and arity checks, but
    // its `ScriptFunction` target id is out of range for the program's
    // script-function table. The lookup must fail with the same typed
    // error used for the missing-prototype branch rather than entering a
    // bogus frame.
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::ScriptFunction(5),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(
        vm.run(),
        Err(VmError::InvalidCallablePrototype(0))
    ));
}

#[test]
fn call_script_rejects_wrong_arity() {
    // Prototype declares arity 1 but the call passes 2 operands.
    let program = call_script_program(
        0,
        2,
        1,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(
        vm.run(),
        Err(VmError::CallableArityMismatch {
            prototype_id: 0,
            expected: 1,
            got: 2
        })
    ));
}

#[test]
fn call_script_rejects_non_script_prototype() {
    // `CallScript` is a static script-function call: a host-import
    // prototype must be rejected instead of routing to the host path.
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::HostImport(0),
        Vec::new(),
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(
        vm.run(),
        Err(VmError::InvalidCallablePrototype(0))
    ));
}

#[test]
fn call_script_preserves_script_depth_limit() {
    let program = call_script_recursion_program();
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_max_script_call_depth(3)
        .expect("positive depth should be accepted");
    assert!(matches!(
        vm.run(),
        Err(VmError::CallStackOverflow { limit: 3 })
    ));
}

#[test]
fn call_script_frame_entry_charges_interruption_ticks() {
    // Frame entry through `CallScript` must charge interruption ticks like
    // `CallValue`: with a tiny fuel budget the recursion exhausts fuel and
    // the vm yields with the fuel reason instead of looping forever.
    let program = call_script_recursion_program();
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_fuel_check_interval(1)
        .expect("interval update should succeed");
    vm.set_fuel(2);
    let status = vm.run().expect("run should yield on fuel exhaustion");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(vm.get_fuel(), Some(0));
}

#[test]
fn call_script_rejects_capture_required_prototype() {
    // `CallScript` supplies no callable environment: a prototype whose
    // capture layout requires cells must be rejected with a typed error.
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::ScriptFunction(0),
        vec![1],
        None,
        vec![0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(
        vm.run(),
        Err(VmError::CallScriptRequiresEnvironment(0))
    ));
}

#[test]
fn call_script_recursion_resumes_caller_locals_intact() {
    // Direct recursion through `CallScript`: each frame keeps its own
    // parameter value, and the caller's locals survive the nested calls.
    let source = r#"
        fn countdown(n: int) -> int {
            if n <= 0 => { 0 } else => { countdown(n - 1) }
        }
        let keep = "alive";
        countdown(3);
        keep;
    "#;
    let compiled = compile_source(source).expect("recursion source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(0), Value::string("alive")]);
}

/// Host function that reports `Pending` once (for a scope-registered
/// operation); the test delivers the completion through `complete_host_op`.
struct PendingOnceHostOp {
    call_count: Arc<AtomicUsize>,
}

impl HostFunction for PendingOnceHostOp {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> Result<CallOutcome, vm::VmError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let op_id = vm
            .host_context()
            .start_operation(vm::operation::OperationSpec::new(PendingOperationDriver))
            .expect("start pending scope operation");
        Ok(CallOutcome::Pending(op_id.raw()))
    }
}

#[test]
fn call_script_rejects_self_slot_required_prototype() {
    // `CallScript` supplies no callable environment: a prototype that
    // requires a self binding is rejected with a typed error even when its
    // capture layout is empty.
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        Some(0),
        vec![0x02, 0x00, 0x00, 0x00, 0x00],
        vec![0x01],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    assert!(matches!(
        vm.run(),
        Err(VmError::CallScriptRequiresEnvironment(0))
    ));
}

#[test]
fn call_script_callee_host_wait_resumes_caller_continuation() {
    // The callee suspends mid-body on a host operation. After the host op
    // completes, the callee frame resumes with its local state intact and
    // returns through the `CallScript` continuation, which finishes with
    // the caller stack below the call operands preserved.
    let program = call_script_program(
        0,
        1,
        1,
        CallableTarget::ScriptFunction(0),
        Vec::new(),
        None,
        // Root: ldc 0 (41), ldc 0 (41), CallScript(0, 1), ret. The first
        // 41 sits below the operand stack base and must survive the
        // nested frame and the suspension.
        vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00],
        // Callee: Call(host 0, 0), ldloc 0 (parameter), ret.
        vec![0x11, 0x00, 0x00, 0x00, 0x0F, 0x00, 0x01],
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.register_function(Box::new(PendingOnceHostOp {
        call_count: Arc::clone(&calls),
    }));

    let status = vm.run().expect("first run should wait");
    let VmStatus::Waiting(op_id) = status else {
        panic!("expected waiting status, got {status:?}");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1, "host op should run once");

    vm.complete_host_op(op_id, Vec::new())
        .expect("host op completion should succeed");
    let status = vm.resume().expect("resume should halt");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "resume must not re-enter the host function"
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(41), Value::Int(41)],
        "caller stack below the operands and the callee result must survive the suspension"
    );
    assert_eq!(vm.call_depth(), 0);
}
