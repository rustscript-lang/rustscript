#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceRef;
use vm::VmResult;

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A direct shared resource borrow cannot be returned by a host function.
        #[pd_host_function(name = "test::direct_resource_ref_return")]
        fn direct_resource_ref_return() -> ResourceRef<'static, Counter> {
            todo!()
        }
    }
}

fn main() {}
