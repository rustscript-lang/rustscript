#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::{GuestResult, Value, VmResult};
mod generated_parent {
    pub mod functions {
        use super::super::*;
        /// Echo a typed guest union without confusing its error with VmError.
        #[pd_host_function(name = "test::guest_result")]
        fn echo(value: GuestResult<i64, String>) -> VmResult<GuestResult<i64, String>> {
            Ok(value)
        }
    }
}
fn main() {
    let descriptor = generated_parent::functions::echo_descriptor();
    assert!(matches!(
        descriptor.schema.return_type,
        vm::HostTypeSchema::Result { .. }
    ));
    for value in [
        vm::result::carrier(true, Value::Int(0)),
        vm::result::carrier(false, Value::string("error")),
    ] {
        assert_eq!(
            generated_parent::functions::echo(std::slice::from_ref(&value)).unwrap(),
            value
        );
    }
}
