use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::mut_return")]
/// A ResourceMut return would hand a mutable borrow across the host boundary.
fn f(value: i64) -> ResourceMut<'_, FakeResource> {
    todo!()
}
