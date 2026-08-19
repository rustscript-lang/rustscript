use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::borrow_return")]
/// A ResourceRef return would hand a borrow across the host boundary.
fn f(value: i64) -> ResourceRef<'_, FakeResource> {
    todo!()
}
