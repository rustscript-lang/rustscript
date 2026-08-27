#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceMut;
use vm::{VmResult, VmError};

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A borrowed mutable resource nested inside Option is rejected.
        #[pd_host_function(name = "test::option_borrowed_mut_resource")]
        fn option_borrowed_mut_resource() -> Option<ResourceMut<'static, Counter>> {
            todo!()
        }
    }
}

fn main() {}
