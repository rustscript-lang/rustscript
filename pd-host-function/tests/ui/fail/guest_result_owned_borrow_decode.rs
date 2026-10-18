use vm::{FromVmValue, GuestResult, ResourceOwned, Value};
struct Counter;
fn main() {
    let _ = <GuestResult<ResourceOwned<Counter>, i64> as FromVmValue<'_>>::from_vm_value(
        &Value::Null,
        "owned",
    );
}
