use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::overlong")]
/// An over-long resource key must be rejected at expansion time.
fn f(
    #[pd_host_param(
        passing = "take_owned",
        key = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    )]
    resource: FakeResource,
) -> i64 {
    todo!()
}
