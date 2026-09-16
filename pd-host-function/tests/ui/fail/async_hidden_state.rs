#![allow(dead_code, unused_imports)]

use pd_host_function::pd_host_function;
use vm::host_api::HostState;
use vm::{HostStateMut, VmResult};

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

    pub mod functions {
        use super::*;

        /// A hidden state borrow cannot cross an async boundary.
        #[pd_host_function(name = "test::async_bump_tuning")]
        async fn async_bump_tuning(tuning: HostStateMut<'_, Tuning>) -> VmResult<i64> {
            Ok(tuning.value)
        }
    }
}

fn main() {}
