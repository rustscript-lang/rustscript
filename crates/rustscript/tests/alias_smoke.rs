#[cfg(feature = "sqlite")]
use rustscript::SqliteHostExt;

/// Verify that the `rustscript` alias crate re-exports the same API as `pd-vm`.
#[test]
fn alias_exports_compile_source() {
    // Valid RustScript: let binding with semicolon
    let result = rustscript::compile_source("let a = 1; let b = a + 2;");
    assert!(result.is_ok(), "should compile: {:?}", result.err());
}

#[test]
fn alias_exports_value_type() {
    let v = rustscript::Value::Int(42);
    if let rustscript::Value::Int(n) = v {
        assert_eq!(n, 42);
    } else {
        panic!("expected Int(42)");
    }
}

#[test]
fn alias_exports_op_code() {
    let _ = rustscript::OpCode::Nop;
    let _ = rustscript::OpCode::Add;
}

#[cfg(feature = "runtime")]
#[test]
fn alias_exports_public_invocation_stream_contract() {
    fn accept_item(_item: rustscript::InvocationItem) {}

    accept_item(rustscript::InvocationItem::Complete(
        rustscript::Value::Null,
    ));
    accept_item(rustscript::InvocationItem::Event(rustscript::Value::Bool(
        true,
    )));

    fn accept_poll(_poll: rustscript::InvocationPoll) {}
    accept_poll(rustscript::InvocationPoll::Pending);
    accept_poll(rustscript::InvocationPoll::Ready(None));
    accept_poll(rustscript::InvocationPoll::Ready(Some(Ok(
        rustscript::InvocationItem::Complete(rustscript::Value::Null),
    ))));

    fn accept_error(_error: rustscript::InvocationError) {}
    accept_error(rustscript::InvocationError::Cancelled(
        rustscript::CancellationReason::Requested,
    ));
    accept_error(rustscript::InvocationError::Host {
        message: "boom".to_string(),
    });
}

#[cfg(feature = "http-client")]
#[test]
fn alias_http_client_includes_runtime_contract() {
    fn accept_runtime_result(_result: rustscript::RuntimeResult<()>) {}

    accept_runtime_result(Ok(()));
}

#[cfg(feature = "sqlite")]
#[test]
fn alias_exports_public_sqlite_configuration() {
    let program = rustscript::compile_source("0;")
        .expect("minimal alias SQLite program should compile")
        .program;
    let mut vm = rustscript::Vm::new(program);
    vm.configure_sqlite(rustscript::SqlitePolicy::default());
    let _limits = rustscript::SqliteLimits::default();
    vm.clear_sqlite_configuration();
}
