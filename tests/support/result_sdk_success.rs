use super::*;

fn owned(branch: bool, payload: i64) -> OwnedResult {
    if branch {
        GuestResult::Ok(ResourceOwned::new(Success(payload)))
    } else {
        GuestResult::Err(ResourceOwned::new(Failure(payload)))
    }
}
fn assert_returned(value: &OwnedResult, branch: bool, payload: i64) {
    match value {
        GuestResult::Ok(value) => {
            assert!(branch);
            assert_eq!(value.as_ref().0, payload);
        }
        GuestResult::Err(value) => {
            assert!(!branch);
            assert_eq!(value.as_ref().0, payload);
        }
    }
}
#[test]
fn vm_aware_success_reclaims_unused_sibling_without_closing_returned_or_unrelated() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for branch in [true, false] {
        let mut store = callback_store();
        let existing = store
            .vm_mut()
            .host_context()
            .insert_resource(Failure(99))
            .unwrap()
            .handle();
        let callback = store
            .vm_script_callback_by_name::<(OwnedResult, OwnedResult), OwnedResult>("return_first")
            .unwrap();
        let closed = CLOSED.load(Ordering::SeqCst);
        let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
        let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
        let returned = callback
            .call_with_vm(&mut store, (owned(branch, 301), owned(!branch, 302)))
            .unwrap();
        assert_returned(&returned, branch, 301);
        assert_eq!(OBSERVED_RESOURCE_COUNT.load(Ordering::SeqCst), 3);
        assert_eq!(
            store
                .vm_mut()
                .host_context()
                .borrow_resource::<Failure>(existing)
                .unwrap()
                .get()
                .0,
            99
        );
        eprintln!(
            "success_sibling branch={branch} live={} close_delta={} drop_ok_delta={} drop_err_delta={} returned_payload=301 unrelated_payload=99",
            store.vm_mut().host_context().resource_count(),
            CLOSED.load(Ordering::SeqCst) - closed,
            DROPPED_OK.load(Ordering::SeqCst) - dropped_ok,
            DROPPED_ERR.load(Ordering::SeqCst) - dropped_err
        );
        assert_eq!(
            store.vm_mut().host_context().resource_count(),
            1,
            "unused publication must be reclaimed before VM destruction"
        );
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 1);
        assert_eq!(
            DROPPED_OK.load(Ordering::SeqCst),
            dropped_ok + usize::from(!branch)
        );
        assert_eq!(
            DROPPED_ERR.load(Ordering::SeqCst),
            dropped_err + usize::from(branch)
        );
        drop(returned);
        store
            .vm_mut()
            .host_context()
            .close_resource::<Failure>(existing, ResourceCloseReason::Requested)
            .unwrap();
    }
}
#[test]
fn vm_aware_success_reclaims_ignored_publication_and_returns_fresh_owner() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for branch in [true, false] {
        let mut store = callback_store();
        let callback = store
            .vm_script_callback_by_name::<(OwnedResult, bool), OwnedResult>("fresh_return")
            .unwrap();
        let closed = CLOSED.load(Ordering::SeqCst);
        let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
        let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
        let returned = callback
            .call_with_vm(&mut store, (owned(branch, 303), !branch))
            .unwrap();
        assert_returned(&returned, !branch, if branch { 29 } else { 17 });
        eprintln!(
            "success_fresh branch={branch} live={} close_delta={} drop_ok_delta={} drop_err_delta={}",
            store.vm_mut().host_context().resource_count(),
            CLOSED.load(Ordering::SeqCst) - closed,
            DROPPED_OK.load(Ordering::SeqCst) - dropped_ok,
            DROPPED_ERR.load(Ordering::SeqCst) - dropped_err
        );
        assert_eq!(
            store.vm_mut().host_context().resource_count(),
            0,
            "ignored publication must be reclaimed before VM destruction"
        );
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 1);
        assert_eq!(
            DROPPED_OK.load(Ordering::SeqCst),
            dropped_ok + usize::from(branch)
        );
        assert_eq!(
            DROPPED_ERR.load(Ordering::SeqCst),
            dropped_err + usize::from(!branch)
        );
        drop(returned);
    }
}

#[test]
fn vm_aware_success_close_failure_attempts_pending_sibling_and_preserves_return() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult, OwnedResult, OwnedResult), OwnedResult>(
            "return_first_three",
        )
        .unwrap();
    PENDING_READY.store(false, Ordering::SeqCst);
    let closed = CLOSED.load(Ordering::SeqCst);
    let returned = callback
        .call_with_vm(
            &mut store,
            (owned(true, 304), owned(true, -101), owned(false, -102)),
        )
        .unwrap();
    assert_returned(&returned, true, 304);
    assert_eq!(store.vm_mut().host_context().resource_count(), 2);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 2);
    store
        .vm_mut()
        .execution_scope()
        .begin_close(ResourceCloseReason::Requested)
        .unwrap();
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        store
            .vm_mut()
            .execution_scope()
            .poll_close(&mut cx)
            .is_pending()
    );
    assert_eq!(store.vm_mut().host_context().resource_count(), 1);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 3);
    PENDING_READY.store(true, Ordering::SeqCst);
    assert!(
        store
            .vm_mut()
            .execution_scope()
            .poll_close(&mut cx)
            .is_ready()
    );
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 3);
    assert_returned(&returned, true, 304);
    eprintln!(
        "success_close_error return_payload=304 initial_sibling_begin=2 failed_open_plus_pending=2 shutdown_retry=1 pending_live=1 terminal_live=0"
    );
}

#[test]
fn vm_aware_success_native_taken_owner_remains_in_opaque_state() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for branch in [true, false] {
        let mut store = callback_store();
        let callback = store
            .vm_script_callback_by_name::<(OwnedResult,), i64>("retain_success")
            .unwrap();
        let closed = CLOSED.load(Ordering::SeqCst);
        let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
        let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
        assert_eq!(
            callback
                .call_with_vm(&mut store, (owned(branch, 305),))
                .unwrap(),
            1
        );
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
        assert_eq!(DROPPED_OK.load(Ordering::SeqCst), dropped_ok);
        assert_eq!(DROPPED_ERR.load(Ordering::SeqCst), dropped_err);
        let retained = store
            .vm_mut()
            .host_context()
            .take_module_state::<OwnedResult>()
            .unwrap();
        assert_returned(&retained, branch, 305);
        drop(retained);
        eprintln!(
            "success_native_take branch={branch} retained_payload=305 table_live=0 close_delta=0"
        );
    }
}

#[test]
fn vm_aware_success_surviving_capture_remains_usable_through_alias() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for branch in [true, false] {
        let mut store = vm::Store::from_vm(machine(
            r#"use result_resource;
            pub fn factory() {
                let mut saved = result_resource::make(true);
                fn callback(value: Result<resource<result.success>, resource<result.failure>>, action: int) -> int {
                    if action == 1 => {
                        result_resource::retire_initial(); saved = value; result_resource::borrow(saved)
                    } else => {
                        result_resource::take(value);
                        if action == 0 => { result_resource::borrow(saved) } else => { result_resource::take(saved) }
                    }
                }
                callback
            }
        "#,
        ));
        store.run().unwrap();
        let factory = store.vm_mut().resolve_exported_callable("factory").unwrap();
        let callable = store.vm_mut().invoke_callable(factory, &[]).unwrap();
        let callback = store
            .vm_script_callback::<(OwnedResult, i64), i64>(callable)
            .unwrap();
        let alias = callback.clone();
        let closed = CLOSED.load(Ordering::SeqCst);
        assert_eq!(
            callback
                .call_with_vm(&mut store, (owned(branch, 306), 1))
                .unwrap(),
            306
        );
        assert_eq!(store.vm_mut().host_context().resource_count(), 1);
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
        assert_eq!(
            alias
                .call_with_vm(&mut store, (owned(true, 307), 0))
                .unwrap(),
            306
        );
        assert_eq!(
            callback
                .call_with_vm(&mut store, (owned(true, 308), -1))
                .unwrap(),
            306
        );
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
        eprintln!(
            "success_capture branch={branch} retained_payload=306 after_return_live=1 final_live=0 close_delta=0"
        );
    }
}
