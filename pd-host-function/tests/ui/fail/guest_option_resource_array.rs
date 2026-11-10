#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::{GuestOption, ResourceOwned, VmResult};
struct Counter;
/// Invalid nested resource array.
#[pd_host_function(name = "test::invalid")]
fn invalid() -> VmResult<GuestOption<Vec<ResourceOwned<Counter>>>> {
    todo!()
}
fn main() {}
