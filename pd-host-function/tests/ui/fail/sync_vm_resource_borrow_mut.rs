#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::resource::ResourceMut;
use vm::{Vm, VmResult};

struct Counter;

mod generated_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// A mutable resource borrow cannot share a mutable VM parameter.
        #[pd_host_function(name = "test::sync_vm_resource_borrow_mut")]
        fn sync_vm_resource_borrow_mut(
            vm: &mut Vm,
            counter: ResourceMut<'_, Counter>,
        ) -> VmResult<i64> {
            let _ = (vm, counter);
            Ok(0)
        }
    }
}

fn main() {}
