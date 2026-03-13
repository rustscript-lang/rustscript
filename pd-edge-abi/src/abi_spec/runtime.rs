#[pd_host_function(name = "rate_limit::allow")]
fn rate_limit_allow(key: &str, limit: i64, window_seconds: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "runtime::sleep")]
fn runtime_sleep(ms: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "runtime::exit")]
fn runtime_exit() {
    unreachable!("abi declaration only")
}
