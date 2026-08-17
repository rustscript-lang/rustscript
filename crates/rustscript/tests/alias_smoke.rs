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
fn alias_exports_public_runtime_event_contract() {
    fn accept_sink<S: rustscript::EventSink>(_sink: S) {}

    struct Sink;
    impl rustscript::EventSink for Sink {
        fn emit(&mut self, _payload: rustscript::EventPayload) -> rustscript::RuntimeResult<()> {
            Ok(())
        }
    }

    accept_sink(Sink);
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
