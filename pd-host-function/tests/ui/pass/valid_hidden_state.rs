#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::host_api::HostState;
use vm::{HostStateMut, HostStateRef, Value, Vm, VmError, VmResult};

struct Tuning {
    value: i64,
}

impl HostState for Tuning {
    const KEY: &'static str = "ui.tuning";

    fn initialize() -> Result<Self, String> {
        Ok(Self { value: 0 })
    }
}

mod generated_parent {
    use super::*;

    pub trait FromArg: Sized {
        fn from_arg(value: &Value, label: &str) -> VmResult<Self>;
    }

    impl FromArg for i64 {
        fn from_arg(value: &Value, label: &str) -> VmResult<Self> {
            match value {
                Value::Int(value) => Ok(*value),
                _ => Err(VmError::HostError(format!("expected {label}"))),
            }
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

        /// Mutates per-VM host state through the hidden parameter form.
        #[pd_host_function(name = "test::bump_tuning")]
        fn bump_tuning(mut tuning: HostStateMut<'_, Tuning>, delta: i64) -> VmResult<i64> {
            tuning.value += delta;
            Ok(tuning.value)
        }

        /// Reads per-VM host state through the shared hidden parameter form.
        #[pd_host_function(name = "test::peek_tuning")]
        fn peek_tuning(tuning: HostStateRef<'_, Tuning>) -> VmResult<i64> {
            Ok(tuning.value)
        }
    }
}

fn main() {}
