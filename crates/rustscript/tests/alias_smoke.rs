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
