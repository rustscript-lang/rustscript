/// Publishes only the selected owned resource.
#[pd_host_function(name = "result_resource::make")]
fn make(ok: bool) -> VmResult<GuestResult<ResourceOwned<Success>, ResourceOwned<Failure>>> {
    Ok(if ok {
        CREATED_OK.fetch_add(1, Ordering::SeqCst);
        GuestResult::Ok(ResourceOwned::new(Success(17)))
    } else {
        CREATED_ERR.fetch_add(1, Ordering::SeqCst);
        GuestResult::Err(ResourceOwned::new(Failure(29)))
    })
}
/// Borrows the active resource without transferring it.
#[pd_host_function(name = "result_resource::borrow")]
fn borrow(value: GuestResult<vm::ResourceRef<'_, Success>, vm::ResourceRef<'_, Failure>>) -> i64 {
    match value {
        GuestResult::Ok(v) => v.get().0,
        GuestResult::Err(v) => v.get().0,
    }
}
/// Mutates the active resource without transferring it.
#[pd_host_function(name = "result_resource::bump")]
fn bump(value: GuestResult<vm::ResourceMut<'_, Success>, vm::ResourceMut<'_, Failure>>) -> i64 {
    match value {
        GuestResult::Ok(mut v) => {
            v.get().0 += 1;
            v.get().0
        }
        GuestResult::Err(mut v) => {
            v.get().0 += 1;
            v.get().0
        }
    }
}
/// Consumes precisely the active resource.
#[pd_host_function(name = "result_resource::take")]
fn take(value: GuestResult<ResourceOwned<Success>, ResourceOwned<Failure>>) -> i64 {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    match value {
        GuestResult::Ok(v) => v.into_inner().0,
        GuestResult::Err(v) => v.into_inner().0,
    }
}
