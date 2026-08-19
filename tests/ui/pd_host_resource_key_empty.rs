use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::empty")]
/// An empty resource key must be rejected at expansion time.
fn f(
    #[pd_host_param(passing = "take_owned", key = "")] resource: FakeResource,
) -> i64 {
    todo!()
}
