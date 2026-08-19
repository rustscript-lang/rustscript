use pd_host_function::pd_host_function;

#[pd_host_function(name = "test::generic")]
/// Generic host functions cannot be instantiated by the adapter.
fn f<T>(resource: ResourceOwned<T>) -> i64 {
    todo!()
}
