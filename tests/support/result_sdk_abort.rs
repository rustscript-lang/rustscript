use super::*;
use std::task::{Context, Poll};

static SUBMITTED: AtomicUsize = AtomicUsize::new(0);
static SUSPENDED: AtomicUsize = AtomicUsize::new(0);
static CANCELLED: AtomicUsize = AtomicUsize::new(0);
static CLEANED: AtomicUsize = AtomicUsize::new(0);
static FUTURE_DROPPED: AtomicUsize = AtomicUsize::new(0);
static ACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub(super) fn retire_initial(vm: &mut Vm) -> VmResult<i64> {
    // The host owns full mutable VM authority. Take the actual old capture via
    // its typed key/generation; do not detach the guest capture cell or consume
    // the newly published callback parameter. Its payload is always 191/192.
    for value in vm.locals().to_vec() {
        let Ok((true, payload)) = vm::result::payload(&value) else {
            continue;
        };
        let Ok(handle) = vm::result::resource_handle(payload) else {
            continue;
        };
        let initial = vm
            .host_context()
            .borrow_resource_with_key::<Success>(
                handle,
                &ResourceTypeKey::new(Success::KEY).unwrap(),
            )
            .is_ok_and(|value| value.get().0 == 17);
        if initial {
            let owned = vm
                .host_context()
                .take_resource_with_key::<Success>(
                    handle,
                    &ResourceTypeKey::new(Success::KEY).unwrap(),
                )
                .unwrap();
            drop(owned);
            return Ok(1);
        }
    }
    Err(vm::VmError::HostError("initial capture not found".into()))
}
struct PendingFuture;
impl std::future::Future for PendingFuture {
    type Output = VmResult<vm::HostFutureOutput>;
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
impl Drop for PendingFuture {
    fn drop(&mut self) {
        FUTURE_DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}
struct Bridge(Option<vm::HostFuture>);
impl vm::HostAsyncBridge for Bridge {
    fn submit_op(&mut self, _: vm::HostOpId, future: vm::HostFuture) -> VmResult<()> {
        assert!(self.0.replace(future).is_none());
        SUBMITTED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn poll_op(&mut self, _: vm::HostOpId, _: &mut Context<'_>) -> Poll<VmResult<vm::CallReturn>> {
        panic!("submitted future only")
    }
    fn poll_submitted_op(
        &mut self,
        _: vm::HostOpId,
        cx: &mut Context<'_>,
    ) -> Poll<VmResult<vm::HostFutureOutput>> {
        self.0.as_mut().unwrap().as_mut().poll(cx)
    }
    fn request_cancel_op(
        &mut self,
        _: vm::HostOpId,
        _: vm::operation::OperationCancelReason,
    ) -> VmResult<()> {
        CANCELLED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn poll_cancel_op(&mut self, _: vm::HostOpId, _: &mut Context<'_>) -> Poll<VmResult<()>> {
        if ACK.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
    fn cleanup_op(&mut self, _: vm::HostOpId, _: vm::HostAsyncOpTerminal) -> VmResult<()> {
        drop(self.0.take().unwrap());
        CLEANED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn suspend(vm: &mut Vm, args: &[Value]) -> VmResult<vm::CallOutcome> {
    SUSPENDED.fetch_add(1, Ordering::SeqCst);
    if args == [Value::Int(0)] {
        Ok(vm::CallOutcome::Yield)
    } else {
        vm.submit_host_future(Box::pin(PendingFuture))
    }
}
pub(super) fn suspend_descriptor() -> vm::HostFunctionDescriptor {
    vm::HostFunctionDescriptor {
        schema: vm::HostFunctionSchema::with_return(
            "result_resource::suspend",
            vec![vm::HostParamSchema::value("mode", vm::HostTypeSchema::Int)],
            vm::HostTypeSchema::Int,
        ),
        binding: vm::HostBindingDescriptor {
            kind: vm::HostBindingKind::StaticStack,
        },
        effects: Vec::new(),
        adapter: vm::HostAdapterDescriptor::StaticStack(suspend),
        resource_types: Vec::new(),
    }
}
#[test]
fn vm_aware_post_admission_yield_and_real_wait_cancellation_before_move() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for mode in [0, 1] {
        for branch in [true, false] {
            let mut store = callback_store();
            store
                .vm_mut()
                .set_async_bridge(Box::new(Bridge(None)))
                .unwrap();
            ACK.store(false, Ordering::SeqCst);
            let callback = store
                .vm_script_callback_by_name::<(OwnedResult, i64), OwnedResult>(
                    "suspend_before_move",
                )
                .unwrap();
            let closed = CLOSED.load(Ordering::SeqCst);
            let cancelled = CANCELLED.load(Ordering::SeqCst);
            let cleaned = CLEANED.load(Ordering::SeqCst);
            let dropped = FUTURE_DROPPED.load(Ordering::SeqCst);
            let suspended = SUSPENDED.load(Ordering::SeqCst);
            let submitted = SUBMITTED.load(Ordering::SeqCst);
            let value = if branch {
                GuestResult::Ok(ResourceOwned::new(Success(91)))
            } else {
                GuestResult::Err(ResourceOwned::new(Failure(92)))
            };
            let error = callback
                .call_with_vm(&mut store, (value, mode))
                .unwrap_err();
            assert!(
                error.to_string().contains(if mode == 0 {
                    "host invocation yielded"
                } else {
                    "host invocation is waiting"
                }),
                "{error}"
            );
            assert_eq!(SUSPENDED.load(Ordering::SeqCst), suspended + 1);
            assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 1);
            assert_eq!(store.vm_mut().host_context().resource_count(), 0);
            assert_eq!(store.vm().call_depth(), 0);
            assert_eq!(
                SUBMITTED.load(Ordering::SeqCst),
                submitted + usize::from(mode == 1)
            );
            assert_eq!(
                CANCELLED.load(Ordering::SeqCst),
                cancelled + usize::from(mode == 1)
            );
            if mode == 1 {
                let mut cx = Context::from_waker(std::task::Waker::noop());
                assert!(store.vm_mut().poll_waiting_host_op(&mut cx).is_pending());
                assert!(
                    !store.vm().is_reusable(),
                    "cancellation acknowledgement is still pending"
                );
                assert_eq!(CLEANED.load(Ordering::SeqCst), cleaned);
                assert_eq!(FUTURE_DROPPED.load(Ordering::SeqCst), dropped);
                ACK.store(true, Ordering::SeqCst);
                assert!(matches!(
                    store.vm_mut().poll_waiting_host_op(&mut cx),
                    Poll::Ready(Ok(()))
                ));
                assert_eq!(CLEANED.load(Ordering::SeqCst), cleaned + 1);
                assert_eq!(FUTURE_DROPPED.load(Ordering::SeqCst), dropped + 1);
                assert_eq!(CANCELLED.load(Ordering::SeqCst), cancelled + 1);
            }
            eprintln!(
                "abort_suspend mode={mode} branch={branch} close=1 live=0 cancel_delta={} future_drop_delta={}",
                CANCELLED.load(Ordering::SeqCst) - cancelled,
                FUTURE_DROPPED.load(Ordering::SeqCst) - dropped
            );
        }
    }
}
#[test]
fn vm_aware_post_admission_close_failure_attempts_pending_sibling_and_preserves_primary() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult, OwnedResult), OwnedResult>("fail_pair")
        .unwrap();
    PENDING_READY.store(false, Ordering::SeqCst);
    let closed = CLOSED.load(Ordering::SeqCst);
    let polls = CLOSE_POLLS.load(Ordering::SeqCst);
    let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
    let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
    let error = callback
        .call_with_vm(
            &mut store,
            (
                GuestResult::Ok(ResourceOwned::new(Success(-101))),
                GuestResult::Err(ResourceOwned::new(Failure(-102))),
            ),
        )
        .unwrap_err();
    assert!(
        matches!(&error, vm::VmError::HostError(message) if message == "callback transport failure"),
        "{error}"
    );
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 2);
    assert_eq!(DROPPED_OK.load(Ordering::SeqCst), dropped_ok);
    assert_eq!(DROPPED_ERR.load(Ordering::SeqCst), dropped_err);
    assert_eq!(store.vm_mut().host_context().resource_count(), 2);
    assert_eq!(CLOSE_POLLS.load(Ordering::SeqCst), polls);
    store
        .vm_mut()
        .execution_scope()
        .begin_close(ResourceCloseReason::Requested)
        .unwrap();
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(
        store
            .vm_mut()
            .execution_scope()
            .poll_close(&mut cx)
            .is_pending()
    );
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 3);
    assert!(!store.vm_mut().host_context().is_scope_quiescent());
    assert_eq!(store.vm_mut().host_context().resource_count(), 1);
    PENDING_READY.store(true, Ordering::SeqCst);
    assert!(
        store
            .vm_mut()
            .execution_scope()
            .poll_close(&mut cx)
            .is_ready()
    );
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
    assert_eq!(DROPPED_OK.load(Ordering::SeqCst), dropped_ok + 1);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 3);
    assert_eq!(DROPPED_ERR.load(Ordering::SeqCst), dropped_err + 1);
    eprintln!(
        "abort_close_error primary_preserved=true initial_sibling_begin=2 failed_open_plus_pending=2 shutdown_retry=1 pending_live=1 terminal_live=0 poll_delta={}",
        CLOSE_POLLS.load(Ordering::SeqCst) - polls
    );
}
#[test]
fn vm_aware_native_opaque_state_owns_taken_resource_after_fatal() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for branch in [true, false] {
        let mut store = callback_store();
        let callback = store
            .vm_script_callback_by_name::<(OwnedResult,), OwnedResult>("retain_before_fatal")
            .unwrap();
        let closed = CLOSED.load(Ordering::SeqCst);
        let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
        let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
        let value = if branch {
            GuestResult::Ok(ResourceOwned::new(Success(211)))
        } else {
            GuestResult::Err(ResourceOwned::new(Failure(212)))
        };
        let error = callback.call_with_vm(&mut store, (value,)).unwrap_err();
        assert!(error.to_string().contains("callback transport failure"));
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
        assert_eq!(DROPPED_OK.load(Ordering::SeqCst), dropped_ok);
        assert_eq!(DROPPED_ERR.load(Ordering::SeqCst), dropped_err);
        let retained = store
            .vm_mut()
            .host_context()
            .take_module_state::<OwnedResult>()
            .unwrap();
        match &retained {
            GuestResult::Ok(value) => assert_eq!(value.as_ref().0, 211),
            GuestResult::Err(value) => assert_eq!(value.as_ref().0, 212),
        }
        drop(retained);
        assert_eq!(
            DROPPED_OK.load(Ordering::SeqCst),
            dropped_ok + usize::from(branch)
        );
        assert_eq!(
            DROPPED_ERR.load(Ordering::SeqCst),
            dropped_err + usize::from(!branch)
        );
    }
}

#[test]
fn vm_aware_transfer_to_surviving_root_capture_before_fatal_stays_usable() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for branch in [true, false] {
        let mut store = vm::Store::from_vm(machine(
            r#"use result_resource;
            pub fn factory() {
                let mut saved = result_resource::make(true);
                fn callback(value: Result<resource<result.success>, resource<result.failure>>, action: int) -> int {
                    if action == 1 => {
                        result_resource::retire_initial();
                        saved = value; result_resource::fatal(); result_resource::borrow(saved)
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
        let value = if branch {
            GuestResult::Ok(ResourceOwned::new(Success(191)))
        } else {
            GuestResult::Err(ResourceOwned::new(Failure(192)))
        };
        let error = callback.call_with_vm(&mut store, (value, 1)).unwrap_err();
        assert!(error.to_string().contains("callback transport failure"));
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
        assert_eq!(store.vm_mut().host_context().resource_count(), 1);
        assert_eq!(
            alias
                .call_with_vm(
                    &mut store,
                    (GuestResult::Ok(ResourceOwned::new(Success(901))), 0)
                )
                .unwrap(),
            if branch { 191 } else { 192 }
        );
        assert_eq!(
            callback
                .call_with_vm(
                    &mut store,
                    (GuestResult::Ok(ResourceOwned::new(Success(902))), -1)
                )
                .unwrap(),
            if branch { 191 } else { 192 }
        );
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
        eprintln!(
            "abort_surviving_capture branch={branch} after_fatal_live=1 borrowed={} final_live=0 close_delta=0",
            if branch { 191 } else { 192 }
        );
    }
}
