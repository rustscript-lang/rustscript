#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::{GuestOption, Value, VmCallable, VmResult};
mod generated_parent {
    pub mod functions {
        use super::super::*;
        /// Echoes a callable payload nested in a logical Option.
        #[pd_host_function(name = "test::option_callable")]
        fn callable(
            value: GuestOption<VmCallable<fn(i64) -> i64>>,
        ) -> VmResult<GuestOption<VmCallable<fn(i64) -> i64>>> {
            Ok(value)
        }
        /// Echoes a Unit-returning callable under a logical Option.
        #[pd_host_function(name = "test::option_callable_unit")]
        fn callable_unit(
            value: GuestOption<VmCallable<fn() -> ()>>,
        ) -> VmResult<GuestOption<VmCallable<fn() -> ()>>> {
            Ok(value)
        }
    }
}
fn main() {
    let descriptor = generated_parent::functions::callable_descriptor();
    let vm::HostTypeSchema::Option(inner) = &descriptor.schema.params[0].ty else {
        panic!("callable Option parameter schema");
    };
    assert!(matches!(
        inner.as_ref(),
        vm::HostTypeSchema::Callable { .. }
    ));
    let unit = generated_parent::functions::callable_unit_descriptor();
    let vm::HostTypeSchema::Option(inner) = &unit.schema.return_type else {
        panic!("Unit callable Option return schema");
    };
    let vm::HostTypeSchema::Callable { params, result } = inner.as_ref() else {
        panic!("Unit callable payload must carry the Callable schema");
    };
    assert!(params.is_empty());
    assert!(matches!(result.as_ref(), vm::HostTypeSchema::Unit));
}
