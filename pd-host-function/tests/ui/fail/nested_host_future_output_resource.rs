#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceMut;
use vm::HostFutureOutput;
use vm::VmResult;

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// HostFutureOutput is transparent for borrowed-return validation.
        #[pd_host_function(name = "test::nested_host_future_output_resource")]
        fn nested_host_future_output_resource() -> VmResult<HostFutureOutput<ResourceMut<'static, Counter>>> {
            todo!()
        }
    }
}

fn main() {}
