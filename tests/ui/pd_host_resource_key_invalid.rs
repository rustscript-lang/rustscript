use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::invalid")]
/// An invalid-character resource key must be rejected at expansion time.
fn f(
    #[pd_host_param(passing = "take_owned", key = "bad key")] resource: FakeResource,
) -> i64 {
    todo!()
}
