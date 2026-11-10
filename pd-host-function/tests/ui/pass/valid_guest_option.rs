#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::{GuestOption, Value, VmResult};
mod generated_parent {
    pub mod functions {
        use super::super::*;
        /// Echoes a typed guest Option with a scalar payload.
        #[pd_host_function(name = "test::option_scalar")]
        fn scalar(value: GuestOption<i64>) -> VmResult<GuestOption<i64>> {
            Ok(value)
        }
        /// Returns every nested Option state.
        #[pd_host_function(name = "test::option_nested")]
        fn nested(
            value: GuestOption<GuestOption<Vec<bool>>>,
        ) -> VmResult<GuestOption<GuestOption<Vec<bool>>>> {
            Ok(value)
        }
        /// A host function with no declared return type exposes logical Unit.
        #[pd_host_function(name = "test::option_unit")]
        fn unit() -> VmResult<()> {
            Ok(())
        }
    }
}
fn main() {
    let descriptor = generated_parent::functions::scalar_descriptor();
    assert!(matches!(
        descriptor.schema.return_type,
        vm::HostTypeSchema::Option(_)
    ));
    let unit = generated_parent::functions::unit_descriptor();
    assert!(matches!(unit.schema.return_type, vm::HostTypeSchema::Unit));
}
