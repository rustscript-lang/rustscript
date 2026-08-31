#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceRef;
use vm::{HostCallResult, VmError};

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// Result and HostCallResult are transparent for borrowed-return validation.
        #[pd_host_function(name = "test::nested_result_host_call_resource")]
        fn nested_result_host_call_resource(
        ) -> Result<HostCallResult<ResourceRef<'static, Counter>>, VmError> {
            todo!()
        }
    }
}

fn main() {}
