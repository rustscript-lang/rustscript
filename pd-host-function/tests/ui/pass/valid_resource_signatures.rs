#![allow(dead_code, unused_imports)]

extern crate vm as vm_sdk;

pub mod vm {
    pub use super::vm_sdk::*;
}

use pd_host_function::pd_host_function;
use vm::resource::{CloseProgress, HostResource, ResourceOwned, ResourceRef};
use vm::resource;
use vm::{Value, Vm, VmError, VmResult};
pub use vm::host_api;

#[derive(Debug)]
struct Counter(i64);

impl HostResource for Counter {
    fn begin_close(
        &mut self,
        _reason: vm::resource::ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

mod generated_parent {
    use super::*;

    pub trait FromArg: Sized {
        fn from_arg(value: &Value, label: &str) -> VmResult<Self>;
    }

    impl FromArg for i64 {
        fn from_arg(_value: &Value, _label: &str) -> VmResult<Self> {
            Ok(0)
        }
    }

    pub fn arg<T: FromArg>(args: &[Value], index: usize, label: &str) -> VmResult<T> {
        args.get(index)
            .ok_or_else(|| VmError::HostError(format!("missing {label}")))
            .and_then(|value| T::from_arg(value, label))
    }

    pub fn borrow_arg<T: FromArg>(args: &[Value], index: usize, label: &str) -> VmResult<T> {
        arg(args, index, label)
    }

    pub mod functions {
        use super::*;

        /// Accepts the public ResourceOwned wrapper as a consuming parameter.
        #[pd_host_function(name = "test::take_owned_wrapper")]
        fn take_owned_wrapper(counter: ResourceOwned<Counter>) -> VmResult<i64> {
            let _ = counter;
            Ok(0)
        }

        /// Accepts a concrete resource through explicit TakeOwned metadata.
        #[pd_host_function(name = "test::take_owned_metadata")]
        fn take_owned_metadata(
            #[pd_host_param(passing = "take_owned")] counter: Counter,
        ) -> VmResult<i64> {
            let _ = counter;
            Ok(0)
        }

        /// Accepts a shared resource borrow without a conflicting VM borrow.
        #[pd_host_function(name = "test::borrow_resource")]
        fn borrow_resource(counter: ResourceRef<'_, Counter>) -> VmResult<i64> {
            let _ = counter;
            Ok(0)
        }

        /// Combines a mutable VM context with an owned resource safely.
        #[pd_host_function(name = "test::vm_and_owned")]
        fn vm_and_owned(vm: &mut Vm, counter: ResourceOwned<Counter>) -> VmResult<i64> {
            let _ = (vm, counter);
            Ok(0)
        }

        /// Uses a mutable VM context with an ordinary value parameter.
        #[pd_host_function(name = "test::vm_and_value")]
        fn vm_and_value(vm: &mut Vm, value: i64) -> VmResult<i64> {
            let _ = vm;
            Ok(value)
        }
    }
}

fn main() {}
