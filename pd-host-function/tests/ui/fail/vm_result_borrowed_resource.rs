#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceRef;
use vm::VmResult;

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A borrowed resource nested inside VmResult is rejected.
        #[pd_host_function(name = "test::vm_result_borrowed_resource")]
        fn vm_result_borrowed_resource() -> VmResult<ResourceRef<'static, Counter>> {
            todo!()
        }
    }
}

fn main() {}
