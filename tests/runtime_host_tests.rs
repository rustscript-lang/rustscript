#![cfg(feature = "runtime")]

#[cfg(feature = "sqlite")]
use vm::SqliteHostExt;
use vm::{
    HostFunctionRegistry, InvocationError, InvocationItem, InvocationPoll, Value, Vm, VmError,
    VmStatus, compile_source,
};

/// Compiles a source, binds the default runtime host registry, and completes
/// the root frame so exported callables can be started.
fn prepared_vm(source: &str) -> Vm {
    let program = compile_source(source)
        .expect("runtime host source should compile")
        .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default runtime host registry should bind");
    assert_eq!(vm.run().expect("root frame should halt"), VmStatus::Halted);
    vm
}

#[test]
fn invocation_input_arrives_through_exported_callable_arguments() {
    let mut vm = prepared_vm(
        r#"
        pub fn run(input: string) -> string {
            input;
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![Value::string("run-input")])
        .expect("invocation should start");

    match invocation.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value)))) => {
            assert_eq!(value, Value::string("run-input"));
        }
        other => panic!("expected the callable input as the Complete value, got {other:?}"),
    }
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn stream_emit_delivers_events_through_the_invocation_stream() {
    let mut vm = prepared_vm(
        r#"
        use stream;
        pub fn run() -> string {
            stream::emit("event-one");
            stream::emit("event-two");
            "done";
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![])
        .expect("invocation should start");

    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("event-one")
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Event(value)))) if value == Value::string("event-two")
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value)))) if value == Value::string("done")
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn invocation_errors_are_typed_without_string_parsing() {
    let mut vm = prepared_vm(
        r#"
        pub fn run(input: int) -> int {
            1 / input;
        }
        "#,
    );
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    let mut invocation = vm
        .start_invocation(callable, vec![Value::Int(0)])
        .expect("invocation should start");

    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Err(InvocationError::Vm(VmError::DivisionByZero))))
    ));
    assert!(matches!(
        invocation.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[cfg(feature = "sqlite")]
#[test]
fn public_sqlite_policy_configures_the_production_vm() {
    let program = compile_source("0;")
        .expect("minimal SQLite host program should compile")
        .program;
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.configure_sqlite(vm::SqlitePolicy::default());
    let _limits = vm::SqliteLimits::default();
    vm.clear_sqlite_configuration();
}
