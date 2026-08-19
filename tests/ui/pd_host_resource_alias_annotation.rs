use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::aliased")]
/// An annotated alias-shaped path is not a canonical resource wrapper.
fn f(
    #[pd_host_param(passing = "borrow")] resource: my_alias::Wrapper<'static, FakeResource>,
) -> i64 {
    todo!()
}
