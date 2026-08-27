#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceRef;
use vm::VmResult;

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A borrowed resource nested inside transparent return wrappers is rejected.
        #[pd_host_function(name = "test::nested_vm_result_option_resource")]
        fn nested_vm_result_option_resource() -> VmResult<Option<ResourceRef<'static, Counter>>> {
            todo!()
        }
    }
}

fn main() {}
