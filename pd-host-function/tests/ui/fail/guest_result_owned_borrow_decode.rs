use vm::{FromVmValue, GuestResult, ResourceOwned, Value};
struct Counter;
fn main() {
    let _ = GuestResult::<ResourceOwned<Counter>, i64>::from_vm_value(&Value::Null, "owned");
}
