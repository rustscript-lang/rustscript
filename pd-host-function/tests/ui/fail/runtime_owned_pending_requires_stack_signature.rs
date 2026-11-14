#![allow(dead_code, unused_imports)]
use pd_host_function::pd_host_function;
use vm::VmResult;
/// The pending dispatch flag needs a stack-dispatch signature.
#[pd_host_function(name = "test::pending", runtime_owned_pending)]
fn pending() -> VmResult<i64> {
    todo!()
}
fn main() {}
