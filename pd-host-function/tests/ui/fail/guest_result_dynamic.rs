#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::{GuestResult, ResourceMut, ResourceRef, Value, VmResult};
struct Counter;
/// Invalid typed guest return.
#[pd_host_function(name = "test::invalid")]
fn invalid() -> VmResult<GuestResult<Value, String>> {
    todo!()
}
fn main() {}
