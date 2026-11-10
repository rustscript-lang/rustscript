#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::{GuestOption, Value, VmResult};
/// Invalid typed guest return.
#[pd_host_function(name = "test::invalid")]
fn invalid() -> VmResult<GuestOption<Value>> {
    todo!()
}
fn main() {}
