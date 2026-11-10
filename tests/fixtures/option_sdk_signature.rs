/// Echoes a logical guest Option with a typed array payload.
#[pd_host_function(name = "option_sdk::fixture")]
fn fixture(value: GuestOption<Vec<i64>>) -> VmResult<GuestOption<Vec<i64>>> {
    Ok(value)
}

/// Returns every nested Option state, including an unselected None.
#[pd_host_function(name = "option_sdk::nested")]
fn nested(mode: i64) -> VmResult<GuestOption<GuestOption<SdkBox>>> {
    Ok(match mode {
        0 => GuestOption::None,
        1 => GuestOption::Some(GuestOption::None),
        _ => GuestOption::Some(GuestOption::Some(SdkBox { value: 7 })),
    })
}

/// Returns the logical Unit value through the typed host boundary.
#[pd_host_function(name = "option_sdk::unit")]
fn unit() -> VmResult<()> {
    Ok(())
}

/// Observes the selected Option payload without erasing its guest schema.
#[pd_host_function(name = "option_sdk::inspect")]
fn inspect(vm: &mut Vm, value: GuestOption<Vec<i64>>) -> i64 {
    let calls = vm
        .host_context()
        .module_state::<usize>()
        .copied()
        .unwrap_or(0);
    vm.host_context().set_module_state(calls + 1);
    match value {
        GuestOption::Some(values) => values.into_iter().sum(),
        GuestOption::None => -1,
    }
}

/// Echoes an Option of a guest Result.
#[pd_host_function(name = "option_sdk::option_result")]
fn option_result(
    value: GuestOption<GuestResult<i64, String>>,
) -> VmResult<GuestOption<GuestResult<i64, String>>> {
    Ok(value)
}

/// Echoes a guest Result containing a guest Option.
#[pd_host_function(name = "option_sdk::result_option")]
fn result_option(
    value: GuestResult<GuestOption<i64>, String>,
) -> VmResult<GuestResult<GuestOption<i64>, String>> {
    Ok(value)
}

/// Publishes only the selected owned resource.
#[pd_host_function(name = "option_sdk::resource_make")]
fn resource_make(present: bool) -> VmResult<GuestOption<ResourceOwned<OptionSuccess>>> {
    Ok(if present {
        CREATED.fetch_add(1, Ordering::SeqCst);
        GuestOption::Some(ResourceOwned::new(OptionSuccess(23)))
    } else {
        GuestOption::None
    })
}

/// Takes precisely the active owned resource.
#[pd_host_function(name = "option_sdk::resource_take")]
fn resource_take(value: GuestOption<ResourceOwned<OptionSuccess>>) -> i64 {
    match value {
        GuestOption::Some(value) => value.into_inner().0,
        GuestOption::None => -1,
    }
}

/// Reports the live resource count before VM teardown.
#[pd_host_function(name = "option_sdk::resource_observe")]
fn resource_observe(vm: &mut Vm) -> i64 {
    let count = vm.host_context().resource_count();
    OBSERVED_RESOURCE_COUNT.store(count, Ordering::SeqCst);
    count as i64
}
