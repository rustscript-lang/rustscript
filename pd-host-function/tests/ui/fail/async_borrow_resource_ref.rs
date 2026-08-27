#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceRef;
use vm::VmResult;

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A shared resource borrow cannot cross an async boundary.
        #[pd_host_function(name = "test::async_borrow_resource_ref")]
        async fn async_borrow_resource_ref(
            counter: ResourceRef<'_, Counter>,
        ) -> VmResult<i64> {
            let _ = counter;
            Ok(0)
        }
    }
}

fn main() {}
