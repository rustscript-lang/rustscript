/// Echo a nullable typed array guest union.
#[pd_host_function(name = "result_sdk::fixture")]
fn fixture(
    value: GuestResult<Option<Vec<i64>>, String>,
) -> VmResult<GuestResult<Option<Vec<i64>>, String>> {
    Ok(value)
}

/// Returns a concrete generic named payload in the guest error branch.
#[pd_host_function(name = "result_sdk::named")]
fn named(ok: bool) -> VmResult<GuestResult<i64, SdkFailure<i64>>> {
    Ok(if ok {
        GuestResult::Ok(7)
    } else {
        GuestResult::Err(SdkFailure { code: 29 })
    })
}

/// Observes the selected optional array without losing its guest schema.
#[pd_host_function(name = "result_sdk::inspect")]
fn inspect(vm: &mut Vm, value: GuestResult<Option<Vec<i64>>, String>) -> i64 {
    let calls = vm
        .host_context()
        .module_state::<usize>()
        .copied()
        .unwrap_or(0);
    vm.host_context().set_module_state(calls + 1);
    match value {
        GuestResult::Ok(Some(values)) => values.into_iter().sum(),
        GuestResult::Ok(None) => -1,
        GuestResult::Err(_) => -2,
    }
}
